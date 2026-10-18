//! Per-kind statements, rendered once from the descriptor when the store opens. Every statement
//! addresses rows by primary key: the root by `id`, child rows by `owner` plus their key.

use std::{collections::BTreeMap, ops::Range, sync::Arc};

use flotilla_schema::{quote_ident, Code, ColumnDef, ColumnType, FieldColumns, Index, Table, TableRole, TableSet};

pub(crate) struct TableSql {
    pub name: String,
    pub role: TableRole,
    pub parent: Option<usize>,
    /// The field this table stores (empty for the root): how an edge names it.
    pub field: String,
    /// Key columns (excluding the owner), then data columns.
    pub columns: Arc<[ColumnDef]>,
    pub key_len: usize,
    /// How many trailing key columns are this table's own key.
    pub own_key: usize,
    /// Earlier names of `field`, which log entries may use.
    pub field_aliases: Vec<String>,
    /// Earlier column names, old to current, which log entries may use.
    pub aliases: BTreeMap<String, String>,
    pub insert: String,
    /// A `child` or state-data table: the row inserted, or its values replaced, by key. Empty for
    /// other tables.
    pub upsert: String,
    pub delete: String,
    /// Child tables: every row of one owner. The root: the row of one id, version first.
    pub select: String,
    /// Indexes into a row's data values of `state` columns, which only an edge moves.
    pub state_columns: Vec<usize>,
    /// The root's and `records` tables' column-shaped fields, each with the statement an edge
    /// sets it with: the root's also moves the version (`?1`) under the optimistic check.
    pub fields: Vec<FieldSql>,
    /// Each label-plus-data state's label field and data fields, which one statement sets.
    pub groups: Vec<GroupSql>,
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
    /// The label-plus-data state the field belongs to (its label or a datum): an index into
    /// [`TableSql::groups`].
    pub group: Option<usize>,
    /// A state's label field: a `state` field that is no datum.
    pub label: bool,
    /// Earlier names of the field, which log entries may use.
    pub aliases: Vec<String>,
}

/// A label-plus-data state's fields (the label field, then its column datums in declaration
/// order, as indexes into [`TableSql::fields`]) and the one statement that sets any of them.
///
/// The per-label CHECK holds after every statement, so a label change and the data it brings or
/// leaves behind are written together. Each field has a flag parameter, then one parameter per
/// column; a column whose field's flag is 0 keeps its value (`CASE WHEN ?flag THEN ?value ELSE
/// "column" END`), so the statement writes exactly the fields an edge names. On the root, `?1` is
/// the new version and the last two parameters the id and the expected version; on a `records`
/// row, the last parameters are the owner and the row's key.
pub(crate) struct GroupSql {
    pub fields: Vec<usize>,
    pub update: String,
}

pub(crate) struct KindSql {
    pub tables: Vec<TableSql>,
    pub root_update_version: String,
    pub root_delete: String,
    pub root_version: String,
    /// Per root state field (by its label field's name): the version and the stored label, which
    /// an edge reads inside its write to check the stamp and the event's `from`.
    pub labels: BTreeMap<String, String>,
    /// Declared indexes by declared name.
    pub indexes: BTreeMap<String, IndexSql>,
    /// The startup scan: this kind's resources homed at `?1`, by id, through
    /// `resource_identity_by_home`. Identities without a root row (references to resources not
    /// stored here, deleted resources) are skipped by the primary-key join.
    pub scan: String,
    /// The kind's edge codes (`#[store(edges = E)]`): the only `edge` codes its entries carry.
    pub edges: Vec<Code>,
    /// The root state an edge moves, and its label codes: what an entry's `from` and `to` name.
    pub edge_state: Option<(String, Vec<Code>)>,
}

/// One index's page queries. Both select `id` then the order columns, filter by the index's own
/// predicate (rendered as the DDL renders it, so SQLite can use the partial index), bind the `eq`
/// values as `?1..`, and order by `order..., id`.
pub(crate) struct IndexSql {
    /// The schema object name, `<table>_<name>`.
    pub object: String,
    pub order: usize,
    pub eq_columns: Arc<[ColumnDef]>,
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
        ColumnType::Interned(_) => format!("(SELECT \"strings\".\"s\" FROM \"strings\" WHERE \"strings\".\"id\" = {name})"),
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
            labels: set.tables[0]
                .fields
                .iter()
                .filter(|field| field.is_label())
                .map(|field| {
                    let label = quote_ident(&set.tables[0].columns[field.columns.start].name);
                    (field.name.clone(), format!("SELECT \"version\", {label} FROM {root} WHERE \"id\" = ?1"))
                })
                .collect(),
            // Root indexes page resources; an index on a nested table (an edge log's) serves history
            // reads, which have no query API yet.
            indexes: set
                .indexes
                .iter()
                .filter(|index| index.table == set.tables[0].name)
                .map(|index| (index.name.clone(), IndexSql::render(&set.tables[0], index)))
                .collect(),
            scan: format!(
                "SELECT i.\"id\" FROM \"resource_identity\" AS i CROSS JOIN {root} AS r ON r.\"id\" = i.\"id\" \
                 WHERE i.\"home\" = ?1 AND i.\"kind\" = ?2 AND i.\"id\" > ?3 ORDER BY i.\"id\" LIMIT ?4"
            ),
            edges: set.edges.clone(),
            edge_state: set.edge_state().map(|(field, codes)| (field.name.clone(), codes.to_vec())),
        }
    }

    /// Every statement of this kind, labelled, for the query-plan tests.
    pub(crate) fn statements(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for table in &self.tables {
            for (what, sql) in [("insert", &table.insert), ("upsert", &table.upsert), ("delete", &table.delete), ("select", &table.select)]
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
            for group in &table.groups {
                out.push((format!("{} set state {}", table.name, table.fields[group.fields[0]].name), group.update.clone()));
            }
            if !table.last_position.is_empty() {
                out.push((format!("{} last position", table.name), table.last_position.clone()));
            }
        }
        let root = &self.tables[0].name;
        out.push((format!("{root} update version"), self.root_update_version.clone()));
        out.push((format!("{root} delete"), self.root_delete.clone()));
        out.push((format!("{root} version"), self.root_version.clone()));
        for (field, sql) in &self.labels {
            out.push((format!("{root} label {field}"), sql.clone()));
        }
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
            eq_columns: index
                .eq
                .iter()
                .map(|name| root.columns.iter().find(|column| &column.name == name).expect("validated eq column").clone())
                .collect(),
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
        ColumnType::Text | ColumnType::Code(_) => "''",
        ColumnType::Blob | ColumnType::Frozen | ColumnType::Cbor => "x''",
        ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp | ColumnType::Interned(_) | ColumnType::Ref => {
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
        let (insert, upsert, delete, select) = if table.parent.is_none() {
            (
                format!(
                    "INSERT INTO {name} (\"id\", \"version\"{}) VALUES (?1, ?2{})",
                    prefixed(&names),
                    prefixed_placeholders(3, names.len())
                ),
                String::new(),
                String::new(),
                format!("SELECT \"version\"{} FROM {name} WHERE \"id\" = ?1", prefixed(&exprs)),
            )
        } else {
            let key_match: Vec<String> = key_names.iter().enumerate().map(|(n, key)| format!(" AND {key} = ?{}", n + 2)).collect();
            let insert = format!("INSERT INTO {name} (\"owner\"{}) VALUES (?1{})", prefixed(&names), prefixed_placeholders(2, names.len()));
            // A collection row is put by its key: inserted, or its values replaced in place, so the
            // rows nested in it stay.
            let upsert = match table.role {
                TableRole::Child | TableRole::State => {
                    let conflict = format!("ON CONFLICT (\"owner\"{}) DO", prefixed(key_names));
                    if data.is_empty() {
                        format!("{insert} {conflict} NOTHING")
                    } else {
                        let set: Vec<String> = data
                            .iter()
                            .map(|column| {
                                let column = quote_ident(&column.name);
                                format!("{column} = excluded.{column}")
                            })
                            .collect();
                        format!("{insert} {conflict} UPDATE SET {}", set.join(", "))
                    }
                }
                TableRole::Root | TableRole::Records | TableRole::History => String::new(),
            };
            (
                insert,
                upsert,
                format!("DELETE FROM {name} WHERE \"owner\" = ?1{}", key_match.concat()),
                format!("SELECT {} FROM {name} WHERE \"owner\" = ?1 ORDER BY {}", exprs.join(", "), key_names.join(", ")),
            )
        };
        let state_columns = table.fields.iter().filter(|field| field.state).flat_map(|field| field.columns.clone()).collect();
        let (mut fields, groups): (Vec<FieldSql>, Vec<GroupSql>) = match table.role {
            TableRole::Root | TableRole::Records => {
                (table.fields.iter().map(|field| FieldSql::render(table, field)).collect(), GroupSql::render_all(table))
            }
            TableRole::Child | TableRole::History | TableRole::State => (Vec::new(), Vec::new()),
        };
        for (index, group) in groups.iter().enumerate() {
            for &field in &group.fields {
                fields[field].group = Some(index);
            }
        }
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
            groups,
            last_position,
            key_len: table.key.len(),
            own_key: table.own_key,
            field_aliases: table.field_aliases.clone(),
            aliases: table.aliases.iter().cloned().collect(),
            columns: columns.into(),
            insert,
            upsert,
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
        FieldSql {
            name: field.name.clone(),
            columns: field.columns.clone(),
            update,
            group: None,
            label: field.is_label(),
            aliases: field.aliases.clone(),
        }
    }
}

impl GroupSql {
    /// One group per label field that has column datums.
    fn render_all(table: &Table) -> Vec<GroupSql> {
        table
            .fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.is_label())
            .filter_map(|(label, field)| {
                let datums = table.fields.iter().enumerate().filter(|(_, datum)| datum.datum_of.as_deref() == Some(field.name.as_str()));
                let mut members = vec![label];
                members.extend(datums.map(|(index, _)| index));
                (members.len() > 1).then(|| GroupSql::render(table, members))
            })
            .collect()
    }

    fn render(table: &Table, fields: Vec<usize>) -> GroupSql {
        let name = quote_ident(&table.name);
        let root = table.parent.is_none();
        let mut next = if root { 2 } else { 1 };
        let mut sets = Vec::new();
        for &field in &fields {
            let flag = next;
            next += 1;
            for column in &table.columns[table.fields[field].columns.clone()] {
                let column = quote_ident(&column.name);
                sets.push(format!("{column} = CASE WHEN ?{flag} THEN ?{next} ELSE {column} END"));
                next += 1;
            }
        }
        let update = if root {
            format!("UPDATE {name} SET \"version\" = ?1, {} WHERE \"id\" = ?{next} AND \"version\" = ?{}", sets.join(", "), next + 1)
        } else {
            let key: Vec<String> =
                table.key.iter().enumerate().map(|(n, key)| format!(" AND {} = ?{}", quote_ident(&key.name), next + 1 + n)).collect();
            format!("UPDATE {name} SET {} WHERE \"owner\" = ?{next}{}", sets.join(", "), key.concat())
        };
        GroupSql { fields, update }
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
    use flotilla_schema::{render_ddl, Code, ColumnType, KindCode, Layout, TableRole, CODES_DDL};
    use rusqlite::Connection;

    use super::KindSql;
    use crate::shared::SHARED_DDL;

    // Behaviour: an interned column named like a `strings` column (`s`, `id`) still reads its
    // own row's string; unqualified, the subquery would compare `strings` with itself.
    #[test]
    fn interned_columns_named_like_strings_columns_read_their_own_string() {
        let mut layout = Layout::root("Note", KindCode::new("k1"));
        layout.column("s", ColumnType::Interned(Code::new("tx")), true);
        layout.column("id_text", ColumnType::Text, true);
        let set = layout.finish();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(&format!("{SHARED_DDL}{CODES_DDL}{}", render_ddl(&set))).expect("ddl");
        conn.execute_batch(
            "INSERT INTO strings (id, domain, s) VALUES (1, 'tx', 'first'), (2, 'tx', 'second'), (3, 'rn', 'n'), (4, 'hm', 'h');
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
        let mut layout = Layout::root("Convoy", KindCode::new("k2"));
        layout.column("name", ColumnType::Interned(Code::new("tx")), true);
        layout.table("steps", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("label", ColumnType::Text, true);
        });
        let sql = KindSql::render(&layout.finish());
        let root = &sql.tables[0];
        assert_eq!(root.insert, "INSERT INTO \"convoy\" (\"id\", \"version\", \"name\") VALUES (?1, ?2, ?3)");
        assert!(root.upsert.is_empty(), "the root row changes field by field");
        assert_eq!(
            root.select,
            "SELECT \"version\", (SELECT \"strings\".\"s\" FROM \"strings\" WHERE \"strings\".\"id\" = \"convoy\".\"name\") \
             FROM \"convoy\" WHERE \"id\" = ?1"
        );
        let steps = &sql.tables[1];
        assert_eq!(steps.insert, "INSERT INTO \"convoy_steps\" (\"owner\", \"pos\", \"label\") VALUES (?1, ?2, ?3)");
        assert_eq!(
            steps.upsert,
            "INSERT INTO \"convoy_steps\" (\"owner\", \"pos\", \"label\") VALUES (?1, ?2, ?3) \
             ON CONFLICT (\"owner\", \"pos\") DO UPDATE SET \"label\" = excluded.\"label\""
        );
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
        let mut layout = Layout::root("Convoy", KindCode::new("k2"));
        layout.field("label", false, |layout| layout.column("label", ColumnType::Text, true));
        layout.field("state", true, |layout| {
            layout.column("state", ColumnType::Code(vec![Code::new("c1"), Code::new("c2")]), true);
            layout.column("state_since", ColumnType::Timestamp, true);
        });
        layout.table("deliverables", TableRole::Records, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("version", ColumnType::Integer, true);
            layout.field("state", true, |layout| layout.column("state", ColumnType::Code(vec![Code::new("c1"), Code::new("c2")]), true));
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
