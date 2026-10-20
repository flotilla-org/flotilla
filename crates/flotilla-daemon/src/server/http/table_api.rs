//! HTTP adapter for the carrier-neutral table source.
use super::{wire, CallerAuth, RouterState};
#[cfg(test)]
use crate::server::tables;
use crate::server::tables::{Failure, Query, TableSource};
use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use futures::StreamExt;

#[derive(Clone, Copy)]
enum Format {
    Json(&'static str),
    Cbor,
    Markdown,
    Mermaid,
    Observed,
}
fn format(headers: &HeaderMap, params: &[(String, String)], rows: bool) -> Result<Format, Failure> {
    if let Some((_, value)) = params.iter().find(|(key, _)| key == "format") {
        return match value.as_str() {
            "json" | "jsonl" => Ok(Format::Json(if rows { "application/x-ndjson" } else { "application/json" })),
            "cbor" => Ok(Format::Cbor),
            "markdown" if !rows => Ok(Format::Markdown),
            "mermaid" if !rows => Ok(Format::Mermaid),
            "observed" if !rows => Ok(Format::Observed),
            _ => Err(Failure::new("unsupported_format", value)),
        };
    }
    let accept = headers
        .get(header::ACCEPT)
        .map(|header| header.to_str().map_err(|_| Failure::new("unsupported_format", "invalid Accept header")))
        .transpose()?
        .unwrap_or("");
    let allowed = if rows {
        vec!["application/x-ndjson", "application/json", "application/jsonl", "application/cbor-seq"]
    } else {
        vec!["application/json", "application/cbor", "text/markdown", "text/vnd.mermaid"]
    };
    let index = wire::negotiate(accept, &allowed).map_err(|error| Failure::new("unsupported_format", error))?;
    Ok(if rows {
        if index == 3 {
            Format::Cbor
        } else {
            Format::Json(allowed[index])
        }
    } else {
        [Format::Json("application/json"), Format::Cbor, Format::Markdown, Format::Mermaid][index]
    })
}
fn failure(error: Failure, encoding: wire::Encoding) -> Response {
    let status = match error.code.as_str() {
        "not_found" | "no_machine" => StatusCode::NOT_FOUND,
        "unauthorized" => StatusCode::FORBIDDEN,
        "unsupported_format" => StatusCode::NOT_ACCEPTABLE,
        "table_source" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    encoding.error_code(status, &error.code, &error.message)
}
fn params(query: Option<String>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_deref().unwrap_or("").as_bytes()).into_owned().collect()
}
fn store(state: &super::HttpRouter) -> Option<std::sync::Arc<flotilla_schema_sqlite::stream::Service>> {
    state.table_store.clone().or_else(|| state.daemon.as_ref().and_then(|daemon| daemon.decided_store().ok()))
}
pub(super) async fn list(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    headers: HeaderMap,
) -> Response {
    let encoding = match wire::Encoding::response(&headers) {
        Ok(encoding) => encoding,
        Err(error) => return wire::Encoding::error_encoding(&headers).error_code(StatusCode::NOT_ACCEPTABLE, "unsupported_format", error),
    };
    if let Some((status, message)) = caller.user_refusal() {
        return encoding.error_code(status, "unauthorized", message);
    }
    match TableSource::list(store(&state)).await {
        Ok(value) => encoding.render(StatusCode::OK, value),
        Err(error) => failure(error, encoding),
    }
}
pub(super) async fn describe(
    state: State<RouterState>,
    caller: Extension<CallerAuth>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(state, caller, name, params(query), headers, false).await
}
pub(super) async fn rows(
    state: State<RouterState>,
    caller: Extension<CallerAuth>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    respond(state, caller, name, params(query), headers, true).await
}
async fn respond(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    name: String,
    params: Vec<(String, String)>,
    headers: HeaderMap,
    rows: bool,
) -> Response {
    let format = match format(&headers, &params, rows) {
        Ok(format) => format,
        Err(error) => return failure(error, wire::Encoding::error_encoding(&headers)),
    };
    let encoding = if matches!(format, Format::Cbor) { wire::Encoding::Cbor } else { wire::Encoding::Json };
    if let Some((status, message)) = caller.user_refusal() {
        return encoding.error_code(status, "unauthorized", message);
    }
    let source = match TableSource::open(name, state.backend.clone(), store(&state)) {
        Ok(source) => source,
        Err(error) => return failure(error, encoding),
    };
    let description = match source.describe().await {
        Ok(description) => description,
        Err(error) => return failure(error, encoding),
    };
    if !rows {
        if matches!(format, Format::Markdown) {
            return ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], description.markdown)
                .into_response();
        }
        if matches!(format, Format::Mermaid | Format::Observed) {
            let get = |name: &str| params.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            return match source.diagram(matches!(format, Format::Observed), get("namespace").unwrap_or("default".into()), get("name")).await
            {
                Ok(diagram) => ([(header::CONTENT_TYPE, "text/vnd.mermaid; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], diagram)
                    .into_response(),
                Err(error) => failure(error, encoding),
            };
        }
        return encoding.render(StatusCode::OK, description.schema);
    }
    let query = match Query::parse(&params, &description.table) {
        Ok(query) => query,
        Err(error) => return failure(error, encoding),
    };
    let messages = match source.messages(description, query).await {
        Ok(messages) => messages,
        Err(error) => return failure(error, encoding),
    };
    let cbor = matches!(format, Format::Cbor);
    let stream = messages.map(move |message| {
        if cbor {
            flotilla_cbor::to_vec(&message, &wire::LIMITS).map_err(std::io::Error::other)
        } else {
            let mut value = serde_json::to_value(message).map_err(std::io::Error::other)?;
            wire::large_integers(&mut value);
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            Ok(bytes)
        }
    });
    (
        [
            (
                header::CONTENT_TYPE,
                match format {
                    Format::Json(media) => media,
                    _ => "application/cbor-seq",
                },
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
#[cfg(test)]
pub(super) fn test_ordering() {
    tables::test_ordering()
}
