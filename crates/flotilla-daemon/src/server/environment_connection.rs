//! Framed crew transport. No HTTP, peer traffic, global subscriptions or
//! administrative requests cross this interface.
use std::{collections::BTreeMap, sync::Arc};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    CallerCrew, CommandAction, CommandCaller, CrewCommandContext, DaemonEvent, EnvironmentId, Message, PrincipalRef, Request, Response,
    ResponseResult,
};
use flotilla_resources::{TerminalSession, TerminalSessionSource, Vessel};
use flotilla_store::StoreQuery;
use flotilla_transport::message::MessageSession;

use super::{remote_commands::RemoteCommandRouter, request_dispatch::RequestDispatcher};

pub(super) async fn environment_crews(daemon: &InProcessDaemon, environment: &EnvironmentId) -> Result<Vec<CallerCrew>, String> {
    let namespace = daemon.provisioning_namespace().await;
    let backend = daemon.resource_backend();
    let sessions = backend
        .query_declared::<TerminalSession>(
            &namespace,
            &StoreQuery { fields: BTreeMap::from([("spec.env_ref".into(), environment.as_str().into())]), ..Default::default() },
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut crews = Vec::new();
    for session in sessions.items {
        let session = session.object;
        let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { continue };
        let Some(crew) = session.status.as_ref().and_then(|status| status.crew.as_ref()) else { continue };
        let vessel = backend.using::<Vessel>(&namespace).get(&context.vessel_ref).await.map_err(|error| error.to_string())?;
        if vessel.spec.convoy_ref != context.convoy || context.namespace != namespace {
            return Err("EnvironmentIdentityInvalid: session ownership disagrees".into());
        }
        crews.push(
            CallerCrew::builder()
                .namespace(namespace.clone())
                .convoy(context.convoy.clone())
                .vessel(context.vessel_ref.clone())
                .role(session.spec.role)
                .crew_id(crew.id.clone())
                .terminal_session(session.metadata.name)
                .build(),
        );
    }
    Ok(crews)
}

fn authorize_context(context: &mut CrewCommandContext, crew: &CallerCrew) -> Result<(), String> {
    if context.namespace.as_ref().is_some_and(|value| value != &crew.namespace)
        || context.convoy.as_ref().is_some_and(|value| value != &crew.convoy)
        || context.vessel_ref.as_ref().is_some_and(|value| value != &crew.vessel)
        || context.role.as_ref().is_some_and(|value| value != &crew.role)
        || context.crew_id.as_ref().is_some_and(|value| value != &crew.crew_id)
    {
        return Err("EnvironmentIdentityMismatch: command names another crew".into());
    }
    *context = CrewCommandContext { crew_id: Some(crew.crew_id.clone()), ..Default::default() };
    Ok(())
}

pub(super) fn authorize(request: &mut Request, crew: &CallerCrew) -> Result<(), String> {
    match request {
        Request::Execute { command } => {
            if command.node_id.is_some() || command.provisioning_target.is_some() || command.context_repo.is_some() {
                return Err("EnvironmentCommandDenied: explicit routing is forbidden".into());
            }
            match &mut command.action {
                CommandAction::CrewComplete { context, force: false, .. }
                | CommandAction::CrewStall { context, .. }
                | CommandAction::CrewPromise { context, .. }
                | CommandAction::QueryCrewCapabilities { context }
                | CommandAction::QueryCrewList { context }
                | CommandAction::QueryMessageContacts { context } => authorize_context(context, crew),
                CommandAction::CrewHandoff { context, target, .. } => {
                    let parts = target.split('/').collect::<Vec<_>>();
                    if parts.len() > 2 && !(parts.len() == 4 && parts[0] == crew.namespace && parts[1] == crew.convoy) {
                        return Err("EnvironmentIdentityMismatch: handoff names another convoy".into());
                    }
                    authorize_context(context, crew)
                }
                CommandAction::QueryExplainConvoy { namespace, name }
                    if name == &crew.convoy && namespace.as_ref().is_none_or(|value| value == &crew.namespace) =>
                {
                    Ok(())
                }
                _ => Err("EnvironmentCommandDenied: command is outside the crew allowlist".into()),
            }
        }
        Request::ArtifactPut { .. } => Ok(()),
        Request::ArtifactList { convoy, .. } if convoy.as_ref().is_none_or(|value| value == &crew.convoy) => {
            *convoy = Some(crew.convoy.clone());
            Ok(())
        }
        Request::AgentHook { event }
            if event.terminal.as_ref().is_some_and(|terminal| {
                terminal.namespace == crew.namespace && Some(&terminal.session_name) == crew.terminal_session.as_ref()
            }) =>
        {
            event.attachable_id = flotilla_agent::attachable::codex_hook_id(None, event.terminal.as_ref());
            Ok(())
        }
        _ => Err("EnvironmentCommandDenied: request is outside the crew allowlist".into()),
    }
}

pub(super) async fn serve(
    session: MessageSession,
    daemon: Arc<InProcessDaemon>,
    router: RemoteCommandRouter,
    environment: EnvironmentId,
    role: Option<String>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let first = tokio::select! {
        result = tokio::time::timeout(super::HELLO_HANDSHAKE_TIMEOUT, session.read()) => result,
        _ = shutdown.changed() => return,
    };
    let Ok(Ok(Some(Message::Hello {
        protocol_version,
        display_name,
        connection_role: Some(flotilla_protocol::ConnectionRole::Client),
        ..
    }))) = first
    else {
        return;
    };
    let build_id = flotilla_daemon_api::build_info::build_id();
    if session
        .write(Message::Hello {
            protocol_version: flotilla_protocol::PROTOCOL_VERSION,
            node_id: daemon.node_id().clone(),
            display_name: flotilla_protocol::hello_display_name(
                daemon.host_name().as_str(),
                build_id,
                flotilla_protocol::PROTOCOL_FINGERPRINT,
            ),
            session_id: daemon.session_id(),
            connection_role: Some(flotilla_protocol::ConnectionRole::Client),
            surface: None,
        })
        .await
        .is_err()
    {
        return;
    }
    if protocol_version != flotilla_protocol::PROTOCOL_VERSION
        || flotilla_protocol::hello_build_info(&display_name)
            .is_none_or(|info| info.protocol_fingerprint != flotilla_protocol::PROTOCOL_FINGERPRINT)
    {
        return;
    }
    loop {
        let message = tokio::select! {
            result = session.read() => result,
            _ = shutdown.changed() => return,
        };
        let Ok(Some(Message::Request { id, mut request })) = message else { return };
        // Re-resolve for every request: a retired session cannot retain authority.
        let identity = async {
            let crews = environment_crews(&daemon, &environment).await?;
            let mut matching = crews.into_iter().filter(|crew| role.as_ref().is_none_or(|role| role == &crew.role));
            let crew = matching.next().ok_or("EnvironmentIdentityUnavailable: no placed crew")?;
            if matching.next().is_some() {
                return Err("EnvironmentIdentityAmbiguous: select a placed crew role".into());
            }
            authorize(&mut request, &crew)?;
            Ok::<_, String>(crew)
        }
        .await;
        let crew = match identity {
            Ok(crew) => crew,
            Err(error) => {
                if session.write(Message::error_response(id, error)).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let caller =
            CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(&crew.namespace), process: None, crew: Some(crew) };
        let mut events = daemon.subscribe();
        let dispatcher = RequestDispatcher::new_with_caller(
            &daemon,
            &router,
            daemon.agent_state_store(),
            uuid::Uuid::new_v4(),
            Default::default(),
            caller,
        )
        .with_placed_terminal_hooks();
        let response = dispatcher.dispatch(id, request).await;
        let command_id = match &response {
            Message::Response { response, .. } => match response.as_ref() {
                ResponseResult::Ok { response } => match response.as_ref() {
                    Response::Execute { command_id } => Some(*command_id),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if session.write(response).await.is_err() {
            return;
        }
        // Relay only this request's completion, never the host's event stream.
        if let Some(command_id) = command_id {
            loop {
                let event = tokio::select! { event = events.recv() => event, _ = shutdown.changed() => return };
                match event {
                    Ok(event @ DaemonEvent::CommandFinished { command_id: finished, .. }) if finished == command_id => {
                        if session.write(Message::Event { event: Box::new(event) }).await.is_err() {
                            return;
                        }
                        break;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    fn placed() -> CallerCrew {
        CallerCrew::builder()
            .namespace("flotilla".into())
            .convoy("own-convoy".into())
            .vessel("vessel-resource".into())
            .role("coder".into())
            .crew_id("own-crew".into())
            .terminal_session("own-terminal".into())
            .build()
    }

    // #3270: every explicit identity dimension must agree with the placement.
    // Generate each dimension, absent/matching/forged, including empty claims.
    #[hegel::test]
    fn explicit_context_cannot_widen_placement(tc: hegel::TestCase) {
        let placed = placed();
        let sampled_dimension = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let sampled_claim = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        // Pin the complete finite domain as well as drawing cases: the development
        // profile must exercise each refusal even with a small case budget.
        for (dimension, claim) in (0..5)
            .flat_map(|dimension| (0..4).map(move |claim| (dimension, claim)))
            .chain(std::iter::once((sampled_dimension, sampled_claim)))
        {
            let mut context = CrewCommandContext::default();
            let (slot, expected) = match dimension {
                0 => (&mut context.namespace, &placed.namespace),
                1 => (&mut context.convoy, &placed.convoy),
                2 => (&mut context.vessel_ref, &placed.vessel),
                3 => (&mut context.role, &placed.role),
                _ => (&mut context.crew_id, &placed.crew_id),
            };
            *slot = match claim {
                0 => None,
                1 => Some(expected.clone()),
                2 => Some("forged".into()),
                _ => Some(String::new()),
            };
            assert_eq!(authorize_context(&mut context, &placed).is_ok(), claim < 2);
            if claim < 2 {
                assert_eq!(context.crew_id.as_deref(), Some("own-crew"));
            }
        }
    }

    // #3270: host administration, global reads and forced settlement are refused.
    #[test]
    fn administrative_requests_and_other_convoys_are_refused() {
        let placed = placed();
        for mut request in [
            Request::Shutdown,
            Request::GetStatus,
            Request::GetTopology,
            Request::ListRepos,
            Request::Execute {
                command: flotilla_protocol::Command::builder()
                    .action(CommandAction::CrewComplete {
                        context: CrewCommandContext::default(),
                        message: None,
                        disposition: None,
                        decision_ledger_ref: None,
                        force: true,
                    })
                    .build(),
            },
            Request::Execute {
                command: flotilla_protocol::Command::builder()
                    .action(CommandAction::CrewHandoff {
                        context: CrewCommandContext::default(),
                        target: "flotilla/foreign/work/coder".into(),
                        message: "handoff".into(),
                        carries: Vec::new(),
                    })
                    .build(),
            },
            Request::ArtifactList { convoy: Some("other-convoy".into()), kind: None, subject: None },
            Request::Execute {
                command: flotilla_protocol::Command::builder()
                    .action(CommandAction::QueryExplainConvoy { namespace: None, name: "other-convoy".into() })
                    .build(),
            },
        ] {
            assert!(authorize(&mut request, &placed).is_err());
        }
        let mut request = Request::ArtifactList { convoy: None, kind: None, subject: None };
        authorize(&mut request, &placed).expect("own artifact list");
        assert!(matches!(request, Request::ArtifactList { convoy: Some(convoy), .. } if convoy == "own-convoy"));
    }
}
