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
use flotilla_resources::{FleetDesignation, ResourceError, FLEET_DESIGNATION_NAME};
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
    match state.backend.definitions::<FleetDesignation>(DEFAULT_PROVISIONING_NAMESPACE).get(FLEET_DESIGNATION_NAME).await {
        Ok(designation) => designation.spec.fleet.filter(|fleet| !fleet.trim().is_empty()),
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => {
            tracing::warn!(%error, "reading the FleetDesignation for the root document failed; serving it unnamed");
            None
        }
    }
}

/// The configured prefix, else the default under the fleet's name (percent-encoded as one path
/// segment); none while the fleet is unnamed or the configured prefix is empty.
fn link_prefix(configured: Option<&str>, fleet: Option<&str>) -> Option<String> {
    let prefix = match configured.map(|prefix| prefix.trim_end_matches('/')).filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => prefix.to_owned(),
        None => {
            let mut url = url::Url::parse(DEFAULT_LINK_PREFIX_HOST).expect("the default host is a URL");
            url.path_segments_mut().expect("an https URL has path segments").push(fleet?);
            url.into()
        }
    };
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
            return with_vary(Redirect::temporary(prefix).into_response());
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
    if matches!(chosen, ROOT_JSON | ROOT_CBOR) {
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(chosen));
    }
    with_vary(response)
}

fn with_vary(mut response: Response) -> Response {
    response.headers_mut().insert(header::VARY, HeaderValue::from_static("Accept"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_prefix_defaults_under_the_encoded_fleet_and_ignores_an_empty_configuration() {
        assert_eq!(link_prefix(None, Some("flotilla")).as_deref(), Some("https://flotilla.work/flotilla"));
        assert_eq!(link_prefix(None, Some("a b/c?d#e")).as_deref(), Some("https://flotilla.work/a%20b%2Fc%3Fd%23e"));
        assert_eq!(link_prefix(None, None), None);
        for empty in ["", "/", "//"] {
            assert_eq!(link_prefix(Some(empty), Some("f")).as_deref(), Some("https://flotilla.work/f"));
        }
        assert_eq!(link_prefix(Some("https://x.example/p/"), None).as_deref(), Some("https://x.example/p"));
    }
}
