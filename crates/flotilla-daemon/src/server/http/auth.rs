//! How a listener authenticated the caller. Each listener attaches one [`CallerAuth`] to the
//! request before routing; routes read it and never inspect the transport (ADR 0065).

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, Extensions, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{AgentHookTerminalRef, CommandCaller, EnvironmentId, PrincipalRef};
use rigging_http_wire::Encoding;

use super::{login::SessionId, RouterState};
use crate::server::{
    caller::{self, CallerResolution, PeerCredential},
    environment_connection,
};

/// Test double only for the kernel/procfs evidence boundary; resolution remains real. A test
/// puts it in the request's extensions in place of the peer's own.
#[cfg(test)]
pub(super) struct PeerEvidence {
    pub caller: CommandCaller,
    pub claims: HashMap<String, String>,
    pub mounts: Option<caller::PeerMounts>,
}

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

/// A caller acting as the user, resolved as [`command_caller`] resolves it. Any other caller is
/// refused in the negotiated error encoding, before a later extractor such as `Wire` reads the
/// body.
pub(super) struct UserCommandCaller(pub(super) CommandCaller);

impl FromRequestParts<RouterState> for UserCommandCaller {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &RouterState) -> Result<Self, Self::Rejection> {
        let UserCaller(caller) = UserCaller::from_request_parts(parts, state).await?;
        let encoding = Encoding::negotiate(&parts.headers).unwrap_or_else(|_| Encoding::for_errors(&parts.headers));
        let RouterState(router) = state;
        let Some(daemon) = router.daemon.as_ref() else {
            return Err(encoding.error_code(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the daemon is unavailable"));
        };
        let socket_path = router.commands.as_ref().map(|commands| commands.socket_path());
        match command_caller(daemon, socket_path, &caller, &parts.extensions).await {
            Ok(resolved) => Ok(Self(resolved.caller)),
            Err((status, message)) => Err(encoding.error_code(status, "unauthorized", message)),
        }
    }
}

/// Who an authenticated caller is to the command and verb routes: a crew when its evidence names
/// one, else the user; `Err` carries the refusal's status and message. An environment's crew is
/// transport-bound, as is a contained crew on the main socket. `extensions` are the request's;
/// tests inject peer evidence there.
pub(super) async fn command_caller(
    daemon: &InProcessDaemon,
    socket_path: Option<&Path>,
    authenticated: &CallerAuth,
    extensions: &Extensions,
) -> Result<CallerResolution, (StatusCode, String)> {
    match authenticated {
        CallerAuth::Socket(Some(peer)) => main_socket_caller(daemon, *peer, socket_path.map(Path::to_path_buf), extensions).await,
        CallerAuth::Socket(None) => Err((StatusCode::FORBIDDEN, "CallerIdentityUnavailable: socket peer credentials unavailable".into())),
        CallerAuth::Environment { environment, peer } => {
            let crews =
                environment_connection::environment_crews(daemon, environment).await.map_err(|error| (StatusCode::FORBIDDEN, error))?;
            let peer = *peer;
            let claims = tokio::task::spawn_blocking(move || caller::claims_from_peer(peer))
                .await
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            let crew = caller::check_claims(crews, &claims).map_err(|error| (StatusCode::FORBIDDEN, error))?;
            let principal_ref = PrincipalRef::implicit_for_namespace(&crew.namespace);
            Ok(CallerResolution { caller: CommandCaller { principal_ref, process: None, crew: Some(crew) }, transport_bound: true })
        }
        CallerAuth::Bearer | CallerAuth::Session(_) => {
            let principal_ref = PrincipalRef::implicit_for_namespace(&daemon.provisioning_namespace().await);
            Ok(CallerResolution { caller: CommandCaller { principal_ref, process: None, crew: None }, transport_bound: false })
        }
        CallerAuth::Hook { .. } => Err((StatusCode::FORBIDDEN, "a hook token reaches only the hook routes".into())),
        CallerAuth::Anonymous => Err((StatusCode::UNAUTHORIZED, "authentication required".into())),
    }
}

/// The caller on the main socket, from its peer's kernel and process evidence.
async fn main_socket_caller(
    daemon: &InProcessDaemon,
    peer: PeerCredential,
    socket_path: Option<PathBuf>,
    extensions: &Extensions,
) -> Result<CallerResolution, (StatusCode, String)> {
    #[cfg(test)]
    let injected_evidence = extensions.get::<Arc<PeerEvidence>>().cloned();
    #[cfg(not(test))]
    let _ = extensions;
    let namespace = daemon.provisioning_namespace().await;
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
            .map(|p| caller::peer_mounts(p.pid, socket_path.as_deref()))
            .transpose()?;
        Ok::<_, String>((caller, claims, mounts))
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
