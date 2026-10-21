//! Natural-value reads for self-describing tables. The SQL boundary validates every
//! identifier and comparison before preparing a statement, and applies all predicates.
use crate::{quote_ident, StoreError};
use flotilla_schema::{ColumnDef, ColumnType, KindRegistry, Table, Value};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use std::collections::BTreeMap;

pub struct TableRead {
    pub descriptor: Table,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub sequence: u64,
}
/// A comparison on a described field. Values remain logical, never store-local ids.
pub enum TableFilter {
    Compare {
        field: String,
        operator: String,
        value: Value,
    },
    /// An unsatisfiable conjunction, evaluated by SQLite without loading any rows.
    None,
}

fn validate_filter_value(column: &ColumnDef, value: &Value) -> Result<(), StoreError> {
    let valid = match (&column.ty, value) {
        (_, Value::Null) => column.nullable,
        (ColumnType::Integer | ColumnType::Timestamp, Value::Int(_)) => true,
        (ColumnType::Bool, Value::Int(value)) => matches!(value, 0 | 1),
        (ColumnType::Text | ColumnType::Interned(_) | ColumnType::Ref, Value::Text(_) | Value::Str(_)) => true,
        (ColumnType::Code(codes), Value::Text(value) | Value::Str(value)) => codes.contains(value),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(StoreError::Stream("filter type mismatch".into()))
    }
}

pub(crate) fn read(
    conn: &Connection,
    kinds: &KindRegistry,
    home: &str,
    name: &str,
    filters: &[TableFilter],
) -> Result<TableRead, StoreError> {
    let (set, source) = kinds
        .iter()
        .find_map(|set| set.tables.iter().find(|table| table.name == name).map(|table| (set, table)))
        .ok_or_else(|| StoreError::Stream("unknown table".into()))?;
    let mut table = source.with_natural_references();
    let mut key = vec![ColumnDef::new("resource.home", ColumnType::Text, false), ColumnDef::new("resource.name", ColumnType::Text, false)];
    for column in &mut key {
        column.add_role("key");
    }
    key.extend(table.key.clone());
    table.key = key;
    let owner = if source.parent.is_none() { "id" } else { set.parent_key_column() };
    let mut expressions = vec![
        "(SELECT s FROM strings WHERE id=i.home) AS \"resource.home\"".into(),
        "(SELECT s FROM strings WHERE id=i.name) AS \"resource.name\"".into(),
    ];
    for column in table.key.iter().skip(2).chain(&table.columns) {
        let original = source.key.iter().chain(&source.columns).find(|original| original.name == column.name);
        let expression = if let Some(original) = original {
            let value = format!("t.{}", quote_ident(&original.name));
            match original.ty {
                ColumnType::Interned(_) => format!("(SELECT s FROM strings WHERE id={value})"),
                ColumnType::Ref => format!("(SELECT s FROM strings WHERE id=(SELECT name FROM resource_identity WHERE id={value}))"),
                ColumnType::Frozen => format!("(SELECT bytes FROM objects WHERE hash={value})"),
                _ => value,
            }
        } else {
            if let Some(reference) = column.name.strip_suffix(".home") {
                format!("(SELECT s FROM strings WHERE id=(SELECT home FROM resource_identity WHERE id=t.{}))", quote_ident(reference))
            } else if let Some(reference) = column.name.strip_suffix(".kind") {
                format!("(SELECT kind FROM resource_identity WHERE id=t.{})", quote_ident(reference))
            } else {
                return Err(StoreError::Stream("unknown reference component".into()));
            }
        };
        expressions.push(format!("{expression} AS {}", quote_ident(&column.name)));
    }
    let base = format!(
        "SELECT {} FROM {} t JOIN resource_identity i ON i.id=t.{} WHERE i.home=(SELECT id FROM strings WHERE domain=? AND s=?)",
        expressions.join(","),
        quote_ident(name),
        quote_ident(owner)
    );
    let columns: Vec<_> = table.key.iter().chain(&table.columns).collect();
    let mut arguments = vec![SqlValue::Text(flotilla_schema::HOME_DOMAIN.to_string()), SqlValue::Text(home.into())];
    let mut clauses = vec![];
    for filter in filters {
        let TableFilter::Compare { field, operator, value } = filter else {
            clauses.push("0".into());
            continue;
        };
        let column =
            columns.iter().find(|column| column.name == *field).ok_or_else(|| StoreError::Stream("unknown filter field".into()))?;
        if !["==", "!=", "<", "<=", ">", ">="].contains(&operator.as_str())
            || matches!(column.ty, ColumnType::Frozen | ColumnType::Cbor | ColumnType::Blob)
        {
            return Err(StoreError::Stream("unsupported filter".into()));
        }
        validate_filter_value(column, value)?;
        let value = match value {
            Value::Null => SqlValue::Null,
            Value::Int(value) => SqlValue::Integer(*value),
            Value::Text(value) | Value::Str(value) => SqlValue::Text(value.clone()),
            _ => return Err(StoreError::Stream("filter type mismatch".into())),
        };
        let operator = if matches!(value, SqlValue::Null) {
            match operator.as_str() {
                "==" => "IS",
                "!=" => "IS NOT",
                _ => return Err(StoreError::Stream("unsupported null comparison".into())),
            }
        } else {
            operator.as_str()
        };
        clauses.push(format!("{} {} ?", quote_ident(field), operator));
        arguments.push(value);
    }
    let sql =
        format!("SELECT * FROM ({base}){}", if clauses.is_empty() { String::new() } else { format!(" WHERE {}", clauses.join(" AND ")) });
    let tx = conn.unchecked_transaction()?;
    let sequence = tx.query_row(
        "SELECT coalesce((SELECT last FROM home_sequence WHERE home=(SELECT id FROM strings WHERE domain=? AND s=?)),0)",
        [flotilla_schema::HOME_DOMAIN.as_str(), home],
        |row| row.get(0),
    )?;
    let rows = {
        let mut statement = tx.prepare(&sql)?;
        let mapped = statement.query_map(params_from_iter(arguments), |row| {
            columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let value = match row.get::<_, SqlValue>(index)? {
                        SqlValue::Null => Value::Null,
                        SqlValue::Integer(value) => Value::Int(value),
                        SqlValue::Text(value) => Value::Text(value),
                        SqlValue::Blob(value) => Value::Blob(value),
                        SqlValue::Real(_) => return Err(rusqlite::Error::InvalidQuery),
                    };
                    Ok((column.name.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    tx.commit()?;
    Ok(TableRead { descriptor: table, rows, sequence })
}

pub(crate) fn observed(conn: &Connection, kind: &str, home: &str, name: Option<&str>) -> Result<Vec<(String, String, u64)>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT v.\"from\",v.\"to\",count(*) FROM log_view v \
         WHERE v.kind=? AND v.home=? AND v.cause='edge' AND (? IS NULL OR v.name=?) \
         GROUP BY v.\"from\",v.\"to\"",
    )?;
    let edges = statement
        .query_map(rusqlite::params![kind, home, name, name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(edges)
}

pub(crate) fn read_log(
    conn: &Connection,
    kinds: &KindRegistry,
    home: &str,
    name: &str,
    filters: &[TableFilter],
    limit: usize,
) -> Result<TableRead, StoreError> {
    if !(1..=10_001).contains(&limit) {
        return Err(StoreError::Stream("log read requires a bounded limit".into()));
    }
    let root = name.strip_suffix(".log").ok_or_else(|| StoreError::Stream("unknown log table".into()))?;
    let set = kinds.iter().find(|set| set.tables[0].name == root).ok_or_else(|| StoreError::Stream("unknown log kind".into()))?;
    let descriptor = set.log_projection();
    let mut clauses = vec!["kind=?".to_owned(), "home=?".to_owned()];
    let mut values = vec![SqlValue::Text(set.code.to_string()), SqlValue::Text(home.into())];
    for filter in filters {
        let TableFilter::Compare { field, operator, value } = filter else {
            clauses.push("0".into());
            continue;
        };
        let column = descriptor
            .key
            .iter()
            .chain(&descriptor.columns)
            .find(|column| column.name == *field)
            .ok_or_else(|| StoreError::Stream("unknown log filter field".into()))?;
        if !["==", "!=", "<", "<=", ">", ">="].contains(&operator.as_str()) {
            return Err(StoreError::Stream("unsupported log filter".into()));
        }
        validate_filter_value(column, value)?;
        let field = if *field == "sequence" { "home_seq" } else { field };
        let operator = if matches!(value, Value::Null) {
            match operator.as_str() {
                "==" => "IS",
                "!=" => "IS NOT",
                _ => return Err(StoreError::Stream("unsupported null comparison".into())),
            }
        } else {
            operator.as_str()
        };
        clauses.push(format!("{} {} ?", quote_ident(field), operator));
        values.push(match value {
            Value::Null => SqlValue::Null,
            Value::Text(value) | Value::Str(value) => SqlValue::Text(value.clone()),
            Value::Int(value) => SqlValue::Integer(*value),
            _ => return Err(StoreError::Stream("log filter type mismatch".into())),
        });
    }
    let tx = conn.unchecked_transaction()?;
    let sequence = tx.query_row(
        "SELECT coalesce((SELECT last FROM home_sequence WHERE home=(SELECT id FROM strings WHERE domain=? AND s=?)),0)",
        [flotilla_schema::HOME_DOMAIN.as_str(), home],
        |row| row.get(0),
    )?;
    let rows = {
        let sql = format!(
            "SELECT home,home_seq,name,cause,edge,\"from\",\"to\",at FROM log_view WHERE {} ORDER BY home_seq LIMIT ?",
            clauses.join(" AND ")
        );
        values.push(SqlValue::Integer(limit as i64));
        let mut statement = tx.prepare(&sql)?;
        let mapped = statement.query_map(params_from_iter(values), |row| {
            descriptor
                .key
                .iter()
                .chain(&descriptor.columns)
                .enumerate()
                .map(|(index, column)| {
                    let value = match row.get::<_, SqlValue>(index)? {
                        SqlValue::Null => Value::Null,
                        SqlValue::Text(value) => Value::Text(value),
                        SqlValue::Integer(value) => Value::Int(value),
                        _ => return Err(rusqlite::Error::InvalidQuery),
                    };
                    Ok((column.name.clone(), value))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    tx.commit()?;
    Ok(TableRead { descriptor, rows, sequence })
}
