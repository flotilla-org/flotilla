//! How a listener authenticated the caller. Each listener attaches one [`CallerAuth`] to the
//! request before routing; routes read it and never inspect the transport (ADR 0065).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use flotilla_protocol::{AgentHookTerminalRef, EnvironmentId};

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

    /// The refusal for an anonymous caller, or a hook token outside the hook routes.
    pub(super) fn refusal_unless_authenticated(&self) -> Option<Response> {
        match self {
            Self::Anonymous => Some(unauthenticated()),
            Self::Hook { .. } => Some(refusal(StatusCode::FORBIDDEN, "a hook token reaches only the hook routes")),
            _ => None,
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
