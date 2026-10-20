//! Derived kinds use their existing descriptor and the SQLite table-read port.
use super::table_api::{bad, encode, error, Query};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use flotilla_protocol::table::{Message, Row};
use flotilla_schema::{ColumnType, Table, Value as Cell};
use flotilla_schema_sqlite::{stream::Service, TableFilter};
use flotilla_state_machines::{
    machines::{agent_session::AgentSession, message::Message as MessageMachine},
    Diagram, Machine,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

type Rows = BTreeMap<Vec<u8>, (Vec<Value>, Row)>;
fn read(service: &Service, name: &str, table: &Table, query: &Query) -> Result<(u64, Table, Vec<Row>), String> {
    service
        .with_store(|store| {
            let home = if query.namespace == "default" { store.home().to_owned() } else { query.namespace.clone() };
            // Validate and coerce literals using the same descriptor that is served.
            let mut filters = vec![];
            for (field, literal) in &query.filter.fields {
                let column = table
                    .key
                    .iter()
                    .chain(&table.columns)
                    .find(|column| &column.name == field)
                    .ok_or_else(|| flotilla_schema_sqlite::StoreError::Stream("unknown column".into()))?;
                let value = match column.ty {
                    ColumnType::Integer => {
                        Cell::Int(literal.parse().map_err(|_| flotilla_schema_sqlite::StoreError::Stream("type mismatch".into()))?)
                    }
                    ColumnType::Timestamp => Cell::Int(
                        chrono::DateTime::parse_from_rfc3339(literal)
                            .map_err(|_| flotilla_schema_sqlite::StoreError::Stream("type mismatch".into()))?
                            .timestamp_micros(),
                    ),
                    ColumnType::Bool => Cell::Int(match literal.as_str() {
                        "true" => 1,
                        "false" => 0,
                        _ => return Err(flotilla_schema_sqlite::StoreError::Stream("type mismatch".into())),
                    }),
                    _ => Cell::Text(literal.clone()),
                };
                filters.push(TableFilter { field: field.clone(), operator: "==".into(), value });
            }
            if query.filter.field_values.values().any(Vec::is_empty) {
                filters.push(TableFilter { field: query.keys[0].clone(), operator: "==".into(), value: Cell::Text(String::new()) });
                filters.push(TableFilter { field: query.keys[0].clone(), operator: "!=".into(), value: Cell::Text(String::new()) });
            }
            let result = store.table_read(&home, name, &filters)?;
            let columns: BTreeMap<_, _> =
                result.descriptor.key.iter().chain(&result.descriptor.columns).map(|column| (column.name.as_str(), &column.ty)).collect();
            let rows = result
                .rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(name, value)| {
                            let value = match value {
                                Cell::Null => Value::Null,
                                Cell::Int(value) => match columns[name.as_str()] {
                                    ColumnType::Bool => json!(value != 0),
                                    ColumnType::Timestamp => {
                                        json!(flotilla_schema::Timestamp(value).to_datetime().map(|time| time.to_rfc3339()))
                                    }
                                    _ => json!(value),
                                },
                                Cell::Text(value) | Cell::Str(value) => json!(value),
                                Cell::Blob(bytes) => match columns[name.as_str()] {
                                    ColumnType::Cbor | ColumnType::Frozen => {
                                        flotilla_cbor::from_slice::<Value>(&bytes, &Default::default())
                                            .map_err(|error| flotilla_schema_sqlite::StoreError::Stream(error.to_string()))?
                                    }
                                    _ => {
                                        use base64::Engine;
                                        json!(base64::engine::general_purpose::STANDARD.encode(bytes))
                                    }
                                },
                                _ => return Err(flotilla_schema_sqlite::StoreError::Stream("unresolved local value".into())),
                            };
                            Ok((name, value))
                        })
                        .collect::<Result<Row, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((result.sequence, result.descriptor, rows))
        })
        .map_err(|error| error.to_string())
}
fn indexed(rows: &[Row], keys: &[String]) -> Result<Rows, String> {
    rows.iter()
        .map(|row| {
            let key: Vec<_> = keys.iter().map(|name| row[name].clone()).collect();
            let bytes = flotilla_cbor::to_vec(&key, &Default::default()).map_err(|error| error.to_string())?;
            Ok((bytes, (key, row.clone())))
        })
        .collect()
}
fn page(name: &str, mut schema: Value, query: &Query, sequence: u64, mut rows: Vec<Row>) -> Result<Vec<Message>, String> {
    let mut messages = vec![];
    let after = match query.after(name) {
        Ok(after) => after,
        Err(reason) => {
            messages.push(Message::Resync { reason });
            None
        }
    };
    schema["fields"]
        .as_array_mut()
        .expect("fields")
        .retain(|field| query.columns.iter().any(|name| Some(name.as_str()) == field["name"].as_str()));
    messages.push(Message::Table { table: name.into(), schema, incarnation: "durable".into(), sequence });
    if let Some(mode) = &query.count {
        messages.push(Message::Count { count: rows.len() as u64, estimate: mode == "estimate", sequence });
    }
    rows.sort_by(|a, b| query.compare(&query.values(a), &query.values(b)));
    if let Some(after) = after {
        rows.retain(|row| query.compare(&query.values(row), &after) == std::cmp::Ordering::Greater);
    }
    let mut rows: Vec<_> = rows.into_iter().skip(query.offset).collect();
    let more = rows.len() > query.limit;
    let partial = more || query.offset > 0 || query.cursor.is_some();
    rows.truncate(query.limit);
    let cursor = if more { rows.last().map(|row| query.cursor(name, row)).transpose()? } else { None };
    messages.extend(super::table_api::batches(rows.iter().map(|row| query.project(row)).collect(), sequence, partial, cursor)?);
    messages.push(Message::Synced { sequence });
    Ok(messages)
}

pub(super) async fn respond(name: &str, rows: bool, params: &[(String, String)], accept: &str, service: Arc<Service>) -> Response {
    let info = service.with_store(|store| {
        Ok(store.kinds().iter().find(|set| set.tables.iter().any(|table| table.name == name) || set.log_table().name == name).cloned())
    });
    let mut set = match info {
        Ok(Some(set)) => set,
        _ => return error(StatusCode::NOT_FOUND, "not_found", "unknown table"),
    };
    let log = set.log_table();
    set.tables.push(log);
    let get = |name: &str| params.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let format = match super::table_api::negotiate(get("format"), accept, rows) {
        Ok(format) => format,
        Err(error) => return error.into_response(),
    };
    if !rows && (format == "mermaid" || format == "observed") {
        let declared = match set.kind.as_str() {
            "Message" => Some(MessageMachine::definition().mermaid(Diagram::Full)),
            "AgentSession" => Some(AgentSession::definition().mermaid(Diagram::Full)),
            _ => None,
        };
        let Some(mut diagram) = declared else { return error(StatusCode::NOT_FOUND, "no_machine", "kind has no declared machine") };
        if format == "observed" {
            let edges =
                service.with_store(|store| store.observed_edges(set.code.as_str(), get("namespace").unwrap_or(store.home()), get("name")));
            match edges {
                Ok(edges) => {
                    diagram = "stateDiagram-v2\n".into();
                    for (from, to, count) in edges {
                        diagram.push_str(&format!("  {from} --> {to}: {count}\n"));
                    }
                }
                Err(err) => return bad("table_source", err).into_response(),
            }
        }
        return ([(header::CONTENT_TYPE, "text/vnd.mermaid; charset=utf-8")], diagram).into_response();
    }
    if !rows && (format == "markdown") {
        return ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], flotilla_schema::render_view([&set])).into_response();
    }
    let mut table = set.tables.iter().find(|table| table.name == name).expect("described table").clone();
    let mut keys = if name.ends_with(".log") {
        vec![]
    } else {
        vec![
            flotilla_schema::ColumnDef { name: "resource.home".into(), ty: ColumnType::Text, nullable: false },
            flotilla_schema::ColumnDef { name: "resource.name".into(), ty: ColumnType::Text, nullable: false },
        ]
    };
    keys.extend(table.key);
    table.key = keys;
    for column in table.key.iter().chain(&table.columns) {
        let scalar = !matches!(column.ty, ColumnType::Frozen | ColumnType::Cbor | ColumnType::Blob);
        let props = table.properties.entry(column.name.clone()).or_default();
        props.insert("sortable".into(), json!(scalar));
        props.insert("whereOperators".into(), json!(if scalar { vec!["=="] } else { vec![] }));
    }
    if !rows {
        return Json(set.frictionless(&table)).into_response();
    }
    let query = match Query::parse(params, &table) {
        Ok(query) => query,
        Err(err) => return err.into_response(),
    };
    let (sequence, _, rows) = match read(&service, name, &table, &query) {
        Ok(rows) => rows,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, "table_source", err),
    };
    let current = match indexed(&rows, &query.keys) {
        Ok(rows) => rows,
        Err(err) => return bad("table_source", err).into_response(),
    };
    let messages = match page(name, set.frictionless(&table), &query, sequence, rows) {
        Ok(messages) => messages,
        Err(err) => return bad("table_source", err).into_response(),
    };
    let cbor = format == "cbor";
    let name = name.to_owned();
    let live = futures::stream::unfold(
        (current, sequence, VecDeque::new(), service, query, table, name, tokio::time::Instant::now()),
        |(mut current, mut sequence, mut pending, service, query, table, name, mut heartbeat)| async move {
            if !query.watch {
                return None;
            }
            loop {
                if let Some(message) = pending.pop_front() {
                    return Some((Ok(message), (current, sequence, pending, service, query, table, name, heartbeat)));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                let latest = service.with_store(|store| {
                    let home = if query.namespace == "default" { store.home().to_owned() } else { query.namespace.clone() };
                    store.read()?.last_sequence(&home)
                });
                match latest {
                    Ok(latest) if latest == sequence => {
                        if heartbeat.elapsed() >= Duration::from_secs(15) {
                            heartbeat = tokio::time::Instant::now();
                            pending.push_back(Message::Progress { sequence });
                        }
                        continue;
                    }
                    Err(err) => return Some((Err(err.to_string()), (current, sequence, pending, service, query, table, name, heartbeat))),
                    _ => {}
                }
                let (next_sequence, _, rows) = match read(&service, &name, &table, &query) {
                    Ok(rows) => rows,
                    Err(err) => return Some((Err(err), (current, sequence, pending, service, query, table, name, heartbeat))),
                };
                if next_sequence == sequence {
                    continue;
                }
                let next = match indexed(&rows, &query.keys) {
                    Ok(rows) => rows,
                    Err(err) => return Some((Err(err), (current, sequence, pending, service, query, table, name, heartbeat))),
                };
                sequence = next_sequence;
                for (key, (tuple, row)) in &next {
                    if current.get(key) != Some(&(tuple.clone(), row.clone())) {
                        pending.push_back(Message::Upsert { key: tuple.clone(), row: query.project(row), sequence });
                    }
                }
                for (key, (tuple, _)) in &current {
                    if !next.contains_key(key) {
                        pending.push_back(Message::Delete { key: tuple.clone(), sequence });
                    }
                }
                if query.count.is_some() && current.len() != next.len() {
                    pending.push_back(Message::Count { count: next.len() as u64, estimate: false, sequence });
                }
                current = next;
                if pending.is_empty() {
                    pending.push_back(Message::Progress { sequence });
                }
            }
        },
    );
    let stream = futures::stream::iter(messages.into_iter().map(Ok::<_, String>))
        .chain(live)
        .map(move |message| message.map_err(std::io::Error::other).and_then(|message| encode(&message, cbor)));
    ([(header::CONTENT_TYPE, if cbor { "application/cbor-seq" } else { "application/x-ndjson" })], Body::from_stream(stream))
        .into_response()
}
