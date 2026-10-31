//! `POST /verbs/{name}` (#4182 ruling 6): one invocation of a native verb, answered with an
//! outcome per subject. The caller is the router's caller identity, named on the invocation's
//! span.
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::Response,
    Extension,
};
use flotilla_protocol::verbs::Invocation;

use super::{wire::Encoding, CallerAuth, RouterState};
use crate::server::verbs::Refusal;

pub(super) async fn invoke(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    // Only callers acting as the user run verbs until policy decides per caller (#4182 ruling 6).
    if let Some((status, message)) = caller.user_refusal() {
        return Encoding::Json.error_code(status, "unauthorized", message);
    }
    let request: Invocation = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return Encoding::Json.error_code(StatusCode::BAD_REQUEST, "invalid_request", error.to_string()),
    };
    let verbs = state.verbs.as_ref().expect("the router fills in its verbs");
    match verbs.invoke(&caller.principal(), &name, request).await {
        Ok(invoked) => Encoding::Json.render(StatusCode::OK, invoked),
        Err(Refusal::UnknownVerb(name)) => Encoding::Json.error_code(StatusCode::NOT_FOUND, "not_found", format!("no verb `{name}`")),
        Err(Refusal::Invalid(message)) => Encoding::Json.error_code(StatusCode::BAD_REQUEST, "invalid_request", message),
        Err(Refusal::Conflict(message)) => Encoding::Json.error_code(StatusCode::CONFLICT, "conflict", message),
    }
}
