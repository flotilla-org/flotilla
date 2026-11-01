//! How a listener authenticated the caller. Each listener attaches one [`CallerAuth`] to the
//! request before routing; routes read it and never inspect the transport (ADR 0065).

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, Extensions, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{AgentHookTerminalRef, CommandCaller, EnvironmentId};
use rigging_http_wire::Encoding;

use super::login::SessionId;
use crate::server::caller::{self, CallerResolution, PeerCredential};

/// Test double only for the kernel/procfs evidence boundary; resolution remains real. A test
/// puts it in the request's extensions in place of the peer's own.
#[cfg(test)]
pub(super) struct PeerEvidence {
    pub caller: CommandCaller,
    pub claims: HashMap<String, String>,
    pub mounts: Option<caller::PeerMounts>,
}

/// Outside tests no request carries injected evidence: the type has no values.
#[cfg(not(test))]
pub(super) enum PeerEvidence {}

/// The peer evidence a test injected into a request, by a lookup that clones nothing else.
pub(super) fn injected_evidence(extensions: &Extensions) -> Option<Arc<PeerEvidence>> {
    extensions.get::<Arc<PeerEvidence>>().cloned()
}

impl PeerEvidence {
    #[cfg(test)]
    fn evidence(&self) -> Evidence {
        let mounts = self
            .mounts
            .as_ref()
            .map(|m| caller::PeerMounts { mountinfo: m.mountinfo.clone(), legacy_main_directory: m.legacy_main_directory });
        (self.caller.clone(), self.claims.clone(), mounts)
    }

    #[cfg(not(test))]
    fn evidence(&self) -> Evidence {
        match *self {}
    }
}

/// A socket peer's resolved process, its environment claims and, when contained, its mounts.
type Evidence = (CommandCaller, HashMap<String, String>, Option<caller::PeerMounts>);

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

/// The caller on the main socket, from its peer's kernel and process evidence: a crew when the
/// evidence names one, else the user. `Err` carries the refusal's status and message.
pub(super) async fn main_socket_caller(
    daemon: &InProcessDaemon,
    peer: PeerCredential,
    socket_path: Option<PathBuf>,
    injected: Option<Arc<PeerEvidence>>,
) -> Result<CallerResolution, (StatusCode, String)> {
    let namespace = daemon.provisioning_namespace().await;
    let evidence = tokio::task::spawn_blocking(move || {
        if let Some(injected) = injected {
            return Ok(injected.evidence());
        }
        let claims = caller::claims_from_peer(Some(peer));
        let caller = caller::caller_from_peer(Some(peer), &namespace)?;
        let mounts = caller
            .process
            .as_ref()
            .filter(|p| p.container.is_some())
            .map(|p| caller::peer_mounts(p.pid, socket_path.as_deref()))
            .transpose()?;
        Ok::<Evidence, String>((caller, claims, mounts))
    })
    .await;
    match evidence {
        Ok(Ok((caller, claims, mounts))) => {
            caller::resolve_main_caller(daemon, caller, &claims, mounts.as_ref()).await.map_err(|error| (StatusCode::FORBIDDEN, error))
        }
        Ok(Err(error)) => Err((StatusCode::FORBIDDEN, error)),
        Err(error) => Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string())),
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
