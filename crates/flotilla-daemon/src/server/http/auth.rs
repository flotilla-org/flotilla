//! How a listener authenticated the caller. Each listener attaches one [`CallerAuth`] to the
//! request before routing; routes read it and never inspect the transport (ADR 0065).

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use flotilla_protocol::{AgentHookTerminalRef, EnvironmentId};
use rigging_http_wire::Encoding;

use super::login::SessionId;
use crate::server::caller::PeerCredential;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum CallerAuth {
    /// The main Unix socket, a full-trust context: the caller acts as the user. The kernel
    /// supplies the peer credential when it can.
    Socket(Option<PeerCredential>),
    /// A per-environment socket: the caller acts only as that environment.
    Environment { environment: EnvironmentId, peer: Option<PeerCredential> },
    /// Loopback TCP carrying the file token; the caller is the user.
    Bearer,
    /// Loopback TCP carrying a browser session cookie; the caller is the user's browser.
    Session(SessionId),
    /// Loopback TCP carrying a hook token: an agent in this managed terminal. It reaches
    /// only the hook routes, and only as that terminal.
    Hook { terminal: AgentHookTerminalRef, generation: String },
    /// Loopback TCP without credentials. It reaches only the login exchange.
    Anonymous,
}

impl CallerAuth {
    /// The refusal for a caller that does not act as the user: an environment or an
    /// anonymous caller.
    pub(super) fn user_refusal(&self) -> Option<(StatusCode, &'static str)> {
        match self {
            Self::Socket(_) | Self::Bearer | Self::Session(_) => None,
            Self::Environment { .. } => Some((StatusCode::FORBIDDEN, "this route is not available to an environment")),
            Self::Hook { .. } => Some((StatusCode::FORBIDDEN, "a hook token reaches only the hook routes")),
            Self::Anonymous => Some((StatusCode::UNAUTHORIZED, "authentication required")),
        }
    }
    pub(super) fn refusal_unless_user(&self) -> Option<Response> {
        self.user_refusal()
            .map(|(status, message)| if status == StatusCode::UNAUTHORIZED { unauthenticated() } else { refusal(status, message) })
    }

    /// Who the caller is, as the `verb.invocation` span names it (#4182 ruling 6): the main socket's
    /// user by uid, the bearer-token holder, a browser session, an environment or a hook's
    /// terminal. Every holder of the daemon's one bearer token is the same `bearer`,
    /// until per-caller policy (ruling 6) needs finer identities.
    pub(super) fn principal(&self) -> String {
        match self {
            Self::Socket(Some(peer)) => format!("socket:uid={}", peer.uid()),
            Self::Socket(None) => "socket".into(),
            Self::Environment { environment, .. } => format!("environment:{environment}"),
            Self::Bearer => "bearer".into(),
            Self::Session(session) => format!("session:{}", session.number()),
            Self::Hook { .. } => "hook".into(),
            Self::Anonymous => "anonymous".into(),
        }
    }

    /// The refusal for an anonymous caller, or a hook token outside the hook routes.
    pub(super) fn refusal_unless_authenticated(&self) -> Option<Response> {
        match self {
            Self::Anonymous => Some(unauthenticated()),
            Self::Hook { .. } => Some(refusal(StatusCode::FORBIDDEN, "a hook token reaches only the hook routes")),
            _ => None,
        }
    }
}

/// A caller acting as the user, taken by signature. Any other caller is refused in the
/// negotiated error encoding, before a later extractor such as `Wire` reads the body.
pub(super) struct UserCaller(pub(super) CallerAuth);

impl<S: Send + Sync> FromRequestParts<S> for UserCaller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let caller = parts.extensions.get::<CallerAuth>().cloned().unwrap_or(CallerAuth::Anonymous);
        match caller.user_refusal() {
            None => Ok(Self(caller)),
            Some((status, message)) => {
                let encoding = Encoding::negotiate(&parts.headers).unwrap_or_else(|_| Encoding::for_errors(&parts.headers));
                Err(encoding.error_code(status, "unauthorized", message))
            }
        }
    }
}

pub(super) fn unauthenticated() -> Response {
    let mut response = refusal(StatusCode::UNAUTHORIZED, "authentication required");
    response.headers_mut().insert(axum::http::header::WWW_AUTHENTICATE, axum::http::HeaderValue::from_static("Bearer"));
    response
}

/// The resource API's error body shape, shared by every route.
pub(super) fn refusal(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "message": message }))).into_response()
}
