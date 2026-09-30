use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc, time::Duration};

use flotilla_core::{
    agents::{AgentEntry, SharedAgentStateStore},
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{ChannelLabel, CommandRunner},
};
use flotilla_protocol::{
    AgentHookEvent, Command, CommandAction, CommandCaller, CommandValue, DaemonEvent, EnvironmentId, LeafAddress, Message, RepoSelector,
    Request, Response,
};
use flotilla_resources::{expected_change_request_leaves, select_convoy_children, Checkout, Convoy, ResourceBackend};
use tracing::warn;

use super::{client_connection::QuerySubscriptions, remote_commands::RemoteCommandRouter};
use crate::{
    artifact::{ArtifactBody, ArtifactPutInput, ArtifactService},
    blob_store::BlobDigest,
};

fn absolute_crew_path(path: &Path, cwd: &str) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    }
}

// Execute gh in the crew environment, where the repository-scoped credential
// is staged. The token stays in that process and never enters daemon memory.
const GITHUB_LEDGER_API: &str = r#"set -eu
GITHUB_TOKEN_FILE=$1
shift
export GITHUB_TOKEN_FILE
if [ -z "${GITHUB_TOKEN_FILE:-}" ] || [ ! -s "$GITHUB_TOKEN_FILE" ]; then
  echo 'github-crew-pr credential missing or empty (GITHUB_TOKEN_FILE)' >&2
  exit 1
fi
GH_TOKEN=$(cat "$GITHUB_TOKEN_FILE")
if [ -z "$GH_TOKEN" ]; then
  echo 'github-crew-pr credential missing or empty (GITHUB_TOKEN_FILE)' >&2
  exit 1
fi
GH_HOST=github.com
export GH_TOKEN GH_HOST
unset GITHUB_TOKEN GH_ENTERPRISE_TOKEN
exec gh api "$@"
"#;

// Projection needs both the resource identity and the execution world's
// runner, cwd, and delivered credential metadata.
#[allow(clippy::too_many_arguments)]
async fn project_decision_ledger(
    backend: &ResourceBackend,
    namespace: &str,
    convoy_name: &str,
    producer: &str,
    body: &[u8],
    runner: &dyn CommandRunner,
    cwd: &Path,
    delivery_env: &BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    let convoy = backend.including_replicas::<Convoy>(namespace).get(convoy_name).await.map_err(|error| error.to_string())?.object;
    let sources = backend.including_replicas::<Checkout>(namespace).list().await.map_err(|error| error.to_string())?;
    let checkouts = select_convoy_children(&convoy, &sources.items);
    let leaves = expected_change_request_leaves(&convoy, &checkouts)?;
    if leaves.is_empty() {
        return Ok(None);
    }
    let Some(leaf) = leaves.first() else {
        return Err("decision ledger projection requires exactly one bound change request".to_string());
    };
    if leaves.iter().any(|candidate| candidate.address != leaf.address) {
        return Err("decision ledger projection requires exactly one bound change request".to_string());
    }
    let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
        return Err("decision ledger projection has an invalid change request binding".to_string());
    };
    let text = std::str::from_utf8(body).map_err(|_| "decision ledger must be UTF-8".to_string())?;
    let digest = BlobDigest::of(body);
    let name = flotilla_resources::artifact_record_name(convoy_name, producer, "decision-ledger", convoy_name);
    let marker = format!("<!-- flotilla-decision-ledger:{name}:{} -->", digest.as_str());
    let endpoint = format!("repos/{scope}/issues/{number}/comments");
    let credential_path = if service == "github.com" { "GITHUB_TOKEN_FILE" } else { "FORGEJO_TOKEN_FILE" };
    let token_file =
        delivery_env.get(credential_path).ok_or_else(|| format!("{credential_path} is absent from the credential delivery record"))?;
    let forgejo_api_url = if service == "github.com" {
        None
    } else {
        Some(delivery_env.get("FORGEJO_API_URL").ok_or("FORGEJO_API_URL is absent from the credential delivery record")?)
    };
    // The comment itself records the projection identity. If the forge accepts a
    // POST but its response or the artifact write is lost, a retry can find it.
    let existing = if service == "github.com" {
        let listing = runner
            .run(
                "sh",
                &["-c", GITHUB_LEDGER_API, "github-ledger-list", token_file, &format!("{endpoint}?per_page=100"), "--paginate", "--slurp"],
                cwd,
                &ChannelLabel::Default,
            )
            .await
            .map_err(|error| format!("GitHub ledger comment lookup failed: {error}"))?;
        serde_json::from_str::<Vec<Vec<serde_json::Value>>>(&listing)
            .map_err(|error| format!("parse ledger comments: {error}"))?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
    } else {
        const FORGEJO_LIST: &str = r#"set -eu
FORGEJO_TOKEN_FILE=$1
FORGEJO_API_URL=$2
shift 2
: "${FORGEJO_API_URL:?missing Forgejo API URL}"
: "${FORGEJO_TOKEN_FILE:?missing Forgejo token file}"
test -s "$FORGEJO_TOKEN_FILE"
token=$(cat "$FORGEJO_TOKEN_FILE")
curl --fail-with-body --silent --show-error \
  -H "Authorization: token $token" \
  "${FORGEJO_API_URL%/}/repos/$1/issues/$2/comments?limit=100&page=$3"
"#;
        let number = number.to_string();
        let mut comments = Vec::new();
        let mut reached_end = false;
        for page in 1..=1000 {
            let page = page.to_string();
            let listing = runner
                .run(
                    "sh",
                    &["-c", FORGEJO_LIST, "list-ledgers", token_file, forgejo_api_url.expect("Forgejo API URL"), scope, &number, &page],
                    cwd,
                    &ChannelLabel::Default,
                )
                .await?;
            let batch =
                serde_json::from_str::<Vec<serde_json::Value>>(&listing).map_err(|error| format!("parse ledger comments: {error}"))?;
            if batch.is_empty() {
                reached_end = true;
                break;
            }
            comments.extend(batch);
        }
        if !reached_end {
            return Err("ledger comment listing exceeded 1000 pages".to_string());
        }
        comments
    };
    if let Some(url) = existing
        .iter()
        .find(|comment| comment.get("body").and_then(serde_json::Value::as_str).is_some_and(|body| body.trim_end().ends_with(&marker)))
        .and_then(|comment| comment.get("html_url"))
        .and_then(serde_json::Value::as_str)
        .filter(|url| url.starts_with("https://"))
    {
        return Ok(Some(url.to_string()));
    }
    let input = serde_json::to_vec(&serde_json::json!({ "body": format!("{text}\n{marker}") })).map_err(|error| error.to_string())?;
    let response = if service == "github.com" {
        runner
            .run_with_input(
                "sh",
                &["-c", GITHUB_LEDGER_API, "github-ledger-post", token_file, "--method", "POST", &endpoint, "--input", "-"],
                cwd,
                &ChannelLabel::Default,
                &input,
            )
            .await
            .map_err(|error| format!("GitHub ledger comment post failed: {error}"))?
    } else {
        // The scoped Forgejo credential is staged in the crew environment.
        // The daemon supplies the JSON body on stdin and never reads the token.
        const FORGEJO_COMMENT: &str = r#"set -eu
FORGEJO_TOKEN_FILE=$1
FORGEJO_API_URL=$2
shift 2
: "${FORGEJO_API_URL:?missing Forgejo API URL}"
: "${FORGEJO_TOKEN_FILE:?missing Forgejo token file}"
test -s "$FORGEJO_TOKEN_FILE"
token=$(cat "$FORGEJO_TOKEN_FILE")
curl --fail-with-body --silent --show-error -X POST \
  -H "Authorization: token $token" -H "Content-Type: application/json" \
  --data-binary @- "${FORGEJO_API_URL%/}/repos/$1/issues/$2/comments"
"#;
        let number = number.to_string();
        runner
            .run_with_input(
                "sh",
                &["-c", FORGEJO_COMMENT, "project-ledger", token_file, forgejo_api_url.expect("Forgejo API URL"), scope, &number],
                cwd,
                &ChannelLabel::Default,
                &input,
            )
            .await?
    };
    serde_json::from_str::<serde_json::Value>(&response)
        .map_err(|error| format!("parse projected ledger comment: {error}"))?
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .map(|url| Some(url.to_string()))
        .ok_or_else(|| "projected ledger comment has no HTTPS URL".to_string())
}

#[allow(clippy::too_many_arguments)]
async fn project_decision_ledger_once(
    backend: &ResourceBackend,
    namespace: &str,
    convoy: &str,
    producer: &str,
    body: &[u8],
    runner: &dyn CommandRunner,
    cwd: &Path,
    delivery_env: &BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    let name = flotilla_resources::artifact_record_name(convoy, producer, "decision-ledger", convoy);
    let existing = match backend.including_replicas::<flotilla_resources::Artifact>(namespace).get(&name).await {
        Ok(record) => Some(record.object),
        Err(flotilla_resources::ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    let digest = BlobDigest::of(body);
    if let Some(url) = existing
        .as_ref()
        .filter(|record| record.spec.digest == digest.as_str())
        .and_then(|record| record.spec.summary.get("comment_url"))
        .and_then(serde_json::Value::as_str)
    {
        return Ok(Some(url.to_string()));
    }
    let mut last_error = None;
    for attempt in 0..3 {
        match project_decision_ledger(backend, namespace, convoy, producer, body, runner, cwd, delivery_env).await {
            Ok(url) => return Ok(url),
            Err(error) => last_error = Some(error),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
        }
    }
    Err(last_error.expect("projection attempted at least once"))
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
        Self::new_with_caller(daemon, remote_command_router, agent_state_store, session_id, query_subscriptions, CommandCaller {
            principal_ref: flotilla_protocol::PrincipalRef::default(),
            process: None,
            crew: None,
        })
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
        Box::pin(self.dispatch_inner(id, request))
    }

    async fn dispatch_inner(&self, id: u64, request: Request) -> Message {
        match request {
            Request::Shutdown => Message::ok_response(id, Response::Shutdown),
            Request::ListRepos => match self.daemon.list_repos().await {
                Ok(repos) => Message::ok_response(id, Response::ListRepos(repos)),
                Err(e) => Message::error_response(id, e),
            },
            Request::ArtifactPut { kind, subject, mut summary, media_type, source_path } => {
                let result = Box::pin(async {
                    let caller = self.caller.crew.as_ref().ok_or("artifact put requires a calling crew session")?;
                    let subject = if kind == "decision-ledger" && subject.is_empty() { caller.convoy.clone() } else { subject };
                    let config = self.daemon.config_store();
                    let settings = config.load_daemon_config()?;
                    let blobs = self.remote_command_router.blob_store()?;
                    let backend = self.daemon.resource_backend();
                    let namespace = self.daemon.provisioning_namespace().await;
                    let service = ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
                    let session = service.caller_session(caller).await?;
                    let runner = self
                        .daemon
                        .command_runner_for_environment(&EnvironmentId::new(session.spec.env_ref.clone()))
                        .ok_or_else(|| format!("artifact environment {} is unavailable", session.spec.env_ref))?;
                    let source_path = absolute_crew_path(&source_path, &session.spec.cwd);
                    let temporary = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
                    runner.read_file_to(&source_path, temporary.path()).await?;
                    if kind == "decision-ledger" {
                        if subject != caller.convoy {
                            return Err("decision ledger subject must be its convoy".to_string());
                        }
                        if !summary.is_empty() {
                            return Err("decision ledger summary is daemon-owned".to_string());
                        }
                        // Validation runs before the forge write; a path is never treated as content.
                        let body = crate::artifact::read_decision_ledger(temporary.path())?;
                        let delivery_env = self.daemon.ledger_delivery_environment(&session.spec.env_ref).await?;
                        let comment_url = project_decision_ledger_once(
                            &backend,
                            &namespace,
                            &caller.convoy,
                            &session.spec.role,
                            &body,
                            runner.as_ref(),
                            Path::new(&session.spec.cwd),
                            &delivery_env,
                        )
                        .await
                        .map_err(|error| format!("decision ledger forge projection failed: {error}"))?;
                        if let Some(comment_url) = comment_url {
                            summary.insert("comment_url".to_string(), serde_json::Value::String(comment_url));
                        }
                    }
                    let input = ArtifactPutInput::builder()
                        .kind(kind)
                        .subject(subject)
                        .summary(summary)
                        .media_type(media_type)
                        .body(ArtifactBody::File(temporary.path().to_path_buf()))
                        .build();
                    let target = self
                        .daemon
                        .resolve_existing_convoy_target(&CommandAction::QueryExplainConvoy {
                            namespace: Some(namespace.clone()),
                            name: caller.convoy.clone(),
                        })
                        .await?;
                    match target {
                        Some(target) if target.node_id != *self.daemon.node_id() => {
                            let (name, spec, owner) = service
                                .prepare_put(caller, input, &settings.artifact_retention_days)
                                .await
                                .map_err(|error| format!("artifact put failed: {error}"))?;
                            let digest = spec.digest.clone();
                            let document = serde_json::json!({
                                "apiVersion": "flotilla.work/v1",
                                "kind": "Artifact",
                                "metadata": { "name": name, "ownerReferences": [owner] },
                                "spec": spec,
                            });
                            let mut events = self.daemon.subscribe();
                            let command_id = self
                                .remote_command_router
                                .dispatch_execute_for_caller(
                                    Command {
                                        node_id: Some(target.node_id),
                                        provisioning_target: None,
                                        context_repo: None,
                                        action: CommandAction::ResourceApply { namespace, document },
                                    },
                                    Some(self.caller.clone()),
                                )
                                .await
                                .map_err(|error| format!("artifact storage dispatch failed: {error}"))?;
                            let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                                loop {
                                    match events.recv().await {
                                        Ok(DaemonEvent::CommandFinished { command_id: finished, result, .. }) if finished == command_id => {
                                            break Ok(result);
                                        }
                                        Ok(_) => {}
                                        Err(error) => break Err(format!("artifact home command event unavailable: {error}")),
                                    }
                                }
                            })
                            .await
                            .map_err(|_| "artifact home command timed out".to_string())??;
                            match result {
                                CommandValue::ResourceObject(_) => Ok(Response::ArtifactPut {
                                    address: format!("artifact/{name}"),
                                    view_url: flotilla_core::config::artifact_view_url(&spec, &settings.blob_stores),
                                    digest,
                                }),
                                CommandValue::Error { message } => Err(format!("artifact storage failed: {message}")),
                                other => Err(format!("unexpected artifact home result: {other:?}")),
                            }
                        }
                        Some(_) => {
                            let object = service
                                .put(caller, input, &settings.artifact_retention_days)
                                .await
                                .map_err(|error| format!("artifact put failed: {error}"))?;
                            Ok(Response::ArtifactPut {
                                address: format!("artifact/{}", object.metadata.name),
                                view_url: flotilla_core::config::artifact_view_url(&object.spec, &settings.blob_stores),
                                digest: object.spec.digest,
                            })
                        }
                        None => {
                            if !self.daemon.has_authoritative_convoy(&namespace, &caller.convoy).await? {
                                return Err("convoy home is unavailable for artifact put".to_string());
                            }
                            let object = service
                                .put(caller, input, &settings.artifact_retention_days)
                                .await
                                .map_err(|error| format!("artifact put failed: {error}"))?;
                            Ok(Response::ArtifactPut {
                                address: format!("artifact/{}", object.metadata.name),
                                view_url: flotilla_core::config::artifact_view_url(&object.spec, &settings.blob_stores),
                                digest: object.spec.digest,
                            })
                        }
                    }
                })
                .await;
                match result {
                    Ok(response) => Message::ok_response(id, response),
                    Err(error) => Message::error_response(id, error),
                }
            }
            Request::ArtifactGet { reference, destination_path } => {
                let result = Box::pin(async {
                    let caller = self.caller.crew.as_ref().ok_or("artifact get requires a calling crew session")?;
                    let blobs = self.remote_command_router.blob_store()?;
                    let backend = self.daemon.resource_backend();
                    let namespace = self.daemon.provisioning_namespace().await;
                    let service = ArtifactService { backend: &backend, blobs: blobs.as_ref(), namespace: &namespace };
                    let session = service.caller_session(caller).await?;
                    let stores = self.daemon.config_store().load_daemon_config()?.blob_stores;
                    let artifact = service.artifact_for_reference(&reference).await?;
                    let view_url = artifact
                        .as_ref()
                        .and_then(|artifact| flotilla_core::config::artifact_view_url(&artifact.spec, &stores))
                        .or_else(|| {
                            crate::blob_store::BlobDigest::parse(&reference)
                                .ok()
                                .and_then(|digest| flotilla_core::config::artifact_view_url_for_digest(digest.as_str(), &stores))
                        });
                    let runner = self
                        .daemon
                        .command_runner_for_environment(&EnvironmentId::new(session.spec.env_ref.clone()))
                        .ok_or_else(|| format!("artifact environment {} is unavailable", session.spec.env_ref))?;
                    let destination_path = absolute_crew_path(&destination_path, &session.spec.cwd);
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
            Request::ArtifactList { convoy, kind, subject } => {
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

            Request::ObserveFocus { targets } => match self.daemon.observe_surface_focus(self.session_id, targets).await {
                Ok(()) => Message::ok_response(id, Response::ObserveFocus),
                Err(error) => Message::error_response(id, error),
            },

            Request::SubscribeWait { subscription } => match self.daemon.subscribe_wait(self.session_id, subscription).await {
                Ok(subscription_id) => Message::ok_response(id, Response::WaitSubscribed { subscription_id }),
                Err(error) => Message::error_response(id, error),
            },

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

#[cfg(test)]
mod ledger_projection_tests {
    use std::{path::PathBuf, sync::Mutex};

    use async_trait::async_trait;
    use flotilla_core::providers::CommandOutput;
    use flotilla_resources::{
        ArtifactSpec, BoundChangeRequest, ConvoyRepositorySpec, ConvoySpec, InMemoryBackend, InputMeta, RepositoryKey,
    };

    use super::*;

    fn github_delivery_env() -> BTreeMap<String, String> {
        BTreeMap::from([("GITHUB_TOKEN_FILE".to_string(), "/staged/github/token".to_string())])
    }

    fn forgejo_delivery_env() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("FORGEJO_TOKEN_FILE".to_string(), "/staged/forgejo/token".to_string()),
            ("FORGEJO_API_URL".to_string(), "https://forgejo.example/api/v1".to_string()),
        ])
    }

    struct IsolatedGithubRunner {
        _directory: tempfile::TempDir,
        path: PathBuf,
        token_file: Option<PathBuf>,
    }

    impl IsolatedGithubRunner {
        fn new(token: Option<&str>) -> Self {
            let directory = tempfile::tempdir().expect("test directory");
            let path = directory.path().join("bin");
            std::fs::create_dir(&path).expect("fake gh directory");
            let gh = path.join("gh");
            std::fs::write(
                &gh,
                "#!/bin/sh\n[ -s \"$GITHUB_TOKEN_FILE\" ] || { echo 'GitHub token file was not exported' >&2; exit 1; }\n[ \"$GH_TOKEN\" = scoped-test-token ] || { echo 'gh auth login required' >&2; exit 1; }\n[ \"$GH_HOST\" = github.com ] || { echo 'wrong GitHub host' >&2; exit 1; }\ncase \" $* \" in\n  *' --method POST '*) cat >/dev/null; printf '%s\\n' '{\"html_url\":\"https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7\"}' ;;\n  *) printf '%s\\n' '[[]]' ;;\nesac\n",
            )
            .expect("fake gh");
            let curl = path.join("curl");
            std::fs::write(
                &curl,
                "#!/bin/sh\ncase \" $* \" in *'Authorization: token scoped-test-token'*) ;; *) echo 'missing scoped Forgejo token' >&2; exit 1 ;; esac\ncase \" $* \" in *'https://forgejo.example/api/v1/repos/acme/repo/issues/42/comments'*) ;; *) echo 'wrong Forgejo API URL' >&2; exit 1 ;; esac\ncase \" $* \" in *' -X POST '*) cat >/dev/null; printf '%s\\n' '{\"html_url\":\"https://forgejo.example/acme/repo/pulls/42#issuecomment-7\"}' ;; *) printf '%s\\n' '[]' ;; esac\n",
            )
            .expect("fake curl");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).expect("executable fake gh");
                std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o700)).expect("executable fake curl");
            }
            let token_file = token.map(|value| {
                let file = directory.path().join("token");
                std::fs::write(&file, value).expect("test token");
                file
            });
            Self { _directory: directory, path, token_file }
        }

        async fn execute(&self, cmd: &str, args: &[&str], input: Option<&[u8]>) -> Result<String, String> {
            use tokio::io::AsyncWriteExt;

            let mut command = tokio::process::Command::new(cmd);
            command.args(args).env_clear().env("PATH", format!("{}:/usr/bin:/bin", self.path.display())).env("GH_HOST", "wrong.example");
            let mut child = command
                .stdin(if input.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(|error| error.to_string())?;
            if let Some(input) = input {
                child.stdin.take().expect("stdin").write_all(input).await.map_err(|error| error.to_string())?;
            }
            let output = child.wait_with_output().await.map_err(|error| error.to_string())?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).to_string())
            }
        }
    }

    #[async_trait]
    impl CommandRunner for IsolatedGithubRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.execute(cmd, args, None).await
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            Err("unexpected run_output".to_string())
        }

        async fn run_with_input(
            &self,
            cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            input: &[u8],
        ) -> Result<String, String> {
            self.execute(cmd, args, Some(input)).await
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    async fn github_convoy() -> ResourceBackend {
        convoy_with_repository("https://github.com/flotilla-org/flotilla.git").await
    }

    async fn convoy_with_repository(url: &str) -> ResourceBackend {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repository_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("single-agent".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url(url.to_string())
                .repo_ref(repository_ref.clone())
                .source_ref("main".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(
                BoundChangeRequest::builder().id("42".to_string()).repository_ref(repository_ref).title("PR".to_string()).build(),
            )
            .build();
        backend.using::<Convoy>("flotilla").create(&InputMeta::builder().name("demo".to_string()).build(), &spec).await.expect("convoy");
        backend
    }

    #[tokio::test]
    async fn github_ledger_projects_with_staged_credential_without_ambient_auth() {
        let backend = github_convoy().await;
        let runner = IsolatedGithubRunner::new(Some("scoped-test-token"));
        let delivery_env =
            BTreeMap::from([("GITHUB_TOKEN_FILE".to_string(), runner.token_file.as_ref().expect("token file").display().to_string())]);
        let url =
            project_decision_ledger(&backend, "flotilla", "demo", "coder", b"## Decision ledger\n", &runner, Path::new("/"), &delivery_env)
                .await
                .expect("project with staged credential");
        assert_eq!(url.as_deref(), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
    }

    #[tokio::test]
    async fn github_ledger_reports_missing_scoped_credential() {
        let backend = github_convoy().await;
        for token in [None, Some("")] {
            let runner = IsolatedGithubRunner::new(token);
            let delivery_env = BTreeMap::from([(
                "GITHUB_TOKEN_FILE".to_string(),
                runner.token_file.as_ref().map_or_else(|| "/missing-token".to_string(), |path| path.display().to_string()),
            )]);
            let error = project_decision_ledger(
                &backend,
                "flotilla",
                "demo",
                "coder",
                b"## Decision ledger\n",
                &runner,
                Path::new("/"),
                &delivery_env,
            )
            .await
            .expect_err("missing or empty credential");
            assert!(error.contains("github-crew-pr credential missing or empty"), "{error}");
        }
    }

    #[tokio::test]
    async fn forgejo_ledger_projects_with_staged_credential_without_ambient_auth() {
        let backend = convoy_with_repository("https://forgejo.example/acme/repo.git").await;
        let runner = IsolatedGithubRunner::new(Some("scoped-test-token"));
        let delivery_env = BTreeMap::from([
            ("FORGEJO_TOKEN_FILE".to_string(), runner.token_file.as_ref().expect("token file").display().to_string()),
            ("FORGEJO_API_URL".to_string(), "https://forgejo.example/api/v1".to_string()),
        ]);
        let url =
            project_decision_ledger(&backend, "flotilla", "demo", "coder", b"## Decision ledger\n", &runner, Path::new("/"), &delivery_env)
                .await
                .expect("project with staged credential");
        assert_eq!(url.as_deref(), Some("https://forgejo.example/acme/repo/pulls/42#issuecomment-7"));
    }

    #[derive(Default)]
    struct CapturingRunner {
        call: Mutex<Option<(Vec<String>, Vec<u8>)>>,
    }

    #[async_trait]
    impl CommandRunner for CapturingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "sh");
            assert!(args.contains(&"--paginate"));
            Ok("[[]]".to_string())
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            Err("unexpected run_output".to_string())
        }

        async fn run_with_input(
            &self,
            cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            input: &[u8],
        ) -> Result<String, String> {
            assert_eq!(cmd, "sh");
            *self.call.lock().expect("capture lock") = Some((args.iter().map(|arg| (*arg).to_string()).collect(), input.to_vec()));
            Ok(r#"{"html_url":"https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"}"#.to_string())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn ledger_projection_sends_file_content_on_stdin() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repository_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("single-agent".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla.git".to_string())
                .repo_ref(repository_ref.clone())
                .source_ref("main".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(
                BoundChangeRequest::builder().id("42".to_string()).repository_ref(repository_ref).title("PR".to_string()).build(),
            )
            .build();
        backend.using::<Convoy>("flotilla").create(&InputMeta::builder().name("demo".to_string()).build(), &spec).await.expect("convoy");
        let runner = CapturingRunner::default();
        let body = b"## Decision ledger\n\n1. **Brief silence:** Naming\n- **Choice:** demo\n- **Alternative:** example\n- **If asking were free:** Which?\n";
        let url = project_decision_ledger(&backend, "flotilla", "demo", "coder", body, &runner, Path::new("/"), &github_delivery_env())
            .await
            .expect("project ledger");
        assert_eq!(url.as_deref(), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
        let (args, input) = runner.call.lock().expect("capture lock").take().expect("gh call");
        assert!(args.iter().any(|arg| arg == "repos/flotilla-org/flotilla/issues/42/comments"));
        assert!(!args.iter().any(|arg| arg.contains("Brief silence")));
        let posted = serde_json::from_slice::<serde_json::Value>(&input).expect("JSON");
        assert!(posted["body"].as_str().expect("comment body").starts_with(std::str::from_utf8(body).expect("UTF-8")));
    }

    #[tokio::test]
    async fn identical_ledger_digest_reuses_existing_comment_url() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let body = b"## Decision ledger\n\n1. **Brief silence:** Naming\n- **Choice:** demo\n- **Alternative:** example\n- **If asking were free:** Which?\n";
        let comment_url = "https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7";
        let spec = ArtifactSpec::builder()
            .convoy("demo".to_string())
            .producer("coder".to_string())
            .kind("decision-ledger".to_string())
            .subject("demo".to_string())
            .summary(std::collections::BTreeMap::from([("comment_url".to_string(), serde_json::json!(comment_url))]))
            .digest(BlobDigest::of(body).as_str().to_string())
            .size(body.len() as u64)
            .media_type("text/markdown".to_string())
            .expires_at(chrono::Utc::now())
            .build();
        let name = flotilla_resources::artifact_record_name("demo", "coder", "decision-ledger", "demo");
        backend
            .using::<flotilla_resources::Artifact>("flotilla")
            .create(&InputMeta::builder().name(name).build(), &spec)
            .await
            .expect("artifact");
        let runner = CapturingRunner::default();
        let result =
            project_decision_ledger_once(&backend, "flotilla", "demo", "coder", body, &runner, Path::new("/"), &github_delivery_env())
                .await
                .expect("reuse comment");
        assert_eq!(result.as_deref(), Some(comment_url));
        assert!(runner.call.lock().expect("capture lock").is_none());
    }

    #[tokio::test]
    async fn ledger_projection_recovers_after_post_succeeds_but_response_fails() {
        struct LostResponseRunner {
            posts: Mutex<usize>,
            comment: Mutex<Option<serde_json::Value>>,
        }

        #[async_trait]
        impl CommandRunner for LostResponseRunner {
            async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
                assert_eq!(cmd, "sh");
                assert!(args.contains(&"--paginate"));
                Ok(serde_json::to_string(&vec![self.comment.lock().expect("comment lock").clone().into_iter().collect::<Vec<_>>()])
                    .expect("comments JSON"))
            }

            async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
                Err("unexpected run_output".to_string())
            }

            async fn run_with_input(
                &self,
                cmd: &str,
                args: &[&str],
                _cwd: &Path,
                _label: &ChannelLabel,
                input: &[u8],
            ) -> Result<String, String> {
                assert_eq!(cmd, "sh");
                assert!(args.contains(&"POST"));
                *self.posts.lock().expect("posts lock") += 1;
                let body =
                    serde_json::from_slice::<serde_json::Value>(input).expect("posted JSON")["body"].as_str().expect("body").to_string();
                *self.comment.lock().expect("comment lock") = Some(serde_json::json!({
                    "body": body,
                    "html_url": "https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"
                }));
                Err("response lost after forge accepted comment".to_string())
            }

            async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
                false
            }
        }

        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repository_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("single-agent".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla.git".to_string())
                .repo_ref(repository_ref.clone())
                .source_ref("main".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(
                BoundChangeRequest::builder().id("42".to_string()).repository_ref(repository_ref).title("PR".to_string()).build(),
            )
            .build();
        backend.using::<Convoy>("flotilla").create(&InputMeta::builder().name("demo".to_string()).build(), &spec).await.expect("convoy");
        let runner = LostResponseRunner { posts: Mutex::new(0), comment: Mutex::new(None) };
        let body = b"## Decision ledger\n\n1. **Brief silence:** Naming\n- **Choice:** demo\n- **Alternative:** example\n- **If asking were free:** Which?\n";
        let url =
            project_decision_ledger_once(&backend, "flotilla", "demo", "coder", body, &runner, Path::new("/"), &github_delivery_env())
                .await
                .expect("transient failure retries and finds accepted comment");
        assert_eq!(url.as_deref(), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
        assert_eq!(*runner.posts.lock().expect("posts lock"), 1);
    }

    #[tokio::test]
    async fn forgejo_projection_finds_comment_after_full_page() {
        struct ForgejoRunner {
            pages: Mutex<Vec<String>>,
            marker: String,
            first_page_len: usize,
        }

        #[async_trait]
        impl CommandRunner for ForgejoRunner {
            async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
                assert_eq!(cmd, "sh");
                let page = args.last().expect("page argument");
                self.pages.lock().expect("pages lock").push((*page).to_string());
                let comments = if page == &"1" {
                    vec![serde_json::json!({ "body": "unrelated comment" }); self.first_page_len]
                } else if page == &"2" {
                    vec![serde_json::json!({
                        "body": format!("ledger\n{}", self.marker),
                        "html_url": "https://forgejo.example/acme/repo/pulls/42#issuecomment-7"
                    })]
                } else {
                    Vec::new()
                };
                serde_json::to_string(&comments).map_err(|error| error.to_string())
            }

            async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
                Err("unexpected run_output".to_string())
            }

            async fn run_with_input(
                &self,
                _cmd: &str,
                _args: &[&str],
                _cwd: &Path,
                _label: &ChannelLabel,
                _input: &[u8],
            ) -> Result<String, String> {
                Err("existing comment should prevent POST".to_string())
            }

            async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
                false
            }
        }

        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repository_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("single-agent".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://forgejo.example/acme/repo.git".to_string())
                .repo_ref(repository_ref.clone())
                .source_ref("main".to_string())
                .target_ref("main".to_string())
                .workspace_slug("repo".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(
                BoundChangeRequest::builder().id("42".to_string()).repository_ref(repository_ref).title("PR".to_string()).build(),
            )
            .build();
        backend.using::<Convoy>("flotilla").create(&InputMeta::builder().name("demo".to_string()).build(), &spec).await.expect("convoy");
        let body = b"## Decision ledger\n\n1. **Brief silence:** Naming\n- **Choice:** demo\n- **Alternative:** example\n- **If asking were free:** Which?\n";
        let name = flotilla_resources::artifact_record_name("demo", "coder", "decision-ledger", "demo");
        for first_page_len in [50, 100] {
            let runner = ForgejoRunner {
                pages: Mutex::new(Vec::new()),
                marker: format!("<!-- flotilla-decision-ledger:{name}:{} -->", BlobDigest::of(body).as_str()),
                first_page_len,
            };
            let url =
                project_decision_ledger_once(&backend, "flotilla", "demo", "coder", body, &runner, Path::new("/"), &forgejo_delivery_env())
                    .await
                    .expect("find comment on second page");
            assert_eq!(url.as_deref(), Some("https://forgejo.example/acme/repo/pulls/42#issuecomment-7"));
            assert_eq!(*runner.pages.lock().expect("pages lock"), ["1", "2", "3"]);
        }
    }
}
