//! Agent hook routes (#3247): the router with injected callers, and the loopback TCP
//! listener driven by a stand-in for Claude Code's HTTP hook client that enforces its
//! documented request and response contract. No Docker and no other host.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use flotilla_agent::{ClaudeCodeHookTransport, SharedAgentStateStore, CLAUDE_CODE_HOOK_TOKEN_ENV};
use flotilla_protocol::{AgentHookTerminalRef, AgentStatus, AttachableId, EnvironmentId};
use flotilla_resources::{
    InputMeta, Selector, TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use super::{
    hooks::{HookKey, HOOK_KEY_FILE},
    tcp::{TOKEN_DIRECTORY, TOKEN_FILE},
    CallerAuth, HookEndpoint, HttpRouter, ListenAddress, ListenerOrigin, LoginStore, TcpListener,
};

const NAMESPACE: &str = "flotilla";
const CODER: &str = "terminal-work-coder";
const REVIEWER: &str = "terminal-work-reviewer";

fn terminal(session_name: &str) -> AgentHookTerminalRef {
    AgentHookTerminalRef { namespace: NAMESPACE.into(), session_name: session_name.into() }
}

fn attachable(session_name: &str) -> AttachableId {
    AttachableId::new(format!("terminal:{NAMESPACE}/{session_name}"))
}

/// A daemon's hook-handling state: its resource store with two running crew terminals,
/// its agent state store, and the router over both.
struct Daemon {
    backend: ResourceBackend,
    agents: SharedAgentStateStore,
    logins: Arc<LoginStore>,
    router: Router,
}

impl Daemon {
    async fn new() -> Self {
        Self::with_backend(ResourceBackend::new(InMemoryBackend::default())).await
    }

    async fn with_backend(backend: ResourceBackend) -> Self {
        for session in [CODER, REVIEWER] {
            running_crew_terminal(&backend, session).await;
        }
        let agents = flotilla_agent::shared_in_memory_agent_state_store();
        let logins = Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock)));
        let router = HttpRouter::builder()
            .backend(backend.clone())
            .agent_state_store(Arc::clone(&agents))
            .logins(Arc::clone(&logins))
            .tcp_origin(ListenerOrigin::default())
            .build()
            .into_router();
        Self { backend, agents, logins, router }
    }

    async fn call(&self, caller: CallerAuth, request: Request<Body>) -> StatusCode {
        let mut request = request;
        request.extensions_mut().insert(caller);
        self.router.clone().oneshot(request).await.expect("router is infallible").status()
    }

    async fn hook_caller(&self, name: &str) -> CallerAuth {
        let session = self.backend.clone().using::<TerminalSession>(NAMESPACE).get(name).await.expect("session");
        CallerAuth::Hook { terminal: terminal(name), generation: session.metadata.creation_timestamp.to_rfc3339() }
    }

    async fn attention(&self, session_name: &str) -> Option<TerminalAttentionState> {
        let session = self.backend.clone().using::<TerminalSession>(NAMESPACE).get(session_name).await.expect("terminal session");
        session.status.and_then(|status| status.attention).map(|attention| attention.state)
    }

    fn agent(&self, id: &AttachableId) -> Option<(AgentStatus, Option<String>)> {
        let store = self.agents.lock().expect("agent state store");
        store.get(id).map(|entry| (entry.status.clone(), entry.session_id.clone()))
    }
}

async fn running_crew_terminal(backend: &ResourceBackend, session_name: &str) {
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let created = sessions
        .create(
            &InputMeta::builder().name(session_name.to_string()).build(),
            &TerminalSessionSpec {
                env_ref: "host-direct".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".into(), adapter: None, model: None },
                    brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "work".into(), copies: Vec::new() },
                    context: Box::new(TerminalCrewContext { namespace: NAMESPACE.into(), convoy: "c".into(), vessel_ref: "v".into() }),
                    message: None,
                },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("create terminal session");
    let status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..TerminalSessionStatus::default() };
    sessions.update_status(session_name, &created.metadata.resource_version, &status).await.expect("running status");
}

/// The JSON Claude Code sends a hook: the documented common input fields and the event's own.
fn hook_input(hook_event_name: &str, session_id: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut input = serde_json::json!({
        "session_id": session_id,
        "transcript_path": format!("/home/crew/.claude/projects/workspace/{session_id}.jsonl"),
        "cwd": "/workspace",
        "permission_mode": "bypassPermissions",
        "hook_event_name": hook_event_name,
    });
    input.as_object_mut().expect("object").extend(extra.as_object().expect("extra fields").clone());
    input
}

fn hook_post(event: &str, input: &serde_json::Value) -> Request<Body> {
    Request::post(format!("/v1/hooks/claude-code/{event}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(input).expect("hook input")))
        .expect("request")
}

/// How Claude Code treats one HTTP hook's response (code.claude.com/docs/en/hooks,
/// "Response handling").
#[derive(Debug, PartialEq)]
enum HookOutcome {
    /// 2xx with an empty body: exit code 0 with no output.
    Succeeded,
    /// 2xx with a JSON object: the command-hook output schema.
    Output(serde_json::Value),
    /// Any other status or body, or a connection failure.
    NonBlockingError(String),
}

/// Stands in for Claude Code's HTTP hook client, per code.claude.com/docs/en/hooks
/// ("HTTP hook fields", "Request details", "Environment variable interpolation", "Response
/// handling"). It refuses settings Claude Code would refuse: an `http` handler on
/// SessionStart or Setup, a non-http URL, unknown handler fields, or non-string headers.
/// It interpolates `$VAR` and `${VAR}` in header values only for variables the handler
/// lists in `allowedEnvVars` (others become empty), sends `POST` with
/// `Content-Type: application/json` and the hook input as the body, and classifies the
/// response as Claude Code does.
struct ClaudeCode {
    settings: serde_json::Value,
    environment: BTreeMap<String, String>,
}

/// What firing one hook event ran.
#[derive(Debug, Default)]
struct Fired {
    commands: Vec<String>,
    http: Vec<HookOutcome>,
}

impl ClaudeCode {
    const COMMAND_ONLY_EVENTS: &[&str] = &["SessionStart", "Setup"];
    const HTTP_FIELDS: &[&str] = &["type", "url", "headers", "allowedEnvVars", "onFailure", "timeout", "if", "statusMessage", "once"];

    async fn fire(&self, hook_event_name: &str, matcher_value: &str, input: &serde_json::Value) -> Fired {
        assert_eq!(input["hook_event_name"], hook_event_name, "the input names its event");
        let mut fired = Fired::default();
        let groups = self.settings["hooks"][hook_event_name].as_array().cloned().unwrap_or_default();
        for group in groups {
            let matcher = group["matcher"].as_str().unwrap_or_default();
            if !(matcher.is_empty() || matcher == "*" || matcher.split('|').any(|alternative| alternative == matcher_value)) {
                continue;
            }
            for handler in group["hooks"].as_array().expect("hook handlers") {
                match handler["type"].as_str().expect("handler type") {
                    "command" => fired.commands.push(handler["command"].as_str().expect("command").to_owned()),
                    "http" => {
                        assert!(
                            !Self::COMMAND_ONLY_EVENTS.contains(&hook_event_name),
                            "Claude Code accepts only command and mcp_tool hooks on {hook_event_name}"
                        );
                        fired.http.push(self.post(handler, input).await);
                    }
                    other => panic!("unexpected handler type {other}"),
                }
            }
        }
        fired
    }

    async fn post(&self, handler: &serde_json::Value, input: &serde_json::Value) -> HookOutcome {
        let fields = handler.as_object().expect("handler object");
        if let Some(unknown) = fields.keys().find(|field| !Self::HTTP_FIELDS.contains(&field.as_str())) {
            panic!("Claude Code does not know HTTP hook field {unknown}");
        }
        let url = url::Url::parse(handler["url"].as_str().expect("url is required")).expect("absolute URL");
        assert_eq!(url.scheme(), "http", "a loopback hook URL is plain HTTP");
        let allowed: BTreeSet<&str> = handler["allowedEnvVars"]
            .as_array()
            .map(|names| names.iter().map(|name| name.as_str().expect("variable name")).collect())
            .unwrap_or_default();
        let mut headers = Vec::new();
        if let Some(configured) = handler.get("headers") {
            for (name, value) in configured.as_object().expect("headers object") {
                headers.push((name.clone(), self.interpolate(value.as_str().expect("header values are strings"), &allowed)));
            }
        }
        let body = serde_json::to_vec(input).expect("hook input");
        let authority = format!("{}:{}", url.host_str().expect("host"), url.port().expect("explicit port"));
        let mut request = format!("POST {} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n", url.path());
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        let address: SocketAddr = url.socket_addrs(|| None).expect("resolve")[0];
        let Ok(mut stream) = tokio::net::TcpStream::connect(address).await else {
            return HookOutcome::NonBlockingError("connection failed".into());
        };
        stream.write_all(request.as_bytes()).await.expect("request head");
        stream.write_all(&body).await.expect("request body");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("response");
        let response = String::from_utf8(response).expect("UTF-8 response");
        let (head, body) = response.split_once("\r\n\r\n").expect("response head");
        assert!(!head.to_ascii_lowercase().contains("transfer-encoding: chunked"), "hook replies are not streamed: {head}");
        let status: u16 = head.split(' ').nth(1).and_then(|code| code.parse().ok()).expect("status line");
        if !(200..300).contains(&status) {
            return HookOutcome::NonBlockingError(format!("{status}: {body}"));
        }
        if body.is_empty() {
            return HookOutcome::Succeeded;
        }
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(output @ serde_json::Value::Object(_)) => HookOutcome::Output(output),
            _ => HookOutcome::NonBlockingError(format!("{status} with a non-JSON body")),
        }
    }

    fn interpolate(&self, value: &str, allowed: &BTreeSet<&str>) -> String {
        let mut out = String::new();
        let mut rest = value;
        while let Some(start) = rest.find('$') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let (name, remainder) = if let Some(braced) = after.strip_prefix('{') {
                let end = braced.find('}').expect("closed ${...}");
                (&braced[..end], &braced[end + 1..])
            } else {
                let end = after.find(|character: char| !(character.is_ascii_alphanumeric() || character == '_')).unwrap_or(after.len());
                (&after[..end], &after[end..])
            };
            if allowed.contains(name) {
                out.push_str(self.environment.get(name).map(String::as_str).unwrap_or_default());
            }
            rest = remainder;
        }
        out.push_str(rest);
        out
    }
}

/// The loopback listener serving a daemon's router, bound beneath `state_dir`.
struct Listening {
    port: u16,
    hooks: HookEndpoint,
    file_token: String,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Listening {
    async fn start(daemon: &Daemon, state_dir: &Path) -> Self {
        let listen = ListenAddress::resolve_with("127.0.0.1:0", |_, _| async { Ok(Vec::new()) }).await.expect("listen address");
        let listener = TcpListener::bind(&listen, state_dir, Arc::clone(&daemon.logins)).await.expect("bind loopback listener");
        let port = listener.local_addr().expect("bound address").port();
        let hooks = HookEndpoint::default();
        hooks.set(listener.origin(), listener.hook_key().clone());
        let file_token = std::fs::read_to_string(state_dir.join(TOKEN_DIRECTORY).join(TOKEN_FILE)).expect("token file");
        let serving = tokio::spawn(listener.serve(daemon.router.clone(), std::future::pending()));
        Self { port, hooks, file_token, serving }
    }

    /// Claude Code as a host-direct crew in `session_name` runs it: the settings overlay and
    /// launch environment the runtime gives that session.
    async fn claude_code(&self, backend: &ResourceBackend, session_name: &str) -> ClaudeCode {
        let launch = self.hooks.claude_code_launch(backend, NAMESPACE, session_name).await.expect("listener is bound");
        assert_eq!(launch.base_url, format!("http://127.0.0.1:{}/v1/hooks/claude-code", self.port));
        let environment = [
            (flotilla_agent::CLAUDE_CODE_HOOK_URL_ENV.to_owned(), launch.base_url.clone()),
            (CLAUDE_CODE_HOOK_TOKEN_ENV.to_owned(), launch.token),
            ("FLOTILLA_NAMESPACE".to_owned(), NAMESPACE.to_owned()),
            ("FLOTILLA_TERMINAL_SESSION".to_owned(), session_name.to_owned()),
            ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "never-sent".to_owned()),
        ];
        let launch_environment = environment.to_vec();
        let settings = flotilla_agent::claude_code_hook_settings_over(ClaudeCodeHookTransport::from_environment(&launch_environment));
        ClaudeCode { settings, environment: environment.into_iter().collect() }
    }

    async fn post(&self, path: &str, authorization: Option<&str>) -> u16 {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await.expect("connect");
        let body = serde_json::to_vec(&hook_input("Stop", "session-1", serde_json::json!({}))).expect("body");
        let mut request = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n", self.port);
        if let Some(authorization) = authorization {
            request.push_str(&format!("Authorization: {authorization}\r\n"));
        }
        request.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        stream.write_all(request.as_bytes()).await.expect("request");
        stream.write_all(&body).await.expect("body");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("response");
        response.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or_else(|| panic!("status line: {response}"))
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

// A host-direct Claude Code crew delivers its hooks over HTTP and its state updates as the
// command hooks updated it: prompts and tools mark it working, Stop idle, a permission
// prompt needs input, and SessionEnd retires its agent. SessionStart stays a command, and
// no variable outside allowedEnvVars (the OAuth token) leaves the session.
#[tokio::test]
async fn host_direct_claude_code_hooks_update_their_terminal_over_http() {
    let daemon = Daemon::new().await;
    let state = tempfile::tempdir().expect("state dir");
    let tcp = Listening::start(&daemon, state.path()).await;
    let claude = tcp.claude_code(&daemon.backend, CODER).await;
    let coder = attachable(CODER);

    let start = claude.fire("SessionStart", "startup", &hook_input("SessionStart", "s-1", serde_json::json!({"source": "startup"}))).await;
    assert_eq!(start.commands, ["flotilla hook claude-code session-start"]);
    assert!(start.http.is_empty());

    let prompt = hook_input("UserPromptSubmit", "s-1", serde_json::json!({"prompt": "fix the bug"}));
    assert_eq!(claude.fire("UserPromptSubmit", "", &prompt).await.http, [HookOutcome::Succeeded]);
    assert_eq!(daemon.attention(CODER).await, Some(TerminalAttentionState::Working));
    assert_eq!(daemon.agent(&coder), Some((AgentStatus::Active, Some("s-1".into()))));

    let tool =
        hook_input("PreToolUse", "s-1", serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_use_id": "t-1"}));
    assert_eq!(claude.fire("PreToolUse", "Bash", &tool).await.http, [HookOutcome::Succeeded]);

    let stop = hook_input("Stop", "s-1", serde_json::json!({"stop_hook_active": false, "last_assistant_message": "done"}));
    assert_eq!(claude.fire("Stop", "", &stop).await.http, [HookOutcome::Succeeded]);
    assert_eq!(daemon.attention(CODER).await, Some(TerminalAttentionState::Idle));
    assert_eq!(daemon.agent(&coder).map(|(status, _)| status), Some(AgentStatus::Idle));

    let permission = hook_input(
        "Notification",
        "s-1",
        serde_json::json!({"message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt"}),
    );
    assert_eq!(claude.fire("Notification", "permission_prompt", &permission).await.http, [HookOutcome::Succeeded]);
    assert_eq!(daemon.attention(CODER).await, Some(TerminalAttentionState::NeedsInput));
    let idle = hook_input("Notification", "s-1", serde_json::json!({"message": "waiting", "notification_type": "idle_prompt"}));
    assert!(claude.fire("Notification", "idle_prompt", &idle).await.http.is_empty(), "unsubscribed notifications post nothing");

    assert_eq!(daemon.attention(REVIEWER).await, None, "another terminal is untouched");

    let end = hook_input("SessionEnd", "s-1", serde_json::json!({"reason": "other"}));
    assert_eq!(claude.fire("SessionEnd", "other", &end).await.http, [HookOutcome::Succeeded]);
    assert_eq!(daemon.agent(&coder), None);
}

// Router level, with each caller injected as its listener would attach it: a hook token's
// terminal decides which session the POST updates, whatever native session ID the body
// names, so one crew cannot alias another's agent. The user's own callers post as
// `flotilla hook` does outside a managed terminal. Environment sockets (until #3485),
// browser sessions and anonymous callers are refused, and a hook token reaches nothing
// but the hook routes.
#[tokio::test]
async fn hook_posts_reach_the_session_their_caller_authenticates() {
    let daemon = Daemon::new().await;
    let reviewer_stop = hook_input("Stop", "reviewer-session", serde_json::json!({"stop_hook_active": false}));
    assert_eq!(daemon.call(daemon.hook_caller(REVIEWER).await, hook_post("user-prompt-submit", &reviewer_stop)).await, StatusCode::OK);
    assert_eq!(daemon.attention(REVIEWER).await, Some(TerminalAttentionState::Working));

    // The coder's token, with the reviewer's native session ID in the body.
    assert_eq!(daemon.call(daemon.hook_caller(CODER).await, hook_post("stop", &reviewer_stop)).await, StatusCode::OK);
    assert_eq!(daemon.attention(CODER).await, Some(TerminalAttentionState::Idle));
    assert_eq!(daemon.attention(REVIEWER).await, Some(TerminalAttentionState::Working), "the reviewer is not aliased");
    assert_eq!(daemon.agent(&attachable(REVIEWER)).map(|(status, _)| status), Some(AgentStatus::Active));
    assert_eq!(daemon.agent(&attachable(CODER)).map(|(status, _)| status), Some(AgentStatus::Idle));

    let unmanaged = hook_input("Stop", "unmanaged-session", serde_json::json!({}));
    for caller in [CallerAuth::Socket(None), CallerAuth::Bearer] {
        assert_eq!(daemon.call(caller, hook_post("stop", &unmanaged)).await, StatusCode::OK);
    }
    let unmanaged_agent = {
        let store = daemon.agents.lock().expect("agent state store");
        let id = store.lookup_by_session_id("unmanaged-session").expect("unmanaged agent recorded once").clone();
        store.get(&id).map(|entry| entry.status.clone())
    };
    assert_eq!(unmanaged_agent, Some(AgentStatus::Idle));

    let environment = CallerAuth::Environment { environment: EnvironmentId::new("env-work"), peer: None };
    let session = daemon.logins.mint_code().ok().and_then(|code| daemon.logins.redeem(&code.code).ok().flatten()).expect("session").0;
    for (caller, status) in [
        (environment, StatusCode::FORBIDDEN),
        (CallerAuth::Session(session), StatusCode::FORBIDDEN),
        (CallerAuth::Anonymous, StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(daemon.call(caller.clone(), hook_post("stop", &unmanaged)).await, status, "{caller:?}");
    }

    let hook = daemon.hook_caller(CODER).await;
    assert_eq!(daemon.call(hook.clone(), Request::get("/v1/hello").body(Body::empty()).expect("request")).await, StatusCode::FORBIDDEN);
    let resources = Request::get("/apis/flotilla.work/v1/namespaces/flotilla/convoys").body(Body::empty()).expect("request");
    assert_eq!(daemon.call(hook.clone(), resources).await, StatusCode::FORBIDDEN);
    let login = Request::get("/login?code=anything").body(Body::empty()).expect("request");
    assert_eq!(daemon.call(hook, login).await, StatusCode::NOT_FOUND);
}

// A hook route takes Claude Code's JSON body for a known event: another media type is 415
// and an unknown event or malformed body is 400, so Claude Code reports a non-blocking error.
#[tokio::test]
async fn hook_routes_take_json_for_known_events() {
    let daemon = Daemon::new().await;
    let hook = daemon.hook_caller(CODER).await;
    let input = hook_input("Stop", "s-1", serde_json::json!({}));
    let text =
        Request::post("/v1/hooks/claude-code/stop").header(header::CONTENT_TYPE, "text/plain").body(Body::from("{}")).expect("request");
    assert_eq!(daemon.call(hook.clone(), text).await, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(daemon.call(hook.clone(), hook_post("teleport", &input)).await, StatusCode::BAD_REQUEST);
    let malformed = Request::post("/v1/hooks/claude-code/stop")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("["))
        .expect("request");
    assert_eq!(daemon.call(hook.clone(), malformed).await, StatusCode::BAD_REQUEST);
    let charset = Request::post("/v1/hooks/claude-code/stop")
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(serde_json::to_vec(&input).expect("input")))
        .expect("request");
    assert_eq!(daemon.call(hook, charset).await, StatusCode::OK);
}

// Over loopback TCP a hook POST needs a hook token for its terminal or the file token. A
// missing token, a token whose terminal was changed, a token signed by another key or a
// hook token presented anywhere but a hook route is 401. The hook key is private and
// outlives a restart, which replaces the file token, so running crews keep delivering.
#[tokio::test]
async fn hook_tokens_are_bound_to_their_terminal_and_survive_restarts() {
    let daemon = Daemon::new().await;
    let state = tempfile::tempdir().expect("state dir");
    let first = Listening::start(&daemon, state.path()).await;
    let token = first.hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await.expect("endpoint").token;
    let stop = "/v1/hooks/claude-code/stop";
    assert_eq!(first.post(stop, Some(&format!("Bearer {token}"))).await, 200);
    assert_eq!(first.post(stop, Some(&format!("Bearer {}", first.file_token))).await, 200, "the user's file token");
    assert_eq!(first.post(stop, None).await, 401, "no token");

    let mut parts: Vec<&str> = token.split('.').collect();
    let reviewer = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, REVIEWER);
    parts[2] = &reviewer;
    assert_eq!(first.post(stop, Some(&format!("Bearer {}", parts.join(".")))).await, 401, "terminal changed under the signature");
    let elsewhere = tempfile::tempdir().expect("other key");
    let foreign = HookKey::load_or_create(elsewhere.path()).expect("other key").mint(&terminal(CODER), "foreign-generation");
    assert_eq!(first.post(stop, Some(&format!("Bearer {foreign}"))).await, 401, "signed by another key");
    assert_eq!(first.post(stop, Some(&format!("Bearer {token}x"))).await, 401, "tampered signature");
    assert_eq!(first.post("/v1/hello", Some(&format!("Bearer {token}"))).await, 401, "hook token off the hook routes");

    let key = state.path().join(TOKEN_DIRECTORY).join(HOOK_KEY_FILE);
    assert_eq!(std::fs::metadata(&key).expect("hook key").permissions().mode() & 0o777, 0o600);
    let file_token = first.file_token.clone();
    drop(first);
    let restarted = Listening::start(&daemon, state.path()).await;
    assert_ne!(restarted.file_token, file_token, "the file token is replaced at each start");
    assert_eq!(restarted.post(stop, Some(&format!("Bearer {token}"))).await, 200, "a running crew's token still verifies");
    assert_eq!(restarted.hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await.expect("endpoint").token, token);
}

// Without a TCP listener there is no hook endpoint, and launches keep command hooks.
#[tokio::test]
async fn no_listener_means_no_hook_endpoint() {
    let daemon = Daemon::new().await;
    assert_eq!(HookEndpoint::default().claude_code_launch(&daemon.backend, NAMESPACE, CODER).await, None);
}

// The hook URL is fixed into each crew's settings at launch, so only a listener with a
// fixed port serves hooks: a port-0 listener binds elsewhere at the next start, and its
// launches keep command hooks rather than post to a dead port.
#[tokio::test]
async fn only_a_fixed_listen_port_serves_hooks() {
    let daemon = Daemon::new().await;
    let state = tempfile::tempdir().expect("state directory");
    let logins = Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock)));
    let ephemeral = ListenAddress::resolve_with("127.0.0.1:0", |_, _| async { Ok(Vec::new()) }).await.expect("listen address");
    let listener = TcpListener::bind(&ephemeral, state.path(), Arc::clone(&logins)).await.expect("bind port 0");
    let hooks = HookEndpoint::default();
    hooks.publish(&listener);
    assert_eq!(hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await, None, "port 0 keeps command hooks");
    drop(listener);

    // A probed free port can be taken before the listener binds it; probe again if so.
    let mut attempts = 0;
    let (port, listener) = loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|probe| probe.local_addr()).expect("free port").port();
        let fixed =
            ListenAddress::resolve_with(&format!("127.0.0.1:{port}"), |_, _| async { Ok(Vec::new()) }).await.expect("listen address");
        match TcpListener::bind(&fixed, state.path(), Arc::clone(&logins)).await {
            Ok(listener) => break (port, listener),
            Err(_) if attempts < 5 => attempts += 1,
            Err(error) => panic!("bind fixed port: {error}"),
        }
    };
    let hooks = HookEndpoint::default();
    hooks.publish(&listener);
    let launch = hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await.expect("a fixed port serves hooks");
    assert_eq!(launch.base_url, format!("http://127.0.0.1:{port}/v1/hooks/claude-code"));
}

// Concurrent creators of the hook key all end up holding the one key that landed on disk,
// so none of them mints tokens the next start refuses.
#[test]
fn concurrent_hook_key_creation_agrees_on_one_key() {
    let directory = tempfile::tempdir().expect("key directory");
    let terminal = terminal(CODER);
    let tokens: Vec<String> = std::thread::scope(|scope| {
        let creators: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| HookKey::load_or_create(directory.path()).expect("hook key").mint(&terminal, "generation")))
            .collect();
        creators.into_iter().map(|creator| creator.join().expect("creator")).collect()
    });
    let reloaded = HookKey::load_or_create(directory.path()).expect("reload").mint(&terminal, "generation");
    assert!(tokens.iter().all(|token| token == &reloaded), "every creator holds the published key");
    let leftovers: Vec<_> = std::fs::read_dir(directory.path()).expect("list").map(|entry| entry.expect("entry").file_name()).collect();
    assert_eq!(leftovers, [std::ffi::OsString::from(HOOK_KEY_FILE)], "no staged files remain");
}

// A token belongs to one creation generation, even when a deleted name is reused.
#[tokio::test]
async fn recreated_terminal_refuses_old_hook_token() {
    let database = tempfile::tempdir().expect("database");
    for backend in [
        ResourceBackend::new(InMemoryBackend::default()),
        ResourceBackend::new(flotilla_store_sqlite::SqliteBackend::open(database.path().join("sessions.sqlite")).expect("SQLite")),
    ] {
        let daemon = Daemon::with_backend(backend).await;
        let state = tempfile::tempdir().expect("state");
        let listener = Listening::start(&daemon, state.path()).await;
        let old = listener.hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await.expect("launch");
        let route = "/v1/hooks/claude-code/stop";
        assert_eq!(listener.post(route, Some(&format!("Bearer {}", old.token))).await, 200);
        daemon.backend.clone().using::<TerminalSession>(NAMESPACE).delete(CODER).await.expect("delete");
        assert_eq!(listener.post(route, Some(&format!("Bearer {}", old.token))).await, 401);
        running_crew_terminal(&daemon.backend, CODER).await;
        assert_eq!(listener.post(route, Some(&format!("Bearer {}", old.token))).await, 401);
        assert_eq!(daemon.attention(CODER).await, None);
        let fresh = listener.hooks.claude_code_launch(&daemon.backend, NAMESPACE, CODER).await.expect("new launch");
        assert_eq!(listener.post(route, Some(&format!("Bearer {}", fresh.token))).await, 200);
    }
}

// Internal processing failures are logged and returned as a generic 500 body.
// Glue: poisoning the real store lock exercises the handler's error delegation.
#[tokio::test]
async fn hook_internal_error_is_not_returned() {
    let daemon = Daemon::new().await;
    let agents = Arc::clone(&daemon.agents);
    let _ = std::thread::spawn(move || {
        let _guard = agents.lock().expect("store");
        panic!("private failure detail");
    })
    .join();
    let mut request = hook_post("stop", &hook_input("Stop", "s", serde_json::json!({})));
    request.extensions_mut().insert(CallerAuth::Bearer);
    let response = daemon.router.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(json, serde_json::json!({"message": "failed to process agent hook event"}));
    assert!(!String::from_utf8_lossy(&body).contains("lock poisoned"));
}

// The MAC binds every identity component, including the creation generation; changing
// any component invalidates it. Pure token logic needs no daemon or socket harness.
#[hegel::test]
fn hook_mac_binds_generation(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Covers empty, short and long identity components, including separator characters.
    let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(128));
    let generation = tc.draw(gs::integers::<u64>());
    let terminal = AgentHookTerminalRef { namespace: "n.".repeat(length), session_name: "s/".repeat(length) };
    let key = HookKey::load_or_create(tempfile::tempdir().expect("key dir").path()).expect("key");
    let token = key.mint(&terminal, &generation.to_string());
    if length == 0 {
        assert!(key.verify(&token).is_none(), "empty terminal identities are refused");
    } else {
        assert_eq!(key.verify(&token), Some((terminal.clone(), generation.to_string())));
        let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
        for index in 1..=4 {
            let original = parts[index].clone();
            parts[index] = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, "changed");
            assert!(key.verify(&parts.join(".")).is_none(), "component {index} is authenticated");
            parts[index] = original;
        }
    }
}

// Glue: a bound endpoint cannot mint an instance token for a missing session;
// the launch retains command hooks and reports the lookup failure in the daemon log.
#[tokio::test]
async fn missing_terminal_keeps_command_hooks() {
    let daemon = Daemon::new().await;
    let endpoint = HookEndpoint::listening_at("http://127.0.0.1:7878");
    assert_eq!(endpoint.claude_code_launch(&daemon.backend, NAMESPACE, "missing").await, None);
}
