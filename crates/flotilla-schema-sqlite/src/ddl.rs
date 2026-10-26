//! Deterministic DDL rendering: tables in pre-order, then indexes sorted by object name, then the
//! kind's rows of the shared `codes` table and one readable view per table (#3392 ruling D4).
//!
//! Identifiers come from Rust field names (never serde names) and are always quoted, so a field
//! named like an SQL keyword still renders valid DDL.
//!
//! Stored codes are short (G7), so a table is read through its view: `<table>_view` has the
//! table's columns in order, with each code column replaced by its variant's Rust name from
//! `codes` and each interned column by its text from `strings`. A store opening an existing
//! database upserts the kind's `codes` rows and deletes any others, and recreates only the views
//! whose statement changed ([`render_views`]), so an unchanged schema changes nothing.

use std::fmt::Write as _;

use flotilla_schema::Code;
use flotilla_schema::{ColumnDef, ColumnType, Condition, Guard, Index, Table, TableRole, TableSet, When};

/// The shared table naming every code: one row per (kind, field, code), `name` the Rust name of
/// the variant carrying it. `kind` is the kind code and `field` the code column: its name on the
/// root table, `<table>.<column>` on a nested table, the table named without the root's prefix.
pub const CODES_DDL: &str = "CREATE TABLE IF NOT EXISTS codes (
  kind TEXT NOT NULL, field TEXT NOT NULL, code TEXT NOT NULL, name TEXT NOT NULL,
  PRIMARY KEY (kind, field, code)) STRICT, WITHOUT ROWID;
";

/// The `codes` field naming a kind's edge codes, the `edge` of its `log` entries (#3392 D4). A
/// table's own fields are Rust identifiers, optionally prefixed by a table and `.`, so a field
/// with `:` never collides with one.
pub const LOG_EDGE_FIELD: &str = "log:edge";

/// The `codes` field naming a kind's state labels, the `from` and `to` of its `log` entries.
pub const LOG_LABEL_FIELD: &str = "log:label";

/// The suffix of a table's readable view: `<table>_view`.
pub use flotilla_schema::VIEW_SUFFIX;

/// Whether statements may meet objects that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlMode {
    /// Plain `CREATE`: the snapshot form.
    Create,
    /// `CREATE ... IF NOT EXISTS`: opening an existing database is idempotent.
    IfNotExists,
}

/// Quotes an SQL identifier.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn render_ddl(set: &TableSet) -> String {
    render_ddl_with(set, DdlMode::Create)
}

pub fn render_ddl_with(set: &TableSet, mode: DdlMode) -> String {
    let exists = match mode {
        DdlMode::Create => "",
        DdlMode::IfNotExists => "IF NOT EXISTS ",
    };
    let mut out = String::new();
    for table in &set.tables {
        render_table(&mut out, set, table, exists);
    }
    let mut indexes: Vec<_> = set.indexes.iter().cloned().chain(set.reference_indexes()).collect();
    indexes.sort_by_key(|index| index.object_name());
    for index in &indexes {
        let columns: Vec<String> = set.index_key(index).iter().map(|column| quote_ident(column)).collect();
        let name = quote_ident(&index.object_name());
        let unique = if index.unique { "UNIQUE " } else { "" };
        let _ = write!(out, "CREATE {unique}INDEX {exists}{name} ON {} ({})", quote_ident(&index.table), columns.join(", "));
        if let Some(predicate) = index.predicate_sql() {
            let _ = write!(out, " WHERE {predicate}");
        }
        out.push_str(";\n");
    }
    render_codes(&mut out, set, mode);
    if mode == DdlMode::Create {
        for (_, view) in render_views(set) {
            let _ = writeln!(out, "{view};");
        }
    }
    out
}

/// The `codes` field of column `column` of `table`; see [`CODES_DDL`]. Column names are Rust
/// identifiers and hold no `.`, so a root column never collides with a nested `<table>.<column>`.
pub fn code_field(set: &TableSet, table: &Table, column: &str) -> String {
    if table.parent.is_none() {
        return column.to_owned();
    }
    let root = &set.tables[0].name;
    let local = table.name.strip_prefix(root.as_str()).and_then(|rest| rest.strip_prefix('_')).unwrap_or(&table.name);
    format!("{local}.{column}")
}

/// A text value as an SQL string literal.
fn text_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The kind's rows of `codes`: its tables' code columns, then, for a kind declaring edges, its
/// edge codes and its root state's labels under [`LOG_EDGE_FIELD`] and [`LOG_LABEL_FIELD`], which
/// the store's readable `log_view` names entries by. Opening an existing database upserts them and
/// deletes the kind's other rows, writing nothing when they are unchanged.
fn render_codes(out: &mut String, set: &TableSet, mode: DdlMode) {
    let kind = text_literal(set.code.as_str());
    let mut rows: Vec<(String, String, String)> = set
        .tables
        .iter()
        .flat_map(|table| {
            // Inherited keys retain their meaning in the descriptor. Existing SQLite views
            // name only a table's own enums, preserving the established DDL exactly.
            table
                .key
                .iter()
                .skip(table.parent_key().len())
                .chain(&table.columns)
                .filter(|column| !column.categories.is_empty())
                .map(move |column| (table, column))
        })
        .flat_map(|(table, codes)| {
            let field = text_literal(&code_field(set, table, &codes.name));
            codes.categories.iter().map(move |(code, name)| (field.clone(), text_literal(code), text_literal(name)))
        })
        .collect();
    let edge = text_literal(LOG_EDGE_FIELD);
    rows.extend(set.edges.iter().zip(&set.edge_names).map(|(code, name)| (edge.clone(), code_literal(*code), text_literal(name))));
    if !set.edges.is_empty() {
        let root = &set.tables[0];
        let labels = set.edge_state().and_then(|(field, _)| {
            let column = &root.columns[field.columns.start].name;
            root.columns.iter().find(|codes| &codes.name == column)
        });
        let label = text_literal(LOG_LABEL_FIELD);
        rows.extend(
            labels
                .into_iter()
                .flat_map(|codes| &codes.categories)
                .map(|(code, name)| (label.clone(), text_literal(code), text_literal(name))),
        );
    }
    if !rows.is_empty() {
        let values: Vec<String> = rows.iter().map(|(field, code, name)| format!("({kind}, {field}, {code}, {name})")).collect();
        let _ = write!(out, "INSERT INTO \"codes\" (\"kind\", \"field\", \"code\", \"name\") VALUES\n  {}", values.join(",\n  "));
        if mode == DdlMode::IfNotExists {
            out.push_str("\nON CONFLICT (\"kind\", \"field\", \"code\") DO UPDATE SET \"name\" = excluded.\"name\" WHERE \"name\" IS NOT excluded.\"name\"");
        }
        out.push_str(";\n");
    }
    if mode == DdlMode::IfNotExists {
        let _ = write!(out, "DELETE FROM \"codes\" WHERE \"kind\" = {kind}");
        if !rows.is_empty() {
            let keys: Vec<String> = rows.iter().map(|(field, code, _)| format!("({field}, {code})")).collect();
            let _ = write!(out, " AND (\"field\", \"code\") NOT IN (VALUES {})", keys.join(", "));
        }
        out.push_str(";\n");
    }
}

/// Each table's readable view, `<table>_view`, as (name, `CREATE VIEW` statement without its
/// `;`): the table's columns in order, code columns as their names and interned columns as their
/// text. `render_ddl` includes them; a store opening an existing database (the `IfNotExists`
/// form, which omits them) recreates only a view whose stored statement differs, so an
/// unchanged schema changes nothing on open.
pub fn render_views(set: &TableSet) -> Vec<(String, String)> {
    set.tables.iter().map(|table| readable(set, table)).collect()
}

fn readable(set: &TableSet, table: &Table) -> (String, String) {
    let name = format!("{}{VIEW_SUFFIX}", table.name);
    let source = quote_ident(&table.name);
    let mut lines: Vec<String> = match table.parent {
        None => ["id", "version"].iter().map(|column| format!("{source}.{}", quote_ident(column))).collect(),
        Some(_) => vec![format!("{source}.{}", quote_ident(set.parent_key_column()))],
    };
    for (position, column) in table.key.iter().chain(&table.columns).enumerate() {
        let value = format!("{source}.{}", quote_ident(&column.name));
        let alias = quote_ident(&column.name);
        let named = position >= table.parent_key().len() && !column.categories.is_empty();
        lines.push(match column.ty {
            ColumnType::Code(_) if named => format!(
                "(SELECT \"name\" FROM \"codes\" WHERE \"kind\" = {} AND \"field\" = {} AND \"code\" = {value}) AS {alias}",
                text_literal(set.code.as_str()),
                text_literal(&code_field(set, table, &column.name)),
            ),
            ColumnType::Interned(_) => format!("(SELECT \"s\" FROM \"strings\" WHERE \"id\" = {value}) AS {alias}"),
            _ => value,
        });
    }
    let sql = format!("CREATE VIEW {} AS SELECT\n  {}\nFROM {source}", quote_ident(&name), lines.join(",\n  "));
    (name, sql)
}

fn render_table(out: &mut String, set: &TableSet, table: &Table, exists: &str) {
    let owner = quote_ident(set.parent_key_column());
    let mut lines = Vec::new();
    match table.parent {
        None => {
            lines.push(format!(
                "{} INTEGER PRIMARY KEY REFERENCES {} ({})",
                quote_ident("id"),
                quote_ident("resource_identity"),
                quote_ident("id")
            ));
            lines.push(format!("{} INTEGER NOT NULL", quote_ident("version")));
        }
        Some(_) => lines.push(format!("{owner} INTEGER NOT NULL")),
    }
    lines.extend(table.key.iter().chain(&table.columns).map(column));
    if let Some(parent_index) = table.parent {
        let parent = &set.tables[parent_index];
        let mut key = vec![owner.clone()];
        key.extend(table.key.iter().map(|c| quote_ident(&c.name)));
        lines.push(format!("PRIMARY KEY ({})", key.join(", ")));
        let foreign = if parent.role == TableRole::Root {
            format!("FOREIGN KEY ({owner}) REFERENCES {} ({})", quote_ident(&parent.name), quote_ident("id"))
        } else {
            let mut local = vec![owner.clone()];
            local.extend(table.parent_key().iter().map(|c| quote_ident(&c.name)));
            let mut remote = vec![owner.clone()];
            remote.extend(parent.key.iter().map(|c| quote_ident(&c.name)));
            format!("FOREIGN KEY ({}) REFERENCES {} ({})", local.join(", "), quote_ident(&parent.name), remote.join(", "))
        };
        lines.push(format!("{foreign} ON DELETE CASCADE"));
    }
    lines.extend(table.guards.iter().map(|guard| format!("CHECK ({})", guard.check_with("\n      "))));
    // Child tables cluster on their primary key, so loading one owner's rows is one range of one b-tree.
    let options = if table.parent.is_some() { "STRICT, WITHOUT ROWID" } else { "STRICT" };
    let _ = writeln!(out, "CREATE TABLE {exists}{} (\n  {}\n) {options};", quote_ident(&table.name), lines.join(",\n  "));
}

pub(crate) fn column(column: &ColumnDef) -> String {
    let name = quote_ident(&column.name);
    let sql_type = match column.ty {
        ColumnType::Text | ColumnType::Code(_) => "TEXT",
        ColumnType::Number => "REAL",
        ColumnType::Blob | ColumnType::Frozen | ColumnType::Cbor => "BLOB",
        _ => "INTEGER",
    };
    let mut out = format!("{name} {sql_type}");
    if !column.nullable {
        out.push_str(" NOT NULL");
    }
    match &column.ty {
        ColumnType::Interned(_) => out.push_str(" REFERENCES \"strings\" (\"id\")"),
        ColumnType::Ref => out.push_str(" REFERENCES \"resource_identity\" (\"id\")"),
        ColumnType::Frozen => out.push_str(" REFERENCES \"objects\" (\"hash\")"),
        ColumnType::Bool => {
            let _ = write!(out, " CHECK ({name} IN (0, 1))");
        }
        ColumnType::Code(codes) => {
            let codes: Vec<String> = codes.iter().map(|code| text_literal(code)).collect();
            let _ = write!(out, " CHECK ({name} IN ({}))", codes.join(", "));
        }
        ColumnType::Integer | ColumnType::Number | ColumnType::Text | ColumnType::Blob | ColumnType::Cbor | ColumnType::Timestamp => {}
    }
    if let Some(default) = column.previous_generation_default() {
        let _ = write!(out, " DEFAULT {default}");
    }
    out
}

/// SQLite rendering of a structured row guard.
pub trait GuardSql {
    fn check(&self) -> String;
    fn check_with(&self, separator: &str) -> String;
}
impl GuardSql for Guard {
    /// `CASE WHEN <cond> THEN <required> IS NOT NULL AND <absent> IS NULL [ELSE <otherwise> IS
    /// NULL] END`. A CASE without a match is null, which a CHECK accepts.
    fn check(&self) -> String {
        self.check_with(" ")
    }

    /// The CHECK with `separator` before `THEN`, `ELSE` and each term, for readable DDL.
    fn check_with(&self, separator: &str) -> String {
        let and = format!("{separator}AND ");
        let required = self.required.iter().map(|name| format!("{} IS NOT NULL", quote_ident(name)));
        let absent = self.absent.iter().map(|name| format!("{} IS NULL", quote_ident(name)));
        let then = conjunction(required.chain(absent), &and);
        let column = quote_ident(&self.column);
        let condition = match &self.when {
            When::Is(code) => format!("{column} = {}", text_literal(code)),
            When::Present => format!("{column} IS NOT NULL"),
        };
        let mut out = format!("CASE WHEN {condition}{separator}THEN {then}");
        if !self.otherwise.is_empty() {
            let otherwise = conjunction(self.otherwise.iter().map(|name| format!("{} IS NULL", quote_ident(name))), &and);
            out.push_str(&format!("{separator}ELSE {otherwise}"));
        }
        out.push_str(&format!("{separator}END"));
        out
    }
}

/// SQLite rendering of an index predicate term.
pub trait ConditionSql {
    fn sql(&self) -> String;
}
impl ConditionSql for Condition {
    fn sql(&self) -> String {
        match self {
            Condition::In { column, codes } => {
                let codes: Vec<String> = codes.iter().copied().map(code_literal).collect();
                format!("{} IN ({})", quote_ident(column), codes.join(", "))
            }
            Condition::NotNull { column } => format!("{} IS NOT NULL", quote_ident(column)),
        }
    }
}
/// SQLite rendering of an index predicate.
pub trait IndexPredicateSql {
    fn predicate_sql(&self) -> Option<String>;
}
impl IndexPredicateSql for Index {
    /// The predicate's terms joined by `AND`; `None` for a full index.
    fn predicate_sql(&self) -> Option<String> {
        if self.predicate.is_empty() {
            None
        } else {
            Some(self.predicate.iter().map(Condition::sql).collect::<Vec<_>>().join(" AND "))
        }
    }
}
/// A code as an SQL string literal. Codes hold only letters, digits and `_`, so need no escaping.
pub fn code_literal(code: Code) -> String {
    format!("'{code}'")
}

fn conjunction(terms: impl Iterator<Item = String>, and: &str) -> String {
    let terms: Vec<String> = terms.collect();
    if terms.is_empty() {
        "1".to_owned()
    } else {
        terms.join(and)
    }
}

#[cfg(test)]
mod tests {
    use super::{render_ddl, render_ddl_with, DdlMode};
    use flotilla_schema::{Code, ColumnType, Condition, DescriptorError, Index, IndexProblem, KindCode, Layout, TableRole, RECORD_VERSION};

    fn text_codes(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    fn codes(codes: &[&str]) -> Vec<Code> {
        codes.iter().map(|code| Code::new(code)).collect()
    }

    fn convoy() -> Layout {
        let mut layout = Layout::root("Convoy", KindCode::new("k2"));
        layout.column("name", ColumnType::Interned(Code::new("tx")), true);
        layout.column("done", ColumnType::Bool, true);
        layout.code("phase", &codes(&["dr", "op", "sh"]), &["Draft", "Open", "Shipped"], false);
        layout.column("workflow", ColumnType::Frozen, true);
        layout.table("deliverables", TableRole::Records, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column(RECORD_VERSION, ColumnType::Integer, true);
            layout.column("target", ColumnType::Ref, true);
            layout.table("evidence", TableRole::History, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column(RECORD_VERSION, ColumnType::Integer, true);
                layout.column("", ColumnType::Blob, true);
            });
        });
        layout.index(Index {
            name: "open".into(),
            table: "convoy".into(),
            eq: vec!["name".into()],
            order: vec![],
            predicate: vec![
                Condition::In { column: "phase".into(), codes: codes(&["dr", "op"]) },
                Condition::NotNull { column: "phase".into() },
            ],
            unique: false,
        });
        layout.index(Index {
            name: "by_name".into(),
            table: "convoy".into(),
            eq: vec!["name".into()],
            order: vec![],
            predicate: vec![],
            unique: false,
        });
        layout
    }

    // Behaviour (DESIGN DDL): STRICT tables in pre-order; the root keyed by its identity id; each
    // child keyed by its parent key (named after the root) plus its logical key, with a cascading FK to its parent's full key;
    // per-type REFERENCES and CHECKs; indexes, declared and one per child-table reference column,
    // sorted by name after the tables. Then (#3392 D4)
    // the kind's `codes` rows, and one view per table naming its codes and interned strings.
    #[test]
    fn renders_tables_keys_and_indexes() {
        let expected = r#"CREATE TABLE "convoy" (
  "id" INTEGER PRIMARY KEY REFERENCES "resource_identity" ("id"),
  "version" INTEGER NOT NULL,
  "name" INTEGER NOT NULL REFERENCES "strings" ("id"),
  "done" INTEGER NOT NULL CHECK ("done" IN (0, 1)),
  "phase" TEXT CHECK ("phase" IN ('dr', 'op', 'sh')),
  "workflow" BLOB NOT NULL REFERENCES "objects" ("hash")
) STRICT;
CREATE TABLE "convoy_deliverables" (
  "convoy" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "target" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  PRIMARY KEY ("convoy", "pos"),
  FOREIGN KEY ("convoy") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_deliverables_evidence" (
  "convoy" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "value" BLOB NOT NULL,
  PRIMARY KEY ("convoy", "deliverables_pos", "pos"),
  FOREIGN KEY ("convoy", "deliverables_pos") REFERENCES "convoy_deliverables" ("convoy", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "convoy_by_name" ON "convoy" ("name");
CREATE INDEX "convoy_deliverables_target_ref" ON "convoy_deliverables" ("target");
CREATE INDEX "convoy_open" ON "convoy" ("name") WHERE "phase" IN ('dr', 'op') AND "phase" IS NOT NULL;
INSERT INTO "codes" ("kind", "field", "code", "name") VALUES
  ('k2', 'phase', 'dr', 'Draft'),
  ('k2', 'phase', 'op', 'Open'),
  ('k2', 'phase', 'sh', 'Shipped');
CREATE VIEW "convoy_view" AS SELECT
  "convoy"."id",
  "convoy"."version",
  (SELECT "s" FROM "strings" WHERE "id" = "convoy"."name") AS "name",
  "convoy"."done",
  (SELECT "name" FROM "codes" WHERE "kind" = 'k2' AND "field" = 'phase' AND "code" = "convoy"."phase") AS "phase",
  "convoy"."workflow"
FROM "convoy";
CREATE VIEW "convoy_deliverables_view" AS SELECT
  "convoy_deliverables"."convoy",
  "convoy_deliverables"."pos",
  "convoy_deliverables"."version",
  "convoy_deliverables"."target"
FROM "convoy_deliverables";
CREATE VIEW "convoy_deliverables_evidence_view" AS SELECT
  "convoy_deliverables_evidence"."convoy",
  "convoy_deliverables_evidence"."deliverables_pos",
  "convoy_deliverables_evidence"."pos",
  "convoy_deliverables_evidence"."version",
  "convoy_deliverables_evidence"."value"
FROM "convoy_deliverables_evidence";
"#;
        let set = convoy().finish();
        set.validate().expect("valid descriptor");
        assert_eq!(render_ddl(&set), expected);
        assert_eq!(render_ddl(&set), render_ddl(&convoy().finish()), "rendering is deterministic");
    }

    #[test]
    fn idempotent_mode_guards_every_statement() {
        let ddl = render_ddl_with(&convoy().finish(), DdlMode::IfNotExists);
        assert_eq!(ddl.matches("CREATE TABLE IF NOT EXISTS ").count(), 3);
        assert_eq!(ddl.matches("CREATE INDEX IF NOT EXISTS ").count(), 3);
        assert!(ddl.contains("ON CONFLICT (\"kind\", \"field\", \"code\") DO UPDATE"), "the kind's codes are upserted");
        assert!(ddl.contains("DELETE FROM \"codes\" WHERE \"kind\" = 'k2' AND (\"field\", \"code\") NOT IN (VALUES ('phase', 'dr'), ('phase', 'op'), ('phase', 'sh'));"), "and only they are kept");
        assert_eq!(ddl.matches("CREATE VIEW ").count(), 0, "the store recreates only changed views");
        assert_eq!(super::render_views(&convoy().finish()).len(), 3);
    }

    // Behaviour (#3392 D4): a nested table's code columns are named `<table>.<column>` in
    // `codes`, the table named without the root's prefix, so two tables' columns never collide.
    #[test]
    fn nested_code_fields_are_named_by_their_table() {
        let mut layout = Layout::root("Note", KindCode::new("k1"));
        layout.table("events", TableRole::History, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column(RECORD_VERSION, ColumnType::Integer, true);
            layout.code("edge", &codes(&["go"]), &["Go"], true);
        });
        let set = layout.finish();
        assert_eq!(super::code_field(&set, &set.tables[1], "edge"), "events.edge");
        assert!(render_ddl(&set).contains("('k1', 'events.edge', 'go', 'Go')"));
    }

    #[test]
    fn guard_checks_render_as_table_constraints() {
        let mut layout = Layout::root("Edge", KindCode::new("k1"));
        layout.column("event", ColumnType::Code(text_codes(&["dr", "go"])), true);
        let datums = [layout.scope(|layout| layout.column("event_cause", ColumnType::Text, true))];
        layout.tag_guard("event", Code::new("dr"), &datums, &[(0, true)]);
        layout.tag_guard("event", Code::new("go"), &datums, &[]);
        let ddl = render_ddl(&layout.finish());
        assert!(ddl.contains(
            "  \"event_cause\" TEXT,\n  CHECK (CASE WHEN \"event\" = 'dr'\n      THEN \"event_cause\" IS NOT NULL\n      END),\n  CHECK (CASE WHEN \"event\" = 'go'\n      THEN \"event_cause\" IS NULL\n      END)\n) STRICT;"
        ), "{ddl}");
    }

    // Behaviour: validation refuses names the DDL cannot hold unambiguously.
    #[test]
    fn validate_refuses_duplicate_and_reserved_names() {
        let mut layout = Layout::root("Edge", KindCode::new("k1"));
        layout.column("cause", ColumnType::Text, true);
        layout.column("cause", ColumnType::Integer, true);
        assert!(matches!(layout.finish().validate(), Err(DescriptorError::DuplicateColumn { column, .. }) if column == "cause"));

        for reserved in ["id", "version"] {
            let mut layout = Layout::root("Edge", KindCode::new("k1"));
            layout.column(reserved, ColumnType::Text, true);
            assert!(matches!(layout.finish().validate(), Err(DescriptorError::ReservedColumn { column, .. }) if column == reserved));
        }
        // A child table adds only its parent key, named after the root table; `id`, `version` and
        // `owner` are free there.
        let child = |name: &str| {
            let mut layout = Layout::root("Edge", KindCode::new("k1"));
            layout.table("items", TableRole::Child, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column(name, ColumnType::Integer, true);
            });
            layout.finish().validate()
        };
        assert!(matches!(child("edge"), Err(DescriptorError::ReservedColumn { column, .. }) if column == "edge"));
        assert_eq!(child("owner"), Ok(()));
        assert_eq!(child("version"), Ok(()));
        assert_eq!(child("id"), Ok(()));
        // `records` and `history` tables start with the version stamp, and reserve its name.
        for role in [TableRole::Records, TableRole::History] {
            let records = |columns: &[&str]| {
                let mut layout = Layout::root("Edge", KindCode::new("k1"));
                layout.table("events", role, |layout| {
                    layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                    for name in columns {
                        layout.column(name, ColumnType::Integer, true);
                    }
                });
                layout.finish().validate()
            };
            assert_eq!(records(&[RECORD_VERSION, "at"]), Ok(()));
            assert!(matches!(records(&["at"]), Err(DescriptorError::MissingStamp { table, .. }) if table == "edge_events"));
            assert!(matches!(records(&["at", RECORD_VERSION]), Err(DescriptorError::MissingStamp { .. })));
            for reserved in ["edge", RECORD_VERSION] {
                let refused = records(&[RECORD_VERSION, reserved]);
                assert!(
                    matches!(refused, Err(DescriptorError::ReservedColumn { column, .. }) if column == reserved),
                    "{role:?} {reserved}"
                );
            }
        }

        // A data column colliding with the table's own key column.
        let mut layout = Layout::root("Edge", KindCode::new("k1"));
        layout.table("items", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("pos", ColumnType::Text, true);
        });
        assert!(matches!(layout.finish().validate(), Err(DescriptorError::DuplicateColumn { column, .. }) if column == "pos"));

        // `a` + field `b_c` and `a_b` + field `c` both render `a_b_c`.
        let mut layout = Layout::root("A", KindCode::new("k1"));
        layout.table("b_c", TableRole::Child, |layout| layout.key(|layout| layout.column("pos", ColumnType::Integer, true)));
        layout.table("b", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.table("c", TableRole::Child, |layout| layout.key(|layout| layout.column("pos", ColumnType::Integer, true)));
        });
        assert!(matches!(layout.finish().validate(), Err(DescriptorError::DuplicateName { name, .. }) if name == "a_b_c"));

        // Indexes share the namespace as `<table>_<name>`.
        let mut layout = Layout::root("Edge", KindCode::new("k1"));
        layout.table("items", TableRole::Child, |layout| layout.key(|layout| layout.column("pos", ColumnType::Integer, true)));
        layout.index(Index { name: "items".into(), table: "edge".into(), eq: vec![], order: vec![], predicate: vec![], unique: false });
        assert!(matches!(layout.finish().validate(), Err(DescriptorError::DuplicateName { name, .. }) if name == "edge_items"));

        assert!(matches!(
            Layout::root("Objects", KindCode::new("k1")).finish().validate(),
            Err(DescriptorError::ReservedName { name, .. }) if name == "objects"
        ));
    }

    // Behaviour (DESIGN indexes): an index is refused unless it is on one of the kind's tables
    // (the root, or a nested table such as an edge log), names existing columns of that table once each, pages only through columns that cannot be null under its
    // predicate, and its predicate can hold (listed codes of a code column, `is_some` of a
    // nullable column). Interned and reference columns may be compared but never order an index.
    #[test]
    fn validate_refuses_malformed_indexes() {
        let problem = |index: Index| {
            let mut layout = Layout::root("Note", KindCode::new("k1"));
            layout.column("phase", ColumnType::Code(text_codes(&["dr", "op"])), true);
            layout.column("due", ColumnType::Timestamp, false);
            layout.column("seq", ColumnType::Integer, true);
            layout.column("owner", ColumnType::Ref, true);
            layout.column("label", ColumnType::Interned(Code::new("tx")), true);
            layout.table("events", TableRole::History, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column(RECORD_VERSION, ColumnType::Integer, true);
                layout.column("at", ColumnType::Timestamp, true);
            });
            layout.index(index);
            match layout.finish().validate() {
                Ok(()) => None,
                Err(DescriptorError::Index { problem, .. }) => Some(problem),
                Err(other) => panic!("unexpected {other}"),
            }
        };
        let index = |eq: &[&str], order: &[&str], predicate: Vec<Condition>| Index {
            name: "i".into(),
            table: "note".into(),
            eq: eq.iter().map(|c| (*c).to_owned()).collect(),
            order: order.iter().map(|c| (*c).to_owned()).collect(),
            predicate,
            unique: false,
        };
        let present = || Condition::NotNull { column: "due".into() };
        assert_eq!(problem(index(&["phase"], &["seq"], vec![])), None);
        assert_eq!(problem(index(&[], &["due"], vec![present()])), None);
        assert_eq!(problem(Index { table: "other".into(), ..index(&["seq"], &[], vec![]) }), Some(IndexProblem::Table("other".into())));
        let events = |eq: &[&str], order: &[&str]| Index { table: "note_events".into(), ..index(eq, order, vec![]) };
        assert_eq!(problem(events(&[], &["at"])), None, "a nested table's own columns");
        assert_eq!(problem(events(&["seq"], &[])), Some(IndexProblem::UnknownColumn("seq".into())), "not the root's");
        assert_eq!(problem(index(&[], &[], vec![])), Some(IndexProblem::NoColumns));
        assert_eq!(problem(index(&["nope"], &[], vec![])), Some(IndexProblem::UnknownColumn("nope".into())));
        assert_eq!(problem(index(&["seq"], &["seq"], vec![])), Some(IndexProblem::RepeatedColumn("seq".into())));
        assert_eq!(problem(index(&[], &["due"], vec![])), Some(IndexProblem::NullableOrder("due".into())));
        for surrogate in ["owner", "label"] {
            assert_eq!(problem(index(&[], &[surrogate], vec![])), Some(IndexProblem::SurrogateOrder(surrogate.into())));
            assert_eq!(problem(index(&[surrogate], &[], vec![])), None, "{surrogate} may still be an eq column");
        }
        let phases = |listed: &[&str]| vec![Condition::In { column: "phase".into(), codes: codes(listed) }];
        assert_eq!(problem(index(&["seq"], &[], phases(&["op"]))), None);
        assert_eq!(problem(index(&["seq"], &[], phases(&["sh"]))), Some(IndexProblem::Codes("phase".into())));
        assert_eq!(problem(index(&["seq"], &[], phases(&[]))), Some(IndexProblem::Codes("phase".into())));
        let in_seq = vec![Condition::In { column: "seq".into(), codes: codes(&["dr"]) }];
        assert_eq!(problem(index(&["phase"], &[], in_seq)), Some(IndexProblem::Codes("seq".into())));
        let seq_present = vec![Condition::NotNull { column: "seq".into() }];
        assert_eq!(problem(index(&["phase"], &[], seq_present)), Some(IndexProblem::NotNullable("seq".into())));
        let unique = |index: Index| Index { unique: true, ..index };
        assert_eq!(problem(unique(events(&["at"], &[]))), None, "unique within a nested table's owner");
        assert_eq!(problem(unique(index(&["seq"], &[], vec![]))), Some(IndexProblem::Unique), "a root row has no owner");
        assert_eq!(problem(unique(events(&[], &["at"]))), Some(IndexProblem::Unique), "eq columns only");
    }

    // Behaviour (#3587 layer 6 labels ruling): a unique index on a nested table leads with its
    // owner's key, the resource's row id and the parent rows' keys, so it is unique per owner.
    #[test]
    fn a_unique_index_is_unique_within_its_owner() {
        let mut layout = Layout::root("Note", KindCode::new("k1"));
        layout.table("links", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("target", ColumnType::Integer, true));
            layout.table("marks", TableRole::Child, |layout| {
                layout.key(|layout| layout.column("n", ColumnType::Integer, true));
                layout.column("tag", ColumnType::Text, true);
            });
            layout.column("name", ColumnType::Text, true);
        });
        for (table, column) in [("note_links", "name"), ("note_links_marks", "tag")] {
            layout.index(Index {
                name: "unique".into(),
                table: table.into(),
                eq: vec![column.into()],
                order: vec![],
                predicate: vec![],
                unique: true,
            });
        }
        let set = layout.finish();
        set.validate().expect("valid descriptor");
        let ddl = render_ddl(&set);
        let indexes: Vec<&str> = ddl.lines().filter(|line| line.starts_with("CREATE UNIQUE INDEX")).collect();
        assert_eq!(
            indexes,
            [
                r#"CREATE UNIQUE INDEX "note_links_marks_unique" ON "note_links_marks" ("note", "links_target", "tag");"#,
                r#"CREATE UNIQUE INDEX "note_links_unique" ON "note_links" ("note", "name");"#,
            ]
        );
    }

    // Behaviour (#3392 lane 3 ruling 1): each reference column of a child table, key or data, gets
    // an index leading with it, partial when it is nullable, unless a declared index already leads
    // with it; the root's reference columns get none.
    #[test]
    fn child_reference_columns_are_indexed() {
        let mut layout = Layout::root("Note", KindCode::new("k1"));
        layout.column("author", ColumnType::Ref, true);
        layout.table("links", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("target", ColumnType::Ref, true));
            layout.column("via", ColumnType::Ref, false);
            layout.column("by", ColumnType::Ref, true);
        });
        layout.index(Index {
            name: "by_by".into(),
            table: "note_links".into(),
            eq: vec!["by".into()],
            order: vec![],
            predicate: vec![],
            unique: false,
        });
        let set = layout.finish();
        set.validate().expect("valid descriptor");
        let ddl = render_ddl(&set);
        let indexes: Vec<&str> = ddl.lines().filter(|line| line.starts_with("CREATE INDEX")).collect();
        assert_eq!(
            indexes,
            [
                r#"CREATE INDEX "note_links_by_by" ON "note_links" ("by");"#,
                r#"CREATE INDEX "note_links_target_ref" ON "note_links" ("target");"#,
                r#"CREATE INDEX "note_links_via_ref" ON "note_links" ("via") WHERE "via" IS NOT NULL;"#,
            ]
        );
    }
}
