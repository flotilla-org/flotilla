//! The database refuses rows that break a variant's column rule, whoever writes them.

use flotilla_schema::{quote_ident, ColumnDef, ColumnType, Guard, Kind, Table};
use hegel::TestCase;
use rusqlite::{types::Value as SqlValue, Connection, ErrorCode};

use crate::fixtures::{
    message::{Pick, COVERING},
    store, Message,
};

/// A stored value of the column's type that satisfies its own column constraint and foreign key,
/// so a refusal can only come from a guard CHECK.
fn valid_value(conn: &Connection, column: &ColumnDef) -> SqlValue {
    let any_id = |table: &str| conn.query_row(&format!("SELECT min(id) FROM {table}"), [], |row| row.get::<_, i64>(0)).expect("an id");
    match &column.ty {
        ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp => SqlValue::Integer(1),
        ColumnType::Code(codes) => SqlValue::Integer(codes[0]),
        ColumnType::Text => SqlValue::Text("x".into()),
        ColumnType::Blob | ColumnType::Frozen => SqlValue::Blob(vec![0]),
        ColumnType::Interned => SqlValue::Integer(any_id("strings")),
        ColumnType::Ref => SqlValue::Integer(any_id("resource_identity")),
    }
}

fn condition(guard: &Guard) -> String {
    format!("{} = {}", quote_ident(&guard.column), guard.value)
}

/// One row id of `table` matching `predicate`.
fn row_where(conn: &Connection, table: &Table, predicate: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT id FROM {} WHERE {predicate} LIMIT 1", quote_ident(&table.name)), [], |row| row.get(0)).ok()
}

/// Sets one column of one row with raw SQL and returns whether SQLite refused it with a CHECK.
fn refused_by_check(conn: &Connection, table: &Table, id: i64, column: &str, value: SqlValue) -> bool {
    let sql = format!("UPDATE {} SET {} = ?1 WHERE id = ?2", quote_ident(&table.name), quote_ident(column));
    match conn.execute(&sql, rusqlite::params![value, id]) {
        Ok(changed) => {
            assert_eq!(changed, 1, "{sql} matched no row");
            false
        }
        Err(rusqlite::Error::SqliteFailure(error, Some(message))) => {
            assert_eq!(error.code, ErrorCode::ConstraintViolation, "{sql}: {message}");
            message.starts_with("CHECK constraint failed")
        }
        Err(other) => panic!("{sql}: {other}"),
    }
}

// Behaviour (#3320 CHECK constraints): for every guard the descriptor declares (every data enum
// variant with columns and every present `Option<flatten>`, nested and flattened ones included),
// a raw SQL write that sets a column of an inactive guard, or nulls a required column of an active
// one, is refused by a CHECK; so is an unknown code in any code column. Cases come from the
// descriptor, not a hand list, so a new variant is covered automatically; the covering generator
// must reach an active row for every guard, or the test fails naming the guard.
#[hegel::test]
fn raw_writes_breaking_a_variant_rule_are_refused(tc: TestCase) {
    let mut store = store();
    let mut write = store.write().expect("write");
    for index in 0..COVERING {
        write.insert(&format!("m{index}"), &Message::generate(&tc, Pick::Cover(index))).expect("insert");
    }
    write.commit().expect("commit");
    let conn = store.into_connection();
    let set = Message::table_set();
    let root = &set.tables[0];
    assert!(!root.guards.is_empty());

    for guard in &root.guards {
        let active = condition(guard);
        // Inactive: the controlling column differs or is null (an enclosing guard is inactive).
        let inactive = row_where(&conn, root, &format!("{} IS NOT {}", quote_ident(&guard.column), guard.value))
            .unwrap_or_else(|| panic!("no row leaves `{active}` inactive"));
        for column in &guard.scope {
            let def = root.columns.iter().find(|def| &def.name == column).expect("scope columns are declared");
            assert!(
                refused_by_check(&conn, root, inactive, column, valid_value(&conn, def)),
                "setting {column} while `{active}` does not hold was accepted"
            );
        }
        let holding = row_where(&conn, root, &active).unwrap_or_else(|| panic!("the covering messages never make `{active}` hold"));
        for column in &guard.required {
            assert!(
                refused_by_check(&conn, root, holding, column, SqlValue::Null),
                "nulling required {column} while `{active}` holds was accepted"
            );
        }
        // Control: rewriting the active row's own values is accepted, so the refusals above are
        // the rule and not the statement.
        for column in &guard.scope {
            let sql = format!("UPDATE message SET {0} = {0} WHERE id = ?1", quote_ident(column));
            assert_eq!(conn.execute(&sql, [holding]).expect("an unchanged row satisfies every CHECK"), 1);
        }
    }

    for column in &root.columns {
        let ColumnType::Code(codes) = &column.ty else { continue };
        let id = row_where(&conn, root, &format!("{} IS NOT NULL", quote_ident(&column.name)))
            .unwrap_or_else(|| panic!("no row sets code column {}", column.name));
        for unknown in [codes.iter().min().expect("codes") - 1, codes.iter().max().expect("codes") + 1] {
            assert!(refused_by_check(&conn, root, id, &column.name, SqlValue::Integer(unknown)), "{} accepted code {unknown}", column.name);
        }
    }
}
