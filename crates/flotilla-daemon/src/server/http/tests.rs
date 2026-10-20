//! The router in-process with injected caller authentication, and the TCP listener's
//! admission policy over the same router, both through `tower::ServiceExt::oneshot`.
//! No listener, socket or network is involved.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Request, StatusCode},
    Router,
};
use chrono::{DateTime, Utc};
use flotilla_protocol::EnvironmentId;
use flotilla_resources::{Clock, Convoy, ConvoySpec, InputMeta};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use hegel::generators as gs;
use tower::ServiceExt;

use super::{
    login::{LOGIN_CODE_LIFETIME, SESSION_LIFETIME, SESSION_LIMIT},
    CallerAuth, HttpRouter, ListenAddress, LoginStore, TcpPolicy, TokenDigest,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
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
    router: Router,
}

impl Incarnation {
    fn new() -> Self {
        Self::with_origin(Some(ORIGIN.into()))
    }

    fn with_origin(tcp_origin: Option<String>) -> Self {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let clock = Arc::new(ManualClock(Mutex::new(DateTime::parse_from_rfc3339("2026-10-19T12:00:00Z").expect("time").into())));
        let logins = Arc::new(LoginStore::new(clock.clone()));
        let router =
            HttpRouter::builder().backend(backend.clone()).logins(Arc::clone(&logins)).maybe_tcp_origin(tcp_origin).build().into_router();
        Self { backend, clock, logins, router }
    }

    /// The router as a listener serves it: the listener attaches the caller.
    async fn call(&self, caller: CallerAuth, request: Request<Body>) -> Reply {
        let mut request = request;
        request.extensions_mut().insert(caller);
        Reply::from(self.router.clone().oneshot(request).await.expect("router is infallible")).await
    }

    /// The TCP listener bound at 127.0.0.1:7878, configured as `spec`.
    fn tcp(&self, spec: &str, resolved: &[&str]) -> Router {
        let resolved = resolved.iter().map(|address| address.parse().expect("address")).collect::<Vec<SocketAddr>>();
        let listen = ListenAddress::resolve_with(spec, |_, _| Ok(resolved)).expect("loopback listen address");
        let bound = SocketAddr::new(listen.bind_address().ip(), 7878);
        Arc::new(TcpPolicy::new(&listen, bound, TokenDigest::new(TOKEN), Arc::clone(&self.logins))).layer(self.router.clone())
    }

    async fn login_cookie(&self) -> String {
        let minted = self.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
        let url = minted.json()["url"].as_str().expect("login url").to_owned();
        let login = self.call(CallerAuth::Anonymous, get(url.strip_prefix(ORIGIN).expect("listener origin"))).await;
        assert_eq!(login.status, StatusCode::SEE_OTHER);
        let cookie = login.headers[header::SET_COOKIE].to_str().expect("cookie");
        cookie.split(';').next().expect("cookie pair").to_owned()
    }
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

async fn tcp_call(router: &Router, request: Request<Body>) -> Reply {
    Reply::from(router.clone().oneshot(request).await.expect("router is infallible")).await
}

fn tcp_request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("request")
}

const BEARER: (&str, &str) = ("authorization", "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
const HOST: (&str, &str) = ("host", "127.0.0.1:7878");
const WEBSOCKET: [(&str, &str); 4] = [
    ("connection", "Upgrade"),
    ("upgrade", "websocket"),
    ("sec-websocket-version", "13"),
    ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

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

    let without_listener = Incarnation::with_origin(None);
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

// A Unix-socket request is judged by its socket alone: a valid session cookie neither
// elevates an environment caller nor changes what the main socket's caller may do.
#[tokio::test]
async fn session_cookie_over_a_unix_socket_is_ignored() {
    let daemon = Incarnation::new();
    let cookie = daemon.login_cookie().await;
    let with_cookie = |request: Request<Body>| {
        let mut request = request;
        request.headers_mut().insert(header::COOKIE, cookie.parse().expect("cookie"));
        request
    };
    let resources = "/apis/flotilla.work/v1/namespaces/flotilla/convoys";
    assert_eq!(daemon.call(environment(), with_cookie(get(resources))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(daemon.call(environment(), with_cookie(post("/v1/web/login-codes"))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(daemon.call(CallerAuth::Socket(None), with_cookie(post("/v1/web/login-codes"))).await.status, StatusCode::CREATED);
}

// A session ends at its lifetime, at logout, or when newer logins push it past the cap;
// logging in again from a session yields a fresh session, never a change of privilege.
#[tokio::test]
async fn sessions_expire_end_at_logout_and_are_capped() {
    let daemon = Incarnation::new();
    let tcp = daemon.tcp("127.0.0.1:7878", &[]);
    let hello = |cookie: &str| tcp_request(Method::GET, "/v1/hello", &[HOST, ("cookie", cookie)]);
    let session_of = |cookie: &str| daemon.logins.session(&HeaderMap::from_iter([(header::COOKIE, cookie.parse().expect("cookie"))]));

    let expiring = daemon.login_cookie().await;
    daemon.clock.advance(SESSION_LIFETIME - chrono::TimeDelta::seconds(1));
    assert_eq!(tcp_call(&tcp, hello(&expiring)).await.status, StatusCode::OK, "within its lifetime");
    daemon.clock.advance(chrono::TimeDelta::seconds(1));
    assert_eq!(tcp_call(&tcp, hello(&expiring)).await.status, StatusCode::UNAUTHORIZED, "past its lifetime");

    let first = daemon.login_cookie().await;
    let mut cookies = Vec::new();
    for _ in 1..SESSION_LIMIT {
        cookies.push(daemon.login_cookie().await);
    }
    assert!(session_of(&first).is_some(), "the cap holds {SESSION_LIMIT} sessions");
    cookies.push(daemon.login_cookie().await);
    assert!(session_of(&first).is_none(), "the oldest session is evicted past the cap");
    assert!(cookies.iter().all(|cookie| session_of(cookie).is_some()));

    let current = cookies.last().expect("a live session").clone();
    let session = session_of(&current).expect("session");
    let minted = daemon.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
    let login = minted.json()["url"].as_str().expect("url").strip_prefix(ORIGIN).expect("origin").to_owned();
    let relogin = tcp_call(&tcp, tcp_request(Method::GET, &login, &[HOST, ("cookie", &current)])).await;
    assert_eq!(relogin.status, StatusCode::SEE_OTHER);
    let fresh = relogin.headers[header::SET_COOKIE].to_str().expect("cookie").split(';').next().expect("pair").to_owned();
    assert_ne!(session_of(&fresh), Some(session), "a new login is a new session");
    assert_eq!(
        tcp_call(&tcp, tcp_request(Method::POST, "/v1/web/login-codes", &[HOST, ("cookie", &fresh), ("origin", ORIGIN)])).await.status,
        StatusCode::FORBIDDEN,
        "a session still cannot mint"
    );

    assert_eq!(
        tcp_call(&tcp, tcp_request(Method::POST, "/logout", &[HOST, ("cookie", &current)])).await.status,
        StatusCode::FORBIDDEN,
        "logout without Origin"
    );
    let logout = tcp_call(&tcp, tcp_request(Method::POST, "/logout", &[HOST, ("cookie", &current), ("origin", ORIGIN)])).await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert_eq!(logout.headers[header::SET_COOKIE], "flotilla_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    assert_eq!(tcp_call(&tcp, hello(&current)).await.status, StatusCode::UNAUTHORIZED, "logged out");
    assert_eq!(tcp_call(&tcp, hello(&fresh)).await.status, StatusCode::OK, "other sessions survive");
    assert_eq!(tcp_call(&tcp, tcp_request(Method::POST, "/logout", &[HOST, BEARER])).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(daemon.call(CallerAuth::Socket(None), post("/logout")).await.status, StatusCode::BAD_REQUEST);
}

// Codes and sessions live in memory: a cookie from one incarnation authenticates nothing
// in the next, as the replaced file token does not.
#[tokio::test]
async fn sessions_end_when_the_daemon_restarts() {
    let before = Incarnation::new();
    let cookie = before.login_cookie().await;
    let hello = |cookie: &str| tcp_request(Method::GET, "/v1/hello", &[HOST, ("cookie", cookie)]);
    assert_eq!(tcp_call(&before.tcp("127.0.0.1:7878", &[]), hello(&cookie)).await.status, StatusCode::OK);
    let after = Incarnation::new();
    assert_eq!(tcp_call(&after.tcp("127.0.0.1:7878", &[]), hello(&cookie)).await.status, StatusCode::UNAUTHORIZED);
}

// Each accepted Host form reaches the route; another name (DNS rebinding), another port or
// no Host at all is misdirected.
#[tokio::test]
async fn tcp_listener_accepts_only_its_own_host_forms() {
    let daemon = Incarnation::new();
    for (spec, resolved, accepted, refused) in [
        ("127.0.0.1:7878", &[][..], &["127.0.0.1:7878", "localhost:7878"][..], &["[::1]:7878", "evil.example:7878", "127.0.0.1:7879"][..]),
        ("[::1]:7878", &[], &["[::1]:7878", "localhost:7878"], &["127.0.0.1:7878", "evil.example:7878", "[::1]:80"]),
        (
            "localhost:0",
            &["127.0.0.1:0"],
            &["localhost:7878", "127.0.0.1:7878", "LOCALHOST:7878"],
            &["localhost:0", "localhost", "[::1]:7878"],
        ),
        (
            "flotilla.localhost:7878",
            &["127.0.0.1:7878"],
            &["flotilla.localhost:7878", "localhost:7878", "127.0.0.1:7878"],
            &["evil.example:7878", "flotilla.localhost:7879"],
        ),
    ] {
        let tcp = daemon.tcp(spec, resolved);
        for host in accepted {
            let reply = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[("host", host), BEARER])).await;
            assert_eq!(reply.status, StatusCode::OK, "{spec}: Host {host}");
        }
        for host in refused {
            let reply = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[("host", host), BEARER])).await;
            assert_eq!(reply.status, StatusCode::MISDIRECTED_REQUEST, "{spec}: Host {host}");
        }
        let missing = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[BEARER])).await;
        assert_eq!(missing.status, StatusCode::MISDIRECTED_REQUEST, "{spec}: no Host");
        let repeated = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, HOST, BEARER])).await;
        assert_eq!(repeated.status, StatusCode::MISDIRECTED_REQUEST, "{spec}: two Hosts");
    }
}

// Generated Host values: only an allowed authority with the bound port is admitted, and
// the Host check runs before authentication, so even a valid token cannot pass a wrong one.
#[hegel::test]
fn tcp_host_check_admits_exactly_the_allowed_authorities(tc: hegel::TestCase) {
    let name = tc.draw(gs::sampled_from(vec![
        "127.0.0.1",
        "localhost",
        "LocalHost",
        "[::1]",
        "evil.example",
        "127.0.0.2",
        "",
        "127.0.0.1.nip.io",
    ]));
    let port = tc.draw(gs::sampled_from(vec!["7878", "7879", "0", "", "07878"]));
    let separator = tc.draw(gs::sampled_from(vec![":", "", "::"]));
    let host = format!("{name}{separator}{port}");
    let admitted = ["127.0.0.1:7878", "localhost:7878"].iter().any(|allowed| allowed.eq_ignore_ascii_case(&host));
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
        std::sync::LazyLock::new(|| tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime"));
    let status = RUNTIME.block_on(async {
        let daemon = Incarnation::new();
        tcp_call(&daemon.tcp("127.0.0.1:7878", &[]), tcp_request(Method::GET, "/v1/hello", &[("host", &host), BEARER])).await.status
    });
    assert_eq!(status, if admitted { StatusCode::OK } else { StatusCode::MISDIRECTED_REQUEST }, "Host {host:?}");
}

// TCP requests need the file token; anything else in Authorization is 401 and never falls
// back to a cookie. A bearer request needs no Origin, because browsers never add the header.
#[tokio::test]
async fn tcp_listener_requires_the_file_token() {
    let daemon = Incarnation::new();
    let tcp = daemon.tcp("127.0.0.1:7878", &[]);
    let cookie = daemon.login_cookie().await;
    let hello = |headers: &[(&str, &str)]| tcp_request(Method::GET, "/v1/hello", headers);
    assert_eq!(tcp_call(&tcp, hello(&[HOST, BEARER])).await.status, StatusCode::OK, "bearer without Origin");
    assert_eq!(tcp_call(&tcp, hello(&[HOST, ("authorization", &format!("bearer {TOKEN}"))])).await.status, StatusCode::OK);
    for authorization in
        [&format!("Bearer {}", &TOKEN[1..]) as &str, &format!("Bearer {TOKEN}x"), &format!("Basic {TOKEN}"), TOKEN, "Bearer "]
    {
        let reply = tcp_call(&tcp, hello(&[HOST, ("authorization", authorization), ("cookie", &cookie)])).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "Authorization {authorization:?}");
    }
    assert_eq!(tcp_call(&tcp, hello(&[HOST, BEARER, BEARER])).await.status, StatusCode::UNAUTHORIZED, "two Authorization headers");
    assert_eq!(tcp_call(&tcp, hello(&[HOST])).await.status, StatusCode::UNAUTHORIZED, "no token");
    let resources = tcp_request(Method::GET, "/apis/flotilla.work/v1/namespaces/flotilla/convoys", &[HOST]);
    assert_eq!(tcp_call(&tcp, resources).await.status, StatusCode::UNAUTHORIZED, "no token on the resource API");
    // Without credentials the login exchange is still reachable.
    let minted = daemon.call(CallerAuth::Socket(None), post("/v1/web/login-codes")).await;
    let login = minted.json()["url"].as_str().expect("url").strip_prefix(ORIGIN).expect("origin").to_owned();
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, &login, &[HOST])).await.status, StatusCode::SEE_OTHER);
}

// A present Origin must be this listener's, on a plain route and on a WebSocket upgrade.
#[tokio::test]
async fn tcp_listener_refuses_foreign_origins() {
    let daemon = Incarnation::new();
    let tcp = daemon.tcp("127.0.0.1:7878", &[]);
    for origin in ["http://evil.example", "http://127.0.0.1:7879", "https://127.0.0.1:7878", "null", "http://127.0.0.1:7878/"] {
        let plain = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, BEARER, ("origin", origin)])).await;
        assert_eq!(plain.status, StatusCode::FORBIDDEN, "Origin {origin} on a plain route");
        let mut headers = vec![HOST, BEARER, ("origin", origin)];
        headers.extend(WEBSOCKET);
        let upgrade = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &headers)).await;
        assert_eq!(upgrade.status, StatusCode::FORBIDDEN, "Origin {origin} on a WebSocket upgrade");
    }
    let doubled = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, BEARER, ("origin", ORIGIN), ("origin", ORIGIN)])).await;
    assert_eq!(doubled.status, StatusCode::FORBIDDEN, "two Origins");
    for origin in [ORIGIN, "http://localhost:7878"] {
        let own = tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, BEARER, ("origin", origin)])).await;
        assert_eq!(own.status, StatusCode::OK, "own origin {origin}");
    }
}

// A cookie-authenticated request that could change state, or open a WebSocket, must carry
// the listener's Origin (CSRF, cross-site WebSocket hijacking). A cookie GET need not.
#[tokio::test]
async fn cookie_requests_that_change_state_need_the_listener_origin() {
    let daemon = Incarnation::new();
    let tcp = daemon.tcp("127.0.0.1:7878", &[]);
    let cookie = daemon.login_cookie().await;
    let session = ("cookie", cookie.as_str());
    let resources = "/apis/flotilla.work/v1/namespaces/flotilla/convoys";
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, resources, &[HOST, session])).await.status, StatusCode::OK, "cookie GET");
    assert_eq!(tcp_call(&tcp, tcp_request(Method::POST, resources, &[HOST, session])).await.status, StatusCode::FORBIDDEN, "cookie POST");
    let admitted = tcp_call(&tcp, tcp_request(Method::POST, resources, &[HOST, session, ("origin", ORIGIN)])).await;
    assert_eq!(admitted.status, StatusCode::METHOD_NOT_ALLOWED, "a cookie POST with Origin reaches the read-only route");
    let mut upgrade = vec![HOST, session];
    upgrade.extend(WEBSOCKET);
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &upgrade)).await.status, StatusCode::FORBIDDEN, "cookie upgrade");
    upgrade.push(("origin", ORIGIN));
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &upgrade)).await.status, StatusCode::OK, "cookie upgrade with Origin");
    let mut bearer_upgrade = vec![HOST, BEARER];
    bearer_upgrade.extend(WEBSOCKET);
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &bearer_upgrade)).await.status, StatusCode::OK, "bearer upgrade");
    // An unknown or ambiguous session cookie is no credential at all.
    let unknown = ("cookie", "flotilla_session=0000");
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, unknown])).await.status, StatusCode::UNAUTHORIZED);
    let twice = format!("{cookie}; {cookie}");
    assert_eq!(tcp_call(&tcp, tcp_request(Method::GET, "/v1/hello", &[HOST, ("cookie", &twice)])).await.status, StatusCode::UNAUTHORIZED);
}

// The listen address must be loopback, and a hostname must resolve only to loopback.
#[test]
fn listen_address_must_be_loopback() {
    let none = |_: &str, _: u16| -> std::io::Result<Vec<SocketAddr>> { panic!("numeric addresses are not resolved") };
    for spec in ["127.0.0.1:7878", "127.4.5.6:0", "[::1]:7878"] {
        let address = ListenAddress::resolve_with(spec, none).expect(spec);
        assert_eq!(address.bind_address(), spec.parse::<SocketAddr>().expect("socket address"));
    }
    for spec in ["10.0.0.1:7878", "0.0.0.0:7878", "[::]:7878", "[2001:db8::1]:7878", "192.168.1.2:80"] {
        let error = ListenAddress::resolve_with(spec, none).expect_err(spec);
        assert!(error.contains(spec) && error.contains("not loopback"), "{error}");
    }
    let resolving = |addresses: &'static [&'static str]| {
        move |_: &str, _: u16| Ok(addresses.iter().map(|address| address.parse().expect("address")).collect())
    };
    let both = ListenAddress::resolve_with("localhost:7878", resolving(&["127.0.0.1:7878", "[::1]:7878"])).expect("loopback hostname");
    assert_eq!(both.bind_address(), "127.0.0.1:7878".parse::<SocketAddr>().expect("address"));
    let any_port = ListenAddress::resolve_with("LocalHost:0", resolving(&["127.0.0.1:0"])).expect("port chosen at bind");
    assert_eq!(any_port.origin(7878), "http://localhost:7878", "login URLs name the configured host and the bound port");
    assert_eq!(ListenAddress::resolve_with("[::1]:0", none).expect("IPv6 loopback").origin(7878), "http://[::1]:7878");
    let error = ListenAddress::resolve_with("rebind.example:7878", resolving(&["127.0.0.1:7878", "192.0.2.7:7878"]))
        .expect_err("a hostname with a non-loopback result");
    assert!(error.contains("rebind.example:7878") && error.contains("192.0.2.7"), "{error}");
    let error =
        ListenAddress::resolve_with("gone.example:7878", |_, _| Err(std::io::Error::other("no such host"))).expect_err("unresolvable");
    assert!(error.contains("gone.example:7878"), "{error}");
    assert!(ListenAddress::resolve_with("empty.example:7878", resolving(&[])).is_err());
    for malformed in ["7878", "localhost", ":7878", "localhost:http", "localhost:70000", "user@localhost:7878"] {
        assert!(ListenAddress::resolve_with(malformed, resolving(&["127.0.0.1:7878"])).is_err(), "{malformed}");
    }
}
