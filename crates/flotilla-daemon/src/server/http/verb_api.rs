//! `POST /verbs/{name}` (#4182 ruling 6): one invocation of a native verb, answered with an
//! outcome per subject. The caller is resolved as the command routes resolve theirs, and named on
//! the invocation's span.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Extension,
};
use flotilla_protocol::verbs::Invocation;
use rigging_http_wire::{Encoding, Wire};

use super::{auth::UserCommandCaller, RouterState};
use crate::server::verbs::{Refusal, Verbs};

/// An invocation names subjects and a few parameters; a larger body is not one.
const BODY_LIMIT: usize = 1024 * 1024;

pub(super) async fn invoke(
    State(RouterState(state)): State<RouterState>,
    Extension(verbs): Extension<Arc<Verbs>>,
    Path(name): Path<String>,
    encoding: Encoding,
    // Only callers acting as the user run verbs until policy decides per caller (#4182 ruling 6).
    // The caller is checked and resolved before `Wire` reads the body.
    UserCommandCaller(caller): UserCommandCaller,
    Wire(request): Wire<Invocation, BODY_LIMIT>,
) -> Response {
    // `UserCommandCaller` refuses a router without a daemon.
    let Some(daemon) = state.daemon.as_ref() else {
        return encoding.error_code(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the daemon is unavailable");
    };
    let namespace = daemon.provisioning_namespace().await;
    match verbs.invoke(&caller.to_string(), &namespace, &name, request).await {
        Ok(invoked) => encoding.render(StatusCode::OK, invoked),
        Err(Refusal::UnknownVerb(name)) => encoding.error_code(StatusCode::NOT_FOUND, "not_found", format!("no verb `{name}`")),
        Err(Refusal::Invalid(message)) => encoding.error_code(StatusCode::BAD_REQUEST, "invalid_request", message),
        Err(Refusal::Conflict(message)) => encoding.error_code(StatusCode::CONFLICT, "conflict", message),
    }
}
