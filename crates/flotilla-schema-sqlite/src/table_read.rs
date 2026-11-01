//! Natural-value reads for self-describing tables. The SQL boundary validates every
//! identifier and comparison before preparing a statement, and applies all predicates.
//! Enum columns read as their readable names ([`Table::readable`]): short codes stay in
//! storage and in the SQL below, and filters compare names.
//!
//! A filter is a checked condition in the `rigging-condition` language (#4108), translated through
//! a [`ColumnMap`] of the expressions this module writes: field paths never reach the SQL, and
//! every literal is a parameter. An address column is matched by its interned id, so `under` and
//! equality search the `strings` table's `UNIQUE(domain, s)` index.
use crate::{ddl::text_literal, quote_ident, StoreError};
use flotilla_schema::{ColumnDef, ColumnType, Filterable, KindRegistry, Table, Value};
use rigging_condition::{Checked, Column, ColumnMap, Condition, InternTable, Param, Path};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use std::collections::BTreeMap;

/// `expression` named: each stored value of `column`'s categories read as its readable name.
/// There is no `ELSE`: a value outside the categories reads as null, never as a code on the wire.
/// None can be stored, as the column's CHECK lists exactly these codes; for the log's `edge`,
/// `from` and `to`, `log_view` already names the codes and its categories map each name to itself.
fn named(expression: String, column: &ColumnDef) -> String {
    if !matches!(column.ty, ColumnType::Code(_)) || column.categories.is_empty() {
        return expression;
    }
    let arms: String =
        column.categories.iter().map(|(value, name)| format!(" WHEN {} THEN {}", text_literal(value), text_literal(name))).collect();
    format!("CASE {expression}{arms} END")
}

pub struct TableRead {
    pub descriptor: Table,
    pub rows: Vec<BTreeMap<String, Value>>,
    pub sequence: u64,
}

/// Each filterable column of a read, by the path the table's [`Filterable`] gives it.
struct Columns {
    fields: Filterable,
    columns: BTreeMap<String, Column>,
}

impl ColumnMap for Columns {
    fn column(&self, path: &Path) -> Option<Column> {
        self.fields.column(path).and_then(|name| self.columns.get(name)).cloned()
    }
}

/// The `WHERE` text of `filter` and its parameters, numbered from `?1`; `1` with none.
fn translate(filter: Option<&Checked<Condition>>, columns: &Columns) -> Result<(String, Vec<SqlValue>), StoreError> {
    let Some(filter) = filter else { return Ok(("1".into(), vec![])) };
    let (sql, params) = filter.to_sql(columns).map_err(|error| StoreError::Stream(format!("filter: {error}")))?;
    let params = params
        .into_iter()
        .map(|param| match param {
            Param::Integer(value) => SqlValue::Integer(value),
            Param::Real(value) => SqlValue::Real(value),
            Param::Text(value) => SqlValue::Text(value),
        })
        .collect();
    Ok((sql, params))
}

/// A stored enum column's readable names mapped to their codes, as parameters.
fn codes(column: &ColumnDef) -> Option<BTreeMap<String, Param>> {
    (matches!(column.ty, ColumnType::Code(_)) && !column.categories.is_empty())
        .then(|| column.categories.iter().map(|(code, name)| (name.clone(), Param::Text(code.clone()))).collect())
}

/// The statement that reads table `name` of `home` with `filter` applied, its arguments, and the
/// table it describes.
pub(crate) fn statement(
    kinds: &KindRegistry,
    home: &str,
    name: &str,
    filter: Option<&Checked<Condition>>,
) -> Result<(String, Vec<SqlValue>, Table), StoreError> {
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
    // What a filter reads: the projected expression, except that an address is matched by its
    // interned id and a stored enum by its code, so SQLite can search their indexes.
    let mut filterable = BTreeMap::from([
        ("resource.home".to_owned(), Column::Value { expr: "(SELECT s FROM strings WHERE id=i.home)".into(), codes: None }),
        ("resource.name".to_owned(), Column::Value { expr: "(SELECT s FROM strings WHERE id=i.name)".into(), codes: None }),
    ]);
    for column in table.key.iter().skip(2).chain(&table.columns) {
        let original = source.key.iter().chain(&source.columns).find(|original| original.name == column.name);
        let expression = if let Some(original) = original {
            let value = format!("t.{}", quote_ident(&original.name));
            match &original.ty {
                ColumnType::Interned(domain) if original.address => {
                    let strings = InternTable {
                        table: "strings".into(),
                        id: "id".into(),
                        domain_column: "domain".into(),
                        text_column: "s".into(),
                        domain: domain.to_string(),
                    };
                    filterable.insert(column.name.clone(), Column::Interned { expr: value.clone(), strings });
                }
                ColumnType::Code(_) => {
                    filterable.insert(column.name.clone(), Column::Value { expr: value.clone(), codes: codes(original) });
                }
                _ => {}
            }
            match original.ty {
                ColumnType::Interned(_) => format!("(SELECT s FROM strings WHERE id={value})"),
                ColumnType::Ref => format!("(SELECT s FROM strings WHERE id=(SELECT name FROM resource_identity WHERE id={value}))"),
                ColumnType::Frozen => format!("(SELECT bytes FROM objects WHERE hash={value})"),
                _ => named(value, original),
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
        filterable.entry(column.name.clone()).or_insert_with(|| Column::Value { expr: expression.clone(), codes: None });
        expressions.push(format!("{expression} AS {}", quote_ident(&column.name)));
    }
    let table = table.readable();
    let (clause, mut arguments) = translate(filter, &Columns { fields: table.filterable(), columns: filterable })?;
    // The filter's parameters are `?1`…`?n`; the home's follow them.
    let (domain, home_param) = (arguments.len() + 1, arguments.len() + 2);
    arguments.extend([SqlValue::Text(flotilla_schema::HOME_DOMAIN.to_string()), SqlValue::Text(home.into())]);
    let sql = format!(
        "SELECT {} FROM {} t JOIN resource_identity i ON i.id=t.{} WHERE i.home=(SELECT id FROM strings WHERE domain=?{domain} AND s=?{home_param}) AND ({clause})",
        expressions.join(","),
        quote_ident(name),
        quote_ident(owner)
    );
    Ok((sql, arguments, table))
}

pub(crate) fn read(
    conn: &Connection,
    kinds: &KindRegistry,
    home: &str,
    name: &str,
    filter: Option<&Checked<Condition>>,
) -> Result<TableRead, StoreError> {
    let (sql, arguments, table) = statement(kinds, home, name, filter)?;
    let columns: Vec<_> = table.key.iter().chain(&table.columns).collect();
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
    filter: Option<&Checked<Condition>>,
    limit: usize,
) -> Result<TableRead, StoreError> {
    if !(1..=10_001).contains(&limit) {
        return Err(StoreError::Stream("log read requires a bounded limit".into()));
    }
    let root = name.strip_suffix(".log").ok_or_else(|| StoreError::Stream("unknown log table".into()))?;
    let set = kinds.iter().find(|set| set.tables[0].name == root).ok_or_else(|| StoreError::Stream("unknown log kind".into()))?;
    let stored = set.log_projection();
    let descriptor = stored.readable();
    // The outer query reads the projection's readable names, so every column compares as the
    // rows carry it.
    let columns = descriptor
        .key
        .iter()
        .chain(&descriptor.columns)
        .map(|column| {
            let source = if column.name == "sequence" { "home_seq" } else { &column.name };
            (column.name.clone(), Column::Value { expr: quote_ident(source), codes: None })
        })
        .collect();
    let (clause, mut values) = translate(filter, &Columns { fields: descriptor.filterable(), columns })?;
    // The filter's parameters are `?1`…`?n`; the inner query's `kind` and `home`, and the limit, follow.
    let (kind_param, home_param, limit_param) = (values.len() + 1, values.len() + 2, values.len() + 3);
    values.extend([SqlValue::Text(set.code.to_string()), SqlValue::Text(home.into()), SqlValue::Integer(limit as i64)]);
    let tx = conn.unchecked_transaction()?;
    let sequence = tx.query_row(
        "SELECT coalesce((SELECT last FROM home_sequence WHERE home=(SELECT id FROM strings WHERE domain=? AND s=?)),0)",
        [flotilla_schema::HOME_DOMAIN.as_str(), home],
        |row| row.get(0),
    )?;
    let rows = {
        let projected: Vec<String> = stored
            .key
            .iter()
            .chain(&stored.columns)
            .map(|column| {
                let source = if column.name == "sequence" { "home_seq" } else { &column.name };
                format!("{} AS {}", named(quote_ident(source), column), quote_ident(source))
            })
            .collect();
        let sql = format!(
            "SELECT * FROM (SELECT {} FROM log_view WHERE kind=?{kind_param} AND home=?{home_param}) WHERE {clause} ORDER BY home_seq LIMIT ?{limit_param}",
            projected.join(","),
        );
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
