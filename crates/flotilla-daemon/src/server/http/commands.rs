//! REST adapter for the existing request dispatcher. Command records are ephemeral and
//! bounded; subscribe before dispatch so even an immediately completed command is retained.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    CommandCaller, CommandValue, DaemonEvent, Message, PrincipalRef, Request as RpcRequest, Response as RpcResponse, ResponseResult,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{watch, Mutex};
use uuid::Uuid;

use super::{auth, CallerAuth, RouterState};
use crate::server::{
    caller, client_connection::QuerySubscriptions, environment_connection, environment_supervision, remote_commands::RemoteCommandRouter,
    request_dispatch::RequestDispatcher,
};

const COMMAND_LIMIT: usize = 1024 * 1024;
const SMALL_LIMIT: usize = 64 * 1024;
const RECORD_LIMIT: usize = 1024;
const MAX_WAIT: f64 = 300.0;

pub(super) struct Commands {
    router: RemoteCommandRouter,
    shutdown: tokio::sync::mpsc::UnboundedSender<()>,
    socket_path: PathBuf,
    records: Mutex<BTreeMap<Uuid, Record>>,
    capacity: Arc<tokio::sync::Semaphore>,
}

struct Record {
    _permit: tokio::sync::OwnedSemaphorePermit,
    command_id: Option<u64>,
    owner: CommandCaller,
    state: watch::Receiver<CommandState>,
}

#[derive(Clone, Serialize)]
struct CommandState {
    id: Uuid,
    state: &'static str,
    steps: Vec<DaemonEvent>,
    outcome: Option<CommandValue>,
}

impl Commands {
    pub(super) fn new(router: RemoteCommandRouter, shutdown: tokio::sync::mpsc::UnboundedSender<()>, socket_path: PathBuf) -> Self {
        Self {
            router,
            shutdown,
            socket_path,
            records: Mutex::new(BTreeMap::new()),
            capacity: Arc::new(tokio::sync::Semaphore::new(RECORD_LIMIT)),
        }
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Json,
    Cbor,
}

impl Encoding {
    fn response(headers: &axum::http::HeaderMap) -> Result<Self, &'static str> {
        // Honor quality weights, including an explicit q=0 exclusion.
        let Some(accept) = headers.get(header::ACCEPT) else { return Ok(Self::Json) };
        let accept = accept.to_str().map_err(|_| "invalid Accept header")?;
        let mut json_quality = None::<f32>;
        let mut cbor_quality = None::<f32>;
        for entry in accept.split(',') {
            let mut parts = entry.trim().split(';');
            let media = parts.next().unwrap_or_default().trim();
            let quality = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .next()
                .unwrap_or("1")
                .parse::<f32>()
                .map_err(|_| "invalid Accept quality")?;
            if !quality.is_finite() || !(0.0..=1.0).contains(&quality) {
                return Err("invalid Accept quality");
            }
            match media {
                "application/json" => json_quality = Some(quality),
                "application/cbor" => cbor_quality = Some(quality),
                "*/*" | "application/*" => {
                    json_quality.get_or_insert(quality);
                    cbor_quality.get_or_insert(quality);
                }
                _ => {}
            }
        }
        let jq = json_quality.unwrap_or(0.0);
        let cq = cbor_quality.unwrap_or(0.0);
        if cq > 0.0 && cq > jq {
            Ok(Self::Cbor)
        } else if jq > 0.0 {
            Ok(Self::Json)
        } else {
            Err("Accept must allow application/json or application/cbor")
        }
    }

    fn render(self, status: StatusCode, value: impl Serialize) -> Response {
        let mut value = match serde_json::to_value(value) {
            Ok(value) => value,
            Err(error) => return auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        let result = match self {
            Self::Cbor => flotilla_cbor::to_vec(&value, &flotilla_cbor::Limits { max_message: 16 * 1024 * 1024, ..Default::default() })
                .map(|bytes| ("application/cbor", bytes))
                .map_err(|e| e.to_string()),
            Self::Json => {
                large_integers(&mut value);
                serde_json::to_vec(&value).map(|bytes| ("application/json", bytes)).map_err(|e| e.to_string())
            }
        };
        match result {
            Ok((content_type, bytes)) => {
                (status, [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store")], bytes).into_response()
            }
            Err(error) => auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error),
        }
    }

    fn error(self, status: StatusCode, message: impl AsRef<str>) -> Response {
        self.render(status, json!({"message": message.as_ref()}))
    }
}

fn large_integers(value: &mut Value) {
    const SAFE: u64 = (1 << 53) - 1;
    match value {
        Value::Number(n) if n.as_u64().is_some_and(|n| n > SAFE) => *value = json!({"u64": n.to_string()}),
        Value::Number(n) if n.as_i64().is_some_and(|n| n < -(SAFE as i64)) => *value = json!({"i64": n.to_string()}),
        Value::Array(values) => values.iter_mut().for_each(large_integers),
        Value::Object(values) => values.values_mut().for_each(large_integers),
        _ => {}
    }
}

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
        ("POST", "/v1/events/replay") => Some(("replay_since", SMALL_LIMIT)),
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
        Err(error) => return auth::refusal(StatusCode::NOT_ACCEPTABLE, error),
    };
    if matches!(authenticated, CallerAuth::Anonymous) {
        let mut response = encoding.error(StatusCode::UNAUTHORIZED, "authentication required");
        response.headers_mut().insert(header::WWW_AUTHENTICATE, axum::http::HeaderValue::from_static("Bearer"));
        return response;
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
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .is_some_and(|v| v.to_str().ok().and_then(|v| v.parse::<u64>().ok()).is_none_or(|size| size > limit as u64))
    {
        return encoding.error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds route limit");
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
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
        _ => CommandCaller { principal_ref: PrincipalRef::implicit_for_namespace(&namespace), process: None, crew: None },
    };
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
                Err(e) => encoding.error(error_status(&e), e),
            };
        }
        let wait = match query.get("wait").map(|v| v.parse::<f64>()).transpose() {
            Ok(wait) if wait.is_none_or(|w| w.is_finite() && (0.0..=MAX_WAIT).contains(&w)) => wait.unwrap_or(0.0),
            _ => return encoding.error(StatusCode::BAD_REQUEST, "wait must be between 0 and 300 seconds"),
        };
        if wait > 0.0 {
            let _ = tokio::time::timeout(Duration::from_secs_f64(wait), receiver.wait_for(|s| s.outcome.is_some())).await;
        }
        return encoding.render(StatusCode::OK, receiver.borrow().clone());
    }
    let params = if limit == 0 {
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
        && matches!(authenticated, CallerAuth::Bearer | CallerAuth::Session(_))
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
    // Pending dispatches also consume capacity. Never hold the record lock across
    // dispatch: unrelated reads, cancellations and artifact requests must stay live.
    let permit = if kind == "execute" {
        let mut records = commands.records.lock().await;
        if commands.capacity.available_permits() == 0 {
            if let Some(id) = records.iter().find(|(_, r)| r.state.borrow().outcome.is_some()).map(|(id, _)| *id) {
                records.remove(&id);
            }
        }
        match commands.capacity.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return encoding.error(StatusCode::CONFLICT, "too many active HTTP commands"),
        }
    } else {
        None
    };
    let mut events = daemon.subscribe();
    let dispatcher = RequestDispatcher::new_with_caller(
        daemon,
        &commands.router,
        daemon.agent_state_store(),
        Uuid::new_v4(),
        QuerySubscriptions::default(),
        caller.clone(),
    );
    let mut response = dispatcher.dispatch(0, rpc).await;
    if let Some(project) = scope {
        environment_supervision::scope_response(&mut response, &caller.crew.as_ref().expect("authorized crew").namespace, &project);
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
        let initial = CommandState { id, state: if outcome.is_some() { "finished" } else { "running" }, steps: Vec::new(), outcome };
        let (sender, receiver) = watch::channel(initial);
        commands
            .records
            .lock()
            .await
            .insert(id, Record { _permit: permit.expect("execute reserves capacity"), command_id, owner: caller, state: receiver });
        if let Some(command_id) = command_id {
            tokio::spawn(async move {
                loop {
                    let event = match events.recv().await {
                        Ok(event) => event,
                        Err(error) => {
                            sender.send_modify(|s| {
                                s.state = "failed";
                                s.outcome = Some(CommandValue::Error {
                                    message: format!("command progress unavailable: {error}; do not retry blindly"),
                                });
                            });
                            break;
                        }
                    };
                    match event {
                        DaemonEvent::CommandFinished { command_id: finished, result, .. } if finished == command_id => {
                            sender.send_modify(|s| {
                                s.state = "finished";
                                s.outcome = Some(result);
                            });
                            break;
                        }
                        event @ DaemonEvent::CommandStepUpdate { command_id: updated, .. } if updated == command_id => {
                            sender.send_modify(|s| {
                                s.steps.push(event);
                            });
                        }
                        _ => {}
                    }
                }
            });
        }
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

fn error_status(message: &str) -> StatusCode {
    if message.contains("conflict")
        || message.contains("already exists")
        || message.contains("has finished")
        || message.starts_with("field ownership refused")
    {
        StatusCode::CONFLICT
    } else if message.contains("requires")
        || message.contains("Identity")
        || message.contains("Denied")
        || message.starts_with("unauthorized:")
    {
        StatusCode::FORBIDDEN
    } else if message.contains("not found")
        || message.contains("unknown command")
        || (message.starts_with("no ") && message.contains(" matches "))
    {
        StatusCode::NOT_FOUND
    } else if message.contains("invalid") || message.contains("must be") || message.contains("cannot") {
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
    use crate::server::http::{HttpRouter, ListenerOrigin, LoginStore};
    use crate::server::tests::placed_crew_daemon;

    struct Harness {
        _temp: tempfile::TempDir,
        router: Router,
        shutdown: tokio::sync::mpsc::UnboundedReceiver<()>,
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
            remote.install_blob_store(Arc::new(crate::blob_store::TieredBlobStore::new(temp.path(), vec![]))).unwrap();
            let (shutdown, receiver) = tokio::sync::mpsc::unbounded_channel();
            let router =
                HttpRouter::for_daemon(daemon.clone(), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
                    .with_commands(remote, shutdown, temp.path().join("daemon.sock"))
                    .into_router();
            Self { _temp: temp, router, shutdown: receiver }
        }

        async fn call(
            &self,
            method: &str,
            path: &str,
            body: Option<Value>,
            cbor: bool,
            caller: CallerAuth,
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
            ("GET", "/v1/artifacts/content?reference=artifact/missing&destination_path=/tmp/output.md", None, StatusCode::NOT_FOUND),
            ("POST", "/v1/events/replay", Some(json!({"last_seen":[]})), StatusCode::OK),
            ("POST", "/v1/shutdown", None, StatusCode::OK),
        ] {
            let mut h = Harness::new().await;
            let other = Harness::new().await;
            let json_reply = h.call(method, path, body.clone(), false, CallerAuth::Socket(None)).await;
            let cbor_h = if path.starts_with("/v1/repos") && method != "GET" { &other } else { &h };
            let cbor_reply = cbor_h.call(method, path, body, true, CallerAuth::Socket(None)).await;
            assert_eq!(json_reply.0, status, "{method} {path}: {}", json_reply.1);
            assert_eq!((json_reply.0, json_reply.1.clone()), (cbor_reply.0, cbor_reply.1), "{method} {path}");
            if path == "/v1/artifacts" && method == "POST" {
                assert_eq!(json_reply.1["message"], "artifact put requires a calling crew session");
            }
            if path == "/v1/shutdown" {
                assert_eq!(h.shutdown.try_recv(), Ok(()));
                assert_eq!(h.shutdown.try_recv(), Ok(()));
            }
        }
    }

    // A real daemon watch stays live across HTTP requests. Wait expires with partial
    // progress, DELETE cancels it, and concurrent waiters receive the same final outcome.
    #[tokio::test]
    async fn command_wait_progress_and_cancel_in_both_encodings() {
        for cbor in [false, true] {
            let h = Harness::new().await;
            let (status, accepted, headers) = h.call("POST", "/v1/commands", Some(watch_command()), cbor, CallerAuth::Socket(None)).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
            let url = accepted["url"].as_str().unwrap();
            assert_eq!(headers[header::LOCATION], url);
            assert_eq!(accepted["id"], url.strip_prefix("/v1/commands/").unwrap());
            let (status, running, _) = h.call("GET", &format!("{url}?wait=0.01"), None, cbor, CallerAuth::Socket(None)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(running["state"], "running");
            assert!(running["outcome"].is_null());
            assert!(!running["steps"].as_array().unwrap().is_empty());
            let wait_path = format!("{url}?wait=2");
            let (first, second, cancel) = tokio::join!(
                h.call("GET", &wait_path, None, cbor, CallerAuth::Socket(None)),
                h.call("GET", &wait_path, None, cbor, CallerAuth::Socket(None)),
                h.call("DELETE", url, None, cbor, CallerAuth::Socket(None)),
            );
            assert_eq!(cancel.0, StatusCode::ACCEPTED);
            assert_eq!(first.1, second.1);
            assert_eq!(first.1["state"], "finished");
            assert_eq!(first.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
            assert_eq!(h.call("DELETE", url, None, cbor, CallerAuth::Socket(None)).await.0, StatusCode::CONFLICT);
            assert_eq!(h.call("GET", &format!("{url}?wait=-1"), None, cbor, CallerAuth::Socket(None)).await.0, StatusCode::BAD_REQUEST);
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
                let reply = h.call("POST", "/v1/commands", Some(command), cbor, CallerAuth::Socket(None)).await;
                assert_eq!(reply.0, StatusCode::ACCEPTED, "{}", reply.1);
                let url = reply.1["url"].as_str().unwrap();
                let finished = h.call("GET", &format!("{url}?wait=2"), None, cbor, CallerAuth::Socket(None)).await;
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
                let body = json!({"last_seen":[]});
                let bytes = if input_cbor {
                    flotilla_cbor::to_vec(&body, &Default::default()).unwrap()
                } else {
                    serde_json::to_vec(&body).unwrap()
                };
                let mut request = HttpRequest::post("/v1/events/replay")
                    .header(header::CONTENT_TYPE, if input_cbor { "application/cbor" } else { "application/json" })
                    .header(header::ACCEPT, if output_cbor { "application/cbor" } else { "application/json" })
                    .body(Body::from(bytes))
                    .unwrap();
                request.extensions_mut().insert(CallerAuth::Bearer);
                let reply = h.router.clone().oneshot(request).await.unwrap();
                assert_eq!(reply.status(), StatusCode::OK);
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
        let h = Harness::new().await;
        for cbor in [false, true] {
            let body = json!({"last_seen":[{"stream":{"kind":"host","environment_id":"absent"},"seq":u64::MAX}]});
            assert_eq!(h.call("POST", "/v1/events/replay", Some(body), cbor, CallerAuth::Bearer).await.0, StatusCode::OK);
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
        ] {
            assert_eq!(error_status(message), status, "{message}");
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
            request.extensions_mut().insert(CallerAuth::Socket(None));
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), StatusCode::PAYLOAD_TOO_LARGE);
            let mut request = HttpRequest::builder().method(method).uri(path).body(Body::from(vec![b' '; limit + 1])).unwrap();
            request.extensions_mut().insert(CallerAuth::Socket(None));
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
            request.extensions_mut().insert(CallerAuth::Socket(None));
            assert_eq!(h.router.clone().oneshot(request).await.unwrap().status(), expected);
        }
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
            let accepted = h.call("POST", "/v1/commands", Some(watch_command()), cbor, CallerAuth::Socket(None)).await;
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
                                CallerAuth::Socket(None),
                            )
                            .await;
                        assert_eq!(reply.0, StatusCode::OK);
                        assert_eq!(reply.1["outcome"].is_null(), !finished);
                    }
                    2 => {
                        let reply = h.call("DELETE", url, None, cbor, CallerAuth::Socket(None)).await;
                        assert_eq!(reply.0, if finished { StatusCode::CONFLICT } else { StatusCode::ACCEPTED });
                        let reply = h.call("GET", &format!("{url}?wait=1"), None, cbor, CallerAuth::Socket(None)).await;
                        assert_eq!(reply.1["outcome"], serde_json::to_value(CommandValue::Cancelled).unwrap());
                        finished = true;
                    }
                    _ => {
                        let reply = h.call("GET", &format!("{url}?wait=NaN"), None, cbor, CallerAuth::Socket(None)).await;
                        assert_eq!(reply.0, StatusCode::BAD_REQUEST);
                    }
                }
            }
            if !finished {
                h.call("DELETE", url, None, cbor, CallerAuth::Socket(None)).await;
                let reply = h.call("GET", &format!("{url}?wait=1"), None, cbor, CallerAuth::Socket(None)).await;
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
