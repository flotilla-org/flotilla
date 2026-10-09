use std::{future::Future, path::Path, sync::Arc, time::Duration};

use flotilla_core::{
    agents::{AgentEntry, SharedAgentStateStore},
    in_process::InProcessDaemon,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    AgentHookEvent, Command, CommandAction, CommandCaller, CommandValue, DaemonEvent, Message, RepoSelector, Request, Response,
};
use tracing::warn;

use super::{caller::missing_crew_message, client_connection::QuerySubscriptions, remote_commands::RemoteCommandRouter};
use crate::artifact::{ArtifactBody, ArtifactPutInput, ArtifactService};

const INTERACTIVE_REQUEST_DEADLINE: Duration = Duration::from_secs(5);

fn is_interactive_request(request: &Request) -> bool {
    // SubscribeQueries registers interest before awaiting result sets, so a
    // cancelled setup could leave a live subscription after an error response.
    // FetchMore performs its side effect only after its last await.
    matches!(request, Request::Execute { command } if command.action.is_query())
        || matches!(request, Request::ListRepos | Request::GetStatus | Request::GetTopology | Request::FetchMore { .. })
}

fn absolute_crew_path(path: &Path, cwd: &str) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    }
}

pub(super) struct RequestDispatcher<'a> {
    daemon: &'a Arc<InProcessDaemon>,
    remote_command_router: &'a RemoteCommandRouter,
    agent_state_store: &'a SharedAgentStateStore,
    session_id: uuid::Uuid,
    query_subscriptions: QuerySubscriptions,
    caller: CommandCaller,
}

impl<'a> RequestDispatcher<'a> {
    #[cfg(test)]
    pub(super) fn new(
        daemon: &'a Arc<InProcessDaemon>,
        remote_command_router: &'a RemoteCommandRouter,
        agent_state_store: &'a SharedAgentStateStore,
        session_id: uuid::Uuid,
        query_subscriptions: QuerySubscriptions,
    ) -> Self {
        Self::new_with_caller(
            daemon,
            remote_command_router,
            agent_state_store,
            session_id,
            query_subscriptions,
            CommandCaller { principal_ref: flotilla_protocol::PrincipalRef::default(), process: None, crew: None },
        )
    }

    pub(super) fn new_with_caller(
        daemon: &'a Arc<InProcessDaemon>,
        remote_command_router: &'a RemoteCommandRouter,
        agent_state_store: &'a SharedAgentStateStore,
        session_id: uuid::Uuid,
        query_subscriptions: QuerySubscriptions,
        caller: CommandCaller,
    ) -> Self {
        Self { daemon, remote_command_router, agent_state_store, session_id, query_subscriptions, caller }
    }

    pub(super) fn dispatch(&self, id: u64, request: Request) -> std::pin::Pin<Box<impl Future<Output = Message> + '_>> {
        Box::pin(async move {
            if is_interactive_request(&request) {
                // Attach queries resolve a plan; terminal creation happens after
                // the client receives that plan, so cancellation cannot leave
                // a half-created local terminal or session.
                match tokio::time::timeout(INTERACTIVE_REQUEST_DEADLINE, self.dispatch_inner(id, request)).await {
                    Ok(response) => response,
                    Err(_) => Message::error_response(id, "resource store busy: interactive request deadline exceeded"),
                }
            } else {
                Box::pin(self.dispatch_inner(id, request)).await
            }
        })
    }

    async fn dispatch_request_shutdown(&self, id: u64, request: Request) -> Message {
        let Request::Shutdown = request else { return Message::error_response(id, "Shutdown request selected the wrong handler") };
        Message::ok_response(id, Response::Shutdown)
    }

    async fn dispatch_request_list_repos(&self, id: u64, request: Request) -> Message {
        let Request::ListRepos = request else { return Message::error_response(id, "ListRepos request selected the wrong handler") };
        match self.daemon.list_repos().await {
            Ok(repos) => Message::ok_response(id, Response::ListRepos(repos)),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn artifact_home_action(&self, node_id: &flotilla_protocol::NodeId, action: CommandAction) -> Result<CommandValue, String> {
        let mut events = self.daemon.subscribe();
        let command_id = self
            .remote_command_router
            .dispatch_execute_for_caller(
                Command { node_id: Some(node_id.clone()), provisioning_target: None, context_repo: None, action },
                Some(self.caller.clone()),
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await {
                    Ok(DaemonEvent::CommandFinished { command_id: finished, result, .. }) if finished == command_id => return Ok(result),
                    Ok(_) => {}
                    Err(error) => return Err(format!("artifact home command event unavailable: {error}")),
                }
            }
        })
        .await
        .map_err(|_| "artifact home command timed out".to_string())?
    }

    async fn dispatch_request_artifact_put(&self, id: u64, request: Request) -> Message {
        let Request::ArtifactPut { kind, subject, summary, media_type, source_path } = request else {
            return Message::error_response(id, "ArtifactPut request selected the wrong handler");
        };
        let result = Box::pin(async {
            let caller = self.caller.crew.as_ref().ok_or_else(|| missing_crew_message("artifact put"))?;
            let subject = if kind == "decision-ledger" && subject.is_empty() { caller.convoy.clone() } else { subject };
            let settings = self.daemon.config_store().load_daemon_config()?;
            let blobs = self.remote_command_router.blob_store()?;
            let backend = self.daemon.resource_backend();
            let namespace = self.daemon.provisioning_namespace().await;
            let service = ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
            let session = service.caller_session(caller).await?;
            let runner = self
                .daemon
                .command_runner_for_environment_ref(&session.spec.env_ref)
                .ok_or_else(|| format!("artifact environment {} is unavailable", session.spec.env_ref))?;
            let source_path = absolute_crew_path(&source_path, &session.spec.cwd);
            let temporary = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
            runner.read_file_to(&source_path, temporary.path()).await?;
            if kind == "decision-ledger" && !summary.is_empty() {
                return Err("decision ledger summary is daemon-owned".to_string());
            }
            let input = ArtifactPutInput::builder()
                .kind(kind)
                .subject(subject)
                .summary(summary)
                .media_type(media_type)
                .body(ArtifactBody::File(temporary.path().to_path_buf()))
                .build();
            // Validation and storage are the complete submission; settlement needs no forge write.
            let (name, spec, owner) = service.prepare_put(caller, input, &settings.artifact_retention_days).await?;
            let target = self
                .daemon
                .resolve_existing_convoy_target(&CommandAction::QueryExplainConvoy {
                    namespace: Some(namespace.clone()),
                    name: caller.convoy.clone(),
                })
                .await?;
            let node_id = match target {
                Some(target) => target.node_id,
                None if self.daemon.has_authoritative_convoy(&namespace, &caller.convoy).await? => self.daemon.node_id().clone(),
                None => return Err("convoy home is unavailable for artifact put".to_string()),
            };
            let document = |spec: &flotilla_resources::ArtifactSpec| {
                serde_json::json!({
                    "apiVersion": "flotilla.work/v1", "kind": "Artifact",
                    "metadata": { "name": name, "ownerReferences": [owner] }, "spec": spec,
                })
            };
            let store = async |spec: &flotilla_resources::ArtifactSpec| match self
                .artifact_home_action(&node_id, CommandAction::ResourceApply { namespace: namespace.clone(), document: document(spec) })
                .await?
            {
                CommandValue::ResourceObject(_) => Ok(()),
                CommandValue::Error { message } => Err(format!("artifact storage failed: {message}")),
                other => Err(format!("unexpected artifact home result: {other:?}")),
            };
            store(&spec).await?;
            Ok(Response::ArtifactPut {
                address: format!("artifact/{name}"),
                view_url: flotilla_core::config::artifact_view_url(&spec, &settings.blob_stores),
                digest: spec.digest,
            })
        })
        .await;
        match result {
            Ok(response) => Message::ok_response(id, response),
            Err(error) => Message::error_response(id, error),
        }
    }

    async fn dispatch_request_artifact_get(&self, id: u64, request: Request) -> Message {
        let Request::ArtifactGet { reference, destination_path } = request else {
            return Message::error_response(id, "ArtifactGet request selected the wrong handler");
        };
        let result = Box::pin(async {
            if let Some(caller) = self.caller.crew.as_ref() {
                let name = reference.strip_prefix("artifact/").unwrap_or(&reference);
                if name.split_once('/').is_some_and(|(namespace, _)| namespace != caller.namespace) {
                    return Err("crew artifact reads cannot cross namespaces".to_string());
                }
            }
            let blobs = self.remote_command_router.blob_store()?;
            let backend = self.daemon.resource_backend();
            let namespace = self.daemon.provisioning_namespace().await;
            let service = ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
            let stores = self.daemon.config_store().load_daemon_config()?.blob_stores;
            let artifact = service.artifact_for_reference(&reference).await?;
            let view_url =
                artifact.as_ref().and_then(|artifact| flotilla_core::config::artifact_view_url(&artifact.spec, &stores)).or_else(|| {
                    crate::blob_store::BlobDigest::parse(&reference)
                        .ok()
                        .and_then(|digest| flotilla_core::config::artifact_view_url_for_digest(digest.as_str(), &stores))
                });
            let (runner, destination_path) = if let Some(caller) = self.caller.crew.as_ref() {
                let session = service.caller_session(caller).await?;
                let runner = self
                    .daemon
                    .command_runner_for_environment_ref(&session.spec.env_ref)
                    .ok_or_else(|| format!("artifact environment {} is unavailable", session.spec.env_ref))?;
                (runner, absolute_crew_path(&destination_path, &session.spec.cwd))
            } else {
                self.daemon.principal_for_surface(self.session_id)?.ok_or("artifact get requires a connected operator principal")?;
                if !destination_path.is_absolute() {
                    return Err("operator artifact destination must be absolute".to_string());
                }
                let runner = self.daemon.local_command_runner().ok_or("local artifact environment is unavailable")?;
                (runner, destination_path)
            };
            let temporary = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
            let size = service.get_to_file(&reference, temporary.path()).await?;
            runner.write_file_from(temporary.path(), &destination_path).await?;
            Ok::<_, String>(Response::ArtifactGet { size, view_url })
        })
        .await;
        match result {
            Ok(response) => Message::ok_response(id, response),
            Err(error) => Message::error_response(id, error),
        }
    }

    async fn dispatch_request_artifact_list(&self, id: u64, request: Request) -> Message {
        let Request::ArtifactList { convoy, kind, subject } = request else {
            return Message::error_response(id, "ArtifactList request selected the wrong handler");
        };
        let result = Box::pin(async {
            let blobs = self.remote_command_router.blob_store()?;
            let backend = self.daemon.resource_backend();
            let namespace = self.daemon.provisioning_namespace().await;
            let service = ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
            let items = service.list(convoy.as_deref(), kind.as_deref(), subject.as_deref()).await?;
            let stores = self.daemon.config_store().load_daemon_config()?.blob_stores;
            let items = items
                .into_iter()
                .map(|item| {
                    let mut value = serde_json::to_value(item.to_k8s_object()).map_err(|error| error.to_string())?;
                    if let Some(url) = flotilla_core::config::artifact_view_url(&item.spec, &stores) {
                        value["view_url"] = serde_json::Value::String(url);
                    }
                    Ok::<_, String>(value)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, String>(Response::ArtifactList { items })
        })
        .await;
        match result {
            Ok(response) => Message::ok_response(id, response),
            Err(error) => Message::error_response(id, error),
        }
    }

    async fn dispatch_request_execute(&self, id: u64, request: Request) -> Message {
        let Request::Execute { command } = request else {
            return Message::error_response(id, "Execute request selected the wrong handler");
        };
        if command.action.is_query() {
            // Query commands: execute synchronously (local or remote)
            // and return the result directly as a QueryResult.
            match self.remote_command_router.dispatch_query(command, self.session_id).await {
                Ok(value) => Message::ok_response(id, Response::QueryResult { command_id: 0, value }),
                Err(e) => Message::error_response(id, e),
            }
        } else {
            // Non-query commands: existing dispatch path
            match self.daemon.principal_for_surface(self.session_id) {
                Ok(principal_ref) => {
                    let mut caller = self.caller.clone();
                    caller.principal_ref = principal_ref.unwrap_or_else(|| self.caller.principal_ref.clone());
                    match self.remote_command_router.dispatch_execute_for_caller(command, Some(caller)).await {
                        Ok(command_id) => Message::ok_response(id, Response::Execute { command_id }),
                        Err(e) => Message::error_response(id, e),
                    }
                }
                Err(e) => Message::error_response(id, e),
            }
        }
    }

    async fn dispatch_request_cancel(&self, id: u64, request: Request) -> Message {
        let Request::Cancel { command_id } = request else {
            return Message::error_response(id, "Cancel request selected the wrong handler");
        };
        match self.remote_command_router.dispatch_cancel(command_id).await {
            Ok(()) => Message::ok_response(id, Response::Cancel),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_refresh(&self, id: u64, request: Request) -> Message {
        let Request::Refresh { repo } = request else { return Message::error_response(id, "Refresh request selected the wrong handler") };
        let command = Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::Refresh { repo: Some(RepoSelector::Path(repo)) },
        };
        match self.daemon.execute(command).await {
            Ok(_) => Message::ok_response(id, Response::Refresh),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_add_repo(&self, id: u64, request: Request) -> Message {
        let Request::AddRepo { path } = request else { return Message::error_response(id, "AddRepo request selected the wrong handler") };
        let command =
            Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::TrackRepoPath { path } };
        match self.daemon.execute(command).await {
            Ok(_) => Message::ok_response(id, Response::AddRepo),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_remove_repo(&self, id: u64, request: Request) -> Message {
        let Request::RemoveRepo { path } = request else {
            return Message::error_response(id, "RemoveRepo request selected the wrong handler");
        };
        let command = Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::UntrackRepo { repo: RepoSelector::Path(path) },
        };
        match self.daemon.execute(command).await {
            Ok(_) => Message::ok_response(id, Response::RemoveRepo),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_replay_since(&self, id: u64, request: Request) -> Message {
        let Request::ReplaySince { last_seen } = request else {
            return Message::error_response(id, "ReplaySince request selected the wrong handler");
        };
        let last_seen = last_seen.into_iter().map(|entry| (entry.stream, entry.seq)).collect();
        match self.daemon.replay_since(&last_seen).await {
            Ok(events) => Message::ok_response(id, Response::ReplaySince(events)),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_subscribe_queries(&self, id: u64, request: Request) -> Message {
        let Request::SubscribeQueries { queries } = request else {
            return Message::error_response(id, "SubscribeQueries request selected the wrong handler");
        };
        // Register interest before computing the replay so no event
        // between the two is dropped; the client ignores any stale
        // delta that races ahead of the replayed result set.
        {
            let mut subscriptions = self.query_subscriptions.write().expect("query subscriptions lock poisoned");
            *subscriptions = queries.iter().map(|cursor| cursor.query.clone()).collect();
        }
        match self.daemon.subscribe_queries(self.session_id, &queries).await {
            Ok(events) => Message::ok_response(id, Response::SubscribeQueries(events)),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_fetch_more(&self, id: u64, request: Request) -> Message {
        let Request::FetchMore { query } = request else {
            return Message::error_response(id, "FetchMore request selected the wrong handler");
        };
        let subscribed = self.query_subscriptions.read().expect("query subscriptions lock poisoned").contains(&query);
        if !subscribed {
            Message::error_response(id, format!("query is not subscribed on this connection: {query}"))
        } else {
            match self.daemon.fetch_more(&query).await {
                Ok(()) => Message::ok_response(id, Response::FetchMore),
                Err(e) => Message::error_response(id, e),
            }
        }
    }

    async fn dispatch_request_observe_focus(&self, id: u64, request: Request) -> Message {
        let Request::ObserveFocus { targets } = request else {
            return Message::error_response(id, "ObserveFocus request selected the wrong handler");
        };
        match self.daemon.observe_surface_focus(self.session_id, targets).await {
            Ok(()) => Message::ok_response(id, Response::ObserveFocus),
            Err(error) => Message::error_response(id, error),
        }
    }

    async fn dispatch_request_subscribe_wait(&self, id: u64, request: Request) -> Message {
        let Request::SubscribeWait { subscription } = request else {
            return Message::error_response(id, "SubscribeWait request selected the wrong handler");
        };
        match self.daemon.subscribe_wait(self.session_id, subscription).await {
            Ok(subscription_id) => Message::ok_response(id, Response::WaitSubscribed { subscription_id }),
            Err(error) => Message::error_response(id, error),
        }
    }

    async fn dispatch_request_get_status(&self, id: u64, request: Request) -> Message {
        let Request::GetStatus = request else { return Message::error_response(id, "GetStatus request selected the wrong handler") };
        match self.daemon.get_status().await {
            Ok(status) => Message::ok_response(id, Response::GetStatus(status)),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_get_topology(&self, id: u64, request: Request) -> Message {
        let Request::GetTopology = request else { return Message::error_response(id, "GetTopology request selected the wrong handler") };
        match self.daemon.get_topology().await {
            Ok(topology) => Message::ok_response(id, Response::GetTopology(topology)),
            Err(e) => Message::error_response(id, e),
        }
    }

    async fn dispatch_request_agent_hook(&self, id: u64, request: Request) -> Message {
        let Request::AgentHook { event } = request else {
            return Message::error_response(id, "AgentHook request selected the wrong handler");
        };
        match self.handle_agent_hook(event).await {
            Ok(()) => Message::ok_response(id, Response::AgentHook),
            Err(e) => {
                warn!(err = %e, "failed to process agent hook event");
                Message::error_response(id, e)
            }
        }
    }

    async fn dispatch_inner(&self, id: u64, request: Request) -> Message {
        match request {
            request @ Request::Shutdown => Box::pin(self.dispatch_request_shutdown(id, request)).await,
            request @ Request::ListRepos => Box::pin(self.dispatch_request_list_repos(id, request)).await,
            request @ Request::ArtifactPut { .. } => Box::pin(self.dispatch_request_artifact_put(id, request)).await,
            request @ Request::ArtifactGet { .. } => Box::pin(self.dispatch_request_artifact_get(id, request)).await,
            request @ Request::ArtifactList { .. } => Box::pin(self.dispatch_request_artifact_list(id, request)).await,
            request @ Request::Execute { .. } => Box::pin(self.dispatch_request_execute(id, request)).await,
            request @ Request::Cancel { .. } => Box::pin(self.dispatch_request_cancel(id, request)).await,
            request @ Request::Refresh { .. } => Box::pin(self.dispatch_request_refresh(id, request)).await,
            request @ Request::AddRepo { .. } => Box::pin(self.dispatch_request_add_repo(id, request)).await,
            request @ Request::RemoveRepo { .. } => Box::pin(self.dispatch_request_remove_repo(id, request)).await,
            request @ Request::ReplaySince { .. } => Box::pin(self.dispatch_request_replay_since(id, request)).await,
            request @ Request::SubscribeQueries { .. } => Box::pin(self.dispatch_request_subscribe_queries(id, request)).await,
            request @ Request::FetchMore { .. } => Box::pin(self.dispatch_request_fetch_more(id, request)).await,
            request @ Request::ObserveFocus { .. } => Box::pin(self.dispatch_request_observe_focus(id, request)).await,
            request @ Request::SubscribeWait { .. } => Box::pin(self.dispatch_request_subscribe_wait(id, request)).await,
            request @ Request::GetStatus => Box::pin(self.dispatch_request_get_status(id, request)).await,
            request @ Request::GetTopology => Box::pin(self.dispatch_request_get_topology(id, request)).await,
            request @ Request::AgentHook { .. } => Box::pin(self.dispatch_request_agent_hook(id, request)).await,
        }
    }

    async fn handle_agent_hook(&self, event: AgentHookEvent) -> Result<(), String> {
        use flotilla_protocol::AgentEventType;

        tracing::info!(
            harness = ?event.harness,
            event_type = ?event.event_type,
            attachable_id = %event.attachable_id,
            session_id = ?event.session_id,
            "received agent hook event"
        );

        {
            let mut store = self.agent_state_store.lock().map_err(|_| "agent state store lock poisoned".to_string())?;

            let attachable_id = if let Some(ref sid) = event.session_id {
                if let Some(existing) = store.lookup_by_session_id(sid) {
                    existing.clone()
                } else {
                    event.attachable_id.clone()
                }
            } else {
                event.attachable_id.clone()
            };

            let changed = if event.event_type == AgentEventType::Ended {
                store.remove(&attachable_id);
                true
            } else if let Some(status) = event.event_type.to_status() {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                let existing = store.get(&attachable_id);
                // TODO(#393): persist event.cwd for CliAgentProvider correlation
                let entry = AgentEntry {
                    harness: event.harness.clone(),
                    status,
                    model: event.model.clone().or_else(|| existing.and_then(|e| e.model.clone())),
                    session_title: existing.and_then(|e| e.session_title.clone()),
                    session_id: event.session_id.clone(),
                    last_event_epoch_secs: now,
                };
                store.upsert(attachable_id, entry);
                true
            } else {
                false
            };

            if changed {
                store.save()?;
            } else {
                // Keep the legacy observer path a no-op when it has no control-plane identity.
            }
        }

        let Some(terminal) = event.terminal.as_ref() else { return Ok(()) };
        let state = match event.event_type {
            AgentEventType::Active | AgentEventType::ToolActive => flotilla_resources::TerminalAttentionState::Working,
            AgentEventType::Idle | AgentEventType::Started => flotilla_resources::TerminalAttentionState::Idle,
            AgentEventType::WaitingForPermission => flotilla_resources::TerminalAttentionState::NeedsInput,
            AgentEventType::Ended | AgentEventType::NoChange => return Ok(()),
        };
        let sessions = self.daemon.resource_backend().using::<flotilla_resources::TerminalSession>(&terminal.namespace);
        let session = sessions.get(&terminal.session_name).await.map_err(|error| error.to_string())?;
        let attention = flotilla_resources::TerminalAttention {
            state,
            as_of: chrono::Utc::now(),
            source: flotilla_resources::TerminalAttentionSource::Hook,
        };
        if event.event_type != AgentEventType::ToolActive
            && session
                .status
                .as_ref()
                .and_then(|status| status.attention.as_ref())
                .is_some_and(|current| !current.should_replace_with(&attention))
        {
            return Ok(());
        }
        flotilla_resources::apply_status_patch(
            &sessions,
            &terminal.session_name,
            &if event.event_type == AgentEventType::ToolActive {
                flotilla_resources::TerminalSessionStatusPatch::ObserveToolActivity { attention }
            } else {
                flotilla_resources::TerminalSessionStatusPatch::ObserveAttention { attention }
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}
