//! Carrier-neutral table reads. Sources lower conditions; paging and live reconciliation
//! have one implementation shared by every carrier and both storage adapters.
mod decided;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use flotilla_address::{Operator, Template};
use flotilla_protocol::table::{Message, Row};
use flotilla_resources::WatchStart;
use flotilla_schema::{ColumnType, Layout, Table};
use flotilla_store::{query_resource_kind, watch_resource_kind_from, ResourceBackend, StoreQuery, REGISTERED_RESOURCE_KINDS};
use futures::{stream::BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};
const LIMIT: usize = 200;
const MAX_LIMIT: usize = 10_000;
const KEY: &str = "metadata.name";
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub code: String,
    pub message: String,
}
impl Failure {
    pub(crate) fn new(code: &str, message: impl ToString) -> Self {
        Self { code: code.into(), message: message.to_string() }
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new("table_source", message)
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
fn bad(code: &str, message: impl ToString) -> Failure {
    Failure::new(code, message)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Condition {
    pub field: String,
    #[serde(with = "comparison")]
    pub operator: Operator,
    pub literal: String,
}
mod comparison {
    use super::Operator;
    use serde::{Deserialize, Deserializer, Serializer};
    pub(super) fn serialize<S: Serializer>(operator: &Operator, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(operator.as_str())
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Operator, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}
/// Bound batches by approximate JSON size; a single large resource stays one message.
pub(crate) fn batches(rows: Vec<Row>, sequence: u64, partial: bool, cursor: Option<String>) -> Result<Vec<Message>, String> {
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

fn descriptor(kind: &str) -> Table {
    let condition_items = json!({
        "type":"object",
        "properties":{
            "type":{"type":"string"},"status":{"type":"string"},
            "reason":{"type":"string"},"message":{"type":"string"},
            "lastTransitionTime":{"type":"string","format":"date-time"}
        }
    });
    let label_values = json!({"type":"string","whereOperators":["=="],"sortable":false});
    let mut layout = Layout::description(kind);
    layout.key(|layout| layout.column(KEY, ColumnType::Text, true));
    for (name, ty, required) in [
        ("metadata.namespace", ColumnType::Text, true),
        ("metadata.resourceVersion", ColumnType::Text, true),
        ("metadata.creationTimestamp", ColumnType::Timestamp, true),
        ("metadata.deletionTimestamp", ColumnType::Timestamp, false),
        ("metadata.labels", ColumnType::Cbor, true),
        ("metadata.annotations", ColumnType::Cbor, true),
        ("status.phase", ColumnType::Text, false),
        ("status.conditions", ColumnType::Cbor, false),
        ("spec", ColumnType::Cbor, true),
        ("status", ColumnType::Cbor, false),
    ] {
        layout.column(name, ty, required);
        match name {
            "metadata.labels" => layout.property("additionalProperties", label_values.clone()),
            "status.phase" => layout.role("state"),
            "status.conditions" => {
                layout.property("type", json!("array"));
                layout.property("items", condition_items.clone());
            }
            _ => (),
        }
    }
    let mut table = layout.finish().tables.remove(0);
    let declared = flotilla_store::declared_resource_kinds().into_iter().find(|entry| entry.paths.plural == kind);
    for field in
        [KEY, "metadata.namespace", "metadata.resourceVersion", "metadata.creationTimestamp", "metadata.deletionTimestamp", "status.phase"]
    {
        let operators: Vec<&str> = if declared.is_some_and(|entry| entry.queryable_fields.contains(&field)) { vec!["=="] } else { vec![] };
        let column = table.key.iter_mut().chain(&mut table.columns).find(|column| column.name == field).expect("declared column");
        column.properties.insert("sortable".into(), json!(field != "metadata.resourceVersion"));
        column.properties.insert("whereOperators".into(), json!(operators));
    }
    for column in table.key.iter_mut().chain(&mut table.columns) {
        column.properties.entry("sortable".into()).or_insert(json!(false));
        column.properties.entry("whereOperators".into()).or_insert(json!([]));
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
        table
            .key
            .iter_mut()
            .chain(&mut table.columns)
            .find(|column| column.name == field)
            .expect("declared column")
            .properties
            .insert("title".into(), json!(title));
    }
    table
}

#[derive(Clone)]
pub(crate) struct Query {
    pub(crate) namespace: String,
    pub(crate) filter: Vec<Condition>,
    pub(crate) sort: Vec<(String, bool)>,
    pub(crate) columns: Vec<String>,
    pub(crate) limit: usize,
    pub(crate) offset: usize,
    pub(crate) cursor: Option<String>,
    pub(crate) count: Option<String>,
    pub(crate) watch: bool,
    pub(crate) keys: Vec<String>,
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
    pub(crate) fn parse(params: &[(String, String)], table: &Table) -> Result<Self, Failure> {
        let get = |name: &str| params.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
        let fields: BTreeSet<_> = table.key.iter().chain(&table.columns).map(|column| column.name.as_str()).collect();
        let mut filter = Vec::new();
        for (_, expression) in params.iter().filter(|(name, _)| name == "where") {
            let clause: Template = format!("$row {expression}").parse().map_err(|err| bad("invalid_filter", err))?;
            let path = clause.path.segments().join(".");
            let label = clause.path.segments().strip_prefix(&["metadata".into(), "labels".into()]);
            let label = label.filter(|parts| parts.len() == 1).map(|parts| parts[0].clone());
            if label.is_some() && !fields.contains("metadata.labels") {
                return Err(bad("unsupported_filter", "table does not declare metadata.labels"));
            }
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
            let operators = if label.is_some() {
                Some(json!(["=="]))
            } else {
                table
                    .key
                    .iter()
                    .chain(&table.columns)
                    .find(|column| column.name == path)
                    .and_then(|column| column.properties.get("whereOperators"))
                    .cloned()
            };
            if !operators.as_ref().and_then(Value::as_array).is_some_and(|ops| ops.contains(&json!(clause.operator.as_str()))) {
                return Err(bad("unsupported_filter", &path));
            }
            filter.push(Condition { field: path, operator: clause.operator, literal: clause.literal });
        }

        let mut sort = Vec::new();
        let default_sort = table.key.iter().map(|column| column.name.as_str()).collect::<Vec<_>>().join(",");
        for field in get("sort").unwrap_or(&default_sort).split(',') {
            let (descending, field) = field.strip_prefix('-').map_or((false, field), |field| (true, field));
            if !fields.contains(field) {
                return Err(bad("unknown_column", field));
            }
            if !table
                .key
                .iter()
                .chain(&table.columns)
                .find(|column| column.name == field)
                .and_then(|column| column.properties.get("sortable"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
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
        for column in table.key.iter().chain(&table.columns).filter(|column| column.reference.is_some()) {
            if columns.contains(&column.name) {
                for suffix in ["kind", "home"] {
                    let component = format!("{}.{}", column.name, suffix);
                    if fields.contains(component.as_str()) && !columns.contains(&component) {
                        columns.push(component);
                    }
                }
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
    pub(crate) fn values(&self, row: &Row) -> Vec<Value> {
        self.sort.iter().map(|(name, _)| row.get(name).cloned().unwrap_or(Value::Null)).collect()
    }
    pub(crate) fn compare(&self, a: &[Value], b: &[Value]) -> Ordering {
        for (((a, b), (_, descending)), (ty, _)) in a.iter().zip(b).zip(&self.sort).zip(&self.sort_types) {
            let order = if *ty == ColumnType::Timestamp {
                match (
                    a.as_str().and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok()),
                    b.as_str().and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok()),
                ) {
                    (Some(a), Some(b)) => a.cmp(&b),
                    _ => compare(a, b),
                }
            } else {
                compare(a, b)
            };
            let order = if *descending { order.reverse() } else { order };
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    }
    pub(crate) fn project(&self, row: &Row) -> Row {
        self.columns.iter().map(|name| (name.clone(), row.get(name).cloned().unwrap_or(Value::Null))).collect()
    }
    pub(crate) fn cursor(&self, table: &str, row: &Row) -> Result<String, String> {
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
    pub(crate) fn after(&self, table: &str) -> Result<Option<Vec<Value>>, String> {
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
                (Value::Number(value), ColumnType::Integer) => value.is_i64() || value.is_u64(),
                (Value::Number(_), ColumnType::Number) => true,
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
fn rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}
fn compare_numbers(a: &serde_json::Number, b: &serde_json::Number) -> Ordering {
    let integer = |n: &serde_json::Number| n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from));
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(a), None) => compare_integer_float(a, b.as_f64().unwrap_or(f64::NAN)),
        (None, Some(b)) => compare_integer_float(b, a.as_f64().unwrap_or(f64::NAN)).reverse(),
        (None, None) => a.as_f64().unwrap_or(f64::NAN).total_cmp(&b.as_f64().unwrap_or(f64::NAN)),
    }
}
fn compare_integer_float(integer: i128, float: f64) -> Ordering {
    const LIMIT: f64 = 1.7014118346046923e38;
    if float >= LIMIT {
        return Ordering::Less;
    }
    if float <= -LIMIT {
        return Ordering::Greater;
    }
    let floor = float.floor();
    integer.cmp(&(floor as i128)).then(if float > floor { Ordering::Less } else { Ordering::Equal })
}
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => compare_numbers(a, b),
        _ => rank(a).cmp(&rank(b)).then_with(|| if rank(a) >= 4 { a.to_string().cmp(&b.to_string()) } else { Ordering::Equal }),
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

/// A complete source snapshot. The carrier never sees backend ids or watch tokens.
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub rows: Vec<Row>,
    pub sequence: u64,
    pub incarnation: String,
    start: WatchStart,
}
#[derive(Clone)]
pub(crate) enum TableSource {
    Resource { backend: ResourceBackend, name: String },
    Decided { service: Arc<flotilla_schema_sqlite::stream::Service>, name: String },
}
pub(crate) struct Description {
    pub table: Table,
    pub schema: Value,
    pub markdown: String,
}
impl TableSource {
    pub(crate) async fn list(service: Option<Arc<flotilla_schema_sqlite::stream::Service>>) -> Result<Value, Failure> {
        let mut kinds: Vec<_> =
            REGISTERED_RESOURCE_KINDS.iter().map(|kind| json!({"kind":kind.kind,"root":kind.plural,"tables":[kind.plural]})).collect();
        if let Some(service) = service {
            kinds.extend(decided::list(&service).await?);
        }
        Ok(json!({"kinds":kinds}))
    }
    pub(crate) fn open(
        name: String,
        backend: ResourceBackend,
        service: Option<Arc<flotilla_schema_sqlite::stream::Service>>,
    ) -> Result<Self, Failure> {
        if REGISTERED_RESOURCE_KINDS.iter().any(|kind| kind.plural == name) {
            return Ok(Self::Resource { backend, name });
        }
        service.map(|service| Self::Decided { service, name }).ok_or_else(|| bad("not_found", "unknown table"))
    }
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Resource { name, .. } | Self::Decided { name, .. } => name,
        }
    }
    pub(crate) async fn describe(&self) -> Result<Description, Failure> {
        match self {
            Self::Resource { name, .. } => {
                let table = descriptor(name);
                Ok(Description { schema: table.frictionless(), markdown: flotilla_schema::render_table_view(&table), table })
            }
            Self::Decided { service, name } => {
                let lookup = name.clone();
                let mut set = decided::blocking(service, move |store| {
                    Ok(store
                        .kinds()
                        .iter()
                        .find(|set| set.tables.iter().any(|table| table.name == lookup) || set.log_projection().name == lookup)
                        .cloned())
                })
                .await?
                .ok_or_else(|| bad("not_found", "unknown table"))?;
                set.tables.push(set.log_projection());
                let markdown = flotilla_schema::render_view([&set]);
                let mut table = set.tables.iter().find(|table| table.name == *name).expect("described table").clone();
                if !name.ends_with(".log") {
                    let mut owner = vec![
                        flotilla_schema::ColumnDef::new("resource.home", ColumnType::Text, false),
                        flotilla_schema::ColumnDef::new("resource.name", ColumnType::Text, false),
                    ];
                    for key in &mut owner {
                        key.add_role("key");
                    }
                    owner.extend(table.key);
                    table.key = owner;
                }
                table = table.with_natural_references();
                for column in table.key.iter_mut().chain(&mut table.columns) {
                    let scalar = !matches!(column.ty, ColumnType::Blob | ColumnType::Cbor | ColumnType::Frozen);
                    column.properties.insert("sortable".into(), json!(scalar));
                    column
                        .properties
                        .insert("whereOperators".into(), json!(if scalar { vec!["==", "!=", "<", "<=", ">", ">="] } else { vec![] }));
                }
                Ok(Description { schema: set.frictionless(&table), table, markdown })
            }
        }
    }
    pub(crate) async fn diagram(&self, observed: bool, namespace: String, record: Option<String>) -> Result<String, Failure> {
        let Self::Decided { service, name } = self else { return Err(bad("no_machine", "resource kind has no declared machine")) };
        let name = name.clone();
        let kind = decided::blocking(service, move |store| {
            Ok(store
                .kinds()
                .iter()
                .find(|set| set.tables.iter().any(|table| table.name == name) || set.log_projection().name == name)
                .map(|set| (set.kind.clone(), set.code.to_string())))
        })
        .await?
        .ok_or_else(|| bad("not_found", "unknown table"))?;
        let declared = crate::runtime::decided_diagrams()
            .map_err(|error| Failure::from(error.to_string()))?
            .remove(&kind.0)
            .ok_or_else(|| bad("no_machine", "kind has no declared machine"))?;
        if !observed {
            return Ok(declared);
        }
        let edges = decided::blocking(service, move |store| {
            store.observed_edges(&kind.1, &decided::home(&namespace, store.home()), record.as_deref())
        })
        .await?;
        let mut diagram = String::from("stateDiagram-v2\n");
        for (from, to, count) in edges {
            diagram.push_str(&format!("  {from} --> {to}: {count}\n"));
        }
        Ok(diagram)
    }
    async fn snapshot(&self, table: &Table, query: &Query) -> Result<Snapshot, Failure> {
        match self {
            Self::Resource { backend, name } => {
                let filter = resource_filter(&query.filter)?;
                let document = query_resource_kind(backend, &query.namespace, name, &filter).await.map_err(source_failure)?;
                let sequence = snapshot_sequence(&document)?;
                let generation = document.pointer("/metadata/generation").and_then(Value::as_str).map(str::to_owned);
                let start = generation.clone().map_or_else(
                    || WatchStart::FromVersion(sequence.to_string()),
                    |generation| WatchStart::FromVersionInGeneration { generation, resource_version: sequence.to_string() },
                );
                Ok(Snapshot {
                    rows: resource_rows(&document, table)?,
                    sequence,
                    incarnation: generation.unwrap_or_else(|| "durable".into()),
                    start,
                })
            }
            Self::Decided { service, name } => {
                let (sequence, _, rows) = decided::read(service, name, table, query).await?;
                Ok(Snapshot { rows, sequence, incarnation: "durable".into(), start: WatchStart::Now })
            }
        }
    }
    fn events(&self, query: &Query, snapshot: &Snapshot) -> BoxStream<'static, Result<(), String>> {
        match self {
            Self::Resource { backend, name } => {
                let (backend, name, namespace) = (backend.clone(), name.clone(), query.namespace.clone());
                let opener: Opener = Box::new(move |start| {
                    let (backend, name, namespace) = (backend.clone(), name.clone(), namespace.clone());
                    Box::pin(async move { watch_resource_kind_from(&backend, &namespace, &name, start).await })
                });
                resource_events(opener, snapshot.start.clone()).boxed()
            }
            Self::Decided { service, .. } => {
                let (service, namespace, sequence) = (service.clone(), query.namespace.clone(), snapshot.sequence);
                futures::stream::unfold((service, namespace, sequence), |(service, namespace, mut sequence)| async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let scope = namespace.clone();
                        match decided::blocking(&service, move |store| {
                            let home = decided::home(&scope, store.home());
                            store.read()?.last_sequence(&home)
                        })
                        .await
                        {
                            Ok(latest) if latest == sequence => continue,
                            Ok(latest) => {
                                sequence = latest;
                                return Some((Ok(()), (service, namespace, sequence)));
                            }
                            Err(error) => return Some((Err(error), (service, namespace, sequence))),
                        }
                    }
                })
                .boxed()
            }
        }
    }
    pub(crate) async fn messages(self, description: Description, query: Query) -> Result<BoxStream<'static, Message>, Failure> {
        if self.name().ends_with(".log") {
            if query.watch {
                return Err(bad("unsupported_watch", "log watches require an incremental log port"));
            }
            if query.cursor.is_some()
                || query.offset != 0
                || query.count.is_some()
                || query.sort != vec![("home".into(), false), ("sequence".into(), false)]
            {
                return Err(bad("unsupported_log_paging", "log reads support only bounded sequence-order snapshots without counts"));
            }
        }
        let snapshot = self.snapshot(&description.table, &query).await?;
        let initial = page(self.name(), description.schema.clone(), &query, &snapshot)?;
        if !query.watch {
            return Ok(futures::stream::iter(initial).boxed());
        }
        let events = self.events(&query, &snapshot);
        let state = Live { source: self, description, query, snapshot, events, pending: initial.into(), failures: 0, done: false };
        Ok(live(state))
    }
}

fn live(state: Live) -> BoxStream<'static, Message> {
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(message) = state.pending.pop_front() {
                return Some((message, state));
            }
            if state.done {
                return None;
            }
            let event = tokio::select! {
                event=state.events.next()=>event.unwrap_or_else(||Err("table change feed ended".into())),
                _=tokio::time::sleep(Duration::from_secs(15))=>return Some((Message::Progress{sequence:state.snapshot.sequence},state)),
            };
            let next = match event {
                Ok(()) => state.source.snapshot(&state.description.table, &state.query).await,
                Err(error) => Err(Failure::from(error)),
            };
            match next {
                Ok(next) => {
                    if next.sequence == state.snapshot.sequence && next.incarnation == state.snapshot.incarnation {
                        continue;
                    }
                    if next.incarnation != state.snapshot.incarnation {
                        state.pending.push_back(Message::Resync { reason: "source incarnation changed".into() });
                        let mut fresh = state.query.clone();
                        fresh.cursor = None;
                        fresh.offset = 0;
                        match page(state.source.name(), state.description.schema.clone(), &fresh, &next) {
                            Ok(messages) => state.pending.extend(messages),
                            Err(error) => {
                                state.pending.push_back(Message::Resync { reason: error.to_string() });
                                state.done = true;
                            }
                        }
                    } else {
                        match diff(&state.snapshot, &next, &state.query) {
                            Ok(messages) => state.pending.extend(messages),
                            Err(error) => {
                                state.pending.push_back(Message::Resync { reason: error });
                                state.done = true;
                            }
                        }
                    }
                    state.snapshot = next;
                    state.failures = 0;
                }
                Err(error) => {
                    state.pending.push_back(Message::Resync { reason: error.to_string() });
                    state.failures += 1;
                    if state.failures >= 3 {
                        state.done = true;
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let mut fresh = state.query.clone();
                    fresh.cursor = None;
                    fresh.offset = 0;
                    match state.source.snapshot(&state.description.table, &fresh).await {
                        Ok(snapshot) => {
                            match page(state.source.name(), state.description.schema.clone(), &fresh, &snapshot) {
                                Ok(messages) => state.pending.extend(messages),
                                Err(error) => {
                                    state.pending.push_back(Message::Resync { reason: error.to_string() });
                                    state.done = true;
                                }
                            }
                            state.events = state.source.events(&state.query, &snapshot);
                            state.snapshot = snapshot;
                        }
                        Err(error) => {
                            state.pending.push_back(Message::Resync { reason: error.to_string() });
                            state.done = true;
                        }
                    }
                }
            }
        }
    })
    .boxed()
}

struct Live {
    source: TableSource,
    description: Description,
    query: Query,
    snapshot: Snapshot,
    events: BoxStream<'static, Result<(), String>>,
    pending: VecDeque<Message>,
    failures: u8,
    done: bool,
}
fn resource_filter(conditions: &[Condition]) -> Result<StoreQuery, Failure> {
    let mut query = StoreQuery::default();
    for condition in conditions {
        if condition.operator != Operator::Equal {
            return Err(bad("unsupported_filter", &condition.field));
        }
        let target = if let Some(label) = condition.field.strip_prefix("metadata.labels.") {
            (&mut query.labels, label)
        } else {
            (&mut query.fields, condition.field.as_str())
        };
        if target.0.get(target.1).is_some_and(|value| value != &condition.literal) {
            query.field_values.insert(KEY.into(), vec![]);
        }
        target.0.insert(target.1.into(), condition.literal.clone());
    }
    Ok(query)
}
fn source_failure(error: flotilla_resources::ResourceError) -> Failure {
    use flotilla_resources::ResourceError;
    match error {
        ResourceError::Invalid { .. } => bad("invalid_request", error),
        ResourceError::Unauthorized { .. } => bad("unauthorized", error),
        _ => Failure::from(error.to_string()),
    }
}
fn page(name: &str, mut schema: Value, query: &Query, snapshot: &Snapshot) -> Result<Vec<Message>, Failure> {
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
    if let Some(keys) = schema.get_mut("foreignKeys").and_then(Value::as_array_mut) {
        keys.retain(|key| {
            key["fields"].as_array().is_some_and(|fields| {
                fields.iter().all(|field| field.as_str().is_some_and(|field| query.columns.iter().any(|name| name == field)))
            })
        });
    }
    messages.push(Message::Table { table: name.into(), schema, incarnation: snapshot.incarnation.clone(), sequence: snapshot.sequence });
    if let Some(mode) = &query.count {
        messages.push(Message::Count { count: snapshot.rows.len() as u64, estimate: mode == "estimate", sequence: snapshot.sequence })
    }
    let mut rows = snapshot.rows.clone();
    rows.sort_by(|a, b| query.compare(&query.values(a), &query.values(b)));
    if let Some(after) = after {
        rows.retain(|row| query.compare(&query.values(row), &after) == Ordering::Greater)
    }
    let mut rows: Vec<_> = rows.into_iter().skip(query.offset).collect();
    let more = rows.len() > query.limit;
    let partial = more || query.offset > 0 || query.cursor.is_some();
    rows.truncate(query.limit);
    let cursor = if more && !name.ends_with(".log") { rows.last().map(|row| query.cursor(name, row)).transpose()? } else { None };
    messages.extend(batches(rows.iter().map(|row| query.project(row)).collect(), snapshot.sequence, partial, cursor)?);
    messages.push(Message::Synced { sequence: snapshot.sequence });
    Ok(messages)
}
type RowIndex = BTreeMap<Vec<u8>, (Vec<Value>, Row)>;
fn indexed(rows: &[Row], keys: &[String]) -> Result<RowIndex, String> {
    rows.iter()
        .map(|row| {
            let tuple: Vec<_> = keys.iter().map(|key| row.get(key).cloned().unwrap_or(Value::Null)).collect();
            Ok((flotilla_cbor::to_vec(&tuple, &Default::default()).map_err(|e| e.to_string())?, (tuple, row.clone())))
        })
        .collect()
}
fn diff(previous: &Snapshot, next: &Snapshot, query: &Query) -> Result<Vec<Message>, String> {
    let previous = indexed(&previous.rows, &query.keys)?;
    let current = indexed(&next.rows, &query.keys)?;
    let mut messages = vec![];
    for (key, (tuple, row)) in &current {
        if previous.get(key).map(|(_, row)| query.project(row)) != Some(query.project(row)) {
            messages.push(Message::Upsert { key: tuple.clone(), row: query.project(row), sequence: next.sequence })
        }
    }
    for (key, (tuple, _)) in &previous {
        if !current.contains_key(key) {
            messages.push(Message::Delete { key: tuple.clone(), sequence: next.sequence })
        }
    }
    if query.count.is_some() && previous.len() != current.len() {
        messages.push(Message::Count { count: current.len() as u64, estimate: false, sequence: next.sequence })
    }
    if messages.is_empty() {
        messages.push(Message::Progress { sequence: next.sequence })
    }
    Ok(messages)
}
type WatchReply = Result<flotilla_store::DynamicResourceWatch, flotilla_resources::ResourceError>;
type Opener = Box<dyn FnMut(WatchStart) -> futures::future::BoxFuture<'static, WatchReply> + Send>;
fn resource_events(opener: Opener, start: WatchStart) -> impl futures::Stream<Item = Result<(), String>> + Send {
    futures::stream::unfold(
        (opener, start, None::<BoxStream<'static, Result<Value, flotilla_resources::ResourceError>>>, 0u8, false),
        |(mut opener, mut start, mut stream, mut failures, mut done)| async move {
            if done {
                return None;
            }
            loop {
                if stream.is_none() {
                    match opener(start.clone()).await {
                        Ok(watch) => stream = Some(watch.stream),
                        Err(error) => return Some((Err(error.to_string()), (opener, start, stream, failures, true))),
                    }
                }
                match stream.as_mut().expect("opened").next().await {
                    Some(Ok(event)) => {
                        if let Some(version) = event["object"].pointer("/metadata/resourceVersion").and_then(Value::as_str) {
                            let previous = match &start {
                                WatchStart::FromVersion(value) | WatchStart::FromVersionInGeneration { resource_version: value, .. } => {
                                    value.parse::<u64>().ok()
                                }
                                WatchStart::Now => None,
                            };
                            if version.parse::<u64>().ok().zip(previous).is_some_and(|(value, previous)| value <= previous) {
                                continue;
                            }
                            start = match &start {
                                WatchStart::FromVersionInGeneration { generation, .. } => {
                                    WatchStart::FromVersionInGeneration { generation: generation.clone(), resource_version: version.into() }
                                }
                                _ => WatchStart::FromVersion(version.into()),
                            };
                        }
                        return Some((Ok(()), (opener, start, stream, 0, false)));
                    }
                    Some(Err(error)) => {
                        failures += 1;
                        if failures >= 3 {
                            done = true;
                            return Some((Err(error.to_string()), (opener, start, stream, failures, done)));
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        stream = None;
                    }
                    None => return Some((Err("table watch ended".into()), (opener, start, stream, failures, true))),
                }
            }
        },
    )
}
#[cfg(test)]
pub(crate) fn test_ordering() {
    assert_eq!(compare(&Value::Null, &json!("a")), Ordering::Less);
    assert_eq!(compare(&json!("\u{10000}"), &json!("\u{e000}")), Ordering::Greater);
}

#[cfg(test)]
mod properties {
    use super::*;
    #[test]
    fn timestamp_keysets_compare_instants_across_precision_and_offsets() {
        let table = descriptor("convoys");
        for descending in [false, true] {
            let field = if descending { "-metadata.deletionTimestamp" } else { "metadata.deletionTimestamp" };
            let query = Query::parse(&[("sort".into(), field.into())], &table).unwrap();
            let whole = vec![json!("2026-10-20T00:00:05Z"), json!("a")];
            let fraction = vec![json!("2026-10-20T00:00:05.123Z"), json!("a")];
            let equivalent = vec![json!("2026-10-20T01:00:05.000+01:00"), json!("a")];
            let null = vec![Value::Null, json!("a")];
            let before = if descending { Ordering::Greater } else { Ordering::Less };
            assert_eq!(query.compare(&whole, &fraction), before);
            assert_eq!(query.compare(&whole, &equivalent), Ordering::Equal);
            assert_eq!(query.compare(&null, &whole), before);
            let row = Row::from([("metadata.name".into(), json!("a")), ("metadata.deletionTimestamp".into(), whole[0].clone())]);
            let cursor = query.cursor("convoys", &row).unwrap();
            let continued = Query::parse(&[("sort".into(), field.into()), ("cursor".into(), cursor)], &table).unwrap();
            let boundary = continued.after("convoys").unwrap().unwrap();
            assert_eq!(continued.compare(&fraction, &boundary), before.reverse());
        }
    }

    // Backend failures are server errors, and malformed snapshots are refused rather
    // than panicking in either snapshot or live-stream decoding (review findings 2–3).
    #[test]
    fn source_failures_and_malformed_documents_are_errors() {
        assert_eq!(source_failure(flotilla_resources::ResourceError::other("disk unavailable")).code, "table_source");
        assert_eq!(source_failure(flotilla_resources::ResourceError::invalid("unsupported predicate")).code, "invalid_request");
        for document in [json!({}), json!({"items":null}), json!({"items":[{}]}), json!({"items":[{"metadata":{"name":3}}]})] {
            assert!(resource_rows(&document, &descriptor("convoys")).is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resource_feed_reopens_after_the_last_delivered_position() {
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = attempts.clone();
        let opener: Opener = Box::new(move |start| {
            let attempt = {
                let mut attempts = observed.lock().unwrap();
                attempts.push(start);
                attempts.len()
            };
            Box::pin(async move {
                let stream: BoxStream<'static, Result<Value, flotilla_resources::ResourceError>> = if attempt == 1 {
                    futures::stream::iter(vec![
                        Ok(json!({"object":{"metadata":{"resourceVersion":"1"}}})),
                        Err(flotilla_resources::ResourceError::other("lost feed")),
                    ])
                    .boxed()
                } else {
                    futures::stream::once(async { Ok(json!({"object":{"metadata":{"resourceVersion":"2"}}})) })
                        .chain(futures::stream::pending())
                        .boxed()
                };
                Ok(flotilla_store::DynamicResourceWatch::builder()
                    .kind("Convoy".into())
                    .plural("convoys".into())
                    .namespace("test".into())
                    .resource_version("0".into())
                    .initial(vec![])
                    .stream(stream)
                    .build())
            })
        });
        let events =
            resource_events(opener, WatchStart::FromVersionInGeneration { generation: "same".into(), resource_version: "0".into() });
        futures::pin_mut!(events);
        assert!(events.next().await.unwrap().is_ok());
        assert!(events.next().await.unwrap().is_ok());
        assert!(
            matches!(&attempts.lock().unwrap()[1],WatchStart::FromVersionInGeneration{generation,resource_version}if generation=="same"&&resource_version=="1")
        );
    }

    // The watch opener stands in for the store subscription boundary. Even if every
    // open succeeds and its stream immediately fails, retries are delayed and bounded.
    #[tokio::test(start_paused = true)]
    async fn immediately_failing_reopened_watches_stop_after_three_attempts() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Arc,
        };
        let opens = Arc::new(AtomicUsize::new(0));
        let observed = opens.clone();
        let opener: Opener = Box::new(move |_| {
            observed.fetch_add(1, AtomicOrdering::SeqCst);
            Box::pin(async {
                Ok(flotilla_store::DynamicResourceWatch::builder()
                    .kind("Convoy".into())
                    .plural("convoys".into())
                    .namespace("test".into())
                    .resource_version("0".into())
                    .initial(vec![])
                    .stream(futures::stream::once(async { Err(flotilla_resources::ResourceError::other("broken watch")) }).boxed())
                    .build())
            })
        });
        let stream = resource_events(opener, WatchStart::FromVersion("0".into()));
        futures::pin_mut!(stream);
        let started = tokio::time::Instant::now();
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.next()).await.expect("bounded retries").unwrap().is_err());
        assert_eq!(opens.load(AtomicOrdering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

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

    fn numeric_value(tc: &hegel::TestCase) -> Value {
        use hegel::generators as gs;
        match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
            0 => Value::Null,
            1 => json!(tc.draw(gs::integers::<i64>())),
            2 => json!(tc.draw(gs::integers::<u64>().min_value(i64::MAX as u64))),
            3 => json!(tc.draw(gs::integers::<i16>()) as f64 / 4.0),
            4 => json!(tc.draw(gs::floats::<f64>().allow_nan(false).allow_infinity(false))),
            _ => json!(tc.draw(gs::integers::<i64>().min_value(-(1 << 53)).max_value(1 << 53)) as f64),
        }
    }

    #[hegel::test]
    fn numeric_sort_orders_rows_and_cursors_page_every_row_once(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let mut table = descriptor("convoys");
        let column = table.columns.iter_mut().find(|column| column.name == "status.phase").expect("phase column");
        column.ty = ColumnType::Number;
        column.nullable = true;
        let descending = tc.draw(gs::booleans());
        let limit = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
        let rows: Vec<Row> =
            (0..count).map(|i| Row::from([(KEY.into(), json!(format!("r{i:02}"))), ("status.phase".into(), numeric_value(&tc))])).collect();
        let sort = format!("{}status.phase", if descending { "-" } else { "" });
        let mut query = Query::parse(&[("sort".into(), sort)], &table).unwrap_or_else(|_| panic!("valid query"));
        query.limit = limit;

        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| query.compare(&query.values(a), &query.values(b)));
        let oracle = |row: &Row| {
            let n = row["status.phase"].as_f64().unwrap_or(f64::NEG_INFINITY);
            if descending {
                -n
            } else {
                n
            }
        };
        for pair in sorted.windows(2) {
            assert!(oracle(&pair[0]) <= oracle(&pair[1]), "{:?} before {:?}", pair[0], pair[1]);
        }

        let mut seen = Vec::new();
        let mut page_query = query.clone();
        loop {
            let snapshot = Snapshot { rows: rows.clone(), sequence: 1, incarnation: "same".into(), start: WatchStart::Now };
            let messages = page("convoys", json!({"fields": []}), &page_query, &snapshot).unwrap();
            let mut cursor = None;
            for message in messages {
                if let Message::Rows { rows, cursor: next, .. } = message {
                    seen.extend(rows.iter().map(|row| row[KEY].clone()));
                    cursor = next;
                }
            }
            match cursor {
                Some(cursor) => page_query.cursor = Some(cursor),
                None => break,
            }
        }
        let expected: Vec<_> = sorted.iter().map(|row| row[KEY].clone()).collect();
        assert_eq!(seen, expected);
    }
}
fn resource_rows(document: &Value, table: &Table) -> Result<Vec<Row>, String> {
    document["items"]
        .as_array()
        .ok_or("missing resource items")?
        .iter()
        .map(|value| {
            value.pointer("/metadata/name").and_then(Value::as_str).ok_or("missing resource name")?;
            Ok(row(value, table))
        })
        .collect()
}

#[cfg(test)]
mod source_contract_tests {
    use super::*;
    #[derive(flotilla_schema::Stored)]
    #[store(kind = "FeedSample", code = "fs")]
    struct FeedSample {
        value: i64,
    }
    #[tokio::test(start_paused = true)]
    async fn every_source_emits_resync_and_a_new_snapshot_on_feed_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut kinds = flotilla_schema::KindRegistry::new();
        kinds.register::<FeedSample>().unwrap();
        let service = Arc::new(flotilla_schema_sqlite::stream::Service::open(&directory.path().join("feed.db"), "home", kinds).unwrap());
        for source in [
            TableSource::Resource {
                backend: ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()),
                name: "convoys".into(),
            },
            TableSource::Decided { service: service.clone(), name: "feed_sample".into() },
        ] {
            let description = source.describe().await.unwrap();
            let query = Query::parse(&[("watch".into(), "true".into())], &description.table).unwrap();
            let snapshot = source.snapshot(&description.table, &query).await.unwrap();
            let mut messages = live(Live {
                source,
                description,
                query,
                snapshot,
                events: futures::stream::once(async { Err("lost change feed".into()) }).boxed(),
                pending: VecDeque::new(),
                failures: 0,
                done: false,
            });
            assert!(matches!(messages.next().await, Some(Message::Resync { .. })));
            assert!(matches!(messages.next().await, Some(Message::Table { .. })));
            assert!(matches!(messages.next().await, Some(Message::Rows { .. })));
            assert!(matches!(messages.next().await, Some(Message::Synced { .. })));
        }
    }
    #[test]
    fn live_diff_observes_projected_values_only() {
        let table = descriptor("convoys");
        let query = Query::parse(&[("columns".into(), "spec".into())], &table).unwrap();
        let row = Row::from([(KEY.into(), json!("one")), ("spec".into(), json!({})), ("metadata.labels".into(), json!({"group":"a"}))]);
        let before = Snapshot { rows: vec![row.clone()], sequence: 1, incarnation: "same".into(), start: WatchStart::Now };
        let mut row = row;
        row.insert("metadata.labels".into(), json!({"group":"b"}));
        let after = Snapshot { rows: vec![row], sequence: 2, incarnation: "same".into(), start: WatchStart::Now };
        assert!(matches!(diff(&before, &after, &query).unwrap().as_slice(), [Message::Progress { sequence: 2 }]));
    }
}
