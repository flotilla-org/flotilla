//! Per-kind statements, rendered once from the descriptor when the store opens. Every statement
//! addresses rows by primary key: the root by `id`, child rows by `owner` plus their key.

use std::{collections::BTreeMap, ops::Range};

use flotilla_schema::{quote_ident, ColumnDef, ColumnType, FieldColumns, Index, Table, TableRole, TableSet};

pub(crate) struct TableSql {
    pub name: String,
    pub role: TableRole,
    pub parent: Option<usize>,
    /// The field this table stores (empty for the root): how an edge names it.
    pub field: String,
    /// Key columns (excluding the owner), then data columns.
    pub columns: Vec<ColumnDef>,
    pub key_len: usize,
    pub insert: String,
    /// Empty when the table has no data columns (a set's rows are all key; a kind with no fields).
    pub update: String,
    pub delete: String,
    /// Child tables: every row of one owner. The root: the row of one id, version first.
    pub select: String,
    /// Indexes into a row's data values of `state` columns, which only an edge moves.
    pub state_columns: Vec<usize>,
    /// The root's and `records` tables' column-shaped fields, each with the statement an edge
    /// sets it with: the root's also moves the version (`?1`) under the optimistic check.
    pub fields: Vec<FieldSql>,
    /// `records` and `history` tables: the last position under one parent row, by primary-key
    /// range; no row when the parent row does not exist, a null when it has no rows yet.
    /// Parameters: the owner, then the parent's key.
    pub last_position: String,
}

/// One field an edge sets, and its statement. `columns` indexes the row's data values.
pub(crate) struct FieldSql {
    pub name: String,
    pub columns: Range<usize>,
    pub update: String,
}

pub(crate) struct KindSql {
    pub tables: Vec<TableSql>,
    pub root_update_version: String,
    pub root_delete: String,
    pub root_version: String,
    /// Declared indexes by declared name.
    pub indexes: BTreeMap<String, IndexSql>,
    /// The startup scan: this kind's resources homed at `?1`, by id, through
    /// `resource_identity_by_home`. Identities without a root row (references to resources not
    /// stored here, deleted resources) are skipped by the primary-key join.
    pub scan: String,
}

/// One index's page queries. Both select `id` then the order columns, filter by the index's own
/// predicate (rendered as the DDL renders it, so SQLite can use the partial index), bind the `eq`
/// values as `?1..`, and order by `order..., id`.
pub(crate) struct IndexSql {
    /// The schema object name, `<table>_<name>`.
    pub object: String,
    pub order: usize,
    /// The first page: parameters are the eq values, then the limit.
    pub first: String,
    /// A later page: eq values, the cursor's order values and id, then the limit.
    pub after: String,
}

/// Interned columns read back as their strings through `strings`' primary key.
/// The outer column is qualified with its table: unqualified, a field named `id` or `s` would
/// resolve to the `strings` column inside the subquery and match the wrong row.
fn select_expr(table: &str, column: &ColumnDef) -> String {
    let name = format!("{table}.{}", quote_ident(&column.name));
    match column.ty {
        ColumnType::Interned => format!("(SELECT \"strings\".\"s\" FROM \"strings\" WHERE \"strings\".\"id\" = {name})"),
        _ => name,
    }
}

fn placeholders(from: usize, count: usize) -> String {
    (from..from + count).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ")
}

fn assignments(columns: &[ColumnDef], from: usize) -> Vec<String> {
    columns.iter().enumerate().map(|(n, column)| format!("{} = ?{}", quote_ident(&column.name), from + n)).collect()
}

impl KindSql {
    pub(crate) fn render(set: &TableSet) -> KindSql {
        let tables = set.tables.iter().map(|table| TableSql::render(set, table)).collect();
        let root = quote_ident(&set.tables[0].name);
        KindSql {
            tables,
            root_update_version: format!("UPDATE {root} SET \"version\" = ?1 WHERE \"id\" = ?2 AND \"version\" = ?3"),
            root_delete: format!("DELETE FROM {root} WHERE \"id\" = ?1 AND \"version\" = ?2"),
            root_version: format!("SELECT \"version\" FROM {root} WHERE \"id\" = ?1"),
            indexes: set.indexes.iter().map(|index| (index.name.clone(), IndexSql::render(&set.tables[0], index))).collect(),
            scan: format!(
                "SELECT i.\"id\" FROM \"resource_identity\" AS i CROSS JOIN {root} AS r ON r.\"id\" = i.\"id\" \
                 WHERE i.\"home\" = ?1 AND i.\"kind\" = ?2 AND i.\"id\" > ?3 ORDER BY i.\"id\" LIMIT ?4"
            ),
        }
    }

    /// Every statement of this kind, labelled, for the query-plan tests.
    pub(crate) fn statements(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for table in &self.tables {
            for (what, sql) in [("insert", &table.insert), ("update", &table.update), ("delete", &table.delete), ("select", &table.select)]
            {
                if !sql.is_empty() {
                    out.push((format!("{} {what}", table.name), sql.clone()));
                }
            }
        }
        for table in &self.tables {
            for field in &table.fields {
                out.push((format!("{} set {}", table.name, field.name), field.update.clone()));
            }
            if !table.last_position.is_empty() {
                out.push((format!("{} last position", table.name), table.last_position.clone()));
            }
        }
        let root = &self.tables[0].name;
        out.push((format!("{root} update version"), self.root_update_version.clone()));
        out.push((format!("{root} delete"), self.root_delete.clone()));
        out.push((format!("{root} version"), self.root_version.clone()));
        for index in self.indexes.values() {
            out.push((format!("index {} first", index.object), index.first.clone()));
            out.push((format!("index {} after", index.object), index.after.clone()));
        }
        out.push((format!("{root} scan"), self.scan.clone()));
        out
    }
}

impl IndexSql {
    fn render(root: &Table, index: &Index) -> IndexSql {
        let table = quote_ident(&root.name);
        let order: Vec<String> = index.order.iter().map(|name| quote_ident(name)).collect();
        let mut terms: Vec<String> = index.predicate_sql().into_iter().collect();
        terms.extend(index.eq.iter().enumerate().map(|(n, name)| format!("{} = ?{}", quote_ident(name), n + 1)));
        let next = index.eq.len() + 1;
        let mut first = terms.clone();
        if index.eq.is_empty() {
            // With no equality prefix, a bare ORDER BY would walk the index from its start, which
            // SQLite plans as a SCAN. A lower bound below every value of the column's storage class
            // makes it a range SEARCH of the same rows.
            let column = root.columns.iter().find(|column| column.name == index.order[0]).expect("validated index column");
            first.push(format!("{} >= {}", order[0], lowest(&column.ty)));
        }
        let mut after = terms;
        if order.is_empty() {
            after.push(format!("\"id\" > ?{next}"));
        } else {
            let cursor = placeholders(next, order.len() + 1);
            after.push(format!("({}, \"id\") > ({cursor})", order.join(", ")));
        }
        let limit = |params: usize| {
            let mut by = order.clone();
            by.push("\"id\"".to_owned());
            format!("ORDER BY {} LIMIT ?{params}", by.join(", "))
        };
        let select = format!("SELECT \"id\"{} FROM {table} WHERE", prefixed(&order));
        IndexSql {
            object: index.object_name(),
            order: order.len(),
            first: format!("{select} {} {}", first.join(" AND "), limit(next)),
            after: format!("{select} {} {}", after.join(" AND "), limit(next + order.len() + 1)),
        }
    }
}

/// A literal at or below every value a STRICT column of this type holds. Exhaustive, so a new
/// column type (a REAL, say) must choose its own lower bound.
fn lowest(ty: &ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "''",
        ColumnType::Blob | ColumnType::Frozen => "x''",
        ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp | ColumnType::Code(_) | ColumnType::Interned | ColumnType::Ref => {
            "-9223372036854775808"
        }
    }
}

impl TableSql {
    fn render(set: &TableSet, table: &Table) -> TableSql {
        let name = quote_ident(&table.name);
        let columns: Vec<ColumnDef> = table.key.iter().chain(&table.columns).cloned().collect();
        let names: Vec<String> = columns.iter().map(|column| quote_ident(&column.name)).collect();
        let exprs: Vec<String> = columns.iter().map(|column| select_expr(&name, column)).collect();
        let key_names = &names[..table.key.len()];
        let data = &table.columns;
        let (insert, update, delete, select) = if table.parent.is_none() {
            let set = assignments(data, 2);
            let next = 2 + data.len();
            (
                format!(
                    "INSERT INTO {name} (\"id\", \"version\"{}) VALUES (?1, ?2{})",
                    prefixed(&names),
                    prefixed_placeholders(3, names.len())
                ),
                if data.is_empty() {
                    String::new()
                } else {
                    format!("UPDATE {name} SET \"version\" = ?1, {} WHERE \"id\" = ?{next} AND \"version\" = ?{}", set.join(", "), next + 1)
                },
                String::new(),
                format!("SELECT \"version\"{} FROM {name} WHERE \"id\" = ?1", prefixed(&exprs)),
            )
        } else {
            let key_match: Vec<String> = key_names.iter().enumerate().map(|(n, key)| format!(" AND {key} = ?{}", n + 2)).collect();
            let update = if data.is_empty() {
                String::new()
            } else {
                let set = assignments(data, 1);
                let owner = 1 + data.len();
                let key_match: Vec<String> =
                    key_names.iter().enumerate().map(|(n, key)| format!(" AND {key} = ?{}", owner + 1 + n)).collect();
                format!("UPDATE {name} SET {} WHERE \"owner\" = ?{owner}{}", set.join(", "), key_match.concat())
            };
            (
                format!("INSERT INTO {name} (\"owner\"{}) VALUES (?1{})", prefixed(&names), prefixed_placeholders(2, names.len())),
                update,
                format!("DELETE FROM {name} WHERE \"owner\" = ?1{}", key_match.concat()),
                format!("SELECT {} FROM {name} WHERE \"owner\" = ?1 ORDER BY {}", exprs.join(", "), key_names.join(", ")),
            )
        };
        let state_columns = table.fields.iter().filter(|field| field.state).flat_map(|field| field.columns.clone()).collect();
        let fields = match table.role {
            TableRole::Root | TableRole::Records => table.fields.iter().map(|field| FieldSql::render(table, field)).collect(),
            TableRole::Child | TableRole::History => Vec::new(),
        };
        let last_position = match (table.role, table.parent) {
            (TableRole::Records | TableRole::History, Some(parent)) => last_position(&set.tables[parent], table),
            _ => String::new(),
        };
        TableSql {
            name: table.name.clone(),
            role: table.role,
            parent: table.parent,
            field: table.field.clone(),
            state_columns,
            fields,
            last_position,
            key_len: table.key.len(),
            columns,
            insert,
            update,
            delete,
            select,
        }
    }
}

impl FieldSql {
    fn render(table: &Table, field: &FieldColumns) -> FieldSql {
        let name = quote_ident(&table.name);
        let columns = &table.columns[field.columns.clone()];
        let update = if table.parent.is_none() {
            let set = assignments(columns, 2);
            let next = 2 + columns.len();
            format!("UPDATE {name} SET \"version\" = ?1, {} WHERE \"id\" = ?{next} AND \"version\" = ?{}", set.join(", "), next + 1)
        } else {
            let set = assignments(columns, 1);
            let owner = 1 + columns.len();
            let key: Vec<String> =
                table.key.iter().enumerate().map(|(n, key)| format!(" AND {} = ?{}", quote_ident(&key.name), owner + 1 + n)).collect();
            format!("UPDATE {name} SET {} WHERE \"owner\" = ?{owner}{}", set.join(", "), key.concat())
        };
        FieldSql { name: field.name.clone(), columns: field.columns.clone(), update }
    }
}

/// The last position of `table` under one row of `parent`, read backwards through the primary
/// key's range so it is one index probe however long the table is. The outer select by the
/// parent's primary key returns no row when the parent row is missing.
fn last_position(parent: &Table, table: &Table) -> String {
    let inner_key: Vec<String> =
        table.parent_key().iter().enumerate().map(|(n, key)| format!(" AND {} = ?{}", quote_ident(&key.name), n + 2)).collect();
    let outer = if parent.parent.is_none() {
        "\"id\" = ?1".to_owned()
    } else {
        let key: Vec<String> =
            parent.key.iter().enumerate().map(|(n, key)| format!(" AND {} = ?{}", quote_ident(&key.name), n + 2)).collect();
        format!("\"owner\" = ?1{}", key.concat())
    };
    format!(
        "SELECT (SELECT \"pos\" FROM {} WHERE \"owner\" = ?1{} ORDER BY \"pos\" DESC LIMIT 1) FROM {} WHERE {outer}",
        quote_ident(&table.name),
        inner_key.concat(),
        quote_ident(&parent.name)
    )
}

fn prefixed(items: &[String]) -> String {
    items.iter().map(|item| format!(", {item}")).collect()
}

fn prefixed_placeholders(from: usize, count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(", {}", placeholders(from, count))
    }
}

#[cfg(test)]
mod tests {
    use flotilla_schema::{render_ddl, ColumnType, KindCode, Layout, TableRole};
    use rusqlite::Connection;

    use super::KindSql;
    use crate::shared::SHARED_DDL;

    // Behaviour: an interned column named like a `strings` column (`s`, `id`) still reads its
    // own row's string; unqualified, the subquery would compare `strings` with itself.
    #[test]
    fn interned_columns_named_like_strings_columns_read_their_own_string() {
        let mut layout = Layout::root("Note", KindCode(1));
        layout.column("s", ColumnType::Interned, true);
        layout.column("id_text", ColumnType::Text, true);
        let set = layout.finish();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(&format!("{SHARED_DDL}{}", render_ddl(&set))).expect("ddl");
        conn.execute_batch(
            "INSERT INTO strings (id, s) VALUES (1, 'first'), (2, 'second'), (3, 'n'), (4, 'h');
             INSERT INTO resource_identity (id, kind, name, home) VALUES (2, 1, 3, 4);
             INSERT INTO note (id, version, s, id_text) VALUES (2, 1, 1, 'x');",
        )
        .expect("rows");
        let select = &KindSql::render(&set).tables[0].select;
        let s: String = conn.query_row(select, [2], |row| row.get(1)).expect("select");
        assert_eq!(s, "first");
    }

    // Glue: the rendered statements name rows by primary key and bind parameters in the order the
    // runtime passes them (data values, then owner and key).
    #[test]
    fn statements_address_rows_by_primary_key() {
        let mut layout = Layout::root("Convoy", KindCode(2));
        layout.column("name", ColumnType::Interned, true);
        layout.table("steps", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("label", ColumnType::Text, true);
        });
        let sql = KindSql::render(&layout.finish());
        let root = &sql.tables[0];
        assert_eq!(root.insert, "INSERT INTO \"convoy\" (\"id\", \"version\", \"name\") VALUES (?1, ?2, ?3)");
        assert_eq!(root.update, "UPDATE \"convoy\" SET \"version\" = ?1, \"name\" = ?2 WHERE \"id\" = ?3 AND \"version\" = ?4");
        assert_eq!(
            root.select,
            "SELECT \"version\", (SELECT \"strings\".\"s\" FROM \"strings\" WHERE \"strings\".\"id\" = \"convoy\".\"name\") \
             FROM \"convoy\" WHERE \"id\" = ?1"
        );
        let steps = &sql.tables[1];
        assert_eq!(steps.insert, "INSERT INTO \"convoy_steps\" (\"owner\", \"pos\", \"label\") VALUES (?1, ?2, ?3)");
        assert_eq!(steps.update, "UPDATE \"convoy_steps\" SET \"label\" = ?1 WHERE \"owner\" = ?2 AND \"pos\" = ?3");
        assert_eq!(steps.delete, "DELETE FROM \"convoy_steps\" WHERE \"owner\" = ?1 AND \"pos\" = ?2");
        assert_eq!(
            steps.select,
            "SELECT \"convoy_steps\".\"pos\", \"convoy_steps\".\"label\" FROM \"convoy_steps\" WHERE \"owner\" = ?1 ORDER BY \"pos\""
        );
    }

    // Glue: an edge's statements: a root field moves the version under the optimistic check and
    // writes only its columns; a records field addresses one row by owner and key; the last
    // position reads one parent row's range backwards, inside a select of the parent row.
    #[test]
    fn edge_statements_write_one_field_and_read_the_last_position() {
        let mut layout = Layout::root("Convoy", KindCode(2));
        layout.field("label", false, |layout| layout.column("label", ColumnType::Text, true));
        layout.field("state", true, |layout| {
            layout.column("state", ColumnType::Code(vec![1, 2]), true);
            layout.column("state_since", ColumnType::Timestamp, true);
        });
        layout.table("deliverables", TableRole::Records, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("version", ColumnType::Integer, true);
            layout.field("state", true, |layout| layout.column("state", ColumnType::Code(vec![1, 2]), true));
            layout.table("evidence", TableRole::Records, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column("version", ColumnType::Integer, true);
            });
        });
        let sql = KindSql::render(&layout.finish());
        let (root, deliverables, evidence) = (&sql.tables[0], &sql.tables[1], &sql.tables[2]);
        assert_eq!(root.state_columns, [1, 2]);
        assert_eq!(
            root.fields[1].update,
            "UPDATE \"convoy\" SET \"version\" = ?1, \"state\" = ?2, \"state_since\" = ?3 WHERE \"id\" = ?4 AND \"version\" = ?5"
        );
        assert_eq!(
            deliverables.fields[0].update,
            "UPDATE \"convoy_deliverables\" SET \"state\" = ?1 WHERE \"owner\" = ?2 AND \"pos\" = ?3"
        );
        assert_eq!(
            deliverables.last_position,
            "SELECT (SELECT \"pos\" FROM \"convoy_deliverables\" WHERE \"owner\" = ?1 ORDER BY \"pos\" DESC LIMIT 1) \
             FROM \"convoy\" WHERE \"id\" = ?1"
        );
        assert_eq!(
            evidence.last_position,
            "SELECT (SELECT \"pos\" FROM \"convoy_deliverables_evidence\" WHERE \"owner\" = ?1 AND \"deliverables_pos\" = ?2 \
             ORDER BY \"pos\" DESC LIMIT 1) FROM \"convoy_deliverables\" WHERE \"owner\" = ?1 AND \"pos\" = ?2"
        );
        assert!(root.last_position.is_empty() && evidence.fields.is_empty());
    }
}
