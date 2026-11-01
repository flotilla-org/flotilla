//! `POST /verbs/{name}` (#4182 ruling 6): one invocation of a native verb, answered with an
//! outcome per subject. The caller is the router's caller identity, named on the invocation's
//! span.
use std::sync::Arc;

use axum::{extract::Path, http::StatusCode, response::Response, Extension};
use flotilla_protocol::verbs::Invocation;
use rigging_http_wire::{Encoding, Wire};

use super::auth::UserCaller;
use crate::server::verbs::{Refusal, Verbs};

/// An invocation names subjects and a few parameters; a larger body is not one.
const BODY_LIMIT: usize = 1024 * 1024;

pub(super) async fn invoke(
    Extension(verbs): Extension<Arc<Verbs>>,
    Path(name): Path<String>,
    encoding: Encoding,
    // Only callers acting as the user run verbs until policy decides per caller (#4182 ruling 6).
    // The caller is checked before `Wire` reads the body.
    UserCaller(caller): UserCaller,
    Wire(request): Wire<Invocation, BODY_LIMIT>,
) -> Response {
    match verbs.invoke(&caller.principal(), &name, request).await {
        Ok(invoked) => encoding.render(StatusCode::OK, invoked),
        Err(Refusal::UnknownVerb(name)) => encoding.error_code(StatusCode::NOT_FOUND, "not_found", format!("no verb `{name}`")),
        Err(Refusal::Invalid(message)) => encoding.error_code(StatusCode::BAD_REQUEST, "invalid_request", message),
        Err(Refusal::Conflict(message)) => encoding.error_code(StatusCode::CONFLICT, "conflict", message),
    }
}
