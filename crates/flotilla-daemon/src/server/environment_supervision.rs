//! Additional authority follows supervision: a crew that stall supervision is routed to may act
//! as its Project's supervisor, by the same resolution that routes the turn.
use std::collections::BTreeMap;

use flotilla_core::in_process::InProcessDaemon;
use flotilla_messaging::MessageReceiverSnapshot;
use flotilla_protocol::{CallerCrew, CommandAction, CrewCommandContext, DaemonEvent, Request};
use flotilla_resources::{Convoy, Project, StallSupervisor, TerminalSession, TerminalSessionSource, CONVOY_LABEL, VESSEL_LABEL};
use flotilla_store::{CascadeDeclarations, StoreQuery};

/// The Project the caller supervises: its own convoy's, when supervision reaches the caller's
/// seat there as stall supervision turns are routed, or reached it for a stall in `target`.
async fn binding(daemon: &InProcessDaemon, crew: &CallerCrew, target: Option<&str>) -> Result<String, String> {
    let backend = daemon.resource_backend();
    let convoy = backend.using::<Convoy>(&crew.namespace).get(&crew.convoy).await.map_err(|e| e.to_string())?;
    let project = convoy.spec.project_ref;
    let terminal = crew.terminal_session.as_ref().ok_or("EnvironmentCommandDenied: no terminal binding")?;
    let session = backend.using::<TerminalSession>(&crew.namespace).get(terminal).await.map_err(|e| e.to_string())?;
    let label = |key: &str| session.metadata.labels.get(key).cloned().ok_or("EnvironmentCommandDenied: terminal has no crew seat");
    let seat = StallSupervisor { convoy: label(CONVOY_LABEL)?, vessel: label(VESSEL_LABEL)?, role: session.spec.role.clone() };
    let projects = backend.including_replicas::<Project>(&crew.namespace).list().await.map_err(|e| e.to_string())?.items;
    let convoys = backend.including_replicas::<Convoy>(&crew.namespace).list().await.map_err(|e| e.to_string())?.items;
    let declarations = CascadeDeclarations::load(&backend, &crew.namespace).await.map_err(|e| e.to_string())?;
    let receivers = MessageReceiverSnapshot::load(&backend, &crew.namespace).await.map_err(|e| e.to_string())?;
    let target = target.and_then(|name| convoys.iter().map(|source| &source.object).find(|convoy| convoy.metadata.name == name));
    if flotilla_crew::leaf_engine::supervises(&receivers, &declarations, &project, &projects, &convoys, target, &seat)? {
        return Ok(project);
    }
    Err("EnvironmentCommandDenied: caller is not the project's current supervisor".into())
}

async fn convoy_scope(
    daemon: &InProcessDaemon,
    crew: &CallerCrew,
    project: &str,
    namespace: &Option<String>,
    name: &str,
) -> Result<(), String> {
    if namespace.as_ref().is_some_and(|ns| ns != &crew.namespace) {
        return Err("EnvironmentIdentityMismatch: foreign namespace".into());
    }
    let convoy = daemon.resource_backend().including_replicas::<Convoy>(&crew.namespace).get(name).await.map_err(|e| e.to_string())?;
    if convoy.object.spec.project_ref != project {
        return Err("EnvironmentIdentityMismatch: foreign project".into());
    }
    Ok(())
}

async fn context_scope(daemon: &InProcessDaemon, crew: &CallerCrew, project: &str, context: &mut CrewCommandContext) -> Result<(), String> {
    if let Some(id) = &context.crew_id {
        let sessions = daemon
            .resource_backend()
            .query_declared::<TerminalSession>(
                &crew.namespace,
                &StoreQuery { fields: BTreeMap::from([("status.crew.id".into(), id.clone())]), ..Default::default() },
            )
            .await
            .map_err(|e| e.to_string())?;
        let mut sessions = sessions.items.into_iter();
        let session = sessions.next().ok_or("EnvironmentIdentityMismatch: unknown target crew")?;
        if sessions.next().is_some() {
            return Err("EnvironmentIdentityMismatch: ambiguous target crew".into());
        }
        let TerminalSessionSource::Agent { context: placed, .. } = session.object.spec.source else {
            return Err("EnvironmentIdentityMismatch: target is not crew".into());
        };
        if context.convoy.as_ref().is_some_and(|name| name != &placed.convoy)
            || context.vessel_ref.as_ref().is_some_and(|name| name != &placed.vessel_ref)
            || context.role.as_ref().is_some_and(|role| role != &session.object.spec.role)
        {
            return Err("EnvironmentIdentityMismatch: conflicting target crew".into());
        }
        context.convoy = Some(placed.convoy);
        context.vessel_ref = Some(placed.vessel_ref);
        context.role = Some(session.object.spec.role);
    }
    let convoy = context.convoy.clone().unwrap_or_else(|| crew.convoy.clone());
    convoy_scope(daemon, crew, project, &context.namespace, &convoy).await?;
    if let Some(vessel) = &context.vessel_ref {
        let vessel = daemon
            .resource_backend()
            .including_replicas::<flotilla_resources::Vessel>(&crew.namespace)
            .get(vessel)
            .await
            .map_err(|e| e.to_string())?;
        if vessel.object.spec.convoy_ref != convoy {
            return Err("EnvironmentIdentityMismatch: foreign vessel".into());
        }
    }
    context.namespace = Some(crew.namespace.clone());
    context.convoy = Some(convoy);
    Ok(())
}

/// Returns the scope for filtering otherwise-global completion projections.
pub(super) async fn authorize(daemon: &InProcessDaemon, request: &mut Request, crew: &CallerCrew) -> Result<Option<String>, String> {
    if super::environment_connection::authorize(request, crew).is_ok() {
        return Ok(None);
    }
    let Request::Execute { command } = request else { return Err("EnvironmentCommandDenied: supervisor request denied".into()) };
    if command.node_id.is_some() || command.provisioning_target.is_some() || command.context_repo.is_some() {
        return Err("EnvironmentCommandDenied: explicit routing is forbidden".into());
    }
    // Reject unsupported actions before doing any authority-store work. Never
    // expose binding lookup errors (including record existence) to a denied caller.
    if !matches!(
        &command.action,
        CommandAction::CrewSupervise { .. }
            | CommandAction::CrewFail { .. }
            | CommandAction::QueryCrewList { .. }
            | CommandAction::QueryCrewCapabilities { .. }
            | CommandAction::QueryMessageContacts { .. }
            | CommandAction::ConvoyAbandon { .. }
            | CommandAction::ConvoyResume { .. }
            | CommandAction::QueryExplainConvoy { .. }
            | CommandAction::ConvoyStart { .. }
            | CommandAction::QueryExplainProject { .. }
            | CommandAction::QueryDispatchBoard { .. }
            | CommandAction::QueryDispatchQueue { .. }
            | CommandAction::QueryFleetList { .. }
            | CommandAction::QueryCrewStalls { .. }
    ) {
        return Err("EnvironmentCommandDenied: command is outside supervisor authority".into());
    }
    // A ruling on a stall names its convoy; whoever that stall's turn went to may rule on it.
    let target = match &command.action {
        CommandAction::CrewSupervise { convoy, .. } => Some(convoy.clone()),
        CommandAction::CrewFail { context, .. } => context.convoy.clone(),
        _ => None,
    };
    let project = binding(daemon, crew, target.as_deref())
        .await
        .map_err(|_| "EnvironmentCommandDenied: no current supervisor binding".to_string())?;
    match &mut command.action {
        CommandAction::CrewSupervise { namespace, convoy, actor_crew_id, .. } => {
            convoy_scope(daemon, crew, &project, namespace, convoy).await?;
            if actor_crew_id.as_ref().is_some_and(|id| id != &crew.crew_id) {
                return Err("EnvironmentIdentityMismatch: forged supervisor".into());
            }
            *actor_crew_id = Some(crew.crew_id.clone());
            *namespace = Some(crew.namespace.clone());
        }
        CommandAction::CrewFail { context, message, .. } => {
            context_scope(daemon, crew, &project, context).await?;
            let reference = context.vessel_ref.as_ref().ok_or("EnvironmentIdentityMismatch: missing target vessel")?;
            let vessel = daemon
                .resource_backend()
                .including_replicas::<flotilla_resources::Vessel>(&crew.namespace)
                .get(reference)
                .await
                .map_err(|e| e.to_string())?;
            // CrewFail's native handler is operator-only. A bound supervisor
            // uses the existing audited supervision ruling, never an operator override.
            // That handler requires this vessel/role to name the stalled obligation.
            command.action = CommandAction::CrewSupervise {
                namespace: Some(crew.namespace.clone()),
                convoy: context.convoy.clone().ok_or("EnvironmentIdentityMismatch: missing target convoy")?,
                vessel: vessel.object.spec.vessel_name,
                role: context.role.clone().ok_or("EnvironmentIdentityMismatch: missing target role")?,
                operation: flotilla_protocol::CrewSupervisionAction::Fail,
                message: message.clone(),
                actor_crew_id: Some(crew.crew_id.clone()),
            };
        }
        CommandAction::QueryCrewList { context }
        | CommandAction::QueryCrewCapabilities { context }
        | CommandAction::QueryMessageContacts { context } => context_scope(daemon, crew, &project, context).await?,
        CommandAction::ConvoyAbandon { namespace, name, .. }
        | CommandAction::ConvoyResume { namespace, name, .. }
        | CommandAction::QueryExplainConvoy { namespace, name } => {
            convoy_scope(daemon, crew, &project, namespace, name).await?;
            *namespace = Some(crew.namespace.clone());
        }
        CommandAction::ConvoyStart { intent } => {
            // Intent has no direct host/repository/checkout override. Admission
            // resolves its issue/change-request inputs within this project charter.
            if intent.project_ref != project || intent.namespace.as_ref().is_some_and(|ns| ns != &crew.namespace) {
                return Err("EnvironmentIdentityMismatch: foreign dispatch project".into());
            }
            intent.namespace = Some(crew.namespace.clone());
        }
        CommandAction::QueryExplainProject { name } if name == &project => {}
        CommandAction::QueryDispatchBoard { project: selected } | CommandAction::QueryDispatchQueue { project: selected } => {
            if selected.as_ref().is_some_and(|value| value != &project) {
                return Err("EnvironmentIdentityMismatch: foreign project".into());
            }
            *selected = Some(project.clone());
        }
        CommandAction::QueryFleetList { project: selected, convoy, crew_id } => {
            if selected.as_ref().is_some_and(|value| value != &project) || crew_id.is_some() {
                return Err("EnvironmentIdentityMismatch: foreign fleet scope".into());
            }
            if let Some(name) = convoy {
                convoy_scope(daemon, crew, &project, &None, name).await?;
            }
            *selected = Some(project.clone());
        }
        // This query has only a `full` flag, no project selector. Its response
        // contains observed_at/full plus rows; scope_value filters rows and
        // recomputes their only cross-row aggregate before either wire path.
        CommandAction::QueryCrewStalls { .. } => {}
        _ => return Err("EnvironmentCommandDenied: command is outside supervisor authority".into()),
    }
    Ok(Some(project))
}

fn scope_value(value: &mut flotilla_protocol::CommandValue, namespace: &str, project: &str) {
    if let flotilla_protocol::CommandValue::CrewStalls(stalls) = value {
        stalls.rows.retain(|row| row.namespace == namespace && row.project.as_deref() == Some(project));
        // Counts must not reveal stalls in another project.
        let mut counts = BTreeMap::new();
        for row in &stalls.rows {
            *counts.entry(row.cause_group.clone()).or_insert(0) += 1;
        }
        for row in &mut stalls.rows {
            row.shared_cause_count = counts[&row.cause_group];
        }
    }
}

pub(super) fn scope_response(message: &mut flotilla_protocol::Message, namespace: &str, project: &str) {
    use flotilla_protocol::{Message, Response, ResponseResult};
    if let Message::Response { response, .. } = message {
        if let ResponseResult::Ok { response } = response.as_mut() {
            if let Response::QueryResult { value, .. } = response.as_mut() {
                scope_value(value, namespace, project);
            }
        }
    }
}

pub(super) fn scope_completion(event: &mut DaemonEvent, namespace: &str, project: &str) {
    if let DaemonEvent::CommandFinished { result, .. } = event {
        scope_value(result, namespace, project);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Project-scoped stall reads exclude foreign rows and foreign correlation
    // counts, even when the underlying host projection grouped them together.
    #[test]
    fn stall_projection_hides_foreign_rows_and_counts() {
        let rows = [("flotilla", "project-a"), ("flotilla", "project-b"), ("other", "project-a")]
            .into_iter()
            .map(|(namespace, project)| {
                flotilla_protocol::CrewStallRow::builder()
                    .namespace(namespace.into())
                    .project(project.into())
                    .convoy("convoy".into())
                    .convoy_display_name("convoy".into())
                    .vessel("work".into())
                    .role("coder".into())
                    .evidence("stalled".into())
                    .cause_group("same".into())
                    .shared_cause_count(3)
                    .artifacts(Vec::new())
                    .build()
            })
            .collect();
        let mut event = DaemonEvent::CommandFinished {
            command_id: 1,
            node_id: flotilla_protocol::NodeId::new("host"),
            repo_identity: flotilla_daemon_api::action_support::empty_repo_identity(),
            repo: None,
            result: flotilla_protocol::CommandValue::CrewStalls(Box::new(flotilla_protocol::CrewStallsResponse {
                rows,
                ..Default::default()
            })),
        };
        // Queries return directly, without a later completion event. Apply the
        // same scope to that actual dispatcher path before writing the response.
        let DaemonEvent::CommandFinished { result, .. } = &event else { panic!("completion") };
        let mut response =
            flotilla_protocol::Message::ok_response(1, flotilla_protocol::Response::QueryResult { command_id: 0, value: result.clone() });
        scope_response(&mut response, "flotilla", "project-a");
        let flotilla_protocol::Message::Response { response, .. } = response else { panic!("response") };
        let flotilla_protocol::ResponseResult::Ok { response } = *response else { panic!("ok") };
        let flotilla_protocol::Response::QueryResult { value: flotilla_protocol::CommandValue::CrewStalls(stalls), .. } = *response else {
            panic!("stalls")
        };
        assert_eq!(stalls.rows.len(), 1);
        assert_eq!(stalls.rows[0].shared_cause_count, 1);
        scope_completion(&mut event, "flotilla", "project-a");
        let DaemonEvent::CommandFinished { result: flotilla_protocol::CommandValue::CrewStalls(stalls), .. } = event else {
            panic!("stalls")
        };
        assert_eq!(stalls.rows.len(), 1);
        assert_eq!(stalls.rows[0].shared_cause_count, 1);
    }
}
