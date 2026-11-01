//! `POST /verbs/{name}` (#4182 ruling 6): one invocation of a native verb, answered with an
//! outcome per subject. The caller is resolved as the command routes resolve it, and named on the
//! invocation's span.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Extension,
};
use flotilla_protocol::{verbs::Invocation, CommandCaller, PrincipalRef};
use rigging_http_wire::{Encoding, Wire};

use super::{
    auth::{main_socket_caller, PeerEvidence, UserCaller},
    CallerAuth, RouterState,
};
use crate::server::verbs::{Refusal, Verbs};

/// An invocation names subjects and a few parameters; a larger body is not one.
const BODY_LIMIT: usize = 1024 * 1024;

pub(super) async fn invoke(
    State(RouterState(state)): State<RouterState>,
    Extension(verbs): Extension<Arc<Verbs>>,
    evidence: Option<Extension<Arc<PeerEvidence>>>,
    Path(name): Path<String>,
    encoding: Encoding,
    // Only callers acting as the user run verbs until policy decides per caller (#4182 ruling 6).
    // The caller is checked before `Wire` reads the body.
    UserCaller(caller): UserCaller,
    Wire(request): Wire<Invocation, BODY_LIMIT>,
) -> Response {
    let Some(daemon) = state.daemon.as_ref() else {
        return encoding.error_code(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the daemon is unavailable");
    };
    let caller = match caller {
        CallerAuth::Socket(Some(peer)) => {
            let socket_path = state.commands.as_ref().map(|commands| commands.socket_path().to_owned());
            match main_socket_caller(daemon, peer, socket_path, evidence.map(|Extension(evidence)| evidence)).await {
                Ok(resolved) => resolved.caller,
                Err((status, message)) => return encoding.error_code(status, "unauthorized", message),
            }
        }
        CallerAuth::Socket(None) => {
            let message = "CallerIdentityUnavailable: socket peer credentials unavailable";
            return encoding.error_code(StatusCode::FORBIDDEN, "unauthorized", message);
        }
        // `UserCaller` let through only the main socket, the bearer token and a browser session.
        // The bearer-token holder and a browser session are both the user, as on the command
        // routes, so the span does not tell them apart; per-caller policy (#4182 ruling 6) would
        // bring that distinction back.
        _ => CommandCaller {
            principal_ref: PrincipalRef::implicit_for_namespace(&daemon.provisioning_namespace().await),
            process: None,
            crew: None,
        },
    };
    match verbs.invoke(&caller.to_string(), &name, request).await {
        Ok(invoked) => encoding.render(StatusCode::OK, invoked),
        Err(Refusal::UnknownVerb(name)) => encoding.error_code(StatusCode::NOT_FOUND, "not_found", format!("no verb `{name}`")),
        Err(Refusal::Invalid(message)) => encoding.error_code(StatusCode::BAD_REQUEST, "invalid_request", message),
        Err(Refusal::Conflict(message)) => encoding.error_code(StatusCode::CONFLICT, "conflict", message),
    }
}
