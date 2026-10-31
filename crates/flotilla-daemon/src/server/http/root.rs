//! `GET /`: the daemon's identity and typed links to everything a client can follow (#4208).
//! Clients find a link by its relation name; a relation whose target this daemon does not
//! serve is omitted rather than left dangling.
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE;
use flotilla_resources::{FleetDesignation, FLEET_DESIGNATION_NAME};
use serde::Serialize;

use super::{wire, CallerAuth, HttpRouter, RouterState};
use crate::server::tables::TableSource;

const ROOT_JSON: &str = "application/vnd.flotilla.root+json";
const ROOT_CBOR: &str = "application/vnd.flotilla.root+cbor";
/// In tie-break order: the vendor types, then the generic ones, then the browser's.
const REPRESENTATIONS: [&str; 5] = [ROOT_JSON, ROOT_CBOR, "application/json", "application/cbor", "text/html"];
const DEFAULT_LINK_PREFIX_HOST: &str = "https://flotilla.work";

#[derive(Serialize)]
struct Link {
    rel: &'static str,
    href: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    templated: bool,
}

impl Link {
    const fn new(rel: &'static str, href: &'static str) -> Self {
        Self { rel, href, templated: false }
    }
}

#[derive(Serialize)]
struct Root {
    #[serde(skip_serializing_if = "Option::is_none")]
    fleet: Option<String>,
    build_id: String,
    protocol_fingerprint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_prefix: Option<String>,
    links: Vec<Link>,
}

/// The fleet's name, from the replicated designation; unnamed until that record exists.
async fn fleet(state: &HttpRouter) -> Option<String> {
    let designation =
        state.backend.definitions::<FleetDesignation>(DEFAULT_PROVISIONING_NAMESPACE).get(FLEET_DESIGNATION_NAME).await.ok()?;
    designation.spec.fleet.filter(|fleet| !fleet.trim().is_empty())
}

/// The configured prefix, else the default under the fleet's name; none while the fleet is unnamed.
fn link_prefix(configured: Option<&str>, fleet: Option<&str>) -> Option<String> {
    let prefix = configured.map(str::to_owned).or_else(|| fleet.map(|fleet| format!("{DEFAULT_LINK_PREFIX_HOST}/{fleet}")))?;
    Some(prefix.trim_end_matches('/').to_owned())
}

/// A table is linked only when this daemon serves it.
fn serves_table(state: &HttpRouter, name: &str) -> bool {
    TableSource::open(name.to_owned(), state.backend.clone(), super::table_api::store(state)).is_ok()
}

fn links(state: &HttpRouter) -> Vec<Link> {
    let mut links = vec![Link::new("tables", "/tables")];
    if serves_table(state, "presentations") {
        links.push(Link::new("presentations", "/tables/presentations/rows"));
    }
    if serves_table(state, "verbs") {
        links.push(Link::new("verbs", "/tables/verbs/rows"));
        links.push(Link { templated: true, ..Link::new("verb", "/verbs/{name}") });
    }
    if state.commands.is_some() {
        links.push(Link::new("commands", "/v1/commands"));
    }
    links
}

pub(super) async fn handle(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = caller.refusal_unless_authenticated() {
        return refused;
    }
    let accept = match headers.get(header::ACCEPT).map(|value| value.to_str()).transpose() {
        Ok(accept) => accept.unwrap_or(""),
        Err(_) => return wire::Encoding::Json.error_code(StatusCode::NOT_ACCEPTABLE, "unsupported_format", "invalid Accept header"),
    };
    let chosen = match wire::negotiate(accept, &REPRESENTATIONS) {
        Ok(index) => REPRESENTATIONS[index],
        Err(error) => return wire::Encoding::error_encoding(&headers).error_code(StatusCode::NOT_ACCEPTABLE, "unsupported_format", error),
    };
    let fleet = fleet(&state).await;
    let link_prefix = link_prefix(state.link_prefix.as_deref(), fleet.as_deref());
    if chosen == "text/html" {
        // flotillad does not render pages: send a browser where they are rendered, else the JSON.
        if let Some(prefix) = &link_prefix {
            return Redirect::temporary(prefix).into_response();
        }
    }
    let encoding = if matches!(chosen, ROOT_CBOR | "application/cbor") { wire::Encoding::Cbor } else { wire::Encoding::Json };
    let root = Root {
        fleet,
        build_id: flotilla_daemon_api::build_info::build_id().to_owned(),
        protocol_fingerprint: flotilla_protocol::PROTOCOL_FINGERPRINT,
        link_prefix,
        links: links(&state),
    };
    let mut response = encoding.render(StatusCode::OK, root);
    let content_type = match chosen {
        ROOT_JSON | ROOT_CBOR => chosen,
        _ => return with_vary(response),
    };
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    with_vary(response)
}

fn with_vary(mut response: Response) -> Response {
    response.headers_mut().insert(header::VARY, HeaderValue::from_static("Accept"));
    response
}
