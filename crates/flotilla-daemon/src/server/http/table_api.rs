//! The resource-backed generic tier of ADR 0067. Filtering always uses the store's
//! declared query port; sorting, projection and portable paging are carrier concerns.
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use flotilla_address::{Operator, Template};
use flotilla_protocol::table::{Message, Row};
use flotilla_resources::WatchStart;
use flotilla_schema::{ColumnType, Table, TableBuilder};
use flotilla_store::{query_resource_kind, watch_resource_kind_from, ResourceBackend, StoreQuery, REGISTERED_RESOURCE_KINDS};
use futures::{stream::BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

pub(super) const MESSAGE_LIMITS: flotilla_cbor::Limits =
    flotilla_cbor::Limits { max_message: 16 * 1024 * 1024, max_depth: 64, max_elements: 1 << 20, max_byte_string: 16 * 1024 * 1024 };

/// Bound batches by approximate JSON size; a single large resource stays one message.
pub(super) fn batches(rows: Vec<Row>, sequence: u64, partial: bool, cursor: Option<String>) -> Result<Vec<Message>, String> {
    let mut messages = vec![];
    let mut batch = vec![];
    let mut size = 0;
    for row in rows {
        let bytes = serde_json::to_vec(&row).map_err(|error| error.to_string())?.len();
        if size + bytes > 512 * 1024 && !batch.is_empty() {
            messages.push(Message::Rows { rows: std::mem::take(&mut batch), sequence, partial, cursor: None });
            size = 0;
        }
        batch.push(row);
        size += bytes;
    }
    messages.push(Message::Rows { rows: batch, sequence, partial, cursor });
    Ok(messages)
}

const LIMIT: usize = 200;
const MAX_LIMIT: usize = 10_000;
const KEY: &str = "metadata.name";

pub(super) fn negotiate(format: Option<&str>, accept: &str, rows: bool) -> Result<&'static str, Failure> {
    let allowed = |format: &str| match (format, rows) {
        ("json" | "jsonl", _) => Some("json"),
        ("cbor", true) => Some("cbor"),
        ("markdown", false) => Some("markdown"),
        ("mermaid", false) => Some("mermaid"),
        ("observed", false) => Some("observed"),
        _ => None,
    };
    if let Some(format) = format {
        return allowed(format).ok_or_else(|| Failure::new(StatusCode::NOT_ACCEPTABLE, "unsupported_format", format));
    }
    if accept.trim().is_empty() {
        return Ok("json");
    }
    let mut choices = vec![];
    for (index, item) in accept.split(',').enumerate() {
        let mut parts = item.trim().split(';');
        let media = parts.next().unwrap_or("").trim();
        let quality = parts.find_map(|part| part.trim().strip_prefix("q=")).map_or(1.0, |q| q.parse::<f32>().unwrap_or(0.0));
        if !quality.is_finite() || quality <= 0.0 || quality > 1.0 {
            continue;
        }
        let format = match media {
            "*/*" | "application/*" | "application/json" => Some("json"),
            "application/x-ndjson" | "application/jsonl" if rows => Some("json"),
            "application/cbor-seq" if rows => Some("cbor"),
            "text/markdown" if !rows => Some("markdown"),
            "text/vnd.mermaid" if !rows => Some("mermaid"),
            _ => None,
        };
        if let Some(format) = format {
            choices.push((quality, index, format));
        }
    }
    choices.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    choices.first().map(|(_, _, format)| *format).ok_or_else(|| Failure::new(StatusCode::NOT_ACCEPTABLE, "unsupported_format", accept))
}

pub(super) fn error(status: StatusCode, code: &str, message: impl ToString) -> Response {
    (status, Json(json!({"code":code,"message":message.to_string()}))).into_response()
}
#[derive(Debug)]
pub(super) struct Failure {
    status: StatusCode,
    code: String,
    message: String,
}
impl Failure {
    fn new(status: StatusCode, code: &str, message: impl ToString) -> Self {
        Self { status, code: code.into(), message: message.to_string() }
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        error(self.status, &self.code, self.message)
    }
}
pub(super) fn bad(code: &str, message: impl ToString) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, code, message)
}

fn descriptor(kind: &str) -> Table {
    let mut builder = TableBuilder::new(kind).key(KEY, ColumnType::Text)
        .column("metadata.namespace", ColumnType::Text, false)
        .column("metadata.resourceVersion", ColumnType::Text, false)
        .column("metadata.creationTimestamp", ColumnType::Timestamp, false)
        .column("metadata.deletionTimestamp", ColumnType::Timestamp, true)
        .column("metadata.labels", ColumnType::Cbor, false)
        .column("metadata.annotations", ColumnType::Cbor, false)
        .column("status.phase", ColumnType::Text, true)
        .column("status.conditions", ColumnType::Cbor, true)
        .column("spec", ColumnType::Cbor, false).column("status", ColumnType::Cbor, true)
        .property("metadata.labels","additionalProperties",json!({"type":"string","whereOperators":["=="],"sortable":false}))
        .property("status.phase","roles",json!(["state"]))
        .property("status.conditions", "type", json!("array"))
        .property("status.conditions", "items", json!({"type":"object","properties":{"type":{"type":"string"},"status":{"type":"string"},"reason":{"type":"string"},"message":{"type":"string"},"lastTransitionTime":{"type":"string","format":"date-time"}}}));
    let declared = flotilla_store::declared_resource_kinds().into_iter().find(|entry| entry.paths.plural == kind);
    for field in
        [KEY, "metadata.namespace", "metadata.resourceVersion", "metadata.creationTimestamp", "metadata.deletionTimestamp", "status.phase"]
    {
        let operators: Vec<&str> = if declared.is_some_and(|entry| entry.queryable_fields.contains(&field)) { vec!["=="] } else { vec![] };
        builder = builder.property(field, "sortable", json!(true)).property(field, "whereOperators", json!(operators));
    }
    let mut table = builder.build();
    for column in table.key.iter().chain(&table.columns) {
        let props = table.properties.entry(column.name.clone()).or_default();
        props.entry("sortable").or_insert(json!(false));
        props.entry("whereOperators").or_insert(json!([]));
    }
    for (field, title) in [
        (KEY, "Name"),
        ("metadata.namespace", "Namespace"),
        ("metadata.creationTimestamp", "Created"),
        ("metadata.deletionTimestamp", "Deleted"),
        ("status.phase", "Phase"),
        ("status.conditions", "Conditions"),
        ("spec", "Spec"),
        ("status", "Status"),
    ] {
        table.properties.entry(field.into()).or_default().insert("title".into(), json!(title));
    }
    table
}

#[derive(Clone)]
pub(super) struct Query {
    pub(super) namespace: String,
    pub(super) filter: StoreQuery,
    pub(super) sort: Vec<(String, bool)>,
    pub(super) columns: Vec<String>,
    pub(super) limit: usize,
    pub(super) offset: usize,
    pub(super) cursor: Option<String>,
    pub(super) count: Option<String>,
    pub(super) watch: bool,
    pub(super) keys: Vec<String>,
    sort_types: Vec<(ColumnType, bool)>,
}
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    version: u8,
    table: String,
    namespace: String,
    sort: Vec<(String, bool)>,
    filter: Value,
    values: Vec<Value>,
}
impl Query {
    pub(super) fn parse(params: &[(String, String)], table: &Table) -> Result<Self, Failure> {
        let get = |name: &str| params.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
        let fields: BTreeSet<_> = table.key.iter().chain(&table.columns).map(|column| column.name.as_str()).collect();
        let mut filter = StoreQuery::default();
        for (_, expression) in params.iter().filter(|(name, _)| name == "where") {
            let clause: Template = format!("$row {expression}").parse().map_err(|err| bad("invalid_filter", err))?;
            let path = clause.path.segments().join(".");
            let label = clause.path.segments().strip_prefix(&["metadata".into(), "labels".into()]);
            let label = label.filter(|parts| parts.len() == 1).map(|parts| parts[0].clone());
            if !fields.contains(path.as_str()) && label.is_none() {
                return Err(bad("unknown_column", &path));
            }
            if let Some(column) = table.key.iter().chain(&table.columns).find(|column| column.name == path) {
                let valid = match &column.ty {
                    ColumnType::Integer => clause.literal.parse::<i64>().is_ok(),
                    ColumnType::Bool => matches!(clause.literal.as_str(), "true" | "false"),
                    ColumnType::Timestamp => chrono::DateTime::parse_from_rfc3339(&clause.literal).is_ok(),
                    ColumnType::Code(codes) => codes.iter().any(|code| code.as_str() == clause.literal),
                    ColumnType::Cbor | ColumnType::Frozen | ColumnType::Blob => false,
                    _ => true,
                };
                if !valid {
                    return Err(bad("type_mismatch", format!("incompatible literal for {path}")));
                }
            }
            if clause.operator != Operator::Equal {
                return Err(bad("unsupported_filter", "store only pushes down equality for resource fields"));
            }
            if let Some(label) = label {
                if filter.labels.get(&label).is_some_and(|value| value != &clause.literal) {
                    filter.field_values.insert(KEY.into(), vec![]);
                }
                filter.labels.insert(label, clause.literal);
            } else {
                let operators = table.properties.get(&path).and_then(|props| props.get("whereOperators")).and_then(Value::as_array);
                if !operators.is_some_and(|ops| ops.contains(&json!("=="))) {
                    return Err(bad("unsupported_filter", &path));
                }
                if filter.fields.get(&path).is_some_and(|value| value != &clause.literal) {
                    filter.field_values.insert(KEY.into(), vec![]);
                }
                filter.fields.insert(path, clause.literal);
            }
        }
        let mut sort = Vec::new();
        let default_sort = table.key.iter().map(|column| column.name.as_str()).collect::<Vec<_>>().join(",");
        for field in get("sort").unwrap_or(&default_sort).split(',') {
            let (descending, field) = field.strip_prefix('-').map_or((false, field), |field| (true, field));
            if !fields.contains(field) {
                return Err(bad("unknown_column", field));
            }
            if !table.properties.get(field).and_then(|props| props.get("sortable")).and_then(Value::as_bool).unwrap_or(false) {
                return Err(bad("unsupported_sort", field));
            }
            if sort.iter().any(|(name, _)| name == field) {
                return Err(bad("invalid_sort", "duplicate sort field"));
            }
            sort.push((field.into(), descending));
        }
        for key in &table.key {
            if !sort.iter().any(|(name, _)| name == &key.name) {
                sort.push((key.name.clone(), false));
            }
        }
        let mut columns: Vec<String> = get("columns")
            .map(|columns| columns.split(',').map(str::to_owned).collect())
            .unwrap_or_else(|| table.key.iter().chain(&table.columns).map(|column| column.name.clone()).collect());
        if columns.iter().any(|name| !fields.contains(name.as_str())) {
            return Err(bad("unknown_column", "unknown projected column"));
        }
        for key in &table.key {
            if !columns.contains(&key.name) {
                columns.push(key.name.clone());
            }
        }
        columns.sort();
        columns.dedup();
        let number = |name, default| -> Result<usize, Failure> {
            get(name).map_or(Ok(default), |value| value.parse().map_err(|_| bad("invalid_paging", name)))
        };
        let limit = number("limit", LIMIT)?;
        if limit == 0 || limit > MAX_LIMIT {
            return Err(bad("invalid_paging", "limit must be between 1 and 10000"));
        }
        let count = get("count").map(str::to_owned);
        if count.as_ref().is_some_and(|count| count != "exact" && count != "estimate") {
            return Err(bad("invalid_count", "expected exact or estimate"));
        }
        let sort_types = sort
            .iter()
            .map(|(name, _)| {
                let column = table.key.iter().chain(&table.columns).find(|column| &column.name == name).expect("validated sort field");
                (column.ty.clone(), column.nullable)
            })
            .collect();
        Ok(Self {
            sort_types,
            keys: table.key.iter().map(|column| column.name.clone()).collect(),
            namespace: get("namespace").unwrap_or("default").into(),
            filter,
            sort,
            columns,
            limit,
            offset: number("offset", 0)?,
            cursor: get("cursor").map(str::to_owned),
            count,
            watch: get("watch").is_some_and(|value| value.is_empty() || value == "true"),
        })
    }
    pub(super) fn values(&self, row: &Row) -> Vec<Value> {
        self.sort.iter().map(|(name, _)| row.get(name).cloned().unwrap_or(Value::Null)).collect()
    }
    pub(super) fn compare(&self, a: &[Value], b: &[Value]) -> Ordering {
        for ((a, b), (_, descending)) in a.iter().zip(b).zip(&self.sort) {
            let order = compare(a, b);
            let order = if *descending { order.reverse() } else { order };
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    }
    pub(super) fn project(&self, row: &Row) -> Row {
        self.columns.iter().map(|name| (name.clone(), row.get(name).cloned().unwrap_or(Value::Null))).collect()
    }
    pub(super) fn cursor(&self, table: &str, row: &Row) -> Result<String, String> {
        let cursor = Cursor {
            version: 1,
            table: table.into(),
            namespace: self.namespace.clone(),
            sort: self.sort.clone(),
            filter: json!(self.filter),
            values: self.values(row),
        };
        flotilla_cbor::to_vec(&cursor, &Default::default()).map(|bytes| URL_SAFE_NO_PAD.encode(bytes)).map_err(|error| error.to_string())
    }
    pub(super) fn after(&self, table: &str) -> Result<Option<Vec<Value>>, String> {
        let Some(cursor) = &self.cursor else { return Ok(None) };
        let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| "invalid cursor")?;
        let cursor: Cursor = flotilla_cbor::from_slice(&bytes, &Default::default()).map_err(|_| "invalid cursor")?;
        if cursor.version != 1
            || cursor.table != table
            || cursor.namespace != self.namespace
            || cursor.sort != self.sort
            || cursor.filter != json!(self.filter)
            || cursor.values.len() != self.sort.len()
        {
            return Err("cursor does not describe this query".into());
        }
        for (value, (ty, nullable)) in cursor.values.iter().zip(&self.sort_types) {
            let valid = match (value, ty) {
                (Value::Null, _) => *nullable,
                (Value::String(value), ColumnType::Timestamp) => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
                (Value::String(value), ColumnType::Code(codes)) => codes.iter().any(|code| code.as_str() == value),
                (Value::String(_), ColumnType::Text | ColumnType::Ref | ColumnType::Interned(_)) => true,
                (Value::Number(value), ColumnType::Integer) => value.as_i64().is_some(),
                (Value::Bool(_), ColumnType::Bool) => true,
                _ => false,
            };
            if !valid {
                return Err("cursor sort values do not match the description".into());
            }
        }
        Ok(Some(cursor.values))
    }
}
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => a.as_i64().cmp(&b.as_i64()),
        _ => a.to_string().cmp(&b.to_string()),
    }
}
fn row(value: &Value, table: &Table) -> Row {
    table
        .key
        .iter()
        .chain(&table.columns)
        .map(|column| {
            let pointer = format!("/{}", column.name.replace('.', "/"));
            (column.name.clone(), value.pointer(&pointer).cloned().unwrap_or(Value::Null))
        })
        .collect()
}
fn snapshot_sequence(document: &Value) -> Result<u64, String> {
    document
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
        .ok_or("missing snapshot position")?
        .parse()
        .map_err(|_| "non-numeric snapshot position".into())
}
pub(super) fn encode(message: &Message, cbor: bool) -> Result<Vec<u8>, std::io::Error> {
    if cbor {
        flotilla_cbor::to_vec(message, &MESSAGE_LIMITS).map_err(std::io::Error::other)
    } else {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub(super) async fn respond(
    target: &str,
    accept: &str,
    backend: ResourceBackend,
    decided: Option<std::sync::Arc<flotilla_schema_sqlite::stream::Service>>,
) -> Response {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts == ["tables"] {
        let mut kinds: Vec<Value> =
            REGISTERED_RESOURCE_KINDS.iter().map(|kind| json!({"kind":kind.kind,"root":kind.plural,"tables":[kind.plural]})).collect();
        if let Some(service) = &decided {
            if let Ok(derived) = service.with_store(|store| Ok(store.kinds().iter().map(|set| json!({"kind":set.kind,"root":set.tables[0].name,"tables":set.tables.iter().map(|table| table.name.clone()).chain(std::iter::once(set.log_table().name)).collect::<Vec<_>>()})).collect::<Vec<_>>())) { kinds.extend(derived); }
        }
        return Json(json!({"kinds":kinds})).into_response();
    }
    let (kind, rows) = match parts.as_slice() {
        ["tables", kind] => (*kind, false),
        ["tables", kind, "rows"] => (*kind, true),
        _ => return error(StatusCode::NOT_FOUND, "not_found", "unknown table route"),
    };
    let Some(kind) = REGISTERED_RESOURCE_KINDS.iter().find(|entry| entry.plural == kind) else {
        return match decided {
            Some(service) => super::derived_tables::respond(kind, rows, &params, accept, service).await,
            None => error(StatusCode::NOT_FOUND, "not_found", "unknown table"),
        };
    };
    let table = descriptor(kind.plural);
    let format = match negotiate(params.iter().find(|(key, _)| key == "format").map(|(_, value)| value.as_str()), accept, rows) {
        Ok(format) => format,
        Err(error) => return error.into_response(),
    };
    if !rows {
        if format == "markdown" {
            let mut layout = flotilla_schema::Layout::root(kind.kind, flotilla_schema::KindCode::new("tb"));
            for column in table.key.iter().chain(&table.columns) {
                layout.column(&column.name, column.ty.clone(), column.nullable);
            }
            let mut set = layout.finish();
            set.tables[0] = table;
            return ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], flotilla_schema::render_view([&set])).into_response();
        }
        if format == "mermaid" || format == "observed" {
            return error(StatusCode::NOT_FOUND, "no_machine", "resource kind has no declared machine");
        }
        return Json(table.frictionless()).into_response();
    }
    let query = match Query::parse(&params, &table) {
        Ok(query) => query,
        Err(response) => return response.into_response(),
    };
    let cbor = format == "cbor";

    let messages = match snapshot(&backend, kind.plural, &table, &query).await {
        Ok(messages) => messages,
        Err(error) => return error.into_response(),
    };
    let live: BoxStream<'static, Result<Message, String>> = if query.watch {
        watch(backend, kind.plural.into(), table, query, messages.1, messages.2).boxed()
    } else {
        futures::stream::empty().boxed()
    };
    let stream = futures::stream::iter(messages.0.into_iter().map(Ok))
        .chain(live)
        .map(move |message| message.map_err(std::io::Error::other).and_then(|message| encode(&message, cbor)));
    ([(header::CONTENT_TYPE, if cbor { "application/cbor-seq" } else { "application/x-ndjson" })], Body::from_stream(stream))
        .into_response()
}

type Snapshot = (Vec<Message>, WatchStart, BTreeMap<String, Row>);
async fn snapshot(backend: &ResourceBackend, kind: &str, table: &Table, query: &Query) -> Result<Snapshot, Failure> {
    let document =
        query_resource_kind(backend, &query.namespace, kind, &query.filter).await.map_err(|error| bad("unsupported_filter", error))?;
    let sequence = snapshot_sequence(&document).map_err(|error| error_response(&error))?;
    let generation = document.pointer("/metadata/generation").and_then(Value::as_str).map(str::to_owned);
    let start = generation.clone().map_or_else(
        || WatchStart::FromVersion(sequence.to_string()),
        |generation| WatchStart::FromVersionInGeneration { generation, resource_version: sequence.to_string() },
    );
    let mut schema = table.frictionless();
    schema["fields"]
        .as_array_mut()
        .expect("fields")
        .retain(|field| query.columns.iter().any(|name| Some(name.as_str()) == field["name"].as_str()));
    let mut messages = vec![];
    let after = match query.after(kind) {
        Ok(after) => after,
        Err(reason) => {
            messages.push(Message::Resync { reason });
            None
        }
    };
    messages.push(Message::Table { table: kind.into(), schema, incarnation: generation.unwrap_or_else(|| "durable".into()), sequence });
    let mut rows: Vec<_> = document["items"]
        .as_array()
        .ok_or_else(|| error_response("missing resource items"))?
        .iter()
        .map(|value| row(value, table))
        .collect();
    let matched = rows.iter().map(|row| (row[KEY].as_str().expect("resource name").to_owned(), row.clone())).collect();
    let count = rows.len() as u64;
    rows.sort_by(|a, b| query.compare(&query.values(a), &query.values(b)));
    if let Some(after) = after {
        rows.retain(|row| query.compare(&query.values(row), &after) == Ordering::Greater);
    }
    let mut rows: Vec<_> = rows.into_iter().skip(query.offset).collect();
    let partial = rows.len() > query.limit || query.cursor.is_some() || query.offset > 0;
    let more = rows.len() > query.limit;
    rows.truncate(query.limit);
    let cursor =
        if more { rows.last().map(|row| query.cursor(kind, row)).transpose().map_err(|error| error_response(&error))? } else { None };
    if let Some(mode) = &query.count {
        messages.push(Message::Count { count, estimate: mode == "estimate", sequence });
    }
    messages.extend(
        batches(rows.iter().map(|row| query.project(row)).collect(), sequence, partial, cursor).map_err(|error| error_response(&error))?,
    );
    messages.push(Message::Synced { sequence });
    Ok((messages, start, matched))
}
fn error_response(message: &str) -> Failure {
    Failure::new(StatusCode::INTERNAL_SERVER_ERROR, "table_source", message)
}

fn watch(
    backend: ResourceBackend,
    kind: String,
    table: Table,
    query: Query,
    start: WatchStart,
    matched: BTreeMap<String, Row>,
) -> impl futures::Stream<Item = Result<Message, String>> + Send {
    struct State {
        backend: ResourceBackend,
        kind: String,
        table: Table,
        query: Query,
        start: WatchStart,
        matched: BTreeMap<String, Row>,
        stream: Option<BoxStream<'static, Result<Value, flotilla_resources::ResourceError>>>,
        pending: std::collections::VecDeque<Message>,
        sequence: u64,
        recovering: bool,
    }
    let sequence = match &start {
        WatchStart::FromVersion(version) | WatchStart::FromVersionInGeneration { resource_version: version, .. } => {
            version.parse().unwrap_or(0)
        }
        WatchStart::Now => 0,
    };
    futures::stream::unfold(
        State { backend, kind, table, query, start, matched, stream: None, pending: Default::default(), sequence, recovering: false },
        |mut state| async move {
            loop {
                if let Some(message) = state.pending.pop_front() {
                    return Some((Ok(message), state));
                }
                if state.stream.is_none() {
                    match watch_resource_kind_from(&state.backend, &state.query.namespace, &state.kind, state.start.clone()).await {
                        Ok(watch) => {
                            state.stream = Some(watch.stream);
                            state.recovering = false;
                        }
                        Err(_) => {
                            if state.recovering {
                                return Some((Err("table watch unavailable after resnapshot".into()), state));
                            }
                            state.recovering = true;
                            let mut query = state.query.clone();
                            query.cursor = None;
                            query.offset = 0;
                            match snapshot(&state.backend, &state.kind, &state.table, &query).await {
                                Ok((messages, start, matched)) => {
                                    state.sequence = messages
                                        .iter()
                                        .find_map(|message| if let Message::Synced { sequence } = message { Some(*sequence) } else { None })
                                        .unwrap_or(state.sequence);
                                    state.start = start;
                                    state.matched = matched;
                                    state.pending.extend(messages);
                                    return Some((Ok(Message::Resync { reason: "watch cursor expired".into() }), state));
                                }
                                Err(_) => return Some((Err("table resnapshot failed".into()), state)),
                            }
                        }
                    }
                }
                let event = tokio::select! {
                    event = state.stream.as_mut().expect("watch opened").next() => event,
                    _ = tokio::time::sleep(Duration::from_secs(15)) => return Some((Ok(Message::Progress {sequence:state.sequence}),state)),
                };
                let value = match event {
                    Some(Ok(event)) => event,
                    None => return Some((Err("table watch ended".into()), state)),
                    Some(Err(_)) => {
                        state.stream = None;
                        continue;
                    }
                };
                let version = value["object"]
                    .pointer("/metadata/resourceVersion")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<u64>().ok());
                if version.is_some_and(|version| version <= state.sequence) {
                    continue;
                }
                // A full pushed-down query gives one consistent membership snapshot even
                // if more changes commit while the event is being consumed.
                let document = match query_resource_kind(&state.backend, &state.query.namespace, &state.kind, &state.query.filter).await {
                    Ok(document) => document,
                    Err(error) => return Some((Err(error.to_string()), state)),
                };
                state.sequence = match snapshot_sequence(&document) {
                    Ok(sequence) => sequence,
                    Err(error) => return Some((Err(error), state)),
                };
                let current: BTreeMap<String, Row> = document["items"]
                    .as_array()
                    .expect("resource items")
                    .iter()
                    .map(|value| {
                        let row = row(value, &state.table);
                        (row[KEY].as_str().expect("resource name").into(), row)
                    })
                    .collect();
                for (name, row) in &current {
                    if state.matched.get(name) != Some(row) {
                        state.pending.push_back(Message::Upsert {
                            key: vec![json!(name)],
                            row: state.query.project(row),
                            sequence: state.sequence,
                        });
                    }
                }
                for name in state.matched.keys().filter(|name| !current.contains_key(*name)) {
                    state.pending.push_back(Message::Delete { key: vec![json!(name)], sequence: state.sequence });
                }
                if state.query.count.is_some() && current.len() != state.matched.len() {
                    state.pending.push_back(Message::Count { count: current.len() as u64, estimate: false, sequence: state.sequence });
                }
                state.matched = current;
                state.start = document.pointer("/metadata/generation").and_then(Value::as_str).map_or_else(
                    || WatchStart::FromVersion(state.sequence.to_string()),
                    |generation| WatchStart::FromVersionInGeneration {
                        generation: generation.into(),
                        resource_version: state.sequence.to_string(),
                    },
                );
            }
        },
    )
}

#[cfg(test)]
pub(super) fn test_ordering() {
    assert_eq!(compare(&Value::Null, &json!("a")), Ordering::Less);
    assert_eq!(compare(&json!("\u{10000}"), &json!("\u{e000}")), Ordering::Greater);
}

#[cfg(test)]
mod properties {
    use super::*;
    // ADR 0067 keysets use every sort value and the primary key, independent of
    // projection. Generate ties, nulls, both directions, and all cursor boundaries.
    #[hegel::test]
    fn portable_cursor_roundtrips_total_sort_position(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let descending = tc.draw(gs::booleans());
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(11));
        let phase = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let params = vec![("sort".into(), format!("{}status.phase", if descending { "-" } else { "" })), ("columns".into(), "spec".into())];
        let query = Query::parse(&params, &descriptor("convoys")).unwrap_or_else(|_| panic!("valid query"));
        let row = Row::from([
            (KEY.into(), json!(format!("row-{index}"))),
            (
                "status.phase".into(),
                match phase {
                    0 => Value::Null,
                    1 => json!("a"),
                    2 => json!("\u{10000}"),
                    _ => json!("\u{e000}"),
                },
            ),
        ]);
        let cursor = query.cursor("convoys", &row).unwrap();
        let mut resumed = query.clone();
        resumed.cursor = Some(cursor);
        let values = resumed.after("convoys").unwrap().unwrap();
        assert_eq!(query.values(&row), values);
        assert_eq!(query.compare(&values, &query.values(&row)), Ordering::Equal);
        assert!(query.project(&row).contains_key(KEY));
        assert!(resumed.after("other-table").is_err());
    }
}
