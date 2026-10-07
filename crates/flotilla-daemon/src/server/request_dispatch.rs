use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    agents::{AgentEntry, SharedAgentStateStore},
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::{ChannelLabel, CommandRunner},
};
use flotilla_protocol::{
    AgentHookEvent, Command, CommandAction, CommandCaller, CommandValue, DaemonEvent, LeafAddress, Message, RepoSelector, Request, Response,
};
use flotilla_resources::{expected_change_request_leaves, select_convoy_children, Checkout, Convoy, ResourceBackend};
use tracing::warn;

use super::{caller::missing_crew_message, client_connection::QuerySubscriptions, remote_commands::RemoteCommandRouter};
use crate::{
    artifact::{ArtifactBody, ArtifactPutInput, ArtifactService},
    blob_store::BlobDigest,
};

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

#[async_trait]
trait LedgerCreationAuthority: Send + Sync {
    async fn reserve(&self, address: &LeafAddress) -> Result<bool, String>;
}

#[derive(bon::Builder)]
struct RoutedLedgerCreation<'a> {
    dispatcher: &'a RequestDispatcher<'a>,
    node_id: flotilla_protocol::NodeId,
    namespace: String,
    name: String,
}

#[async_trait]
impl LedgerCreationAuthority for RoutedLedgerCreation<'_> {
    async fn reserve(&self, address: &LeafAddress) -> Result<bool, String> {
        match self
            .dispatcher
            .artifact_home_action(
                &self.node_id,
                CommandAction::ArtifactReserveLedgerComment {
                    namespace: self.namespace.clone(),
                    name: self.name.clone(),
                    address: address.clone(),
                },
            )
            .await?
        {
            CommandValue::LedgerCommentCreationReserved { granted } => Ok(granted),
            CommandValue::Error { message } => Err(message),
            other => Err(format!("unexpected ledger reservation result: {other:?}")),
        }
    }
}

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
    authority: &dyn LedgerCreationAuthority,
) -> Result<Vec<String>, String> {
    let convoy = backend.including_replicas::<Convoy>(namespace).get(convoy_name).await.map_err(|error| error.to_string())?.object;
    let sources = backend.including_replicas::<Checkout>(namespace).list().await.map_err(|error| error.to_string())?;
    let checkouts = select_convoy_children(&convoy, &sources.items);
    let leaves = expected_change_request_leaves(&convoy, &checkouts)?;
    let mut urls = Vec::new();
    let mut projected = HashSet::new();
    for leaf in leaves {
        if !projected.insert(leaf.address.clone()) {
            continue;
        }
        let url = project_ledger_comment(convoy_name, producer, body, &leaf.address, runner, cwd, delivery_env, authority).await?;
        urls.push(url);
    }
    Ok(urls)
}

/// Projects a machine-owned comment: revisions replace the whole body, including
/// manual edits. Identity is carried by the generated marker on the final line.
/// The artifact authority grants at most one POST attempt per change request.
/// Every retry lists first, recovering accepted writes even after response loss.
#[allow(clippy::too_many_arguments)]
async fn project_ledger_comment(
    convoy_name: &str,
    producer: &str,
    body: &[u8],
    address: &LeafAddress,
    runner: &dyn CommandRunner,
    cwd: &Path,
    delivery_env: &BTreeMap<String, String>,
    authority: &dyn LedgerCreationAuthority,
) -> Result<String, String> {
    let LeafAddress::ChangeRequest { service, scope, number } = address else {
        return Err("decision ledger projection has an invalid change request binding".to_string());
    };
    let text = std::str::from_utf8(body).map_err(|_| "decision ledger must be UTF-8".to_string())?;
    let digest = BlobDigest::of(body);
    let name = flotilla_resources::artifact_record_name(convoy_name, producer, "decision-ledger", convoy_name);
    let marker = format!("<!-- flotilla-decision-ledger:{name}:{} -->", digest.as_str());
    let endpoint = format!("repos/{scope}/issues/{number}/comments");
    // Match DefaultLabeler::label_for in flotilla-core/src/providers/mod.rs:
    // it derives command labels from the program and first argument.
    let channel = ChannelLabel::Command("sh -c".to_string());
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
                &channel,
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
                    &[
                        "-c",
                        FORGEJO_LIST,
                        "list-ledgers",
                        token_file,
                        forgejo_api_url.ok_or("Forgejo API URL is unavailable")?,
                        scope,
                        &number,
                        &page,
                    ],
                    cwd,
                    &channel,
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
    // Keep the digest in the marker for retry idempotence, but select by the
    // stable artifact name. Numeric comment IDs order creation on both forges.
    let prefix = format!("<!-- flotilla-decision-ledger:{name}:");
    let selected = existing
        .iter()
        .filter(|comment| {
            comment.get("body").and_then(serde_json::Value::as_str).is_some_and(|body| {
                body.trim_end()
                    .rsplit_once('\n')
                    .map_or(body.trim_end(), |(_, last)| last)
                    .strip_prefix(&prefix)
                    .is_some_and(|suffix| suffix.ends_with(" -->"))
            })
        })
        .max_by_key(|comment| comment.get("id").and_then(serde_json::Value::as_u64));
    let (method, endpoint) = if let Some(comment) = selected {
        if comment["body"].as_str().is_some_and(|body| body.trim_end().ends_with(&marker)) {
            // Refuse malformed URLs rather than POSTing a duplicate of an
            // unchanged ledger whose existing comment was already found.
            return comment
                .get("html_url")
                .and_then(serde_json::Value::as_str)
                .filter(|url| url.starts_with("https://"))
                .map(str::to_string)
                .ok_or_else(|| "existing ledger comment has no HTTPS URL".to_string());
        }
        let id = comment.get("id").and_then(serde_json::Value::as_u64).ok_or("existing ledger comment has no numeric ID")?;
        ("PATCH", format!("repos/{scope}/issues/comments/{id}"))
    } else {
        if !authority.reserve(address).await? {
            return Err("ledger comment creation is pending or uncertain; retry lookup, or reconcile the reserved attempt at the artifact authority".into());
        }
        ("POST", endpoint)
    };
    let input = serde_json::to_vec(&serde_json::json!({ "body": format!("{text}\n{marker}") })).map_err(|error| error.to_string())?;
    let response = if service == "github.com" {
        runner
            .run_with_input(
                "sh",
                &["-c", GITHUB_LEDGER_API, "github-ledger-write", token_file, "--method", method, &endpoint, "--input", "-"],
                cwd,
                &channel,
                &input,
            )
            .await
            .map_err(|error| format!("GitHub ledger comment write failed: {error}"))?
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
curl --fail-with-body --silent --show-error -X "$1" \
  -H "Authorization: token $token" -H "Content-Type: application/json" \
  --data-binary @- "${FORGEJO_API_URL%/}/$2"
"#;
        runner
            .run_with_input(
                "sh",
                &[
                    "-c",
                    FORGEJO_COMMENT,
                    "project-ledger",
                    token_file,
                    forgejo_api_url.ok_or("Forgejo API URL is unavailable")?,
                    method,
                    &endpoint,
                ],
                cwd,
                &channel,
                &input,
            )
            .await?
    };
    serde_json::from_str::<serde_json::Value>(&response)
        .map_err(|error| format!("parse projected ledger comment: {error}"))?
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .map(|url| url.to_string())
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
    authority: &dyn LedgerCreationAuthority,
) -> Result<Vec<String>, String> {
    // Recheck every current binding even for identical content: a stored first
    // URL cannot prove that PRs bound after the previous put received the ledger.
    let mut last_error = None;
    for attempt in 0..3 {
        match project_decision_ledger(backend, namespace, convoy, producer, body, runner, cwd, delivery_env, authority).await {
            Ok(url) => return Ok(url),
            Err(error) => last_error = Some(error),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
        }
    }
    Err(last_error.expect("projection attempted at least once"))
}

// Projection metadata is diagnostic; even a failed projection must leave the ledger publishable.
fn decision_ledger_projection_summary(result: Result<Vec<String>, String>) -> BTreeMap<String, serde_json::Value> {
    let mut summary = BTreeMap::new();
    match result {
        Ok(urls) => {
            if let Some(url) = urls.first() {
                summary.insert("comment_url".into(), serde_json::json!(url));
            }
            summary.insert("projection_count".into(), serde_json::json!(urls.len()));
        }
        Err(error) => {
            // Keep daemon-owned diagnostics within the artifact summary size limit.
            summary.insert("projection_error".into(), serde_json::json!(error.chars().take(512).collect::<String>()));
        }
    }
    summary
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
            // Store and validate the artifact before requesting permission for any forge POST.
            let (name, mut spec, owner) = service.prepare_put(caller, input, &settings.artifact_retention_days).await?;
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
            if spec.kind == "decision-ledger" {
                let body = crate::artifact::read_decision_ledger(temporary.path())?;
                let authority = RoutedLedgerCreation::builder()
                    .dispatcher(self)
                    .node_id(node_id.clone())
                    .namespace(namespace.clone())
                    .name(name.clone())
                    .build();
                let projection = async {
                    let delivery_env = self.daemon.ledger_delivery_environment(&namespace, &session.spec.env_ref).await?;
                    project_decision_ledger_once(
                        &backend,
                        &namespace,
                        &caller.convoy,
                        &session.spec.role,
                        &body,
                        runner.as_ref(),
                        Path::new(&session.spec.cwd),
                        &delivery_env,
                        &authority,
                    )
                    .await
                }
                .await;
                spec.summary = decision_ledger_projection_summary(projection);
                // A crash between the initial envelope write and this diagnostic
                // write can leave an empty or stale summary. The durable creation
                // reservation survives; the next put rechecks the forge marker.
                store(&spec).await?;
            }
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

#[cfg(test)]
mod ledger_projection_tests {
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
    };

    use async_trait::async_trait;
    use flotilla_core::providers::CommandOutput;
    use flotilla_resources::{
        ArtifactSpec, BoundChangeRequest, ConvoyRepositorySpec, ConvoySpec, InMemoryBackend, InputMeta, RepositoryKey,
    };
    use tokio::sync::Barrier;

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

    struct IsolatedForgeRunner {
        _directory: tempfile::TempDir,
        path: PathBuf,
        token_file: Option<PathBuf>,
    }

    impl IsolatedForgeRunner {
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
    impl CommandRunner for IsolatedForgeRunner {
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

    // #2677: no PR, successful projections (including duplicates/multiple PRs), and errors
    // all produce publishable artifact metadata. Errors remain diagnostic rather than dropping the ledger.
    #[hegel::test]
    fn ledger_projection_metadata_preserves_artifact_evidence(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let fails = tc.draw(gs::booleans());
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(600));
        // Unicode scalar values and JSON-escaped control characters exercise the byte budget.
        let character = ["é", "\0", "🦀"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let urls = vec!["https://example.test/comment".to_string(); count];
        let result = if fails { Err(character.repeat(length)) } else { Ok(urls) };
        let summary = decision_ledger_projection_summary(result);
        assert_eq!(summary.contains_key("projection_error"), fails);
        assert_eq!(summary.contains_key("comment_url"), !fails && count > 0);
        assert_eq!(summary.get("projection_count"), (!fails).then(|| serde_json::json!(count)).as_ref());
        assert!(serde_json::to_vec(&summary).expect("metadata").len() <= 4096);
    }

    #[tokio::test]
    async fn github_ledger_projects_with_staged_credential_without_ambient_auth() {
        let backend = github_convoy().await;
        let runner = IsolatedForgeRunner::new(Some("scoped-test-token"));
        let delivery_env =
            BTreeMap::from([("GITHUB_TOKEN_FILE".to_string(), runner.token_file.as_ref().expect("token file").display().to_string())]);
        let url = project_decision_ledger(
            &backend,
            "flotilla",
            "demo",
            "coder",
            b"## Decision ledger\n",
            &runner,
            Path::new("/"),
            &delivery_env,
            &AllowCreation,
        )
        .await
        .expect("project with staged credential");
        assert_eq!(url.first().map(String::as_str), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
    }

    #[tokio::test]
    async fn github_ledger_reports_missing_scoped_credential() {
        let backend = github_convoy().await;
        for token in [None, Some("")] {
            let runner = IsolatedForgeRunner::new(token);
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
                &AllowCreation,
            )
            .await
            .expect_err("missing or empty credential");
            assert!(error.contains("github-crew-pr credential missing or empty"), "{error}");
        }
    }

    #[tokio::test]
    async fn forgejo_ledger_projects_with_staged_credential_without_ambient_auth() {
        let backend = convoy_with_repository("https://forgejo.example/acme/repo.git").await;
        let runner = IsolatedForgeRunner::new(Some("scoped-test-token"));
        let delivery_env = BTreeMap::from([
            ("FORGEJO_TOKEN_FILE".to_string(), runner.token_file.as_ref().expect("token file").display().to_string()),
            ("FORGEJO_API_URL".to_string(), "https://forgejo.example/api/v1".to_string()),
        ]);
        let url = project_decision_ledger(
            &backend,
            "flotilla",
            "demo",
            "coder",
            b"## Decision ledger\n",
            &runner,
            Path::new("/"),
            &delivery_env,
            &AllowCreation,
        )
        .await
        .expect("project with staged credential");
        assert_eq!(url.first().map(String::as_str), Some("https://forgejo.example/acme/repo/pulls/42#issuecomment-7"));
    }

    #[derive(Default)]
    struct CapturingRunner {
        calls: Mutex<Vec<(Vec<String>, Vec<u8>)>>,
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
            self.calls.lock().expect("capture lock").push((args.iter().map(|arg| (*arg).to_string()).collect(), input.to_vec()));
            Ok(r#"{"html_url":"https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"}"#.to_string())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    // Real forge process calls are recorded with repository-scoped credentials.
    // Recording targets a disposable comment on a configured issue or PR.
    #[tokio::test]
    async fn ledger_revision_forge_replay() {
        use flotilla_core::providers::replay::{self, Masks};

        let recording = std::env::var("REPLAY").is_ok_and(|mode| mode != "replay");
        let service = "github.com".to_string();
        let scope = "flotilla-org/flotilla".to_string();
        let number = 2600;
        let env = BTreeMap::from([(
            "GITHUB_TOKEN_FILE".into(),
            if recording { std::env::var("GITHUB_TOKEN_FILE").expect("injected GitHub credential") } else { "/staged/github/token".into() },
        )]);
        let mut masks = Masks::new();
        for (key, value) in &env {
            masks.add(value, format!("{{{key}}}"));
        }
        masks.add(&service, "{service}");
        masks.add(&scope, "{scope}");
        let path = format!("{}/tests/fixtures/ledger_revision_github.yaml", env!("CARGO_MANIFEST_DIR"));
        let session = replay::test_session(&path, masks);
        let runner = replay::test_runner(&session);
        let address = LeafAddress::ChangeRequest { service, scope, number };
        let first = b"## Decision ledger\n\nReplay recording for #2600: initial revision.";
        let revised = b"## Decision ledger\n\nReplay recording for #2600: updated revision.";
        let url = project_ledger_comment("replay-2600", "coder", first, &address, runner.as_ref(), Path::new("/"), &env, &AllowCreation)
            .await
            .expect("first projection");
        let updated =
            project_ledger_comment("replay-2600", "coder", revised, &address, runner.as_ref(), Path::new("/"), &env, &AllowCreation)
                .await
                .expect("PATCH revision");
        assert_eq!(updated, url);
        let retry =
            project_ledger_comment("replay-2600", "coder", revised, &address, runner.as_ref(), Path::new("/"), &env, &AllowCreation)
                .await
                .expect("unchanged retry");
        assert_eq!(retry, url);
        session.finish();
    }

    pub(super) struct AllowCreation;

    #[async_trait]
    impl LedgerCreationAuthority for AllowCreation {
        async fn reserve(&self, _: &LeafAddress) -> Result<bool, String> {
            Ok(true)
        }
    }

    // The runner replaces the forge process boundary; comments are real in-memory state.
    #[derive(Default)]
    struct CommentRunner {
        comments: Mutex<Vec<serde_json::Value>>,
        writes: Mutex<Vec<(String, String)>>,
        first_listings: Option<Arc<Barrier>>,
        listings: AtomicUsize,
        lose_response: AtomicBool,
        refuse_write: AtomicBool,
    }

    #[async_trait]
    impl CommandRunner for CommentRunner {
        async fn run(&self, _cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            let first_page = args.contains(&"--slurp") || args.last() == Some(&"1");
            let listing = {
                let comments = self.comments.lock().expect("comments lock");
                if args.contains(&"--slurp") {
                    serde_json::json!([*comments]).to_string()
                } else if first_page {
                    serde_json::json!(*comments).to_string()
                } else {
                    "[]".into()
                }
            };
            // Pin the race: both callers capture an empty first listing before
            // either can request a creation reservation at the authority.
            if first_page && self.listings.fetch_add(1, Ordering::SeqCst) < 2 {
                if let Some(barrier) = &self.first_listings {
                    barrier.wait().await;
                }
            }
            Ok(listing)
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            Err("unexpected run_output".into())
        }

        async fn run_with_input(
            &self,
            _cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            input: &[u8],
        ) -> Result<String, String> {
            let method = args.iter().find(|arg| matches!(**arg, "POST" | "PATCH")).expect("write method");
            let endpoint = args.iter().find(|arg| arg.starts_with("repos/")).expect("write endpoint");
            self.writes.lock().expect("writes lock").push(((*method).into(), (*endpoint).into()));
            if self.refuse_write.load(Ordering::SeqCst) {
                return Err("forge refused the write".into());
            }
            let mut comments = self.comments.lock().expect("comments lock");
            let body = serde_json::from_slice::<serde_json::Value>(input).expect("JSON")["body"].clone();
            let id = if *method == "PATCH" {
                endpoint.rsplit('/').next().expect("comment id").parse::<u64>().expect("numeric id")
            } else {
                1 + comments.iter().filter_map(|comment| comment["id"].as_u64()).max().unwrap_or(0)
            };
            let comment = serde_json::json!({"id": id, "body": body, "html_url": format!("https://forge.example/comment/{id}")});
            if *method == "PATCH" {
                *comments.iter_mut().find(|comment| comment["id"] == id).expect("existing comment") = comment.clone();
            } else {
                comments.push(comment.clone());
            }
            if self.lose_response.swap(false, Ordering::SeqCst) {
                return Err("accepted write response lost".into());
            }
            Ok(comment.to_string())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    pub(super) struct StoredLedgerCreation {
        backend: ResourceBackend,
        name: String,
    }

    #[async_trait]
    impl LedgerCreationAuthority for StoredLedgerCreation {
        async fn reserve(&self, address: &LeafAddress) -> Result<bool, String> {
            flotilla_resources::reserve_ledger_comment_creation(&self.backend, "flotilla", &self.name, address)
                .await
                .map_err(|error| error.to_string())
        }
    }

    pub(super) async fn stored_creation(backend: ResourceBackend) -> StoredLedgerCreation {
        let name = flotilla_resources::artifact_record_name("demo", "coder", "decision-ledger", "demo");
        let spec = flotilla_resources::ArtifactSpec::builder()
            .convoy("demo".into())
            .producer("coder".into())
            .kind("decision-ledger".into())
            .subject("demo".into())
            .digest("digest".into())
            .size(1)
            .media_type("text/markdown".into())
            .expires_at(chrono::Utc::now())
            .build();
        backend
            .using::<flotilla_resources::Artifact>("flotilla")
            .create(&InputMeta::builder().name(name.clone()).build(), &spec)
            .await
            .expect("ledger envelope");
        StoredLedgerCreation { backend, name }
    }

    // #2623: concurrent empty listings must emit exactly one POST. Both forges
    // recover its identity after response loss, retries, and subsequent revisions.
    #[tokio::test]
    async fn concurrent_ledger_creation_has_one_identity_on_both_forges() {
        for service in ["github.com", "forgejo.example"] {
            for sqlite in [false, true] {
                let backend = if sqlite {
                    ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open_in_memory().expect("sqlite"))
                } else {
                    ResourceBackend::InMemory(InMemoryBackend::default())
                };
                let authority = stored_creation(backend).await;
                let runner = CommentRunner {
                    first_listings: Some(Arc::new(Barrier::new(2))),
                    lose_response: AtomicBool::new(true),
                    ..Default::default()
                };
                let env = if service == "github.com" { github_delivery_env() } else { forgejo_delivery_env() };
                let address = LeafAddress::ChangeRequest { service: service.into(), scope: "acme/repo".into(), number: 42 };
                let first = b"## Decision ledger\nfirst";
                let revised = b"## Decision ledger\nrevised";
                let project = async |body: &[u8]| {
                    project_ledger_comment("demo", "coder", body, &address, &runner, Path::new("/"), &env, &authority).await
                };
                let (one, two) = tokio::join!(project(first), project(first));
                assert!(one.is_err() || two.is_err(), "one accepted response is deliberately lost");
                assert_eq!(runner.comments.lock().expect("comments").len(), 1);
                assert_eq!(runner.writes.lock().expect("writes").iter().filter(|(method, _)| method == "POST").count(), 1);
                let url = project(first).await.expect("recover accepted comment by marker");
                assert_eq!(project(revised).await.expect("revision"), url);
                assert_eq!(project(revised).await.expect("unchanged revision"), url);
                assert_eq!(runner.comments.lock().expect("comments").len(), 1);
                assert_eq!(
                    runner.writes.lock().expect("writes").iter().map(|(method, _)| method.as_str()).collect::<Vec<_>>(),
                    ["POST", "PATCH"]
                );
                // Persisted reservations survive serialized round trips and spec revisions.
                let resolver = authority.backend.using::<flotilla_resources::Artifact>("flotilla");
                let stored = resolver.get(&authority.name).await.expect("reserved ledger");
                let round_trip: flotilla_resources::ResourceObject<flotilla_resources::Artifact> =
                    serde_json::from_value(serde_json::to_value(&stored).expect("serialize")).expect("decode");
                assert_eq!(round_trip.status, stored.status);
                // Previous-generation unit statuses serialize as null. Decode the
                // full stored envelope without regenerating the golden corpus.
                let mut previous = serde_json::to_value(&stored).expect("serialize prior envelope");
                previous["status"] = serde_json::Value::Null;
                let previous: flotilla_resources::ResourceObject<flotilla_resources::Artifact> =
                    serde_json::from_value(previous).expect("decode previous unit status");
                assert!(previous.status.is_none());
                assert!(matches!(
                    resolver
                        .update_status(&authority.name, &stored.metadata.resource_version, &flotilla_resources::ArtifactStatus::default())
                        .await,
                    Err(flotilla_resources::ResourceError::Invalid { .. })
                ));
                resolver
                    .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &stored.spec)
                    .await
                    .expect("revise envelope");
                assert!(!authority.reserve(&address).await.expect("reservation retained"));
            }
        }
    }

    // Restart must not forget a creation attempt whose external outcome is unknown.
    #[tokio::test]
    async fn ledger_creation_reservation_survives_authority_restart() {
        let directory = tempfile::tempdir().expect("authority store");
        let path = directory.path().join("resources.sqlite");
        let address = LeafAddress::ChangeRequest { service: "github.com".into(), scope: "acme/repo".into(), number: 42 };
        let name = {
            let authority = stored_creation(ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open(&path).expect("open"))).await;
            assert!(authority.reserve(&address).await.expect("first reservation"));
            authority.name
        };
        let authority = StoredLedgerCreation {
            backend: ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open(&path).expect("reopen")),
            name,
        };
        assert!(!authority.reserve(&address).await.expect("durable reservation"));
    }

    // An absent comment after a reserved attempt is uncertain. Never retry POST,
    // including when a process stops after reservation or the forge refuses it.
    #[tokio::test]
    async fn ledger_uncertain_creation_never_reposts() {
        for service in ["github.com", "forgejo.example"] {
            for sent in [false, true] {
                let authority = stored_creation(ResourceBackend::InMemory(InMemoryBackend::default())).await;
                let runner = CommentRunner { refuse_write: AtomicBool::new(true), ..Default::default() };
                let env = if service == "github.com" { github_delivery_env() } else { forgejo_delivery_env() };
                let address = LeafAddress::ChangeRequest { service: service.into(), scope: "acme/repo".into(), number: 42 };
                if !sent {
                    assert!(authority.reserve(&address).await.expect("reserve before crash"));
                }
                let project =
                    async || project_ledger_comment("demo", "coder", b"ledger", &address, &runner, Path::new("/"), &env, &authority).await;
                assert!(project().await.is_err());
                runner.refuse_write.store(false, Ordering::SeqCst);
                assert!(project().await.expect_err("uncertain retry").contains("pending or uncertain"));
                assert!(runner.comments.lock().expect("comments").is_empty());
                assert_eq!(runner.writes.lock().expect("writes").len(), usize::from(sent));
            }
        }
    }

    // #2600: each artifact posts once, retries unchanged content without writing,
    // and updates the newest legacy duplicate in place when its content changes.
    #[tokio::test]
    async fn ledger_revisions_update_newest_comment_on_both_forges() {
        for service in ["github.com", "forgejo.example"] {
            let runner = CommentRunner::default();
            let env = if service == "github.com" { github_delivery_env() } else { forgejo_delivery_env() };
            let address = LeafAddress::ChangeRequest { service: service.into(), scope: "acme/repo".into(), number: 42 };
            let first = b"## Decision ledger\nfirst";
            let revised = b"## Decision ledger\nrevised";
            let url = project_ledger_comment("demo", "coder", first, &address, &runner, Path::new("/"), &env, &AllowCreation)
                .await
                .expect("first projection");
            assert_eq!(
                runner.writes.lock().expect("writes lock").as_slice(),
                [("POST".into(), "repos/acme/repo/issues/42/comments".into())]
            );
            assert_eq!(
                project_ledger_comment("demo", "coder", first, &address, &runner, Path::new("/"), &env, &AllowCreation)
                    .await
                    .expect("retry"),
                url
            );
            assert_eq!(runner.writes.lock().expect("writes lock").len(), 1);
            project_ledger_comment("demo", "coder", revised, &address, &runner, Path::new("/"), &env, &AllowCreation)
                .await
                .expect("revision");
            assert_eq!(runner.comments.lock().expect("comments lock").len(), 1);
            assert_eq!(runner.writes.lock().expect("writes lock")[1], ("PATCH".into(), "repos/acme/repo/issues/comments/1".into()));
            let older = runner.comments.lock().expect("comments lock")[0].clone();
            let mut newer = older.clone();
            newer["id"] = serde_json::json!(9);
            newer["html_url"] = serde_json::json!("https://forge.example/comment/9");
            // Deliberately unsorted: listing order must not decide which duplicate wins.
            *runner.comments.lock().expect("comments lock") = vec![newer, older.clone()];
            project_ledger_comment("demo", "coder", first, &address, &runner, Path::new("/"), &env, &AllowCreation)
                .await
                .expect("legacy duplicates");
            let comments = runner.comments.lock().expect("comments lock");
            assert_eq!(comments.len(), 2);
            assert_eq!(comments[1], older);
            assert!(comments[0]["body"].as_str().expect("body").starts_with("## Decision ledger\nfirst"));
            assert_eq!(runner.writes.lock().expect("writes lock")[2], ("PATCH".into(), "repos/acme/repo/issues/comments/9".into()));
        }
    }

    // A same-digest match with no HTTPS URL must fail without creating a duplicate.
    #[tokio::test]
    async fn ledger_retry_refuses_missing_or_insecure_url_without_writing() {
        for service in ["github.com", "forgejo.example"] {
            for url in [serde_json::Value::Null, serde_json::json!("http://forge.example/comment/1")] {
                let body = b"## Decision ledger\nfirst";
                let name = flotilla_resources::artifact_record_name("demo", "coder", "decision-ledger", "demo");
                let runner = CommentRunner::default();
                runner.comments.lock().expect("comments lock").push(serde_json::json!({
                    "id": 1, "html_url": url,
                    "body": format!("ledger\n<!-- flotilla-decision-ledger:{name}:{} -->", BlobDigest::of(body).as_str())
                }));
                let address = LeafAddress::ChangeRequest { service: service.into(), scope: "acme/repo".into(), number: 42 };
                let env = if service == "github.com" { github_delivery_env() } else { forgejo_delivery_env() };
                let error = project_ledger_comment("demo", "coder", body, &address, &runner, Path::new("/"), &env, &AllowCreation)
                    .await
                    .expect_err("existing comment URL is unusable");
                assert!(error.contains("no HTTPS URL"), "{error}");
                assert!(runner.writes.lock().expect("writes lock").is_empty());
                assert_eq!(runner.comments.lock().expect("comments lock").len(), 1);
            }
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
        let url = project_decision_ledger(
            &backend,
            "flotilla",
            "demo",
            "coder",
            body,
            &runner,
            Path::new("/"),
            &github_delivery_env(),
            &AllowCreation,
        )
        .await
        .expect("project ledger");
        assert_eq!(url.first().map(String::as_str), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
        let (args, input) = runner.calls.lock().expect("capture lock").pop().expect("gh call");
        assert!(args.iter().any(|arg| arg == "repos/flotilla-org/flotilla/issues/42/comments"));
        assert!(!args.iter().any(|arg| arg.contains("Brief silence")));
        let posted = serde_json::from_slice::<serde_json::Value>(&input).expect("JSON");
        assert!(posted["body"].as_str().expect("comment body").starts_with(std::str::from_utf8(body).expect("UTF-8")));
    }

    #[tokio::test]
    async fn stored_ledger_url_does_not_skip_current_bindings() {
        let backend = github_convoy().await;
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
        // #2527: the same ledger must reach a newly bound PR after it was stored.
        let convoys = backend.using::<Convoy>("flotilla");
        let mut convoy = convoys.get("demo").await.expect("convoy");
        convoy.spec.subjects.push(flotilla_resources::DeclaredSubject {
            subject: flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
                id: "43".into(),
            },
            relationship: flotilla_protocol::Relationship::Produces,
            issue: None,
            change_request: None,
        });
        convoys
            .update(&InputMeta::builder().name("demo".into()).build(), &convoy.metadata.resource_version, &convoy.spec)
            .await
            .expect("new PR binding");
        let runner = CapturingRunner::default();
        let result = project_decision_ledger_once(
            &backend,
            "flotilla",
            "demo",
            "coder",
            body,
            &runner,
            Path::new("/"),
            &github_delivery_env(),
            &AllowCreation,
        )
        .await
        .expect("reuse comment");
        assert_eq!(result.first().map(String::as_str), Some(comment_url));
        assert_eq!(result.len(), 2);
        let calls = runner.calls.lock().expect("capture lock");
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|(args, _)| args.iter().any(|arg| arg == "repos/flotilla-org/flotilla/issues/43/comments")));
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
        let url = project_decision_ledger_once(
            &backend,
            "flotilla",
            "demo",
            "coder",
            body,
            &runner,
            Path::new("/"),
            &github_delivery_env(),
            &AllowCreation,
        )
        .await
        .expect("transient failure retries and finds accepted comment");
        assert_eq!(url.first().map(String::as_str), Some("https://github.com/flotilla-org/flotilla/pull/42#issuecomment-7"));
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
            let url = project_decision_ledger_once(
                &backend,
                "flotilla",
                "demo",
                "coder",
                body,
                &runner,
                Path::new("/"),
                &forgejo_delivery_env(),
                &AllowCreation,
            )
            .await
            .expect("find comment on second page");
            assert_eq!(url.first().map(String::as_str), Some("https://forgejo.example/acme/repo/pulls/42#issuecomment-7"));
            assert_eq!(*runner.pages.lock().expect("pages lock"), ["1", "2", "3"]);
        }
    }
}

#[cfg(all(test, not(feature = "skip-no-sandbox-tests")))]
mod ledger_forgejo_contract;
