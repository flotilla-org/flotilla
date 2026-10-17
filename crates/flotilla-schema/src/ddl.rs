//! Deterministic DDL rendering: tables in pre-order, then indexes sorted by object name.
//!
//! Identifiers come from Rust field names (never serde names) and are always quoted, so a field
//! named like an SQL keyword still renders valid DDL.

use std::fmt::Write as _;

use crate::descriptor::{code_literal, ColumnDef, ColumnType, Table, TableRole, TableSet};

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
    let mut indexes: Vec<_> = set.indexes.iter().collect();
    indexes.sort_by_key(|index| index.object_name());
    for index in indexes {
        let columns: Vec<String> = index.columns().map(quote_ident).collect();
        let name = quote_ident(&index.object_name());
        let _ = write!(out, "CREATE INDEX {exists}{name} ON {} ({})", quote_ident(&index.table), columns.join(", "));
        if let Some(predicate) = index.predicate_sql() {
            let _ = write!(out, " WHERE {predicate}");
        }
        out.push_str(";\n");
    }
    out
}

fn render_table(out: &mut String, set: &TableSet, table: &Table, exists: &str) {
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
        Some(_) => lines.push(format!("{} INTEGER NOT NULL", quote_ident("owner"))),
    }
    lines.extend(table.key.iter().chain(&table.columns).map(column));
    if let Some(parent_index) = table.parent {
        let parent = &set.tables[parent_index];
        let mut key = vec![quote_ident("owner")];
        key.extend(table.key.iter().map(|c| quote_ident(&c.name)));
        lines.push(format!("PRIMARY KEY ({})", key.join(", ")));
        let foreign = if parent.role == TableRole::Root {
            format!("FOREIGN KEY ({}) REFERENCES {} ({})", quote_ident("owner"), quote_ident(&parent.name), quote_ident("id"))
        } else {
            let mut local = vec![quote_ident("owner")];
            local.extend(table.parent_key().iter().map(|c| quote_ident(&c.name)));
            let mut remote = vec![quote_ident("owner")];
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

fn column(column: &ColumnDef) -> String {
    let name = quote_ident(&column.name);
    let sql_type = match column.ty {
        ColumnType::Text | ColumnType::Code(_) => "TEXT",
        ColumnType::Blob | ColumnType::Frozen | ColumnType::Cbor => "BLOB",
        _ => "INTEGER",
    };
    let mut out = format!("{name} {sql_type}");
    if !column.nullable {
        out.push_str(" NOT NULL");
    }
    match &column.ty {
        ColumnType::Interned => out.push_str(" REFERENCES \"strings\" (\"id\")"),
        ColumnType::Ref => out.push_str(" REFERENCES \"resource_identity\" (\"id\")"),
        ColumnType::Frozen => out.push_str(" REFERENCES \"objects\" (\"hash\")"),
        ColumnType::Bool => {
            let _ = write!(out, " CHECK ({name} IN (0, 1))");
        }
        ColumnType::Code(codes) => {
            let codes: Vec<String> = codes.iter().copied().map(code_literal).collect();
            let _ = write!(out, " CHECK ({name} IN ({}))", codes.join(", "));
        }
        ColumnType::Integer | ColumnType::Text | ColumnType::Blob | ColumnType::Cbor | ColumnType::Timestamp => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{render_ddl, render_ddl_with, DdlMode};
    use crate::{
        code::Code,
        descriptor::{ColumnType, Condition, DescriptorError, Index, IndexProblem, TableRole, RECORD_VERSION},
        layout::Layout,
        value::KindCode,
    };

    fn codes(codes: &[&str]) -> Vec<Code> {
        codes.iter().map(|code| Code::new(code)).collect()
    }

    fn convoy() -> Layout {
        let mut layout = Layout::root("Convoy", KindCode::new("k2"));
        layout.column("name", ColumnType::Interned, true);
        layout.column("done", ColumnType::Bool, true);
        layout.column("phase", ColumnType::Code(codes(&["dr", "op", "sh"])), false);
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
        });
        layout.index(Index { name: "by_name".into(), table: "convoy".into(), eq: vec!["name".into()], order: vec![], predicate: vec![] });
        layout
    }

    // Behaviour (DESIGN DDL): STRICT tables in pre-order; the root keyed by its identity id; each
    // child keyed by owner plus its logical key, with a cascading FK to its parent's full key;
    // per-type REFERENCES and CHECKs; indexes sorted by name after the tables.
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
  "owner" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "target" INTEGER NOT NULL REFERENCES "resource_identity" ("id"),
  PRIMARY KEY ("owner", "pos"),
  FOREIGN KEY ("owner") REFERENCES "convoy" ("id") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE TABLE "convoy_deliverables_evidence" (
  "owner" INTEGER NOT NULL,
  "deliverables_pos" INTEGER NOT NULL,
  "pos" INTEGER NOT NULL,
  "version" INTEGER NOT NULL,
  "value" BLOB NOT NULL,
  PRIMARY KEY ("owner", "deliverables_pos", "pos"),
  FOREIGN KEY ("owner", "deliverables_pos") REFERENCES "convoy_deliverables" ("owner", "pos") ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX "convoy_by_name" ON "convoy" ("name");
CREATE INDEX "convoy_open" ON "convoy" ("name") WHERE "phase" IN ('dr', 'op') AND "phase" IS NOT NULL;
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
        assert_eq!(ddl.matches("CREATE INDEX IF NOT EXISTS ").count(), 2);
    }

    #[test]
    fn guard_checks_render_as_table_constraints() {
        let mut layout = Layout::root("Edge", KindCode::new("k1"));
        layout.column("event", ColumnType::Code(codes(&["dr", "go"])), true);
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
        // A child table adds only `owner`; `id` and `version` are free there.
        let child = |name: &str| {
            let mut layout = Layout::root("Edge", KindCode::new("k1"));
            layout.table("items", TableRole::Child, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column(name, ColumnType::Integer, true);
            });
            layout.finish().validate()
        };
        assert!(matches!(child("owner"), Err(DescriptorError::ReservedColumn { column, .. }) if column == "owner"));
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
            for reserved in ["owner", RECORD_VERSION] {
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
        layout.index(Index { name: "items".into(), table: "edge".into(), eq: vec![], order: vec![], predicate: vec![] });
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
            layout.column("phase", ColumnType::Code(codes(&["dr", "op"])), true);
            layout.column("due", ColumnType::Timestamp, false);
            layout.column("seq", ColumnType::Integer, true);
            layout.column("owner", ColumnType::Ref, true);
            layout.column("label", ColumnType::Interned, true);
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
    }
}
