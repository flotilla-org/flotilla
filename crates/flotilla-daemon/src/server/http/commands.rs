//! REST adapter for the existing request dispatcher. Command records are ephemeral and
//! bounded; subscribe before dispatch so even an immediately completed command is retained.
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    response::Response,
    Extension,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    CommandCaller, CommandValue, DaemonEvent, Message, PrincipalRef, Request as RpcRequest, Response as RpcResponse, ResponseResult,
};
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use tokio::sync::{
    broadcast::{self, error::RecvError},
    mpsc, watch, Mutex, OwnedSemaphorePermit, Semaphore,
};
use tokio::{task::JoinHandle, time::Instant};
use uuid::Uuid;

use super::{CallerAuth, RouterState};
use crate::server::{
    caller, client_connection::QuerySubscriptions, environment_connection, environment_supervision, remote_commands::RemoteCommandRouter,
    request_dispatch::RequestDispatcher,
};

const COMMAND_LIMIT: usize = 1024 * 1024;
const SMALL_LIMIT: usize = 64 * 1024;
const RECORD_LIMIT: usize = 1024;
const MAX_WAIT: f64 = 300.0;
const MAX_STEPS: usize = 256;
const MAX_PROGRESS_BYTES: usize = 256 * 1024;
const UNKNOWN_RETENTION: Duration = Duration::from_secs(300);

// Test double only for the kernel/procfs evidence boundary; resolution remains real.
#[cfg(test)]
struct PeerEvidence {
    caller: CommandCaller,
    claims: std::collections::HashMap<String, String>,
    mounts: Option<caller::PeerMounts>,
}

pub(super) struct Commands {
    router: RemoteCommandRouter,
    shutdown: mpsc::UnboundedSender<()>,
    socket_path: PathBuf,
    records: Mutex<BTreeMap<Uuid, Record>>,
    admission: Mutex<()>,
    capacity: Arc<Semaphore>,
    eviction_sequence: Arc<AtomicU64>,
}

struct Record {
    _permit: OwnedSemaphorePermit,
    command_id: Option<u64>,
    owner: CommandCaller,
    state: watch::Receiver<CommandState>,
    collector: Option<JoinHandle<()>>,
}

impl Drop for Record {
    fn drop(&mut self) {
        // Evicting an uncertain record must not leave an orphan event collector.
        // It deliberately does not cancel the underlying daemon execution.
        if let Some(collector) = &self.collector {
            collector.abort();
        }
    }
}

#[derive(Clone)]
struct RetainedStep {
    event: DaemonEvent,
    encoded_bytes: usize,
}

impl Serialize for RetainedStep {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.event.serialize(serializer)
    }
}

#[derive(Clone, Serialize)]
struct CommandState {
    id: Uuid,
    state: &'static str,
    steps: VecDeque<RetainedStep>,
    outcome: Option<CommandValue>,
    progress_truncated: bool,
    observation_error: Option<String>,
    #[serde(skip)]
    progress_bytes: usize,
    #[serde(skip)]
    eviction_order: Option<u64>,
    #[serde(skip)]
    unknown_since: Option<Instant>,
    #[serde(skip)]
    collector_closed: bool,
    #[serde(skip)]
    completion_scope: CompletionScope,
}

impl Commands {
    pub(super) fn new(router: RemoteCommandRouter, shutdown: mpsc::UnboundedSender<()>, socket_path: PathBuf) -> Self {
        Self {
            router,
            shutdown,
            socket_path,
            records: Mutex::new(BTreeMap::new()),
            admission: Mutex::new(()),
            capacity: Arc::new(Semaphore::new(RECORD_LIMIT)),
            eviction_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn has_capacity(&self) -> bool {
        self.capacity.available_permits() > 0
            || self.records.lock().await.values().any(|r| r.state.borrow().evictable_order(Instant::now()).is_some())
    }

    async fn reserve(&self) -> Result<OwnedSemaphorePermit, &'static str> {
        self.reserve_at(Instant::now()).await
    }

    async fn reserve_at(&self, now: Instant) -> Result<OwnedSemaphorePermit, &'static str> {
        let mut records = self.records.lock().await;
        if self.capacity.available_permits() == 0 {
            let oldest = records
                .iter()
                .filter_map(|(id, record)| record.state.borrow().evictable_order(now).map(|order| (*id, order)))
                .min_by_key(|(_, order)| *order);
            // Each submission reserves one permit, so evicting one record is sufficient.
            if let Some((id, _)) = oldest {
                records.remove(&id);
            }
        }
        self.capacity.clone().try_acquire_owned().map_err(|_| "HTTP command record capacity is occupied by active or pending commands")
    }
}

#[cfg(test)]
use super::wire::large_integers;
use super::wire::Encoding;

// The route determines the request kind; callers cannot select a different operation
// through a body tag. GET/DELETE control requests have a zero-byte body limit.
fn route(method: &Method, path: &str) -> Option<(&'static str, usize)> {
    match (method.as_str(), path) {
        ("POST", "/v1/commands") => Some(("execute", COMMAND_LIMIT)),
        ("GET", "/v1/status") => Some(("get_status", 0)),
        ("GET", "/v1/topology") => Some(("get_topology", 0)),
        ("GET", "/v1/repos") => Some(("list_repos", 0)),
        ("POST", "/v1/repos") => Some(("add_repo", SMALL_LIMIT)),
        ("DELETE", "/v1/repos") => Some(("remove_repo", SMALL_LIMIT)),
        ("POST", "/v1/repos/refresh") => Some(("refresh", SMALL_LIMIT)),
        ("POST", "/v1/artifacts") => Some(("artifact_put", SMALL_LIMIT)),
        ("GET", "/v1/artifacts") => Some(("artifact_list", 0)),
        ("GET", "/v1/artifacts/content") => Some(("artifact_get", 0)),
        ("POST", "/v1/shutdown") => Some(("shutdown", 0)),
        _ => None,
    }
}

pub(super) async fn handle(
    State(RouterState(state)): State<RouterState>,
    Extension(authenticated): Extension<CallerAuth>,
    request: Request,
) -> Response {
    let encoding = match Encoding::response(request.headers()) {
        Ok(encoding) => encoding,
        Err(error) => {
            return Encoding::error_encoding(request.headers()).error_code(StatusCode::NOT_ACCEPTABLE, "unsupported_format", error)
        }
    };
    if matches!(authenticated, CallerAuth::Anonymous) {
        return encoding.error(StatusCode::UNAUTHORIZED, "authentication required");
    }

    let Some(daemon) = state.daemon.as_ref() else {
        return encoding.error(StatusCode::SERVICE_UNAVAILABLE, "daemon is unavailable");
    };
    let Some(commands) = state.commands.as_ref() else {
        return encoding.error(StatusCode::SERVICE_UNAVAILABLE, "command service is unavailable");
    };
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let query: BTreeMap<String, String> =
        url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes()).into_owned().collect();
    let command_resource = path.strip_prefix("/v1/commands/");
    let (kind, limit) = if command_resource.is_some() {
        if method != Method::GET && method != Method::DELETE {
            return encoding.error(StatusCode::METHOD_NOT_ALLOWED, "use GET or DELETE");
        }
        ("command_record", 0)
    } else {
        match route(&method, &path) {
            Some(route) => route,
            None => return encoding.error(StatusCode::METHOD_NOT_ALLOWED, "method is not available on this route"),
        }
    };
    if let Some(length) = request.headers().get(header::CONTENT_LENGTH) {
        let Some(size) = length.to_str().ok().and_then(|v| v.parse::<u64>().ok()) else {
            return encoding.error(StatusCode::BAD_REQUEST, "invalid Content-Length");
        };
        if size > limit as u64 {
            return encoding.error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds route limit");
        }
    }
    #[cfg(test)]
    let injected_evidence = request.extensions().get::<Arc<PeerEvidence>>().cloned();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if limit > 0 && !matches!(content_type.as_str(), "application/json" | "application/cbor") {
        return encoding.error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "use application/json or application/cbor");
    }
    // to_bytes applies http-body-util's limiting body: unknown-length/chunked streams
    // stop at the limit too, rather than collecting first and checking afterwards.
    let bytes = match to_bytes(request.into_body(), limit).await {
        Ok(bytes) => bytes,
        Err(_) => return encoding.error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds route limit"),
    };
    let namespace = daemon.provisioning_namespace().await;
    let mut transport_bound = false;
    let caller = match &authenticated {
        CallerAuth::Socket(Some(peer)) => {
            let peer = *peer;
            let socket_path = commands.socket_path.clone();
            let evidence = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(evidence) = injected_evidence {
                    return Ok((
                        evidence.caller.clone(),
                        evidence.claims.clone(),
                        evidence
                            .mounts
                            .as_ref()
                            .map(|m| caller::PeerMounts { mountinfo: m.mountinfo.clone(), legacy_main_directory: m.legacy_main_directory }),
                    ));
                }
                let claims = caller::claims_from_peer(Some(peer));
                let caller = caller::caller_from_peer(Some(peer), &namespace)?;
                let mounts = caller
                    .process
                    .as_ref()
                    .filter(|p| p.container.is_some())
                    .map(|p| caller::peer_mounts(p.pid, Some(&socket_path)))
                    .transpose()?;
                Ok::<_, String>((caller, claims, mounts))
            })
            .await;
            match evidence {
                Ok(Ok((caller, claims, mounts))) => match caller::resolve_main_caller(daemon, caller, &claims, mounts.as_ref()).await {
                    Ok(resolved) => {
                        transport_bound = resolved.transport_bound;
                        resolved.caller
                    }
                    Err(error) => return encoding.error(StatusCode::FORBIDDEN, error),
                },
                Ok(Err(error)) => return encoding.error(StatusCode::FORBIDDEN, error),
                Err(error) => return encoding.error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            }
        }
        CallerAuth::Environment { environment, peer } => {
            let crews = match environment_connection::environment_crews(daemon, environment).await {
                Ok(c) => c,
                Err(e) => return encoding.error(StatusCode::FORBIDDEN, e),
            };
            let peer = *peer;
            let claims = match tokio::task::spawn_blocking(move || caller::claims_from_peer(peer)).await {
                Ok(c) => c,
                Err(e) => return encoding.error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            let crew = match caller::check_claims(crews, &claims) {
                Ok(c) => c,
                Err(e) => return encoding.error(StatusCode::FORBIDDEN, e),
            };
            transport_bound = true;
            CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(&crew.namespace), process: None, crew: Some(crew) }
        }
        CallerAuth::Bearer | CallerAuth::Session(_) => {
            CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(&namespace), process: None, crew: None }
        }
        CallerAuth::Socket(None) => {
            return encoding.error(StatusCode::FORBIDDEN, "CallerIdentityUnavailable: socket peer credentials unavailable")
        }
        CallerAuth::Hook { .. } => return encoding.error(StatusCode::FORBIDDEN, "a hook token reaches only the hook routes"),
        CallerAuth::Anonymous => return encoding.error(StatusCode::UNAUTHORIZED, "authentication required"),
    };
    if transport_bound && method == Method::DELETE && command_resource.is_some() {
        return encoding.error(StatusCode::FORBIDDEN, "EnvironmentCommandDenied: cancellation is outside supervisor authority");
    }
    if let Some(id) = command_resource {
        let id = match Uuid::parse_str(id) {
            Ok(id) => id,
            Err(_) => return encoding.error(StatusCode::BAD_REQUEST, "invalid command id"),
        };
        let records = commands.records.lock().await;
        let Some(record) = records.get(&id) else {
            return encoding.error(StatusCode::NOT_FOUND, "command not found");
        };
        if record.owner.crew != caller.crew || (transport_bound && caller.crew.is_none()) {
            return encoding.error(StatusCode::FORBIDDEN, "command belongs to another caller");
        }
        let mut receiver = record.state.clone();
        let command_id = record.command_id;
        drop(records);
        if method == Method::DELETE {
            if receiver.borrow().outcome.is_some() {
                return encoding.error(StatusCode::CONFLICT, "command has finished");
            }
            let Some(command_id) = command_id else {
                return encoding.error(StatusCode::CONFLICT, "command cannot be cancelled");
            };
            return match commands.router.dispatch_cancel(command_id).await {
                Ok(()) => encoding.render(StatusCode::ACCEPTED, json!({"id": id})),
                Err(e) => encoding.error(cancel_status(&e, &receiver.borrow()), e),
            };
        }
        let wait = match query.get("wait").map(|v| v.parse::<f64>()).transpose() {
            Ok(wait) if wait.is_none_or(|w| w.is_finite() && (0.0..=MAX_WAIT).contains(&w)) => wait.unwrap_or(0.0),
            _ => return encoding.error(StatusCode::BAD_REQUEST, "wait must be between 0 and 300 seconds"),
        };
        if wait > 0.0 {
            let _ =
                tokio::time::timeout(Duration::from_secs_f64(wait), receiver.wait_for(|s| s.outcome.is_some() || s.collector_closed)).await;
        }
        return encoding.render(StatusCode::OK, receiver.borrow().clone());
    }
    let params = if limit == 0 || (kind == "remove_repo" && bytes.is_empty()) {
        serde_json::to_value(&query).expect("string map")
    } else if content_type == "application/cbor" {
        match flotilla_cbor::from_slice::<Value>(&bytes, &flotilla_cbor::Limits { max_message: limit, ..Default::default() }) {
            Ok(v) => v,
            Err(e) => return encoding.error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(e) => return encoding.error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    };
    let params = if kind == "execute" { json!({"command": params}) } else { params };
    let envelope = if matches!(kind, "get_status" | "get_topology" | "list_repos" | "shutdown") {
        json!({"kind":kind})
    } else {
        json!({"kind":kind,"params":params})
    };
    let mut rpc: RpcRequest = match serde_json::from_value(envelope) {
        Ok(r) => r,
        Err(e) => return encoding.error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    if matches!(rpc, RpcRequest::ArtifactPut { .. } | RpcRequest::ArtifactGet { .. })
        && !matches!(authenticated, CallerAuth::Socket(_) | CallerAuth::Environment { .. })
    {
        return encoding.error(StatusCode::FORBIDDEN, "artifact path operations require a Unix socket");
    }
    let scope = if transport_bound {
        let Some(crew) = caller.crew.as_ref() else {
            return encoding.error(StatusCode::FORBIDDEN, "calling crew is unavailable");
        };
        match environment_supervision::authorize(daemon, &mut rpc, crew).await {
            Ok(scope) => scope,
            Err(e) => return encoding.error(StatusCode::FORBIDDEN, e),
        }
    } else {
        None
    };
    // Serialize admissions, not record reads: retain the eviction candidate until
    // dispatch succeeds so a caller error cannot destroy an earlier outcome.
    let _admission = if kind == "execute" {
        let guard = commands.admission.lock().await;
        if !commands.has_capacity().await {
            return encoding.error(StatusCode::CONFLICT, "HTTP command record capacity is occupied by active or pending commands");
        }
        Some(guard)
    } else {
        None
    };
    let events = daemon.subscribe();
    let dispatcher = RequestDispatcher::new_with_caller(
        daemon,
        &commands.router,
        daemon.agent_state_store(),
        Uuid::new_v4(),
        QuerySubscriptions::default(),
        caller.clone(),
    );
    let dispatcher = if transport_bound { dispatcher } else { dispatcher.with_authenticated_user() };
    let mut response = dispatcher.dispatch(0, rpc).await;
    if let Some(project) = scope.as_ref() {
        environment_supervision::scope_response(&mut response, &caller.crew.as_ref().expect("authorized crew").namespace, project);
    }
    let response = match response {
        Message::Response { response, .. } => match *response {
            ResponseResult::Ok { response } => *response,
            ResponseResult::Err { message } => return encoding.error(error_status(&message), message),
        },
        _ => return encoding.error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected dispatcher response"),
    };
    if kind == "execute" {
        let id = Uuid::new_v4();
        let (command_id, outcome) = match response {
            RpcResponse::Execute { command_id } => (Some(command_id), None),
            RpcResponse::QueryResult { value, .. } => (None, Some(value)),
            _ => return encoding.error(StatusCode::INTERNAL_SERVER_ERROR, "unexpected execute response"),
        };
        let permit = commands.reserve().await.expect("serialized admission retains an eligible slot");
        let completion_scope = CompletionScope {
            transport_bound,
            project: scope.map(|project| (caller.crew.as_ref().expect("authorized crew").namespace.clone(), project)),
        };
        let mut initial = CommandState::new(id, outcome, &commands.eviction_sequence);
        initial.completion_scope = completion_scope;
        let completion_scope = initial.completion_scope.clone();
        let (sender, receiver) = watch::channel(initial);
        let collector = command_id.map(|command_id| {
            tokio::spawn(collect_progress(events, command_id, sender, commands.eviction_sequence.clone(), completion_scope))
        });
        commands.records.lock().await.insert(id, Record { _permit: permit, command_id, owner: caller, state: receiver, collector });
        let url = format!("/v1/commands/{id}");
        let mut response = encoding.render(StatusCode::ACCEPTED, json!({"id": id, "command_id": command_id, "url": url}));
        response.headers_mut().insert(header::LOCATION, url.parse().expect("UUID URL"));
        return response;
    }
    if matches!(response, RpcResponse::Shutdown) && commands.shutdown.send(()).is_err() {
        return encoding.error(StatusCode::SERVICE_UNAVAILABLE, "shutdown service unavailable");
    }
    let value = serde_json::to_value(response).expect("protocol response serializes");
    encoding.render(
        if kind == "artifact_put" || kind == "add_repo" { StatusCode::CREATED } else { StatusCode::OK },
        value.get("data").cloned().unwrap_or_else(|| json!({})),
    )
}

impl CommandState {
    fn new(id: Uuid, outcome: Option<CommandValue>, order: &AtomicU64) -> Self {
        let outcome_is_terminal = outcome.is_some();
        Self {
            id,
            state: if outcome.is_some() { "finished" } else { "running" },
            steps: VecDeque::new(),
            eviction_order: outcome.as_ref().map(|_| order.fetch_add(1, Ordering::Relaxed)),
            outcome,
            progress_truncated: false,
            observation_error: None,
            progress_bytes: 0,
            unknown_since: None,
            collector_closed: outcome_is_terminal,
            completion_scope: CompletionScope::default(),
        }
    }

    fn observation_lost(&mut self, message: String, closed: bool, order: &AtomicU64) {
        self.state = "unknown";
        self.progress_truncated = true;
        self.observation_error = Some(message);
        self.collector_closed = closed;
        // Repeated lag must not keep extending retention indefinitely.
        self.unknown_since.get_or_insert_with(Instant::now);
        self.eviction_order.get_or_insert_with(|| order.fetch_add(1, Ordering::Relaxed));
    }

    fn evictable_order(&self, now: Instant) -> Option<u64> {
        let expired = self.unknown_since.is_some_and(|since| now.saturating_duration_since(since) >= UNKNOWN_RETENTION);
        if self.outcome.is_some() || self.collector_closed || expired {
            self.eviction_order
        } else {
            None
        }
    }

    fn push_step(&mut self, event: DaemonEvent) {
        // Treat unencodable events as oversized: omit them and report truncated history.
        let size = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
        if size > MAX_PROGRESS_BYTES {
            self.progress_truncated = true;
            return;
        }
        while self.steps.len() >= MAX_STEPS || self.progress_bytes + size > MAX_PROGRESS_BYTES {
            let removed = self.steps.pop_front().expect("retained history exceeds a limit");
            self.progress_bytes -= removed.encoded_bytes;
            self.progress_truncated = true;
        }
        self.progress_bytes += size;
        self.steps.push_back(RetainedStep { event, encoded_bytes: size });
    }
}

#[derive(Clone, Default)]
struct CompletionScope {
    transport_bound: bool,
    project: Option<(String, String)>,
}

async fn collect_progress(
    mut events: broadcast::Receiver<DaemonEvent>,
    command_id: u64,
    sender: watch::Sender<CommandState>,
    order: Arc<AtomicU64>,
    scope: CompletionScope,
) {
    loop {
        let mut event = match events.recv().await {
            Ok(event) => event,
            Err(error) => {
                sender.send_modify(|s| {
                    s.observation_lost(
                        format!("command progress unavailable: {error}; do not retry blindly"),
                        matches!(error, RecvError::Closed),
                        &order,
                    )
                });
                match error {
                    RecvError::Lagged(_) => continue,
                    RecvError::Closed => break,
                }
            }
        };
        if let Some((namespace, project)) = &scope.project {
            environment_supervision::scope_completion(&mut event, namespace, project);
        }
        match event {
            DaemonEvent::CommandFinished { command_id: finished, result, .. } if finished == command_id => {
                sender.send_modify(|s| {
                    s.state = "finished";
                    s.outcome = Some(result);
                    s.eviction_order = Some(order.fetch_add(1, Ordering::Relaxed));
                    s.observation_error = None;
                    s.unknown_since = None;
                    s.collector_closed = true;
                });
                break;
            }
            event @ DaemonEvent::CommandStepUpdate { command_id: updated, .. } if updated == command_id && !scope.transport_bound => {
                sender.send_modify(|s| s.push_step(event))
            }
            _ => {}
        }
    }
}

fn cancel_status(message: &str, state: &CommandState) -> StatusCode {
    if state.outcome.is_some() || message == "no matching active command" || message.starts_with("unknown command") {
        StatusCode::CONFLICT
    } else {
        error_status(message)
    }
}

// TODO(#3584): carry typed dispatcher errors; provider prose is not a trustworthy error class.
fn error_status(message: &str) -> StatusCode {
    if message.starts_with("resource conflict for ") || message.starts_with("field ownership refused") || message == "command has finished"
    {
        StatusCode::CONFLICT
    } else if message.starts_with("CallerIdentity")
        || message.starts_with("Environment")
        || message.starts_with("unauthorized:")
        || message == "artifact put requires a calling crew session"
        || message == "artifact get requires a connected operator principal"
    {
        StatusCode::FORBIDDEN
    } else if message.starts_with("resource not found:")
        || message.starts_with("unknown command")
        || message.starts_with("no Repository matches ")
    {
        StatusCode::NOT_FOUND
    } else if message.starts_with("invalid resource:") || message == "operator artifact destination must be absolute" {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request as HttpRequest, Router};
    use flotilla_config::ConfigStore;
    use flotilla_core::in_process::InProcessDaemon;
    use flotilla_protocol::{Command, CommandAction, HostName};
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;
    use tower::ServiceExt;

    use super::*;
    use crate::blob_store::{BlobStore, TieredBlobStore};
    use crate::server::http::{HttpRouter, ListenerOrigin, LoginStore};
    use crate::server::tests::{caller_mountinfo, placed_crew_daemon, supervisor_daemon};

    struct Harness {
        _temp: tempfile::TempDir,
        router: Router,
        shutdown: mpsc::UnboundedReceiver<()>,
        blobs: Arc<TieredBlobStore>,
        commands: Arc<Commands>,
    }

    impl Harness {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"http-command-test\"\n").unwrap();
            let config = Arc::new(ConfigStore::with_base(temp.path()));
            let daemon = InProcessDaemon::new_with_resource_backend(
                vec![],
                config,
                flotilla_discovery_testkit::fake_discovery(false),
                HostName::new("http-test"),
                ResourceBackend::new(InMemoryBackend::default()),
                Arc::new(crate::peer::HostPeerOverlay::default()),
            )
            .await;
            Self::from_daemon(temp, daemon)
        }

        fn from_daemon(temp: tempfile::TempDir, daemon: Arc<InProcessDaemon>) -> Self {
            let manager = Arc::new(Mutex::new(crate::peer::PeerManager::new(daemon.node_id().clone())));
            let remote = crate::server::build_remote_command_router(&daemon, &manager);
            let blobs = Arc::new(TieredBlobStore::new(temp.path(), vec![]));
            remote.install_blob_store(blobs.clone()).unwrap();
            let (shutdown, receiver) = mpsc::unbounded_channel();
            let http =
                HttpRouter::for_daemon(daemon.clone(), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
                    .with_commands(remote, shutdown, temp.path().join("daemon.sock"));
            let commands = http.commands.as_ref().unwrap().clone();
            let router = http.into_router();
            Self { _temp: temp, router, shutdown: receiver, blobs, commands }
        }

        async fn call(
            &self,
            method: &str,
            path: &str,
            body: Option<Value>,
            cbor: bool,
            caller: CallerAuth,
        ) -> (StatusCode, Value, axum::http::HeaderMap) {
            self.call_with_evidence(method, path, body, cbor, caller, None).await
        }

        async fn call_with_evidence(
            &self,
            method: &str,
            path: &str,
            body: Option<Value>,
            cbor: bool,
            caller: CallerAuth,
            evidence: Option<Arc<PeerEvidence>>,
        ) -> (StatusCode, Value, axum::http::HeaderMap) {
            let media = if cbor { "application/cbor" } else { "application/json" };
            let bytes = body
                .map(|v| if cbor { flotilla_cbor::to_vec(&v, &Default::default()).unwrap() } else { serde_json::to_vec(&v).unwrap() })
                .unwrap_or_default();
            let mut request = HttpRequest::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, media)
                .header(header::ACCEPT, media)
                .body(Body::from(bytes))
                .unwrap();
            request.extensions_mut().insert(caller);
            if let Some(evidence) = evidence {
                request.extensions_mut().insert(evidence);
            }
            let response = self.router.clone().oneshot(request).await.unwrap();
            let (parts, body) = response.into_parts();
            let bytes = to_bytes(body, 16 * COMMAND_LIMIT).await.unwrap();
            let value = if cbor {
                flotilla_cbor::from_slice(&bytes, &Default::default()).unwrap()
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            assert_eq!(parts.headers[header::CONTENT_TYPE], media);
            (parts.status, value, parts.headers)
        }
    }

    fn socket_caller() -> CallerAuth {
        CallerAuth::Socket(Some(caller::PeerCredential::from_kernel(1000, Some(42))))
    }

    fn host_evidence() -> Arc<PeerEvidence> {
        Arc::new(PeerEvidence {
            caller: CommandCaller {
                principal_ref: PrincipalRef::implicit_for_namespace("flotilla"),
                process: Some(flotilla_protocol::CallerProcess::builder().pid(42).uid(1000).build()),
                crew: None,
            },
            claims: Default::default(),
            mounts: None,
        })
    }

    // A hook credential never inherits operator authority on any command route.
    #[tokio::test]
    async fn hook_tokens_are_refused() {
        let h = Harness::new().await;
        for cbor in [false, true] {
            for (method, path, body) in [
                ("POST", "/v1/shutdown", None),
                ("POST", "/v1/repos", Some(json!({"path":"/tmp/repo"}))),
                ("POST", "/v1/commands", Some(watch_command())),
                ("GET", "/v1/commands/00000000-0000-0000-0000-000000000001", None),
            ] {
                let caller = CallerAuth::Hook {
                    terminal: flotilla_protocol::AgentHookTerminalRef { namespace: "flotilla".into(), session_name: "crew-session".into() },
                    generation: "generation".into(),
                };
                let reply = h.call(method, path, body, cbor, caller).await;
                assert_eq!(reply.0, StatusCode::FORBIDDEN);
                assert_eq!(reply.1["message"], "a hook token reaches only the hook routes");
            }
        }
    }

    // Missing kernel identity is refused, and removed replay recovery has no HTTP route.
    #[tokio::test]
    async fn missing_peer_and_replay_are_refused() {
        let h = Harness::new().await;
        for cbor in [false, true] {
            let reply = h.call("POST", "/v1/shutdown", None, cbor, CallerAuth::Socket(None)).await;
            assert_eq!(reply.0, StatusCode::FORBIDDEN);
            assert_eq!(reply.1["message"], "CallerIdentityUnavailable: socket peer credentials unavailable");
            let reply = h
                .router
                .clone()
                .oneshot(HttpRequest::post("/v1/events/replay").extension(CallerAuth::Bearer).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    // Malformed length is a caller error, checked before the body is polled.
    #[tokio::test]
    async fn malformed_content_length_is_bad_request() {
        let h = Harness::new().await;
        for length in ["invalid", "-1", "18446744073709551616"] {
            let body =
                Body::from_stream(futures::stream::poll_fn(|_| -> std::task::Poll<Option<Result<axum::body::Bytes, std::io::Error>>> {
                    panic!("body polled")
                }));
            let reply = h
                .router
                .clone()
                .oneshot(
                    HttpRequest::post("/v1/commands")
                        .header(header::CONTENT_LENGTH, length)
                        .extension(CallerAuth::Bearer)
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
        }
    }

    // Failed dispatch at full capacity must preserve every existing completed outcome.
    #[tokio::test]
    async fn failed_dispatch_preserves_completed_records() {
        let h = Harness::new().await;
        let mut ids = Vec::new();
        for _ in 0..RECORD_LIMIT {
            let id = Uuid::new_v4();
            ids.push(id);
            let (_, state) = watch::channel(CommandState::new(id, Some(CommandValue::Cancelled), &h.commands.eviction_sequence));
            let permit = h.commands.reserve().await.unwrap();
            h.commands.records.lock().await.insert(
                id,
                Record {
                    _permit: permit,
                    command_id: None,
                    owner: CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace("flotilla"), process: None, crew: None },
                    state,
                    collector: None,
                },
            );
        }
        let bad =
            serde_json::to_value(Command::builder().action(CommandAction::QueryCrewList { context: Default::default() }).build()).unwrap();
        let reply = h.call("POST", "/v1/commands", Some(bad), false, CallerAuth::Bearer).await;
        assert!(reply.0.is_client_error() || reply.0.is_server_error(), "{}", reply.1);
        for id in ids {
            assert!(h.commands.records.lock().await.contains_key(&id));
        }
    }

    // The Unix branch resolves injected kernel/procfs evidence through live placement.
    // A container reaching the main socket retains crew authority, never operator authority.
    #[tokio::test]
    async fn unix_peer_evidence_keeps_container_crew_bound() {
        use flotilla_resources::{Environment, EnvironmentStatus, InputMeta};
        for cbor in [false, true] {
            let (temp, daemon) = placed_crew_daemon("unix-crew").await;
            let environments = daemon.resource_backend().using::<Environment>("flotilla");
            let env = environments
                .create(
                    &InputMeta::builder().name("unix-crew".into()).build(),
                    &serde_json::from_value(json!({"docker":{"host_ref":"host","image":"image"}})).unwrap(),
                )
                .await
                .unwrap();
            environments
                .update_status(
                    "unix-crew",
                    &env.metadata.resource_version,
                    &EnvironmentStatus {
                        runtime_observation: Some(flotilla_protocol::EnvironmentRuntimeObservation {
                            container_id: Some("kernel-container".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let h = Harness::from_daemon(temp, daemon);
            let mut evidence = host_evidence();
            Arc::get_mut(&mut evidence).unwrap().caller.process.as_mut().unwrap().container = Some("kernel-container".into());
            Arc::get_mut(&mut evidence).unwrap().mounts = Some(caller_mountinfo(&flotilla_protocol::EnvironmentId::new("unix-crew"), true));
            let denied = h.call_with_evidence("POST", "/v1/shutdown", None, cbor, socket_caller(), Some(evidence.clone())).await;
            assert_eq!(denied.0, StatusCode::FORBIDDEN);
            let action = CommandAction::CrewPromise {
                context: Default::default(),
                operation: flotilla_protocol::commands::CrewPromiseOperation::Promise { id: "http-promise".into(), kind: "pr".into() },
            };
            let accepted = h
                .call_with_evidence(
                    "POST",
                    "/v1/commands",
                    Some(serde_json::to_value(Command::builder().action(action).build()).unwrap()),
                    cbor,
                    socket_caller(),
                    Some(evidence.clone()),
                )
                .await;
            assert_eq!(accepted.0, StatusCode::ACCEPTED, "{}", accepted.1);
            let url = accepted.1["url"].as_str().unwrap();
            let done = h.call_with_evidence("GET", &format!("{url}?wait=2"), None, cbor, socket_caller(), Some(evidence.clone())).await;
            assert_eq!(done.1["state"], "finished");
            assert_eq!(done.1["steps"], json!([]));
            assert_eq!(h.call_with_evidence("DELETE", url, None, cbor, socket_caller(), Some(evidence)).await.0, StatusCode::FORBIDDEN);
            let host = h.call_with_evidence("GET", "/v1/status", None, cbor, socket_caller(), Some(host_evidence())).await;
            assert_eq!(host.0, StatusCode::OK);
        }
    }

    // Async supervisor dispatch retains the bound project on its record. The same
    // collector scope filters foreign completion rows and suppresses step events.
    #[tokio::test]
    async fn async_crew_completion_is_scoped() {
        for cbor in [false, true] {
            let (temp, daemon) = supervisor_daemon().await;
            let h = Harness::from_daemon(temp, daemon);
            let caller =
                CallerAuth::Environment { environment: flotilla_protocol::EnvironmentId::new("supervisor-environment"), peer: None };
            let command = Command::builder()
                .action(CommandAction::ConvoyAbandon { namespace: None, name: "target".into(), reason: "scope test".into() })
                .build();
            let accepted = h.call("POST", "/v1/commands", Some(serde_json::to_value(command).unwrap()), cbor, caller.clone()).await;
            assert_eq!(accepted.0, StatusCode::ACCEPTED, "{}", accepted.1);
            assert!(accepted.1["command_id"].is_number());
            let id = Uuid::parse_str(accepted.1["id"].as_str().unwrap()).unwrap();
            let scope = h.commands.records.lock().await[&id].state.borrow().completion_scope.clone();
            assert!(scope.transport_bound);
            assert_eq!(scope.project, Some(("flotilla".into(), "project-a".into())));
            let url = accepted.1["url"].as_str().unwrap();
            assert_eq!(h.call("DELETE", url, None, cbor, caller.clone()).await.0, StatusCode::FORBIDDEN);
            let done = h.call("GET", &format!("{url}?wait=2"), None, cbor, caller.clone()).await;
            assert_eq!(done.1["state"], "finished");
            assert_eq!(done.1["steps"], json!([]));

            // Event bus boundary: deterministic foreign data and a step on the
            // dispatched command's retained scope, consumed by the real collector.
            let order = Arc::new(AtomicU64::new(0));
            let (events, input) = broadcast::channel(4);
            let (sender, state) = watch::channel(CommandState::new(id, None, &order));
            {
                let mut records = h.commands.records.lock().await;
                let record = records.get_mut(&id).unwrap();
                if let Some(collector) = record.collector.take() {
                    collector.abort();
                }
                record.state = state.clone();
            }
            let task = tokio::spawn(collect_progress(input, 77, sender, order, scope));
            events.send(step("private progress".into())).unwrap();
            let rows = ["project-a", "project-b"]
                .into_iter()
                .map(|project| {
                    flotilla_protocol::CrewStallRow::builder()
                        .namespace("flotilla".into())
                        .project(project.into())
                        .convoy("convoy".into())
                        .convoy_display_name("convoy".into())
                        .vessel("work".into())
                        .role("coder".into())
                        .evidence("stalled".into())
                        .cause_group("same".into())
                        .shared_cause_count(2)
                        .artifacts(Vec::new())
                        .build()
                })
                .collect();
            let mut event = finished();
            if let DaemonEvent::CommandFinished { result, .. } = &mut event {
                *result = CommandValue::CrewStalls(Box::new(flotilla_protocol::CrewStallsResponse { rows, ..Default::default() }));
            }
            events.send(event).unwrap();
            task.await.unwrap();
            assert!(state.borrow().steps.is_empty());
            {
                let state = state.borrow();
                let Some(CommandValue::CrewStalls(stalls)) = &state.outcome else { panic!("scoped stalls") };
                assert_eq!(stalls.rows.len(), 1);
                assert_eq!(stalls.rows[0].project.as_deref(), Some("project-a"));
                assert_eq!(stalls.rows[0].shared_cause_count, 1);
            }
            let reply = h.call("GET", url, None, cbor, caller).await;
            assert_eq!(reply.0, StatusCode::OK);
            assert_eq!(reply.1["steps"], json!([]));
            let wire = reply.1["outcome"].to_string();
            assert!(wire.contains("project-a"));
            assert!(!wire.contains("project-b"));
        }
    }

    fn watch_command() -> Value {
        serde_json::to_value(
            Command::builder()
                .action(CommandAction::ResourceWatch {
                    namespace: "flotilla".into(),
                    kind: "convoy".into(),
                    name: None,
                    include_replicas: false,
                    replica_sources: false,
                    cursor: None,
                })
                .build(),
        )
        .unwrap()
    }

    // Every stateless REST operation delegates to the existing daemon behavior in both
    // encodings. Missing crew identity and unavailable artifact paths retain their messages.
    #[tokio::test]
    async fn routes_have_equal_json_and_cbor_outcomes() {
        for (method, path, body, status) in [
            ("GET", "/v1/status", None, StatusCode::OK),
            ("GET", "/v1/topology", None, StatusCode::OK),
            ("GET", "/v1/repos", None, StatusCode::OK),
            ("POST", "/v1/repos", Some(json!({"path":"/missing-http-test-repo"})), StatusCode::CREATED),
            ("DELETE", "/v1/repos", Some(json!({"path":"/missing-http-test-repo"})), StatusCode::NOT_FOUND),
            ("POST", "/v1/repos/refresh", Some(json!({"repo":"/missing-http-test-repo"})), StatusCode::NOT_FOUND),
            ("GET", "/v1/artifacts", None, StatusCode::OK),
            (
                "POST",
                "/v1/artifacts",
                Some(
                    json!({"kind":"explainer","subject":"commit","summary":{},"media_type":"text/markdown","source_path":"/tmp/report.md"}),
                ),
                StatusCode::FORBIDDEN,
            ),
            ("GET", "/v1/artifacts/content?reference=artifact/missing&destination_path=/tmp/output.md", None, StatusCode::FORBIDDEN),
        ] {
            let h = Harness::new().await;
            let json_reply = h.call(method, path, body.clone(), false, CallerAuth::Bearer).await;
            // Only mutations need a fresh daemon; read-only replies share the same
            // incarnation so random node IDs do not obscure encoding equivalence.
            let cbor_reply = if path.starts_with("/v1/repos") && method != "GET" {
                Harness::new().await.call(method, path, body, true, CallerAuth::Bearer).await
            } else {
                h.call(method, path, body, true, CallerAuth::Bearer).await
            };
            assert_eq!(json_reply.0, status, "{method} {path}: {}", json_reply.1);
            assert_eq!((json_reply.0, json_reply.1.clone()), (cbor_reply.0, cbor_reply.1), "{method} {path}");
            if path == "/v1/artifacts" && method == "POST" {
                assert_eq!(json_reply.1["message"], "artifact path operations require a Unix socket");
            }
        }
    }

    // Shutdown delegates exactly once per HTTP request, independently in each encoding.
    #[tokio::test]
    async fn shutdown_signals_once_per_request() {
        for cbor in [false, true] {
            let mut h = Harness::new().await;
            assert_eq!(h.call("POST", "/v1/shutdown", None, cbor, CallerAuth::Bearer).await.0, StatusCode::OK);
            assert_eq!(h.shutdown.try_recv(), Ok(()));
            assert!(h.shutdown.try_recv().is_err());
        }
    }

    // A real daemon watch stays live across HTTP requests. Wait expires with partial
    // progress, DELETE cancels it, and concurrent waiters receive the same final outcome.
    #[tokio::test]
    async fn command_wait_progress_and_cancel_in_both_encodings() {
        for cbor in [false, true] {
            let h = Harness::new().await;
            let (status, accepted, headers) = h.call("POST", "/v1/commands", Some(watch_command()), cbor, CallerAuth::Bearer).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
            let url = accepted["url"].as_str().unwrap();
            assert_eq!(headers[header::LOCATION], url);
            assert_eq!(accepted["id"], url.strip_prefix("/v1/commands/").unwrap());
            let (status, running, _) = h.call("GET", &format!("{url}?wait=0.01"), None, cbor, CallerAuth::Bearer).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(running["state"], "running");
            assert!(running["outcome"].is_null());
            assert!(!running["steps"].as_array().unwrap().is_empty());
            let wait_path = format!("{url}?wait=2");
            let (first, second, cancel) = tokio::join!(
                h.call("GET", &wait_path, None, cbor, CallerAuth::Bearer),
                h.call("GET", &wait_path, None, cbor, CallerAuth::Bearer),
                h.call("DELETE", url, None, cbor, CallerAuth::Bearer),
            );
            assert_eq!(cancel.0, StatusCode::ACCEPTED);
            assert_eq!(first.1, second.1);
            assert_eq!(first.1["state"], "finished");
            assert_eq!(first.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
            assert_eq!(h.call("DELETE", url, None, cbor, CallerAuth::Bearer).await.0, StatusCode::CONFLICT);
            assert_eq!(h.call("GET", &format!("{url}?wait=-1"), None, cbor, CallerAuth::Bearer).await.0, StatusCode::BAD_REQUEST);
        }
    }

    // Subscribe before dispatch: both immediate synchronous query results and commands
    // that finish before the POST returns must remain available at the returned URL.
    #[tokio::test]
    async fn immediate_completions_and_queries_are_retained() {
        for cbor in [false, true] {
            let h = Harness::new().await;
            for action in [CommandAction::Refresh { repo: None }, CommandAction::QueryHostList {}] {
                let command = serde_json::to_value(Command::builder().action(action).build()).unwrap();
                let reply = h.call("POST", "/v1/commands", Some(command), cbor, CallerAuth::Bearer).await;
                assert_eq!(reply.0, StatusCode::ACCEPTED, "{}", reply.1);
                let url = reply.1["url"].as_str().unwrap();
                let finished = h.call("GET", &format!("{url}?wait=2"), None, cbor, CallerAuth::Bearer).await;
                assert_eq!(finished.0, StatusCode::OK);
                assert_eq!(finished.1["state"], "finished");
                let outcome: CommandValue = serde_json::from_value(finished.1["outcome"].clone()).unwrap();
                assert!(!matches!(outcome, CommandValue::Error { .. }), "{outcome:?}");
            }
        }
    }

    // Request and response encodings are independent; invalid bodies and unsupported
    // media types are caller errors and cannot reach the command dispatcher.
    #[tokio::test]
    async fn negotiation_and_invalid_bodies() {
        let h = Harness::new().await;
        for input_cbor in [false, true] {
            for output_cbor in [false, true] {
                let body = watch_command();
                let bytes = if input_cbor {
                    flotilla_cbor::to_vec(&body, &Default::default()).unwrap()
                } else {
                    serde_json::to_vec(&body).unwrap()
                };
                let mut request = HttpRequest::post("/v1/commands")
                    .header(header::CONTENT_TYPE, if input_cbor { "application/cbor" } else { "application/json" })
                    .header(header::ACCEPT, if output_cbor { "application/cbor" } else { "application/json" })
                    .body(Body::from(bytes))
                    .unwrap();
                request.extensions_mut().insert(CallerAuth::Bearer);
                let reply = h.router.clone().oneshot(request).await.unwrap();
                assert_eq!(reply.status(), StatusCode::ACCEPTED);
                assert_eq!(reply.headers()[header::CONTENT_TYPE], if output_cbor { "application/cbor" } else { "application/json" });
            }
        }
        for (media, accept, status) in [
            ("text/plain", "application/json", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ("application/json", "text/html", StatusCode::NOT_ACCEPTABLE),
            ("application/json", "application/json;q=0,application/cbor;q=0", StatusCode::NOT_ACCEPTABLE),
            ("application/json", "application/json", StatusCode::BAD_REQUEST),
            ("application/cbor", "application/cbor", StatusCode::BAD_REQUEST),
        ] {
            let mut request = HttpRequest::post("/v1/commands")
                .header(header::CONTENT_TYPE, media)
                .header(header::ACCEPT, accept)
                .body(Body::from("invalid"))
                .unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            let reply = h.router.clone().oneshot(request).await.unwrap();
            assert_eq!(reply.status(), status, "{media}, {accept}");
        }
        for (accept, media) in
            [("application/json;q=0.2,application/cbor;q=0.8", "application/cbor"), ("application/cbor;q=0,*/*;q=1", "application/json")]
        {
            let mut request = HttpRequest::get("/v1/status").header(header::ACCEPT, accept).body(Body::empty()).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            let reply = h.router.clone().oneshot(request).await.unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.headers()[header::CONTENT_TYPE], media);
        }
    }

    // TCP authority never authorizes daemon-side filesystem operations, even with a
    // valid user credential. Unix callers without a crew keep today's identity refusal.
    #[tokio::test]
    async fn tcp_artifact_paths_are_refused() {
        let h = Harness::new().await;
        for cbor in [false, true] {
            for (method, path, body) in [
                (
                    "POST",
                    "/v1/artifacts",
                    Some(json!({"kind":"explainer","subject":"test","summary":{},"media_type":"text/plain","source_path":"/etc/passwd"})),
                ),
                ("GET", "/v1/artifacts/content?reference=artifact/test&destination_path=/tmp/copied", None),
            ] {
                let reply = h.call(method, path, body, cbor, CallerAuth::Bearer).await;
                assert_eq!(reply.0, StatusCode::FORBIDDEN);
                assert_eq!(reply.1["message"], "artifact path operations require a Unix socket");
            }
        }
    }

    // JSON accepts plain large integer request fields. Only JSON responses wrap them;
    // the negotiated CBOR response keeps the same numeric value without precision loss.
    #[tokio::test]
    async fn integer_request_and_response_contract() {
        let command = Command::builder()
            .action(CommandAction::QueryDaemonLogs {
                query: flotilla_protocol::commands::DaemonLogQuery { since_seconds: Some(u64::MAX), level: None, target: None },
            })
            .build();
        // Request decoders accept the same plain integer model, without response wrappers.
        let json = serde_json::to_vec(&command).unwrap();
        let cbor = flotilla_cbor::to_vec(&serde_json::to_value(&command).unwrap(), &Default::default()).unwrap();
        assert_eq!(serde_json::from_slice::<Command>(&json).unwrap(), command);
        assert_eq!(flotilla_cbor::from_slice::<Command>(&cbor, &Default::default()).unwrap(), command);
        for cbor in [false, true] {
            let response = if cbor { Encoding::Cbor } else { Encoding::Json }.render(StatusCode::OK, json!([u64::MAX, i64::MIN]));
            let body = to_bytes(response.into_body(), COMMAND_LIMIT).await.unwrap();
            let value: Value =
                if cbor { flotilla_cbor::from_slice(&body, &Default::default()).unwrap() } else { serde_json::from_slice(&body).unwrap() };
            assert_eq!(
                value,
                if cbor { json!([u64::MAX, i64::MIN]) } else { json!([{"u64":u64::MAX.to_string()},{"i64":i64::MIN.to_string()}]) }
            );
        }
    }

    // An outcome outside the CBOR profile is a daemon error, but the refusal itself
    // still uses the negotiated encoding so a CBOR-only client can read it.
    #[tokio::test]
    async fn encoding_faults_keep_the_negotiated_error_body() {
        let mut nested = Value::Null;
        for _ in 0..65 {
            nested = json!([nested]);
        }
        let response = Encoding::Cbor.render(StatusCode::OK, nested);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/cbor");
        let bytes = to_bytes(response.into_body(), COMMAND_LIMIT).await.unwrap();
        let error: Value = flotilla_cbor::from_slice(&bytes, &Default::default()).unwrap();
        assert!(error["message"].as_str().unwrap().contains("depth-limit"));
    }

    // Glue: the legacy dispatcher's string error vocabulary maps to HTTP status classes.
    #[test]
    fn dispatcher_error_status_classes() {
        for (message, status) in [
            ("resource conflict for test: stale version", StatusCode::CONFLICT),
            ("field ownership refused 1 violation(s)", StatusCode::CONFLICT),
            ("unauthorized: foreign principal", StatusCode::FORBIDDEN),
            ("invalid resource: unknown kind", StatusCode::BAD_REQUEST),
            ("resource not found: test", StatusCode::NOT_FOUND),
            ("no Repository matches 'missing'", StatusCode::NOT_FOUND),
            ("store unavailable", StatusCode::INTERNAL_SERVER_ERROR),
            ("provider failed reading /tmp/requires-invalid-not found-conflict", StatusCode::INTERNAL_SERVER_ERROR),
            ("resource not found: requires-invalid-conflict", StatusCode::NOT_FOUND),
        ] {
            assert_eq!(error_status(message), status, "{message}");
        }
    }

    // Authenticated Unix users can retrieve artifact bytes without creating a surface
    // session. The daemon still writes to an absolute path, through its environment runner.
    #[tokio::test]
    async fn unix_user_artifact_get_uses_router_identity() {
        let h = Harness::new().await;
        let digest = h.blobs.put(b"artifact contents").await.unwrap();
        for cbor in [false, true] {
            let destination = h._temp.path().join(if cbor { "cbor-output" } else { "json-output" });
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("reference", digest.as_str())
                .append_pair("destination_path", destination.to_str().unwrap())
                .finish();
            let reply = h
                .call_with_evidence("GET", &format!("/v1/artifacts/content?{query}"), None, cbor, socket_caller(), Some(host_evidence()))
                .await;
            assert_eq!(reply.0, StatusCode::OK, "{}", reply.1);
            assert_eq!(reply.1["size"], 17);
            assert_eq!(tokio::fs::read(destination).await.unwrap(), b"artifact contents");
        }
    }

    // Environment authority is re-resolved from placement on every request. It cannot
    // administer the daemon, inspect another caller's command, or survive crew retirement.
    #[tokio::test]
    async fn environment_callers_keep_their_placement_scope() {
        for cbor in [false, true] {
            let (temp, daemon) = placed_crew_daemon("http-crew").await;
            let h = Harness::from_daemon(temp, daemon.clone());
            let caller = CallerAuth::Environment { environment: flotilla_protocol::EnvironmentId::new("http-crew"), peer: None };
            let forbidden = h.call("GET", "/v1/status", None, cbor, caller.clone()).await;
            assert_eq!(forbidden.0, StatusCode::FORBIDDEN);
            assert!(forbidden.1["message"].as_str().unwrap().starts_with("EnvironmentCommandDenied:"));
            let command =
                serde_json::to_value(Command::builder().action(CommandAction::QueryCrewList { context: Default::default() }).build())
                    .unwrap();
            let accepted = h.call("POST", "/v1/commands", Some(command), cbor, caller.clone()).await;
            assert_eq!(accepted.0, StatusCode::ACCEPTED, "{}", accepted.1);
            let url = accepted.1["url"].as_str().unwrap();
            assert_eq!(h.call("GET", url, None, cbor, CallerAuth::Bearer).await.0, StatusCode::FORBIDDEN);
            let own = h.call("GET", url, None, cbor, caller.clone()).await;
            assert_eq!(own.0, StatusCode::OK);
            assert!(!own.1["outcome"].is_null());
            let sessions = daemon.resource_backend().using::<flotilla_resources::TerminalSession>("flotilla");
            let session = sessions.get("placed-session").await.unwrap();
            let mut status = session.status.unwrap();
            status.crew = None;
            sessions.update_status("placed-session", &session.metadata.resource_version, &status).await.unwrap();
            let retired = h.call("GET", url, None, cbor, caller).await;
            assert_eq!(retired.0, StatusCode::FORBIDDEN);
            assert_eq!(retired.1["message"], "EnvironmentIdentityUnavailable: no placed crew");
        }
    }

    // Known oversize bodies are refused without polling the body stream. Chunked bodies
    // are bounded during collection, and even GET routes enforce their zero-byte limit.
    #[tokio::test]
    async fn body_limits_precede_buffering() {
        let h = Harness::new().await;
        for (method, path, limit) in
            [("POST", "/v1/commands", COMMAND_LIMIT), ("POST", "/v1/artifacts", SMALL_LIMIT), ("GET", "/v1/status", 0)]
        {
            let body = Body::from_stream(futures::stream::once(async {
                panic!("oversized body must not be polled");
                #[allow(unreachable_code)]
                Ok::<_, std::io::Error>(bytes::Bytes::new())
            }));
            let mut request = HttpRequest::builder().method(method).uri(path).header(header::CONTENT_LENGTH, limit + 1).body(body).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
            let mut request = HttpRequest::builder().method(method).uri(path).body(Body::from(vec![b' '; limit + 1])).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);

            // Limits are inclusive, including the empty-body route's zero boundary.
            let (mut boundary, expected) = match path {
                "/v1/commands" => (
                    serde_json::to_vec(&Command::builder().action(CommandAction::Refresh { repo: None }).build()).unwrap(),
                    StatusCode::ACCEPTED,
                ),
                "/v1/artifacts" => (
                    serde_json::to_vec(
                        &json!({"kind":"explainer","subject":"test","summary":{},"media_type":"text/plain","source_path":"/tmp/report"}),
                    )
                    .unwrap(),
                    StatusCode::FORBIDDEN,
                ),
                _ => (Vec::new(), StatusCode::OK),
            };
            boundary.resize(limit, b' ');
            let mut request =
                HttpRequest::builder().method(method).uri(path).header(header::CONTENT_LENGTH, limit).body(Body::from(boundary)).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), expected);
        }
    }

    fn user() -> CommandCaller {
        CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace("flotilla"), process: None, crew: None }
    }

    fn step(description: String) -> DaemonEvent {
        DaemonEvent::CommandStepUpdate {
            command_id: 77,
            node_id: flotilla_protocol::NodeId::new("test"),
            repo_identity: flotilla_daemon_api::action_support::empty_repo_identity(),
            repo: None,
            step_index: 0,
            step_count: 1,
            description,
            status: flotilla_protocol::StepStatus::Started,
        }
    }

    fn finished() -> DaemonEvent {
        DaemonEvent::CommandFinished {
            command_id: 77,
            node_id: flotilla_protocol::NodeId::new("test"),
            repo_identity: flotilla_daemon_api::action_support::empty_repo_identity(),
            repo: None,
            result: CommandValue::Cancelled,
        }
    }

    // ADR 0065 gives token/session callers the same user authority as the main socket.
    // Pin administrative routes explicitly, including shutdown, and shared user ownership.
    #[tokio::test]
    async fn tcp_user_policy_and_shared_operator_commands() {
        let logins = LoginStore::new(Arc::new(flotilla_resources::SystemClock));
        let code = logins.mint_code().unwrap();
        let (session, _) = logins.redeem(&code.code).unwrap().unwrap();
        for cbor in [false, true] {
            for caller in [CallerAuth::Bearer, CallerAuth::Session(session)] {
                let mut h = Harness::new().await;
                for (method, path, body, expected) in [
                    ("GET", "/v1/status", None, StatusCode::OK),
                    ("DELETE", "/v1/repos?path=/missing", None, StatusCode::NOT_FOUND),
                    ("POST", "/v1/repos/refresh", Some(json!({"repo":"/missing"})), StatusCode::NOT_FOUND),
                    ("POST", "/v1/repos", Some(json!({"path":"/user-selected-path"})), StatusCode::CREATED),
                    ("POST", "/v1/shutdown", None, StatusCode::OK),
                ] {
                    let reply = h.call(method, path, body, cbor, caller.clone()).await;
                    assert_eq!(reply.0, expected, "{method} {path}: {}", reply.1);
                }
                assert_eq!(h.shutdown.try_recv(), Ok(()));
            }
            let h = Harness::new().await;
            let accepted = h.call("POST", "/v1/commands", Some(watch_command()), cbor, CallerAuth::Bearer).await;
            let url = accepted.1["url"].as_str().unwrap();
            assert_eq!(h.call("GET", url, None, cbor, CallerAuth::Session(session)).await.0, StatusCode::OK);
            assert_eq!(
                h.call_with_evidence("DELETE", url, None, cbor, socket_caller(), Some(host_evidence())).await.0,
                StatusCode::ACCEPTED
            );
            let final_state = h.call("GET", &format!("{url}?wait=1"), None, cbor, CallerAuth::Session(session)).await;
            assert_eq!(final_state.1["state"], "finished");
        }
    }

    // Full capacity rejects live work; when completions exist, completion order rather
    // than UUID order selects eviction, including completions with a reserved permit.
    #[tokio::test]
    async fn record_capacity_evicts_oldest_completion() {
        let h = Harness::new().await;
        for n in 0..RECORD_LIMIT {
            let id = Uuid::from_u128((RECORD_LIMIT - n) as u128);
            let permit = h.commands.reserve().await.unwrap();
            let (_, receiver) = watch::channel(CommandState::new(id, None, &h.commands.eviction_sequence));
            h.commands
                .records
                .lock()
                .await
                .insert(id, Record { _permit: permit, command_id: Some(n as u64), owner: user(), state: receiver, collector: None });
        }
        assert!(h.commands.reserve().await.is_err());
        let oldest_id = Uuid::from_u128(RECORD_LIMIT as u128);
        for id in [oldest_id, Uuid::from_u128(1)] {
            let (sender, receiver) = watch::channel(CommandState::new(id, Some(CommandValue::Cancelled), &h.commands.eviction_sequence));
            h.commands.records.lock().await.get_mut(&id).unwrap().state = receiver;
            drop(sender);
        }
        let permit = h.commands.reserve().await.unwrap();
        let records = h.commands.records.lock().await;
        assert!(!records.contains_key(&oldest_id));
        assert!(records.contains_key(&Uuid::from_u128(1)));
        assert_eq!(records.len(), RECORD_LIMIT - 1);
        drop(records);
        drop(permit);
    }

    // Lagged/closed observations must never fabricate an execution outcome. A lagged
    // observer keeps consuming and can recover the actual finish event later.
    #[tokio::test]
    async fn lagged_progress_is_unknown_then_recovers() {
        let order = Arc::new(AtomicU64::new(0));
        let (events, receiver) = broadcast::channel(1);
        events.send(step("first".into())).unwrap();
        events.send(step("second".into())).unwrap();
        let (sender, mut state) = watch::channel(CommandState::new(Uuid::new_v4(), None, &order));
        let task = tokio::spawn(collect_progress(receiver, 77, sender, order.clone(), CompletionScope::default()));
        tokio::time::timeout(Duration::from_secs(1), state.wait_for(|s| s.state == "unknown")).await.unwrap().unwrap();
        assert!(state.borrow().outcome.is_none());
        assert!(state.borrow().progress_truncated);
        assert!(state.borrow().observation_error.is_some());
        events.send(finished()).unwrap();
        task.await.unwrap();
        assert_eq!(state.borrow().state, "finished");
        assert!(state.borrow().observation_error.is_none());
        assert!(state.borrow().unknown_since.is_none());
        assert!(matches!(state.borrow().outcome, Some(CommandValue::Cancelled)));

        let (events, receiver) = broadcast::channel(1);
        let (sender, state) = watch::channel(CommandState::new(Uuid::new_v4(), None, &order));
        drop(events);
        collect_progress(receiver, 77, sender, order, CompletionScope::default()).await;
        assert_eq!(state.borrow().state, "unknown");
        assert!(state.borrow().outcome.is_none());
        assert!(state.borrow().collector_closed);
        assert!(state.borrow().evictable_order(Instant::now()).is_some());
    }

    // Generated histories span empty, count/byte boundaries, oversized individual updates,
    // and repeated step indexes. Each retained suffix stays inside both limits.
    #[hegel::test]
    fn progress_history_is_bounded(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(MAX_STEPS + 5));
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(4096));
        let mut state = CommandState::new(Uuid::nil(), None, &AtomicU64::new(0));
        for _ in 0..count {
            state.push_step(step("x".repeat(length)));
            assert!(state.steps.len() <= MAX_STEPS);
            assert!(state.progress_bytes <= MAX_PROGRESS_BYTES);
        }
        state.push_step(step("x".repeat(MAX_PROGRESS_BYTES + 1)));
        assert!(state.progress_truncated);
        assert!(state.progress_bytes <= MAX_PROGRESS_BYTES);
        // Count and byte edges must both evict while preserving the newest event.
        for _ in 0..MAX_STEPS + 1 {
            state.push_step(step(String::new()));
        }
        assert_eq!(state.steps.len(), MAX_STEPS);
        assert!(state.progress_truncated);
    }

    // Pin the scheduling boundary where the daemon has removed the active execution but
    // the HTTP observer has not yet received completion. Existing resources conflict (409).
    #[tokio::test]
    async fn cancel_after_unobserved_completion_is_conflict() {
        let h = Harness::new().await;
        let id = Uuid::new_v4();
        let (_, receiver) = watch::channel(CommandState::new(id, None, &h.commands.eviction_sequence));
        let permit = h.commands.reserve().await.unwrap();
        h.commands
            .records
            .lock()
            .await
            .insert(id, Record { _permit: permit, command_id: Some(77), owner: user(), state: receiver, collector: None });
        for cbor in [false, true] {
            let reply = h.call("DELETE", &format!("/v1/commands/{id}"), None, cbor, CallerAuth::Bearer).await;
            assert_eq!(reply.0, StatusCode::CONFLICT);
        }
    }

    // Wait expiry is only a snapshot: an execution finishing later retains its real
    // outcome at the same URL, without cancellation or replacing the record.
    #[tokio::test]
    async fn expired_wait_can_observe_later_completion() {
        let h = Harness::new().await;
        let id = Uuid::new_v4();
        let (events, receiver) = broadcast::channel(4);
        let (sender, state) = watch::channel(CommandState::new(id, None, &h.commands.eviction_sequence));
        let permit = h.commands.reserve().await.unwrap();
        h.commands.records.lock().await.insert(id, Record { _permit: permit, command_id: Some(77), owner: user(), state, collector: None });
        let task = tokio::spawn(collect_progress(receiver, 77, sender, h.commands.eviction_sequence.clone(), CompletionScope::default()));
        let url = format!("/v1/commands/{id}");
        let running = h.call("GET", &format!("{url}?wait=0.001"), None, false, CallerAuth::Bearer).await;
        assert_eq!(running.1["state"], "running");
        assert!(running.1["outcome"].is_null());
        events.send(finished()).unwrap();
        let done = h.call("GET", &format!("{url}?wait=1"), None, true, CallerAuth::Bearer).await;
        assert_eq!(done.1["state"], "finished");
        assert_eq!(done.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
        task.await.unwrap();
    }

    // Media types ignore case and quality parameter whitespace. An invalid CBOR Accept
    // still yields a CBOR-readable refusal; absent Content-Type remains the JSON default.
    #[tokio::test]
    async fn negotiation_normalizes_case_whitespace_and_errors() {
        let h = Harness::new().await;
        for (accept, status, media) in [
            ("Application/JSON; q = 1", StatusCode::OK, "application/json"),
            ("Application/CBOR; q = 1", StatusCode::OK, "application/cbor"),
            ("Application/CBOR; q = invalid", StatusCode::NOT_ACCEPTABLE, "application/cbor"),
        ] {
            let mut request = HttpRequest::get("/v1/status").header(header::ACCEPT, accept).body(Body::empty()).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            let response = h.router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CONTENT_TYPE], media);
            if status == StatusCode::NOT_ACCEPTABLE {
                let bytes = to_bytes(response.into_body(), SMALL_LIMIT).await.unwrap();
                let error: Value = flotilla_cbor::from_slice(&bytes, &Default::default()).unwrap();
                assert_eq!(error["message"], "invalid Accept quality");
            }
        }
        for (media, bytes) in [
            (None, serde_json::to_vec(&watch_command()).unwrap()),
            (Some("Application/CBOR"), flotilla_cbor::to_vec(&watch_command(), &Default::default()).unwrap()),
        ] {
            let mut builder = HttpRequest::post("/v1/commands");
            if let Some(media) = media {
                builder = builder.header(header::CONTENT_TYPE, media);
            }
            let mut request = builder.body(Body::from(bytes)).unwrap();
            request.extensions_mut().insert(CallerAuth::Bearer);
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), StatusCode::ACCEPTED);
        }
    }

    // Unknown records retain a bounded recovery window. At full capacity, the grace
    // boundary releases the oldest uncertain slot and aborts its orphan collector.
    #[tokio::test]
    async fn unknown_records_expire_and_eviction_stops_collectors() {
        let h = Harness::new().await;
        let now = Instant::now();
        let (events, _) = broadcast::channel(1);
        let mut first_state = None;
        for n in 0..RECORD_LIMIT {
            let id = Uuid::from_u128((RECORD_LIMIT - n) as u128);
            let permit = h.commands.reserve_at(now).await.unwrap();
            let mut state = CommandState::new(id, None, &h.commands.eviction_sequence);
            state.observation_lost("lost completion".into(), false, &h.commands.eviction_sequence);
            state.unknown_since = Some(now);
            state.observation_lost("another lag".into(), false, &h.commands.eviction_sequence);
            assert_eq!(state.unknown_since, Some(now), "repeated lag cannot extend retention");
            let (sender, receiver) = watch::channel(state);
            if n == 0 {
                first_state = Some(receiver.clone());
            }
            let collector = Some(tokio::spawn(collect_progress(
                events.subscribe(),
                n as u64,
                sender,
                h.commands.eviction_sequence.clone(),
                CompletionScope::default(),
            )));
            h.commands
                .records
                .lock()
                .await
                .insert(id, Record { _permit: permit, command_id: Some(n as u64), owner: user(), state: receiver, collector });
        }
        assert!(h.commands.reserve_at(now + UNKNOWN_RETENTION - Duration::from_nanos(1)).await.is_err());
        let permit = h.commands.reserve_at(now + UNKNOWN_RETENTION).await.unwrap();
        assert!(!h.commands.records.lock().await.contains_key(&Uuid::from_u128(RECORD_LIMIT as u128)));
        assert_eq!(h.commands.records.lock().await.len(), RECORD_LIMIT - 1);
        let mut first_state = first_state.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), first_state.changed()).await.unwrap().is_err(),
            "eviction must abort the collector and close its watch sender"
        );
        assert!(first_state.borrow().outcome.is_none(), "eviction cannot invent an execution outcome");
        drop(permit);
    }

    // A transient unknown state is not a terminal long-poll condition. Wait through it,
    // recover the real outcome, and clear the observation error in both encodings.
    #[tokio::test]
    async fn transient_unknown_waits_for_actual_completion() {
        for cbor in [false, true] {
            let h = Harness::new().await;
            let id = Uuid::new_v4();
            let mut state = CommandState::new(id, None, &h.commands.eviction_sequence);
            state.observation_lost("transient lag".into(), false, &h.commands.eviction_sequence);
            let (sender, receiver) = watch::channel(state);
            let observer = sender.clone();
            let (events, input) = broadcast::channel(4);
            let collector =
                Some(tokio::spawn(collect_progress(input, 77, sender, h.commands.eviction_sequence.clone(), CompletionScope::default())));
            let permit = h.commands.reserve().await.unwrap();
            h.commands
                .records
                .lock()
                .await
                .insert(id, Record { _permit: permit, command_id: Some(77), owner: user(), state: receiver, collector });
            let url = format!("/v1/commands/{id}?wait=2");
            let mut request = Box::pin(h.call("GET", &url, None, cbor, CallerAuth::Bearer));
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    tokio::select! {
                        biased;
                        reply = &mut request => panic!("unknown returned before completion: {}", reply.1),
                        () = tokio::task::yield_now() => {},
                    }
                    if observer.receiver_count() > 1 {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            events.send(finished()).unwrap();
            let reply = request.await;
            assert_eq!(reply.1["state"], "finished");
            assert_eq!(reply.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
            assert!(reply.1["observation_error"].is_null());
            assert_eq!(reply.1["progress_truncated"], true);
        }
    }

    // A closed observer cannot recover, so it is immediately evictable and releases
    // long polls without pretending the underlying execution has a known outcome.
    #[tokio::test]
    async fn closed_unknown_is_evictable_and_releases_wait() {
        let h = Harness::new().await;
        let id = Uuid::new_v4();
        let (sender, receiver) = watch::channel(CommandState::new(id, None, &h.commands.eviction_sequence));
        let (events, input) = broadcast::channel(1);
        drop(events);
        collect_progress(input, 77, sender, h.commands.eviction_sequence.clone(), CompletionScope::default()).await;
        assert!(receiver.borrow().evictable_order(Instant::now()).is_some());
        let permit = h.commands.reserve().await.unwrap();
        h.commands
            .records
            .lock()
            .await
            .insert(id, Record { _permit: permit, command_id: Some(77), owner: user(), state: receiver, collector: None });
        let url = format!("/v1/commands/{id}?wait=300");
        let reply = tokio::time::timeout(Duration::from_secs(1), h.call("GET", &url, None, true, CallerAuth::Bearer)).await.unwrap();
        assert_eq!(reply.1["state"], "unknown");
        assert!(reply.1["outcome"].is_null());
        assert!(reply.1["observation_error"].is_string());
    }

    // Cached byte accounting and the deque are private: each retained step keeps the
    // existing event wire shape and never exposes its cached size or wrapper object.
    #[test]
    fn retained_step_keeps_event_wire_shape() {
        let event = step("wire contract".into());
        let expected = serde_json::to_value(&event).unwrap();
        let retained = RetainedStep { event, encoded_bytes: 999 };
        assert_eq!(serde_json::to_value(retained).unwrap(), expected);
    }

    // Generated operation sequences cover repeated cancellation, empty/positive waits,
    // invalid waits and reads after completion. The HTTP adapter needs a router fixture;
    // routing/placement itself is delegated unchanged to the existing scenario engine.
    #[hegel::test]
    fn generated_command_lifecycle(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let cbor = tc.draw(gs::booleans());
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
        let operations: Vec<usize> = (0..count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(3))).collect();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let h = Harness::new().await;
            let accepted = h.call("POST", "/v1/commands", Some(watch_command()), cbor, CallerAuth::Bearer).await;
            assert_eq!(accepted.0, StatusCode::ACCEPTED);
            let url = accepted.1["url"].as_str().unwrap();
            let mut finished = false;
            for operation in operations {
                match operation {
                    0 | 1 => {
                        let reply = h
                            .call(
                                "GET",
                                &format!("{url}?wait={}", if operation == 0 { "0" } else { "0.001" }),
                                None,
                                cbor,
                                CallerAuth::Bearer,
                            )
                            .await;
                        assert_eq!(reply.0, StatusCode::OK);
                        assert_eq!(reply.1["outcome"].is_null(), !finished);
                    }
                    2 => {
                        let reply = h.call("DELETE", url, None, cbor, CallerAuth::Bearer).await;
                        assert_eq!(reply.0, if finished { StatusCode::CONFLICT } else { StatusCode::ACCEPTED });
                        let reply = h.call("GET", &format!("{url}?wait=1"), None, cbor, CallerAuth::Bearer).await;
                        assert_eq!(reply.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
                        finished = true;
                    }
                    _ => {
                        let reply = h.call("GET", &format!("{url}?wait=NaN"), None, cbor, CallerAuth::Bearer).await;
                        assert_eq!(reply.0, StatusCode::BAD_REQUEST);
                    }
                }
            }
            if !finished {
                h.call("DELETE", url, None, cbor, CallerAuth::Bearer).await;
                let reply = h.call("GET", &format!("{url}?wait=1"), None, cbor, CallerAuth::Bearer).await;
                assert!(!reply.1["outcome"].is_null());
            }
        });
    }

    // ADR 0063 wraps only integers outside the interoperable JSON range, recursively;
    // CBOR retains the original integer. The generator spans both limits and extrema.
    #[hegel::test]
    fn json_integer_rendering_has_exact_boundaries(tc: hegel::TestCase) {
        let n = tc.draw(hegel::generators::integers::<i64>());
        let mut value = json!({"nested":[n, u64::MAX]});
        large_integers(&mut value);
        let expected = if n > (1i64 << 53) - 1 {
            json!({"u64":n.to_string()})
        } else if n < -((1i64 << 53) - 1) {
            json!({"i64":n.to_string()})
        } else {
            json!(n)
        };
        assert_eq!(value["nested"][0], expected);
        assert_eq!(value["nested"][1], json!({"u64":u64::MAX.to_string()}));
        for n in [-(1i64 << 53), -((1i64 << 53) - 1), (1i64 << 53) - 1, 1i64 << 53, i64::MIN, i64::MAX] {
            let mut value = json!(n);
            large_integers(&mut value);
            assert_eq!(value.is_number(), (-((1i64 << 53) - 1)..=((1i64 << 53) - 1)).contains(&n));
        }
    }
}
