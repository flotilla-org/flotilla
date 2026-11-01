//! The router in-process with injected caller authentication (`tower::ServiceExt::oneshot`),
//! the Unix front door over a socket pair, and the loopback TCP listener on a real
//! 127.0.0.1 port: flotilla's authentication behind rigging-http-serve's Host and Origin
//! policy. No Docker and no other host.

use std::{
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use chrono::{DateTime, Utc};
use flotilla_protocol::EnvironmentId;
use flotilla_resources::{Clock, Convoy, ConvoySpec, InputMeta};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use super::{
    login::{LOGIN_CODE_LIFETIME, SESSION_LIFETIME, SESSION_LIMIT},
    tcp::{TOKEN_DIRECTORY, TOKEN_FILE},
    CallerAuth, HttpRouter, ListenAddress, ListenerOrigin, LoginStore, TcpListener,
};

const ORIGIN: &str = "http://127.0.0.1:7878";

struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn advance(&self, by: chrono::TimeDelta) {
        *self.0.lock().expect("clock") += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

/// One daemon incarnation's router: its own resource store, login store and clock.
struct Incarnation {
    backend: ResourceBackend,
    clock: Arc<ManualClock>,
    logins: Arc<LoginStore>,
    origin: ListenerOrigin,
    router: Router,
}

impl Incarnation {
    fn new() -> Self {
        Self::with_origin(ListenerOrigin::known(ORIGIN))
    }

    fn with_origin(origin: ListenerOrigin) -> Self {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let clock = Arc::new(ManualClock(Mutex::new(DateTime::parse_from_rfc3339("2026-10-19T12:00:00Z").expect("time").into())));
        let logins = Arc::new(LoginStore::new(clock.clone()));
        let router =
            HttpRouter::builder().backend(backend.clone()).logins(Arc::clone(&logins)).tcp_origin(origin.clone()).build().into_router();
        Self { backend, clock, logins, origin, router }
    }

    /// The router as a listener serves it: the listener attaches the caller.
    async fn call(&self, caller: CallerAuth, request: Request<Body>) -> Reply {
        let mut request = request;
        request.extensions_mut().insert(caller);
        Reply::from(self.router.clone().oneshot(request).await.expect("router is infallible")).await
    }

    /// A fresh login URL's path and query, minted over the main socket.
    async fn login_path(&self) -> String {
        let minted = self.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
        let url = minted.json()["url"].as_str().expect("login url").to_owned();
        url.strip_prefix(self.origin.get().expect("listener origin")).expect("URL on the listener origin").to_owned()
    }

    /// A session cookie pair (`flotilla_session=...`) from a login exchange.
    async fn login_cookie(&self) -> String {
        let login = self.call(CallerAuth::Anonymous, get(&self.login_path().await)).await;
        assert_eq!(login.status, StatusCode::SEE_OTHER);
        cookie_pair(login.headers[header::SET_COOKIE].to_str().expect("cookie"))
    }

    fn session_of(&self, cookie: &str) -> Option<super::login::SessionId> {
        self.logins.session(&HeaderMap::from_iter([(header::COOKIE, cookie.parse().expect("cookie"))]))
    }
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().expect("cookie pair").to_owned()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: bytes::Bytes,
}

impl Reply {
    async fn from(response: axum::response::Response) -> Self {
        let (parts, body) = response.into_parts();
        Self { status: parts.status, headers: parts.headers, body: axum::body::to_bytes(body, usize::MAX).await.expect("body") }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| panic!("JSON body ({error}): {:?}", self.body))
    }
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).expect("request")
}

fn post(uri: &str) -> Request<Body> {
    Request::post(uri).body(Body::empty()).expect("request")
}

fn environment() -> CallerAuth {
    CallerAuth::Environment { environment: EnvironmentId::new("env-work"), peer: None }
}

// Every authenticated caller, whatever its listener, sees the same build identity and
// protocol fingerprint; an unauthenticated TCP caller sees none.
#[tokio::test]
async fn hello_reports_build_identity_and_protocol_fingerprint() {
    let daemon = Incarnation::new();
    for caller in [CallerAuth::Socket(None), environment(), CallerAuth::Bearer] {
        let reply = daemon.call(caller, get("/v1/hello")).await;
        assert_eq!(reply.status, StatusCode::OK);
        let hello = reply.json();
        assert_eq!(hello["build_id"], flotilla_daemon_api::build_info::build_id());
        assert_eq!(hello["protocol_fingerprint"], flotilla_protocol::PROTOCOL_FINGERPRINT);
        assert_eq!(hello["protocol_version"], flotilla_protocol::PROTOCOL_VERSION);
    }
    let reply = daemon.call(CallerAuth::Anonymous, get("/v1/hello")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.headers[header::WWW_AUTHENTICATE], "Bearer");
}

// The resource API keeps its paths, statuses and JSON error bodies on the router, and acts
// only for callers that are the user: an environment socket gains no store access.
#[tokio::test]
async fn resource_api_serves_user_callers_with_its_existing_contract() {
    let daemon = Incarnation::new();
    daemon
        .backend
        .using::<Convoy>("flotilla")
        .create(&InputMeta::builder().name("listed".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("seed convoy");
    let path = "/apis/flotilla.work/v1/namespaces/flotilla/convoys";
    for caller in [CallerAuth::Socket(None), CallerAuth::Bearer] {
        let reply = daemon.call(caller, get(path)).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(reply.json()["items"][0]["metadata"]["name"], "listed");
    }
    let missing = daemon.call(CallerAuth::Socket(None), get(&format!("{path}/absent"))).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.json()["message"].is_string());
    let unknown = daemon.call(CallerAuth::Socket(None), get("/elsewhere")).await;
    assert_eq!((unknown.status, unknown.json()), (StatusCode::NOT_FOUND, serde_json::json!({"message": "unknown resource API path"})));
    let write = daemon.call(CallerAuth::Socket(None), post(path)).await;
    assert_eq!((write.status, write.json()), (StatusCode::METHOD_NOT_ALLOWED, serde_json::json!({"message": "resource API is read-only"})));
    let invalid = daemon.call(CallerAuth::Socket(None), get(&format!("{path}?labelSelector=invalid"))).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);

    assert_eq!(daemon.call(environment(), get(path)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(daemon.call(CallerAuth::Anonymous, get(path)).await.status, StatusCode::UNAUTHORIZED);
}

// A collection watch streams the initial list and later changes as JSON lines.
#[tokio::test]
async fn resource_watch_streams_newline_delimited_events() {
    let daemon = Incarnation::new();
    let convoys = daemon.backend.using::<Convoy>("flotilla");
    convoys
        .create(&InputMeta::builder().name("before".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("seed convoy");
    let mut request = get("/apis/flotilla.work/v1/namespaces/flotilla/convoys?watch=true");
    request.extensions_mut().insert(CallerAuth::Socket(None));
    let response = daemon.router.clone().oneshot(request).await.expect("router");
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    convoys
        .create(&InputMeta::builder().name("after".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("watched convoy");
    let mut received = Vec::new();
    while !String::from_utf8_lossy(&received).contains("\"after\"") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), futures::StreamExt::next(&mut body))
            .await
            .expect("watch event in time")
            .expect("watch open")
            .expect("watch chunk");
        received.extend_from_slice(&chunk);
    }
    let lines = String::from_utf8(received).expect("UTF-8");
    let events =
        lines.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).expect("one JSON event per line")).collect::<Vec<_>>();
    assert!(events.iter().any(|event| event["object"]["metadata"]["name"] == "before"));
    assert!(events.iter().any(|event| event["type"] == "ADDED" && event["object"]["metadata"]["name"] == "after"));
}

// Only the main Unix socket mints login codes, and only when a TCP listener exists to
// print in the URL. A browser session or bearer token cannot mint further sessions.
#[tokio::test]
async fn login_codes_are_minted_over_the_main_socket_only() {
    let daemon = Incarnation::new();
    let minted = daemon.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
    assert_eq!(minted.status, StatusCode::CREATED);
    let url = minted.json()["url"].as_str().expect("url").to_owned();
    let code = url.strip_prefix(&format!("{ORIGIN}/login?code=")).expect("login URL on the listener origin");
    assert_eq!(code.len(), 64, "256-bit code in hex");
    assert_eq!(minted.json()["expires_at"], (daemon.clock.now() + LOGIN_CODE_LIFETIME).to_rfc3339());

    let session = daemon.login_cookie().await;
    let session = daemon.logins.session(&HeaderMap::from_iter([(header::COOKIE, session.parse().expect("cookie"))])).expect("session");
    for caller in [CallerAuth::Bearer, CallerAuth::Session(session), environment()] {
        assert_eq!(daemon.call(caller, post("/v1/web/login-codes")).await.status, StatusCode::FORBIDDEN);
    }
    assert_eq!(daemon.call(CallerAuth::Anonymous, post("/v1/web/login-codes")).await.status, StatusCode::UNAUTHORIZED);

    let without_listener = Incarnation::with_origin(ListenerOrigin::default());
    assert_eq!(without_listener.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await.status, StatusCode::CONFLICT);
}

// Visiting the login URL sets the session cookie and redirects to `/`. The code is single
// use, even within its lifetime, and expires. Socket callers never hold cookies, so the
// exchange is not served to them.
#[tokio::test]
async fn login_code_is_single_use_and_expires() {
    let daemon = Incarnation::new();
    let mint = || async {
        let minted = daemon.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
        minted.json()["url"].as_str().expect("url").strip_prefix(ORIGIN).expect("origin").to_owned()
    };
    let login = mint().await;
    let reply = daemon.call(CallerAuth::Anonymous, get(&login)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.headers[header::LOCATION], "/");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    // The code never leaves the login request: no referrer, and the redirect names only `/`.
    assert_eq!(reply.headers[header::REFERRER_POLICY], "no-referrer");
    let cookie = reply.headers[header::SET_COOKIE].to_str().expect("cookie");
    let (pair, attributes) = cookie.split_once("; ").expect("cookie attributes");
    assert!(pair.strip_prefix("flotilla_session=").is_some_and(|value| value.len() == 64 && value != login), "{cookie}");
    assert_eq!(attributes, "HttpOnly; SameSite=Strict; Path=/");
    assert_eq!(daemon.call(CallerAuth::Anonymous, get(&login)).await.status, StatusCode::UNAUTHORIZED, "second use");

    let expiring = mint().await;
    daemon.clock.advance(LOGIN_CODE_LIFETIME);
    assert_eq!(daemon.call(CallerAuth::Anonymous, get(&expiring)).await.status, StatusCode::UNAUTHORIZED, "expired");

    let unused = mint().await;
    assert_eq!(daemon.call(CallerAuth::Socket(None), get(&unused)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(daemon.call(environment(), get(&unused)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(daemon.call(CallerAuth::Anonymous, get("/login")).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(daemon.call(CallerAuth::Anonymous, get(&unused)).await.status, StatusCode::SEE_OTHER, "the refused visits did not use it");
}

// Sessions end at their lifetime, at logout, or when newer logins push them past the cap.
// A session that logs in again gets a fresh session, never more privilege.
#[tokio::test]
async fn sessions_expire_end_at_logout_and_are_capped() {
    let daemon = Incarnation::new();
    let expiring = daemon.login_cookie().await;
    daemon.clock.advance(SESSION_LIFETIME - chrono::TimeDelta::seconds(1));
    assert!(daemon.session_of(&expiring).is_some(), "within its lifetime");
    daemon.clock.advance(chrono::TimeDelta::seconds(1));
    assert!(daemon.session_of(&expiring).is_none(), "past its lifetime");

    let first = daemon.login_cookie().await;
    let mut cookies = Vec::new();
    for _ in 1..SESSION_LIMIT {
        cookies.push(daemon.login_cookie().await);
    }
    assert!(daemon.session_of(&first).is_some(), "the cap holds {SESSION_LIMIT} sessions");
    cookies.push(daemon.login_cookie().await);
    assert!(daemon.session_of(&first).is_none(), "the oldest session is evicted past the cap");
    assert!(cookies.iter().all(|cookie| daemon.session_of(cookie).is_some()));

    let current = cookies.last().expect("a live session").clone();
    let session = daemon.session_of(&current).expect("session");
    let relogin = daemon.call(CallerAuth::Session(session), get(&daemon.login_path().await)).await;
    assert_eq!(relogin.status, StatusCode::SEE_OTHER);
    let fresh = cookie_pair(relogin.headers[header::SET_COOKIE].to_str().expect("cookie"));
    let fresh_session = daemon.session_of(&fresh).expect("fresh session");
    assert_ne!(fresh_session, session, "a new login is a new session");
    assert_eq!(daemon.call(CallerAuth::Session(fresh_session), post("/v1/web/login-codes")).await.status, StatusCode::FORBIDDEN);

    let logout = daemon.call(CallerAuth::Session(session), post("/logout")).await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert_eq!(logout.headers[header::SET_COOKIE], "flotilla_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    assert!(daemon.session_of(&current).is_none(), "logged out");
    assert!(daemon.session_of(&fresh).is_some(), "other sessions survive");
    assert_eq!(daemon.call(CallerAuth::Bearer, post("/logout")).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(daemon.call(CallerAuth::Anonymous, post("/logout")).await.status, StatusCode::UNAUTHORIZED);
}

/// Send one request through the Unix front door, as the main or an environment socket
/// serves it, and return the raw response.
async fn through_front_door(router: Router, caller: CallerAuth, request: &str) -> String {
    let (mut client, mut server) = tokio::net::UnixStream::pair().expect("socket pair");
    let serving = tokio::spawn(async move {
        let first = server.read_u8().await.expect("preface");
        super::serve_sniffed(server, first, router, move |_| caller, std::future::pending()).await
    });
    client.write_all(request.as_bytes()).await.expect("request");
    let mut response = String::new();
    client.read_to_string(&mut response).await.expect("response");
    serving.await.expect("front door").expect("served");
    response
}

// A Unix-socket request is judged by its socket alone. A valid session cookie on the main
// socket does not make it a session (logout, which only a session may do, is 400), and on
// an environment socket it does not lift the environment's limits.
#[tokio::test]
async fn session_cookie_over_a_unix_socket_is_ignored() {
    let daemon = Incarnation::new();
    let cookie = daemon.login_cookie().await;
    let request = |method: &str, path: &str| {
        format!("{method} {path} HTTP/1.1\r\nHost: flotilla\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };
    let logout = through_front_door(daemon.router.clone(), CallerAuth::Socket(None), &request("POST", "/logout")).await;
    assert!(logout.starts_with("HTTP/1.1 400"), "{logout}");
    assert!(daemon.session_of(&cookie).is_some(), "the session was not ended");
    let resources = "/apis/flotilla.work/v1/namespaces/flotilla/convoys";
    let listed = through_front_door(daemon.router.clone(), environment(), &request("GET", resources)).await;
    assert!(listed.starts_with("HTTP/1.1 403"), "{listed}");
}

// The resource API's selector tests, unchanged, through the Unix front door: the server
// honors selectors instead of returning the entire collection, and malformed and
// undeclared field queries fail rather than silently broadening.
#[tokio::test]
async fn resource_http_honors_declared_selectors() {
    let daemon = Incarnation::new();
    let backend = daemon.backend.clone();
    let request = |parameters: String| {
        let router = daemon.router.clone();
        async move {
            let request = format!(
                "GET /apis/flotilla.work/v1/namespaces/flotilla/convoys?{parameters} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            );
            through_front_door(router, CallerAuth::Socket(None), &request).await
        }
    };
    let items = |response: &str| -> serde_json::Value {
        let body = response.split_once("\r\n\r\n").expect("HTTP response").1;
        serde_json::from_str::<serde_json::Value>(body).expect("JSON body")["items"].clone()
    };
    for (name, label) in [("selected", "target"), ("excluded", "other"), ("punctuation()!", "a+b")] {
        let meta =
            InputMeta::builder().name(name.to_owned()).labels(std::collections::BTreeMap::from([("team".into(), label.into())])).build();
        backend.using::<Convoy>("flotilla").create(&meta, &ConvoySpec::builder().workflow_ref("workflow".into()).build()).await.unwrap();
    }
    for parameters in ["labelSelector=team%3Dtarget", "fieldSelector=metadata.name%3Dselected"] {
        let response = request(parameters.into()).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(items(&response).as_array().unwrap().len(), 1);
        assert_eq!(items(&response)[0]["metadata"]["name"], "selected");
    }
    for (parameter, selector) in [("labelSelector", "team=a+b"), ("fieldSelector", "metadata.name=punctuation()!")] {
        let parameters = url::form_urlencoded::Serializer::new(String::new()).append_pair(parameter, selector).finish();
        let response = request(parameters).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(items(&response).as_array().unwrap().len(), 1);
        assert_eq!(items(&response)[0]["metadata"]["name"], "punctuation()!");
    }
    for parameters in [
        "labelSelector=invalid",
        "fieldSelector=spec.unknown%3Dx",
        "labelSelector=a!%3Db",
        "labelSelector=a%20in%20(x)",
        "labelSelector=a%3D%3Db",
    ] {
        assert!(request(parameters.into()).await.starts_with("HTTP/1.1 400"), "{parameters}");
    }
    let query = flotilla_store::StoreQuery::labels(&std::collections::BTreeMap::from([("team".into(), "target".into())]), false, true);
    let parameters: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("storeQuery", &serde_json::to_string(&query).unwrap())
        .append_pair("labelSelector", "team=target")
        .finish();
    assert!(request(parameters).await.starts_with("HTTP/1.1 400"), "duplicate merged selector key is refused");
    let parameters: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("storeQuery", &serde_json::to_string(&query).unwrap())
        .append_pair("fieldSelector", "metadata.name=selected")
        .finish();
    let response = request(parameters).await;
    assert!(response.starts_with("HTTP/1.1 200"), "distinct query and selector predicates are combined");
    assert_eq!(items(&response).as_array().unwrap().len(), 1);
}

/// A TCP listener over one incarnation's router, bound on 127.0.0.1 with a chosen port.
struct Listening {
    daemon: Incarnation,
    port: u16,
    token: String,
    serving: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Listening {
    async fn start(spec: &str, state_dir: &Path) -> Self {
        let listen = ListenAddress::resolve_with(spec, |_, _| async { Ok(vec!["127.0.0.1:0".parse::<SocketAddr>().expect("address")]) })
            .await
            .expect("loopback listen address");
        let daemon = Incarnation::with_origin(ListenerOrigin::default());
        let key = super::HookEndpoint::load(state_dir).expect("hook key").key().cloned().expect("loaded key");
        let listener = TcpListener::bind(&listen, state_dir, Arc::clone(&daemon.logins), key).await.expect("bind loopback listener");
        daemon.origin.set(listener.origin().to_owned());
        let port = listener.local_addr().expect("bound address").port();
        let token = std::fs::read_to_string(state_dir.join(TOKEN_DIRECTORY).join(TOKEN_FILE)).expect("token file");
        let serving = tokio::spawn(listener.serve(daemon.router.clone(), std::future::pending()));
        Self { daemon, port, token, serving }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.host())
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// One raw HTTP/1.1 request, so Host and Origin are exactly what the test sends.
    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", self.port)).await.expect("connect");
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("response");
        let status = response.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or_else(|| panic!("status line: {response}"));
        (status, response)
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

// Each start writes a fresh 256-bit token, owner-only, replacing the last; a token from a
// previous start authenticates nothing, and neither does a session from one.
#[tokio::test]
async fn token_file_is_private_and_replaced_at_each_start() {
    let state = tempfile::tempdir().expect("state dir");
    let first = Listening::start("127.0.0.1:0", state.path()).await;
    let path = state.path().join(TOKEN_DIRECTORY).join(TOKEN_FILE);
    assert_eq!(std::fs::metadata(&path).expect("token").permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(path.parent().expect("dir")).expect("dir").permissions().mode() & 0o777, 0o700);
    assert!(first.token.len() == 64 && first.token.bytes().all(|byte| byte.is_ascii_hexdigit()), "{}", first.token);
    let cookie = cookie_pair(&{
        let (status, response) = first.send("GET", &first.daemon.login_path().await, &[("Host", &first.host())]).await;
        assert_eq!(status, 303, "{response}");
        response.lines().find_map(|line| line.strip_prefix("set-cookie: ")).expect("Set-Cookie").to_owned()
    });
    assert_eq!(first.send("GET", "/v1/hello", &[("Host", &first.host()), ("Cookie", &cookie)]).await.0, 200);

    let second = Listening::start("127.0.0.1:0", state.path()).await;
    assert_ne!(second.token, first.token, "a new start replaces the token");
    assert_eq!(std::fs::read_to_string(&path).expect("token"), second.token);
    assert_eq!(std::fs::metadata(&path).expect("token").permissions().mode() & 0o777, 0o600);
    assert_eq!(second.send("GET", "/v1/hello", &[("Host", &second.host()), ("Authorization", &first.bearer())]).await.0, 401);
    assert_eq!(
        second.send("GET", "/v1/hello", &[("Host", &second.host()), ("Cookie", &cookie)]).await.0,
        401,
        "sessions end with their daemon"
    );
    assert_eq!(second.send("GET", "/v1/hello", &[("Host", &second.host()), ("Authorization", &second.bearer())]).await.0, 200);
}

// The token directory is narrowed to owner-only when it already exists, and a symlink in its
// place is refused rather than followed: the listener does not start.
#[tokio::test]
async fn token_directory_is_narrowed_and_never_followed() {
    let state = tempfile::tempdir().expect("state dir");
    let directory = state.path().join(TOKEN_DIRECTORY);
    std::fs::create_dir(&directory).expect("existing directory");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).expect("widen");
    let tcp = Listening::start("127.0.0.1:0", state.path()).await;
    assert_eq!(std::fs::metadata(&directory).expect("dir").permissions().mode() & 0o777, 0o700);
    drop(tcp);

    let elsewhere = tempfile::tempdir().expect("elsewhere");
    std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o755)).expect("widen");
    let linked = tempfile::tempdir().expect("state dir");
    std::os::unix::fs::symlink(elsewhere.path(), linked.path().join(TOKEN_DIRECTORY)).expect("symlink");
    let listen = ListenAddress::resolve_with("127.0.0.1:0", |_, _| async { Ok(Vec::new()) }).await.expect("listen address");
    let logins = Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock)));
    let refused = TcpListener::bind(&listen, linked.path(), logins, super::hooks::HookKey::generate().expect("key")).await;
    assert!(refused.is_err_and(|error| error.contains("symlinks are refused")));
    assert_eq!(std::fs::metadata(elsewhere.path()).expect("target").permissions().mode() & 0o777, 0o755, "the target was not touched");
}

// TCP requests need the file token. Missing, wrong, repeated or non-Bearer credentials
// are 401 and never fall back to a cookie; a bearer request needs no Origin. Only the
// login exchange is reachable without credentials.
#[tokio::test]
async fn tcp_requests_need_the_file_token() {
    let state = tempfile::tempdir().expect("state dir");
    let tcp = Listening::start("127.0.0.1:0", state.path()).await;
    let host = tcp.host();
    let host = ("Host", host.as_str());
    let bearer = tcp.bearer();
    assert_eq!(tcp.send("GET", "/v1/hello", &[host, ("Authorization", &bearer)]).await.0, 200, "bearer without Origin");
    assert_eq!(tcp.send("GET", "/v1/hello", &[host, ("Authorization", &format!("bearer {}", tcp.token))]).await.0, 200);
    let cookie = tcp.daemon.login_cookie().await;
    for authorization in
        [format!("Bearer {}", &tcp.token[1..]), format!("Bearer {}x", tcp.token), format!("Basic {}", tcp.token), tcp.token.clone()]
    {
        let (status, _) = tcp.send("GET", "/v1/hello", &[host, ("Authorization", &authorization), ("Cookie", &cookie)]).await;
        assert_eq!(status, 401, "Authorization {authorization:?}");
    }
    assert_eq!(tcp.send("GET", "/v1/hello", &[host, ("Authorization", &bearer), ("Authorization", &bearer)]).await.0, 401, "two");
    assert_eq!(tcp.send("GET", "/v1/hello", &[host]).await.0, 401, "no token");
    assert_eq!(tcp.send("GET", "/apis/flotilla.work/v1/namespaces/flotilla/convoys", &[host]).await.0, 401);
    assert_eq!(tcp.send("GET", &tcp.daemon.login_path().await, &[host]).await.0, 303, "the login exchange needs no credentials");
}

// Each accepted Host form reaches the route; another name (DNS rebinding), another port or
// no Host at all is misdirected, before authentication.
#[tokio::test]
async fn tcp_listener_accepts_only_its_own_host_forms() {
    let state = tempfile::tempdir().expect("state dir");
    for (spec, configured) in [("127.0.0.1:0", None), ("flotilla.localhost:0", Some("flotilla.localhost"))] {
        let tcp = Listening::start(spec, state.path()).await;
        let bearer = tcp.bearer();
        let port = tcp.port;
        let mut accepted = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        accepted.extend(configured.map(|name| format!("{name}:{port}")));
        for host in &accepted {
            assert_eq!(tcp.send("GET", "/v1/hello", &[("Host", host), ("Authorization", &bearer)]).await.0, 200, "{spec}: Host {host}");
        }
        for host in
            [format!("evil.example:{port}"), format!("127.0.0.1:{}", port.wrapping_add(1)), format!("[::1]:{port}"), "127.0.0.1".into()]
        {
            assert_eq!(tcp.send("GET", "/v1/hello", &[("Host", &host), ("Authorization", &bearer)]).await.0, 421, "{spec}: Host {host}");
        }
        assert_eq!(tcp.send("GET", "/v1/hello", &[("Authorization", &bearer)]).await.0, 421, "{spec}: no Host");
    }
}

const WEBSOCKET: [(&str, &str); 3] =
    [("Upgrade", "websocket"), ("Sec-WebSocket-Version", "13"), ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")];

// A present Origin must be this listener's, on a plain route and on a WebSocket upgrade.
// A cookie-authenticated request that could change state, or open a WebSocket, must carry
// it (CSRF, cross-site WebSocket hijacking); a cookie GET and any bearer request need not.
#[tokio::test]
async fn tcp_origin_rules_close_rebinding_hijacking_and_csrf() {
    let state = tempfile::tempdir().expect("state dir");
    let tcp = Listening::start("127.0.0.1:0", state.path()).await;
    let (host, origin, bearer) = (tcp.host(), tcp.origin(), tcp.bearer());
    let host = ("Host", host.as_str());
    let upgrade = |mut headers: Vec<(&'static str, String)>| {
        headers.push(("Connection", "Upgrade".into()));
        headers.extend(WEBSOCKET.map(|(name, value)| (name, value.to_owned())));
        headers
    };
    let send = |method: &'static str, path: &'static str, headers: Vec<(&'static str, String)>| {
        let tcp = &tcp;
        async move {
            let mut all = vec![host];
            all.extend(headers.iter().map(|(name, value)| (*name, value.as_str())));
            tcp.send(method, path, &all).await.0
        }
    };
    for foreign in ["http://evil.example", "http://127.0.0.1:1", "https://127.0.0.1", "null"] {
        let headers = vec![("Authorization", bearer.clone()), ("Origin", foreign.to_owned())];
        assert_eq!(send("GET", "/v1/hello", headers.clone()).await, 403, "Origin {foreign} on a plain route");
        assert_eq!(send("GET", "/v1/hello", upgrade(headers)).await, 403, "Origin {foreign} on an upgrade");
    }
    assert_eq!(send("GET", "/v1/hello", vec![("Authorization", bearer.clone()), ("Origin", origin.clone())]).await, 200, "own Origin");
    assert_ne!(send("GET", "/v1/hello", upgrade(vec![("Authorization", bearer.clone())])).await, 403, "bearer upgrade needs no Origin");

    let cookie = tcp.daemon.login_cookie().await;
    let resources = "/apis/flotilla.work/v1/namespaces/flotilla/convoys";
    assert_eq!(send("GET", resources, vec![("Cookie", cookie.clone())]).await, 200, "cookie GET");
    assert_eq!(send("POST", resources, vec![("Cookie", cookie.clone())]).await, 403, "cookie POST without Origin");
    assert_eq!(send("POST", resources, vec![("Cookie", cookie.clone()), ("Origin", origin.clone())]).await, 405, "admitted to the route");
    assert_eq!(send("GET", "/v1/hello", upgrade(vec![("Cookie", cookie.clone())])).await, 403, "cookie upgrade without Origin");
    assert_eq!(send("POST", "/logout", vec![("Cookie", cookie.clone())]).await, 403, "logout needs Origin");
    assert_eq!(send("POST", "/logout", vec![("Cookie", cookie.clone()), ("Origin", origin.clone())]).await, 204);
    assert_eq!(send("GET", "/v1/hello", vec![("Cookie", cookie)]).await, 401, "logged out");
}

#[derive(flotilla_schema::Stored)]
#[store(kind = "HttpStreamSample", code = "hs")]
struct HttpStreamSample {
    value: String,
}

// Replication cursors require a peer-issued capability on every transport. The same
// global budget bounds HTTP and routed workers, leaving capacity for unrelated SQLite work.
#[tokio::test]
async fn http_change_stream_requires_peer_session() {
    use flotilla_protocol::CommandValue;
    use flotilla_schema::{KindRegistry, NewChange, Timestamp};
    use flotilla_schema_sqlite::stream::Service;
    let (_dir, daemon) = super::super::tests::empty_daemon().await;
    let files = tempfile::tempdir().expect("files");
    let mut kinds = KindRegistry::new();
    kinds.register::<HttpStreamSample>().expect("kind");
    let service = Arc::new(Service::open(&files.path().join("home.db"), "home", kinds).expect("service"));
    service
        .with_store(|store| {
            let mut write = store.write()?;
            for index in 0..20 {
                write.insert(
                    &format!("sample-{index}"),
                    &HttpStreamSample { value: "sample".into() },
                    &NewChange::new("test", Timestamp(1)),
                )?;
            }
            write.commit()
        })
        .expect("seed more frames than the producer queues");
    daemon.install_decided_store(Arc::clone(&service)).expect("store");
    let router =
        HttpRouter::for_daemon(Arc::clone(&daemon), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
            .into_router();
    // Non-GET requests do not consult the session map, including unissued tokens.
    let mut request = get(flotilla_schema_sqlite::stream::PATH);
    *request.method_mut() = axum::http::Method::POST;
    request.extensions_mut().insert(CallerAuth::Socket(None));
    request.headers_mut().insert(super::super::change_stream::SESSION_HEADER, "unissued-session".parse().expect("token"));
    assert_eq!(router.clone().oneshot(request).await.expect("method refusal").status(), StatusCode::METHOD_NOT_ALLOWED);
    let login = Incarnation::new();
    let session = login.session_of(&login.login_cookie().await).expect("session");
    for caller in [CallerAuth::Socket(None), CallerAuth::Bearer, CallerAuth::Session(session), environment()] {
        let mut request = get(flotilla_schema_sqlite::stream::PATH);
        request.extensions_mut().insert(caller);
        request.headers_mut().insert(super::super::change_stream::SESSION_HEADER, "unissued-session".parse().expect("token"));
        assert_eq!(router.clone().oneshot(request).await.expect("refusal").status(), StatusCode::FORBIDDEN);
    }
    let mut bodies = Vec::new();
    for index in 0..16 {
        let CommandValue::ChangeStreamFrame { session, .. } =
            daemon.next_change_stream_frame(&format!("peer-{index}"), None, None).await.expect("peer-issued cursor")
        else {
            panic!("frame")
        };
        let mut request = get(flotilla_schema_sqlite::stream::PATH);
        request.extensions_mut().insert(CallerAuth::Socket(None));
        request.headers_mut().insert(super::super::change_stream::SESSION_HEADER, session.parse().expect("token"));
        let response = router.clone().oneshot(request).await.expect("peer HTTP continuation");
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response.into_body());
    }
    assert!(daemon.next_change_stream_frame("peer-17", None, None).await.expect_err("global cap").contains("too many"));
    let read = tokio::task::spawn_blocking(move || service.with_store(|store| store.read()?.last_sequence("home")));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), read)
            .await
            .expect("unrelated SQLite read progresses")
            .expect("reader task")
            .expect("read"),
        40,
        "twenty writes, each a change and its entry"
    );
    // HTTP continues after the routed Begin and ends at the same Keepalive.
    let chunks: Vec<_> = futures::StreamExt::collect(bodies.remove(0).into_data_stream()).await;
    assert_eq!(chunks.len(), 22, "20 resources, End and Keepalive");
    assert!(matches!(
        flotilla_schema_sqlite::stream::Frame::decode(chunks.last().expect("keepalive").as_ref().expect("chunk")).expect("frame"),
        flotilla_schema_sqlite::stream::Frame::Keepalive { .. }
    ));
    // Both transports use the same idle expiry. Explicit advance also works with
    // the blocked producers, which prevent automatic paused-time advancement.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(16)).await;
    let first = bodies.remove(0);
    let mut stream = first.into_data_stream();
    let failure = futures::StreamExt::next(&mut stream).await.expect("expired cursor frame").expect("encoded failure");
    assert!(matches!(flotilla_schema_sqlite::stream::Frame::decode(&failure).expect("frame"),
        flotilla_schema_sqlite::stream::Frame::Failure { message } if message == "change-stream session expired"));
    tokio::time::resume();
}

// The listen address must be loopback, and a hostname must resolve only to loopback.
#[tokio::test]
async fn listen_address_must_be_loopback() {
    let unresolved = |_: String, _: u16| async { panic!("numeric addresses are not resolved") };
    let resolving = |addresses: &'static [&'static str]| {
        move |_: String, _: u16| async move { Ok(addresses.iter().map(|address| address.parse().expect("address")).collect()) }
    };
    for spec in ["127.0.0.1:7878", "127.4.5.6:0", "[::1]:7878"] {
        let address = ListenAddress::resolve_with(spec, unresolved).await.expect(spec);
        assert_eq!(address.bind_address(), spec.parse::<SocketAddr>().expect("socket address"));
    }
    for spec in ["10.0.0.1:7878", "0.0.0.0:7878", "[::]:7878", "[2001:db8::1]:7878", "192.168.1.2:80"] {
        let error = ListenAddress::resolve_with(spec, unresolved).await.expect_err(spec);
        assert!(error.contains(spec) && error.contains("not loopback"), "{error}");
    }
    let both =
        ListenAddress::resolve_with("localhost:7878", resolving(&["127.0.0.1:7878", "[::1]:7878"])).await.expect("loopback hostname");
    assert_eq!(both.bind_address(), "127.0.0.1:7878".parse::<SocketAddr>().expect("address"));
    let any_port = ListenAddress::resolve_with("LocalHost:0", resolving(&["127.0.0.1:0"])).await.expect("port chosen at bind");
    assert_eq!(any_port.origin(7878), "http://localhost:7878", "login URLs name the configured host and the bound port");
    assert_eq!(ListenAddress::resolve_with("[::1]:0", unresolved).await.expect("IPv6 loopback").origin(7878), "http://[::1]:7878");
    let error = ListenAddress::resolve_with("rebind.example:7878", resolving(&["127.0.0.1:7878", "192.0.2.7:7878"]))
        .await
        .expect_err("a hostname with a non-loopback result");
    assert!(error.contains("rebind.example:7878") && error.contains("192.0.2.7"), "{error}");
    let error = ListenAddress::resolve_with("gone.example:7878", |_, _| async { Err(std::io::Error::other("no such host")) })
        .await
        .expect_err("unresolvable");
    assert!(error.contains("gone.example:7878"), "{error}");
    assert!(ListenAddress::resolve_with("empty.example:7878", resolving(&[])).await.is_err());
    for malformed in ["7878", "localhost", ":7878", "localhost:http", "localhost:70000", "user@localhost:7878"] {
        assert!(ListenAddress::resolve_with(malformed, resolving(&["127.0.0.1:7878"])).await.is_err(), "{malformed}");
    }
}

// Glue: rigging's default Unix caller carries kernel credentials but no flotilla
// listener identity. It must reach the protected hello route as Anonymous (401).
#[tokio::test]
async fn unknown_unix_listener_caller_is_anonymous() {
    let daemon = Incarnation::new();
    let mut request = get("/v1/hello");
    request.extensions_mut().insert(rigging_http_serve::Caller::<CallerAuth>::Unix(rigging_http_serve::PeerCredentials {
        uid: 1000,
        gid: 1000,
        pid: Some(42),
    }));
    let reply = Reply::from(daemon.router.oneshot(request).await.expect("router")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json()["message"], "authentication required");
}

fn get_accepting(uri: &str, accept: &str) -> Request<Body> {
    Request::get(uri).header(header::ACCEPT, accept).body(Body::empty()).expect("request")
}

fn rels(root: &serde_json::Value) -> Vec<&str> {
    root["links"].as_array().expect("links").iter().map(|link| link["rel"].as_str().expect("rel")).collect()
}

async fn designate_fleet(daemon: &Incarnation, fleet: &str) {
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, FLEET_DESIGNATION_NAME};
    daemon
        .backend
        .definitions::<flotilla_resources::Project>("flotilla")
        .apply(
            &InputMeta::builder().name("project-map".into()).build(),
            &flotilla_resources::ProjectSpec::builder()
                .display_name("project-map".into())
                .default_workflow_ref("w".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("charter project");
    daemon
        .backend
        .definitions::<FleetDesignation>("flotilla")
        .apply(
            &InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build(),
            &FleetDesignationSpec { fleet: Some(fleet.into()), project: "project-map".into(), image_cache: None },
        )
        .await
        .expect("designation");
}

// The root document names the build and links only what the daemon serves: the `verbs` table and
// its route (#4197), no presentations table, no commands without a command router.
#[tokio::test]
async fn root_document_links_only_what_is_served() {
    let daemon = Incarnation::new();
    for caller in [CallerAuth::Socket(None), environment(), CallerAuth::Bearer] {
        let reply = daemon.call(caller, get("/")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers[header::CONTENT_TYPE], "application/vnd.flotilla.root+json");
        let root = reply.json();
        assert_eq!(root["build_id"], flotilla_daemon_api::build_info::build_id());
        assert_eq!(root["protocol_fingerprint"], flotilla_protocol::PROTOCOL_FINGERPRINT);
        assert_eq!(rels(&root), ["tables", "verbs", "verb"]);
        assert_eq!(root["links"][0]["href"], "/tables");
        assert_eq!(root["links"][1]["href"], "/tables/verbs/rows");
        assert_eq!((&root["links"][2]["href"], &root["links"][2]["templated"]), (&"/verbs/{name}".into(), &true.into()));
    }
    assert_eq!(daemon.call(CallerAuth::Anonymous, get("/")).await.status, StatusCode::UNAUTHORIZED);
}

// A browser is sent to the fleet's link prefix; with no fleet named and no prefix, it gets JSON.
#[tokio::test]
async fn root_document_redirects_browsers_through_the_link_prefix() {
    let daemon = Incarnation::new();
    let browser = "text/html,application/xhtml+xml,*/*;q=0.8";
    let reply = daemon.call(CallerAuth::Bearer, get_accepting("/", browser)).await;
    assert_eq!(reply.status, StatusCode::OK, "no prefix: JSON");
    assert!(reply.json()["link_prefix"].is_null());

    designate_fleet(&daemon, "flotilla").await;
    let reply = daemon.call(CallerAuth::Bearer, get_accepting("/", browser)).await;
    assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(reply.headers[header::LOCATION], "https://flotilla.work/flotilla");
    let json = daemon.call(CallerAuth::Bearer, get_accepting("/", "application/json")).await;
    assert_eq!(json.status, StatusCode::OK);
    assert_eq!(json.json()["fleet"], "flotilla");
    assert_eq!(json.json()["link_prefix"], "https://flotilla.work/flotilla");

    let configured = HttpRouter::builder()
        .backend(daemon.backend.clone())
        .logins(Arc::clone(&daemon.logins))
        .link_prefix("https://pages.example/fleet/".into())
        .build()
        .into_router();
    let reply = configured.oneshot(with_caller(get_accepting("/", "text/html"), CallerAuth::Bearer)).await.expect("router");
    assert_eq!(reply.headers()[header::LOCATION], "https://pages.example/fleet");
}

fn with_caller(mut request: Request<Body>, caller: CallerAuth) -> Request<Body> {
    request.extensions_mut().insert(caller);
    request
}

// The CBOR twin is chosen by Accept.
#[tokio::test]
async fn root_document_has_a_cbor_twin() {
    let daemon = Incarnation::new();
    let reply = daemon.call(CallerAuth::Bearer, get_accepting("/", "application/vnd.flotilla.root+cbor")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CONTENT_TYPE], "application/vnd.flotilla.root+cbor");
    let root: serde_json::Value = flotilla_cbor::from_slice(&reply.body, &super::wire::LIMITS).expect("CBOR root document");
    assert_eq!(rels(&root), ["tables", "verbs", "verb"]);
    assert_eq!(root["build_id"], flotilla_daemon_api::build_info::build_id());
}

#[tokio::test]
async fn root_document_refuses_an_unacceptable_accept() {
    let daemon = Incarnation::new();
    let reply = daemon.call(CallerAuth::Bearer, get_accepting("/", "image/png")).await;
    assert_eq!(reply.status, StatusCode::NOT_ACCEPTABLE);
}
