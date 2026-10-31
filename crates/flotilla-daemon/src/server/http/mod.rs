//! flotillad's one HTTP router (ADR 0065). It is built once and served on every listener
//! through rigging-http-serve: the main socket and environment sockets ([`unix`]) and the
//! loopback TCP listener ([`tcp`]). Each listener attaches a [`CallerAuth`] before routing,
//! so routes never inspect the transport. The router takes the daemon (or, for
//! resource-handler tests, a bare resource backend) and needs no listener: tests drive it
//! with `tower::ServiceExt::oneshot`.

mod auth;
mod commands;
#[cfg(test)]
mod hook_tests;
mod hooks;
mod login;
pub(in crate::server) mod metrics;
mod resource_api;
mod root;
mod table_api;
#[cfg(test)]
mod table_tests;
mod tcp;
#[cfg(test)]
mod tests;
mod unix;
mod wire;

use std::sync::{Arc, OnceLock};

use axum::{
    body::Body,
    extract::{RawQuery, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_store::ResourceBackend;
use futures::StreamExt;

pub(in crate::server) use auth::CallerAuth;
pub use hooks::{HookEndpoint, HookLaunch};
pub(in crate::server) use login::LoginStore;
use resource_api::Reply;
use rigging_http_serve::Caller;

pub(in crate::server) use tcp::{ListenAddress, TcpListener};
pub(in crate::server) use unix::{is_http_preface, serve_sniffed};

/// Login and hello paths; everything else falls through to the resource API.
pub(in crate::server) const HELLO_PATH: &str = "/v1/hello";
pub(in crate::server) const LOGIN_CODES_PATH: &str = "/v1/web/login-codes";
pub(in crate::server) const LOGIN_PATH: &str = "/login";
pub(in crate::server) const LOGOUT_PATH: &str = "/logout";

/// What the router serves from. Production supplies the daemon; resource-handler tests may
/// supply only a backend.
#[derive(bon::Builder)]
pub(in crate::server) struct HttpRouter {
    backend: ResourceBackend,
    #[builder(default)]
    metrics_enabled: bool,
    load_metrics: Option<Arc<metrics::LoadMetrics>>,
    #[builder(skip)]
    commands: Option<Arc<commands::Commands>>,
    daemon: Option<Arc<InProcessDaemon>>,
    /// Where hook routes record agent state; without it they answer 503.
    agent_state_store: Option<flotilla_agent::SharedAgentStateStore>,
    table_store: Option<Arc<flotilla_schema_sqlite::stream::Service>>,
    logins: Arc<LoginStore>,
    /// The TCP listener's origin, for login URLs; unset when no TCP listener is declared.
    #[builder(default)]
    tcp_origin: ListenerOrigin,
    /// Verifies the hook tokens environment callers carry; without its key they are refused.
    #[builder(default)]
    hooks: HookEndpoint,
    /// Where `GET /` sends a browser (`[web] link_prefix`); unset means the fleet default.
    link_prefix: Option<String>,
}

/// The TCP listener's origin, known only once it binds (its port may be chosen then). The
/// router is built before that, because the Unix listeners serve it from startup.
#[derive(Clone, Default)]
pub(in crate::server) struct ListenerOrigin(Arc<OnceLock<String>>);

impl ListenerOrigin {
    #[cfg(test)]
    pub(in crate::server) fn known(origin: &str) -> Self {
        let known = Self::default();
        known.set(origin.to_owned());
        known
    }

    pub(in crate::server) fn set(&self, origin: String) {
        let _ = self.0.set(origin);
    }

    pub(in crate::server) fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
}

#[derive(Clone)]
struct RouterState(Arc<HttpRouter>);

impl HttpRouter {
    /// The router over a running daemon.
    pub(in crate::server) fn for_daemon(daemon: Arc<InProcessDaemon>, logins: Arc<LoginStore>, tcp_origin: ListenerOrigin) -> Self {
        Self {
            backend: daemon.resource_backend(),
            metrics_enabled: false,
            load_metrics: None,
            agent_state_store: Some(Arc::clone(daemon.agent_state_store())),
            commands: None,
            daemon: Some(daemon),
            table_store: None,
            logins,
            tcp_origin,
            hooks: HookEndpoint::default(),
            link_prefix: None,
        }
    }

    pub(in crate::server) fn with_hooks(mut self, hooks: HookEndpoint) -> Self {
        self.hooks = hooks;
        self
    }

    pub(in crate::server) fn with_link_prefix(mut self, link_prefix: Option<String>) -> Self {
        self.link_prefix = link_prefix;
        self
    }

    pub(in crate::server) fn with_load_metrics(mut self, metrics: Arc<metrics::LoadMetrics>, enabled: bool) -> Self {
        self.load_metrics = Some(metrics);
        self.metrics_enabled = enabled;
        self
    }

    pub(in crate::server) fn with_commands(
        mut self,
        router: super::remote_commands::RemoteCommandRouter,
        shutdown: tokio::sync::mpsc::UnboundedSender<()>,
        socket_path: std::path::PathBuf,
    ) -> Self {
        self.commands = Some(Arc::new(commands::Commands::new(router, shutdown, socket_path)));
        self
    }

    pub(in crate::server) fn into_router(self) -> Router {
        Router::new()
            .route("/", get(root::handle))
            .route("/metrics", get(metrics::handle))
            .route(HELLO_PATH, get(hello))
            .route("/tables", get(table_api::list))
            .route("/tables/{table}", get(table_api::describe))
            .route("/tables/{table}/rows", get(table_api::rows))
            .route(LOGIN_CODES_PATH, post(mint_login_code))
            .route(LOGIN_PATH, get(login))
            .route(LOGOUT_PATH, post(logout))
            .route(hooks::CLAUDE_CODE_HOOK_ROUTE, post(hooks::claude_code))
            .route(hooks::CODEX_HOOK_ROUTE, post(hooks::codex))
            .route("/v1/commands", axum::routing::any(commands::handle))
            .route("/v1/commands/{id}", axum::routing::any(commands::handle))
            .route("/v1/status", axum::routing::any(commands::handle))
            .route("/v1/topology", axum::routing::any(commands::handle))
            .route("/v1/repos", axum::routing::any(commands::handle))
            .route("/v1/repos/refresh", axum::routing::any(commands::handle))
            .route("/v1/artifacts", axum::routing::any(commands::handle))
            .route("/v1/artifacts/content", axum::routing::any(commands::handle))
            .route("/v1/shutdown", axum::routing::any(commands::handle))
            .fallback(resource_api)
            .with_state(RouterState(Arc::new(self)))
            .layer(middleware::from_fn(caller_from_listener))
    }
}

/// rigging-http-serve installs `Caller<CallerAuth>`; routes read the `CallerAuth` inside.
/// Every flotilla listener supplies `Caller::Custom`. An unmapped Unix peer grants no
/// application identity; it and a request with no caller reach routes anonymously.
async fn caller_from_listener(mut request: Request, next: Next) -> Response {
    if let Some(caller) = request.extensions_mut().remove::<Caller<CallerAuth>>() {
        let caller = match caller {
            Caller::Custom(caller) => caller,
            Caller::Unix(_) => CallerAuth::Anonymous,
            Caller::Bearer => CallerAuth::Bearer,
            Caller::Cookie => CallerAuth::Anonymous,
        };
        request.extensions_mut().insert(caller);
    } else if request.extensions().get::<CallerAuth>().is_none() {
        request.extensions_mut().insert(CallerAuth::Anonymous);
    }
    next.run(request).await
}

/// The daemon's build identity and protocol fingerprint, as the framed `Hello` carries them.
async fn hello(Extension(caller): Extension<CallerAuth>) -> Response {
    if let Some(refused) = caller.refusal_unless_authenticated() {
        return refused;
    }
    Json(serde_json::json!({
        "build_id": flotilla_daemon_api::build_info::build_id(),
        "protocol_version": flotilla_protocol::PROTOCOL_VERSION,
        "protocol_fingerprint": flotilla_protocol::PROTOCOL_FINGERPRINT,
    }))
    .into_response()
}

/// Mint a one-time login URL. Only the main Unix socket may: a browser session or a bearer
/// token must not be able to mint further sessions.
async fn mint_login_code(State(RouterState(state)): State<RouterState>, Extension(caller): Extension<CallerAuth>) -> Response {
    match caller {
        CallerAuth::Socket(_) => {}
        CallerAuth::Anonymous => return auth::unauthenticated(),
        _ => return auth::refusal(StatusCode::FORBIDDEN, "login codes are minted over the main Unix socket"),
    }
    let Some(origin) = state.tcp_origin.get() else {
        return auth::refusal(StatusCode::CONFLICT, "no HTTP listener is configured");
    };
    match state.logins.mint_code() {
        Ok(code) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "url": format!("{origin}{LOGIN_PATH}?code={}", code.code),
                "expires_at": code.expires_at.to_rfc3339(),
            })),
        )
            .into_response(),
        Err(error) => auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// Exchange a login code for a session cookie, then send the browser to `/`. Socket callers
/// never hold cookies, so the exchange exists only on the TCP listener.
async fn login(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    RawQuery(query): RawQuery,
) -> Response {
    if matches!(caller, CallerAuth::Socket(_) | CallerAuth::Environment { .. } | CallerAuth::Hook { .. }) {
        return auth::refusal(StatusCode::NOT_FOUND, "browser login is served on the HTTP listener");
    }
    let code =
        url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()).find(|(key, _)| key == "code").map(|(_, code)| code.into_owned());
    let Some(code) = code else {
        return auth::refusal(StatusCode::BAD_REQUEST, "login requires a code");
    };
    match state.logins.redeem(&code) {
        Ok(Some((_, token))) => {
            let mut response = Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .header(header::CACHE_CONTROL, "no-store")
                .header(header::REFERRER_POLICY, "no-referrer")
                .body(Body::empty())
                .expect("static login response");
            match HeaderValue::from_str(&login::session_cookie(&token)) {
                Ok(cookie) => response.headers_mut().insert(header::SET_COOKIE, cookie),
                Err(_) => return auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, "invalid session cookie"),
            };
            response
        }
        Ok(None) => auth::refusal(StatusCode::UNAUTHORIZED, "login code is invalid, used or expired"),
        Err(error) => auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// End the caller's browser session and clear its cookie. Only a session can log out; the
/// TCP policy requires the listener's Origin on this cookie-authenticated POST.
async fn logout(State(RouterState(state)): State<RouterState>, Extension(caller): Extension<CallerAuth>) -> Response {
    let session = match caller {
        CallerAuth::Session(session) => session,
        CallerAuth::Anonymous => return auth::unauthenticated(),
        _ => return auth::refusal(StatusCode::BAD_REQUEST, "only a browser session can log out"),
    };
    state.logins.end(session);
    let mut response = StatusCode::NO_CONTENT.into_response();
    match HeaderValue::from_str(&login::cleared_session_cookie()) {
        Ok(cookie) => response.headers_mut().insert(header::SET_COOKIE, cookie),
        Err(_) => return auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, "invalid session cookie"),
    };
    response
}

/// A peer-issued cursor authenticates HTTP replication before the user resource routes.
async fn resource_api(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    request: Request,
) -> Response {
    if request.uri().path() == flotilla_schema_sqlite::stream::PATH {
        if request.method() != axum::http::Method::GET {
            return auth::refusal(StatusCode::METHOD_NOT_ALLOWED, "resource API is read-only");
        }
        if !matches!(caller, CallerAuth::Socket(_)) {
            return auth::refusal(StatusCode::FORBIDDEN, "change-stream requires peer replication transport");
        }
        let Some(session) = request.headers().get(super::change_stream::SESSION_HEADER).and_then(|value| value.to_str().ok()) else {
            return auth::refusal(StatusCode::FORBIDDEN, "change-stream requires peer replication transport");
        };
        let Some(daemon) = &state.daemon else {
            return auth::refusal(StatusCode::SERVICE_UNAVAILABLE, "decided store unavailable");
        };
        let peer = match daemon.change_stream_peer(session).await {
            Ok(peer) => peer,
            Err(error) => return auth::refusal(StatusCode::FORBIDDEN, &error),
        };
        return streaming(
            super::change_stream::CONTENT_TYPE,
            Body::from_stream(super::change_stream::body(Arc::clone(daemon), peer, session.to_owned())),
        );
    }
    if let Some(refused) = caller.refusal_unless_user() {
        return refused;
    }
    let target = request.uri().path_and_query().map_or_else(|| request.uri().path().to_owned(), |target| target.as_str().to_owned());
    let reply = resource_api::respond(request.method().as_str(), &target, state.backend.clone(), state.daemon.as_ref()).await;
    render(reply).await
}

async fn render(reply: Reply) -> Response {
    match reply {
        Reply::Json { status, body } => {
            (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
        }
        Reply::Watch(watch) => {
            let initial = futures::stream::iter(watch.initial.into_iter().map(Ok::<_, String>));
            let lines = initial.chain(watch.stream.map(|event| event.map_err(|error| error.to_string()))).map(|event| {
                event.and_then(|event| resource_api::watch_line(&event)).map(bytes::Bytes::from).map_err(std::io::Error::other)
            });
            streaming("application/json", Body::from_stream(lines))
        }
        Reply::Record { service, kind, name, now } => match super::change_stream::query_record(service, &kind, &name, now).await {
            Ok(bytes) => ([(header::CONTENT_TYPE, super::change_stream::CONTENT_TYPE)], bytes).into_response(),
            Err(error) => auth::refusal(StatusCode::INTERNAL_SERVER_ERROR, &error),
        },
    }
}

fn streaming(content_type: &'static str, body: Body) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Test-only: serve a sniffed main-socket connection with the router over `backend` (and the
/// daemon when given), exactly as the front door does, so resource API client tests drive
/// the router through the real Unix-socket path.
#[cfg(test)]
pub(in crate::server) async fn serve_test_connection(
    stream: tokio::net::UnixStream,
    first_byte: u8,
    backend: ResourceBackend,
    daemon: Option<Arc<InProcessDaemon>>,
) -> std::io::Result<()> {
    let logins = Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock)));
    let router = match daemon {
        Some(daemon) => HttpRouter::for_daemon(daemon, logins, ListenerOrigin::default()),
        None => HttpRouter::builder().backend(backend).logins(logins).build(),
    }
    .into_router();
    serve_sniffed(stream, first_byte, router, |peer| CallerAuth::Socket(Some(peer)), std::future::pending()).await
}
