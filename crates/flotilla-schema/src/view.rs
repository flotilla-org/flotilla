//! A readable view of generated tables, for schema review before a table shape is built on
//! (CLAUDE.md, "schema review before build"): each table's width, the columns that expanding data
//! enums adds, its child tables and its indexes, with tables wider than [`WIDE`] called out.

use std::{collections::BTreeSet, fmt::Write};

use crate::descriptor::{ColumnType, Table, TableRole, TableSet, RESERVED_CHILD_COLUMNS, RESERVED_ROOT_COLUMNS};

/// A table wider than this many columns is called out for review.
pub const WIDE: usize = 30;

/// The columns a table has in the backend: the root's `id` and `version`, or a nested table's
/// `owner` and key, plus the descriptor's columns (a records or history table's stamp included).
pub fn column_count(table: &Table) -> usize {
    match table.role {
        TableRole::Root => RESERVED_ROOT_COLUMNS.len() + table.columns.len(),
        _ => RESERVED_CHILD_COLUMNS.len() + table.key.len() + table.columns.len(),
    }
}

/// Columns that exist because a data enum was expanded into its row, by the enum's tag column: the
/// tag and every column its variants declare, nested enums and flattened options included. Only
/// outermost enums are listed; a nested enum's columns count toward the enum holding it.
pub fn enum_columns(table: &Table) -> Vec<(&str, usize)> {
    let tag = |name: &str| table.columns.iter().any(|column| column.name == name && matches!(column.ty, ColumnType::Code(_)));
    // A guard's controlling column is a data enum's tag (a code) or an optional flatten's flag.
    let tags: BTreeSet<&str> = table.guards.iter().map(|guard| guard.column.as_str()).filter(|column| tag(column)).collect();
    let mut groups = Vec::new();
    let mut nested = BTreeSet::new();
    for &root in &tags {
        let mut members: BTreeSet<&str> = BTreeSet::new();
        let mut frontier = vec![root];
        while let Some(column) = frontier.pop() {
            for guard in table.guards.iter().filter(|guard| guard.column == column) {
                for name in &guard.scope {
                    if members.insert(name.as_str()) {
                        frontier.push(name.as_str());
                    }
                }
            }
        }
        nested.extend(members.iter().copied().filter(|member| tags.contains(member)));
        groups.push((root, 1 + members.len()));
    }
    groups.retain(|(root, _)| !nested.contains(root));
    groups
}

/// The view of `sets` as Markdown: a summary table of every table, then the wide tables.
pub fn render_view<'a>(sets: impl IntoIterator<Item = &'a TableSet>) -> String {
    let mut out = String::from("| Table | Role | Columns | From enum expansion | Child tables | Indexes |\n|---|---|--:|---|---|---|\n");
    let mut wide = Vec::new();
    for set in sets {
        for (position, table) in set.tables.iter().enumerate() {
            let count = column_count(table);
            let enums = enum_columns(table);
            let expanded: usize = enums.iter().map(|(_, count)| count).sum();
            let enums_text = if enums.is_empty() {
                "–".to_owned()
            } else {
                let parts: Vec<String> = enums.iter().map(|(tag, count)| format!("`{tag}` {count}")).collect();
                format!("{expanded}: {}", parts.join(", "))
            };
            let children: Vec<String> = set
                .tables
                .iter()
                .filter(|child| child.parent == Some(position))
                .map(|child| format!("`{}` ({})", child.name, role(child.role)))
                .collect();
            let indexes: Vec<String> = set
                .indexes
                .iter()
                .filter(|index| index.table == table.name)
                .map(|index| format!("`{}` ({})", index.name, index.columns().collect::<Vec<_>>().join(", ")))
                .collect();
            let flag = if count > WIDE { " **wide**" } else { "" };
            let _ = writeln!(
                out,
                "| `{}` | {} | {count}{flag} | {enums_text} | {} | {} |",
                table.name,
                role(table.role),
                list(&children),
                list(&indexes)
            );
            if count > WIDE {
                wide.push(format!("- `{}`: {count} columns, {expanded} from enum expansion", table.name));
            }
        }
    }
    out.push_str(&format!("\nTables wider than {WIDE} columns:\n"));
    if wide.is_empty() {
        out.push_str("- none\n");
    } else {
        out.push_str(&wide.join("\n"));
        out.push('\n');
    }
    out
}

fn role(role: TableRole) -> &'static str {
    match role {
        TableRole::Root => "root",
        TableRole::Child => "child",
        TableRole::Records => "records",
        TableRole::History => "history",
    }
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "–".to_owned()
    } else {
        items.join(", ")
    }
}
