//! SQLite table source. All store operations run on blocking workers.
use super::{Query, Row, Table};
use flotilla_schema::{ColumnType, Value as Cell};
use flotilla_schema_sqlite::{stream::Service, TableFilter};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
pub(super) async fn blocking<T: Send + 'static>(
    service: &Arc<Service>,
    op: impl FnOnce(&mut flotilla_schema_sqlite::Store) -> Result<T, flotilla_schema_sqlite::StoreError> + Send + 'static,
) -> Result<T, String> {
    let service = Arc::clone(service);
    tokio::task::spawn_blocking(move || service.with_store(op)).await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())
}

pub(super) async fn list(service: &Arc<Service>) -> Result<Vec<Value>, String> {
    blocking(service, |store| {
        Ok(store
            .kinds()
            .iter()
            .map(|set| {
                let names: Vec<_> =
                    set.tables.iter().map(|table| table.name.clone()).chain(std::iter::once(set.log_projection().name)).collect();
                json!({"kind":set.kind,"root":set.tables[0].name,"tables":names})
            })
            .collect())
    })
    .await
}

pub(super) fn home(namespace: &str, local: &str) -> String {
    if namespace == "default" {
        local.into()
    } else {
        namespace.into()
    }
}

pub(super) async fn read(service: &Arc<Service>, name: &str, table: &Table, query: &Query) -> Result<(u64, Table, Vec<Row>), String> {
    let (service, name, table, query) = (Arc::clone(service), name.to_owned(), table.clone(), query.clone());
    tokio::task::spawn_blocking(move || read_sync(&service, &name, &table, &query)).await.map_err(|error| error.to_string())?
}

fn read_sync(service: &Service, name: &str, table: &Table, query: &Query) -> Result<(u64, Table, Vec<Row>), String> {
    service
        .with_store(|store| {
            let home = home(&query.namespace, store.home());
            // Validate and coerce literals using the same descriptor that is served.
            let mut filters = vec![];
            for condition in &query.filter {
                let (field, literal) = (&condition.field, &condition.literal);
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
                filters.push(TableFilter::Compare { field: field.clone(), operator: condition.operator.as_str().into(), value });
            }
            let result = store.table_read(&home, name, &filters, query.limit + 1)?;
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
