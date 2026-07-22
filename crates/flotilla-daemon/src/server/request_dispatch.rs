use std::sync::Arc;

use flotilla_core::{
    agents::{AgentEntry, SharedAgentStateStore},
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
};
use flotilla_protocol::{AgentHookEvent, Command, CommandAction, Message, RepoSelector, Request, Response};
use tracing::warn;

use super::{client_connection::QuerySubscriptions, remote_commands::RemoteCommandRouter};

pub(super) struct RequestDispatcher<'a> {
    daemon: &'a Arc<InProcessDaemon>,
    remote_command_router: &'a RemoteCommandRouter,
    agent_state_store: &'a SharedAgentStateStore,
    session_id: uuid::Uuid,
    query_subscriptions: QuerySubscriptions,
}

impl<'a> RequestDispatcher<'a> {
    pub(super) fn new(
        daemon: &'a Arc<InProcessDaemon>,
        remote_command_router: &'a RemoteCommandRouter,
        agent_state_store: &'a SharedAgentStateStore,
        session_id: uuid::Uuid,
        query_subscriptions: QuerySubscriptions,
    ) -> Self {
        Self { daemon, remote_command_router, agent_state_store, session_id, query_subscriptions }
    }

    pub(super) async fn dispatch(&self, id: u64, request: Request) -> Message {
        match request {
            Request::ListRepos => match self.daemon.list_repos().await {
                Ok(repos) => Message::ok_response(id, Response::ListRepos(repos)),
                Err(e) => Message::error_response(id, e),
            },

            Request::GetState { repo } => match self.daemon.get_state(&RepoSelector::Path(repo)).await {
                Ok(snapshot) => Message::ok_response(id, Response::GetState(Box::new(snapshot))),
                Err(e) => Message::error_response(id, e),
            },

            Request::Execute { command } => {
                if command.action.is_query() {
                    // Query commands: execute synchronously (local or remote)
                    // and return the result directly as a QueryResult.
                    match self.remote_command_router.dispatch_query(command, self.session_id).await {
                        Ok(value) => Message::ok_response(id, Response::QueryResult { command_id: 0, value }),
                        Err(e) => Message::error_response(id, e),
                    }
                } else {
                    // Non-query commands: existing dispatch path
                    match self.remote_command_router.dispatch_execute(command).await {
                        Ok(command_id) => Message::ok_response(id, Response::Execute { command_id }),
                        Err(e) => Message::error_response(id, e),
                    }
                }
            }

            Request::Cancel { command_id } => match self.remote_command_router.dispatch_cancel(command_id).await {
                Ok(()) => Message::ok_response(id, Response::Cancel),
                Err(e) => Message::error_response(id, e),
            },

            Request::Refresh { repo } => {
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

            Request::AddRepo { path } => {
                let command =
                    Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::TrackRepoPath { path } };
                match self.daemon.execute(command).await {
                    Ok(_) => Message::ok_response(id, Response::AddRepo),
                    Err(e) => Message::error_response(id, e),
                }
            }

            Request::RemoveRepo { path } => {
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

            Request::ReplaySince { last_seen } => {
                let last_seen = last_seen.into_iter().map(|entry| (entry.stream, entry.seq)).collect();
                match self.daemon.replay_since(&last_seen).await {
                    Ok(events) => Message::ok_response(id, Response::ReplaySince(events)),
                    Err(e) => Message::error_response(id, e),
                }
            }

            Request::SubscribeQueries { queries } => {
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

            Request::FetchMore { query } => {
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

            Request::GetStatus => match self.daemon.get_status().await {
                Ok(status) => Message::ok_response(id, Response::GetStatus(status)),
                Err(e) => Message::error_response(id, e),
            },

            Request::GetTopology => match self.daemon.get_topology().await {
                Ok(topology) => Message::ok_response(id, Response::GetTopology(topology)),
                Err(e) => Message::error_response(id, e),
            },

            Request::AgentHook { event } => match self.handle_agent_hook(event).await {
                Ok(()) => Message::ok_response(id, Response::AgentHook),
                Err(e) => {
                    warn!(err = %e, "failed to process agent hook event");
                    Message::error_response(id, e)
                }
            },
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
            AgentEventType::Active => flotilla_resources::TerminalAttentionState::Working,
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
        if session
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
            &flotilla_resources::TerminalSessionStatusPatch::ObserveAttention { attention },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}
