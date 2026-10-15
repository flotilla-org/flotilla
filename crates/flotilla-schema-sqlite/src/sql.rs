//! Per-kind statements, rendered once from the descriptor when the store opens. Every statement
//! addresses rows by primary key: the root by `id`, child rows by `owner` plus their key.

use flotilla_schema::{quote_ident, ColumnDef, ColumnType, TableRole, TableSet};

pub(crate) struct TableSql {
    pub name: String,
    pub role: TableRole,
    pub parent: Option<usize>,
    /// Key columns (excluding the owner), then data columns.
    pub columns: Vec<ColumnDef>,
    pub key_len: usize,
    pub insert: String,
    /// Empty when the table has no data columns (a set's rows are all key; a kind with no fields).
    pub update: String,
    pub delete: String,
    /// Child tables: every row of one owner. The root: the row of one id, version first.
    pub select: String,
}

pub(crate) struct KindSql {
    pub tables: Vec<TableSql>,
    pub root_update_version: String,
    pub root_delete: String,
    pub root_version: String,
}

/// Interned columns read back as their strings through `strings`' primary key.
fn select_expr(column: &ColumnDef) -> String {
    let name = quote_ident(&column.name);
    match column.ty {
        ColumnType::Interned => format!("(SELECT \"s\" FROM \"strings\" WHERE \"id\" = {name})"),
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
        let tables = set.tables.iter().map(TableSql::render).collect();
        let root = quote_ident(&set.tables[0].name);
        KindSql {
            tables,
            root_update_version: format!("UPDATE {root} SET \"version\" = ?1 WHERE \"id\" = ?2 AND \"version\" = ?3"),
            root_delete: format!("DELETE FROM {root} WHERE \"id\" = ?1 AND \"version\" = ?2"),
            root_version: format!("SELECT \"version\" FROM {root} WHERE \"id\" = ?1"),
        }
    }
}

impl TableSql {
    fn render(table: &flotilla_schema::Table) -> TableSql {
        let name = quote_ident(&table.name);
        let columns: Vec<ColumnDef> = table.key.iter().chain(&table.columns).cloned().collect();
        let names: Vec<String> = columns.iter().map(|column| quote_ident(&column.name)).collect();
        let exprs: Vec<String> = columns.iter().map(select_expr).collect();
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
        TableSql {
            name: table.name.clone(),
            role: table.role,
            parent: table.parent,
            key_len: table.key.len(),
            columns,
            insert,
            update,
            delete,
            select,
        }
    }
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
    use flotilla_schema::{ColumnType, KindCode, Layout, TableRole};

    use super::KindSql;

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
            "SELECT \"version\", (SELECT \"s\" FROM \"strings\" WHERE \"id\" = \"name\") FROM \"convoy\" WHERE \"id\" = ?1"
        );
        let steps = &sql.tables[1];
        assert_eq!(steps.insert, "INSERT INTO \"convoy_steps\" (\"owner\", \"pos\", \"label\") VALUES (?1, ?2, ?3)");
        assert_eq!(steps.update, "UPDATE \"convoy_steps\" SET \"label\" = ?1 WHERE \"owner\" = ?2 AND \"pos\" = ?3");
        assert_eq!(steps.delete, "DELETE FROM \"convoy_steps\" WHERE \"owner\" = ?1 AND \"pos\" = ?2");
        assert_eq!(steps.select, "SELECT \"pos\", \"label\" FROM \"convoy_steps\" WHERE \"owner\" = ?1 ORDER BY \"pos\"");
    }
}
