//! flotillad's one HTTP router (ADR 0065). It is built once and served on every listener;
//! each listener attaches a [`CallerAuth`] before routing, so routes never inspect the
//! transport. The router takes the daemon (or, for resource-handler tests, a bare resource
//! backend) and needs no listener: tests drive it with `tower::ServiceExt::oneshot`.

// Production serves only the startup address check until rigging#21 lets the listeners serve
// this router; then the main socket, environment sockets and TCP listener use it all. Remove
// this allow in the wiring change that completes #3248.
#![cfg_attr(not(test), allow(dead_code))]

mod auth;
mod login;
pub(in crate::server) mod resource_api;
mod tcp;
#[cfg(test)]
pub(in crate::server) mod test_front_end;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{RawQuery, Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_store::ResourceBackend;
use futures::StreamExt;

pub(in crate::server) use auth::CallerAuth;
pub(in crate::server) use login::LoginStore;
use resource_api::Reply;
// The TCP listener's policy is applied once rigging#21 serves the router (#3248).
#[cfg_attr(not(test), allow(unused_imports))]
pub(in crate::server) use tcp::{ListenAddress, TcpPolicy, TokenDigest};

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
    daemon: Option<Arc<InProcessDaemon>>,
    logins: Arc<LoginStore>,
    /// The TCP listener's origin, for login URLs; `None` when no TCP listener is declared.
    tcp_origin: Option<String>,
}

#[derive(Clone)]
struct RouterState(Arc<HttpRouter>);

impl HttpRouter {
    /// The router over a running daemon.
    pub(in crate::server) fn for_daemon(daemon: Arc<InProcessDaemon>, logins: Arc<LoginStore>, tcp_origin: Option<String>) -> Self {
        Self { backend: daemon.resource_backend(), daemon: Some(daemon), logins, tcp_origin }
    }

    pub(in crate::server) fn into_router(self) -> Router {
        Router::new()
            .route(HELLO_PATH, get(hello))
            .route(LOGIN_CODES_PATH, post(mint_login_code))
            .route(LOGIN_PATH, get(login))
            .route(LOGOUT_PATH, post(logout))
            .fallback(resource_api)
            .with_state(RouterState(Arc::new(self)))
    }
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
    let Some(origin) = &state.tcp_origin else {
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
    if matches!(caller, CallerAuth::Socket(_) | CallerAuth::Environment { .. }) {
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

/// The resource API, with its paths, query parameters, statuses and bodies unchanged.
async fn resource_api(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    request: Request,
) -> Response {
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
        Reply::ChangeStream { service, after, clock, once } => {
            streaming(super::change_stream::CONTENT_TYPE, Body::from_stream(super::change_stream::body(service, after, clock, once)))
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
