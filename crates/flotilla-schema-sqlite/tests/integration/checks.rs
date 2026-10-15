//! The database refuses rows that break a variant's column rule, whoever writes them.

use flotilla_schema::{quote_ident, ColumnDef, ColumnType, Guard, Table, TableRole, RECORD_VERSION};
use flotilla_schema_sqlite::Write;
use hegel::TestCase;
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, ErrorCode};

use crate::fixtures::{registry, store, Convoy, Fixture, Message, Pick, TerminalSession};

/// A stored value of the column's type that satisfies its own column constraint and foreign key,
/// so a refusal can only come from a guard CHECK.
fn valid_value(conn: &Connection, column: &ColumnDef) -> SqlValue {
    let any = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, SqlValue>(0)).expect("a stored row");
    match &column.ty {
        ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp => SqlValue::Integer(1),
        ColumnType::Code(codes) => SqlValue::Integer(codes[0]),
        ColumnType::Text => SqlValue::Text("x".into()),
        ColumnType::Blob | ColumnType::Cbor => SqlValue::Blob(vec![0]),
        ColumnType::Frozen => any("SELECT hash FROM objects LIMIT 1"),
        ColumnType::Interned => any("SELECT min(id) FROM strings"),
        ColumnType::Ref => any("SELECT min(id) FROM resource_identity"),
    }
}

fn condition(guard: &Guard) -> String {
    format!("{} = {}", quote_ident(&guard.column), guard.value)
}

/// A row's primary key columns: `id` on the root; `owner` and the logical key on child tables.
fn primary_key(table: &Table) -> Vec<String> {
    match table.parent {
        None => vec![quote_ident("id")],
        Some(_) => std::iter::once(quote_ident("owner")).chain(table.key.iter().map(|column| quote_ident(&column.name))).collect(),
    }
}

/// The primary key of one row of `table` matching `predicate`.
fn row_where(conn: &Connection, table: &Table, predicate: &str) -> Option<Vec<SqlValue>> {
    let key = primary_key(table);
    let sql = format!("SELECT {} FROM {} WHERE {predicate} LIMIT 1", key.join(", "), quote_ident(&table.name));
    conn.query_row(&sql, [], |row| (0..key.len()).map(|n| row.get(n)).collect()).ok()
}

/// Sets one column of one row with raw SQL and returns whether SQLite refused it with a CHECK.
fn refused_by_check(conn: &Connection, table: &Table, key: &[SqlValue], column: &str, value: SqlValue) -> bool {
    let matches: Vec<String> = primary_key(table).iter().enumerate().map(|(n, name)| format!("{name} = ?{}", n + 2)).collect();
    let sql = format!("UPDATE {} SET {} = ?1 WHERE {}", quote_ident(&table.name), quote_ident(column), matches.join(" AND "));
    let params = std::iter::once(value).chain(key.iter().cloned());
    match conn.execute(&sql, params_from_iter(params)) {
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

fn insert_covering<K: Fixture>(write: &mut Write<'_>, tc: &TestCase) {
    for index in 0..K::COVERING {
        write.insert(&format!("r{index}"), &K::generate(tc, Pick::Cover(index))).expect("insert");
    }
}

// Behaviour (#3320 CHECK constraints): for every guard every fixture kind declares, in every
// table (every data enum variant with columns and every present `Option<flatten>`, nested,
// flattened and in child tables), a raw SQL write that sets a column of an inactive guard, or
// nulls a required column of an active one, is refused by a CHECK; so is an unknown code in any
// code column. Cases come from the descriptors, not a hand list, so a new variant or table is
// covered automatically; the covering generators must reach an active and an inactive row for
// every guard, or the test fails naming the guard.
#[hegel::test]
fn raw_writes_breaking_a_variant_rule_are_refused(tc: TestCase) {
    let mut store = store();
    let mut write = store.write().expect("write");
    insert_covering::<Message>(&mut write, &tc);
    insert_covering::<Convoy>(&mut write, &tc);
    insert_covering::<TerminalSession>(&mut write, &tc);
    write.commit().expect("commit");
    let conn = store.into_connection();
    let mut guards = 0;
    for set in registry().iter() {
        for table in &set.tables {
            guards += table.guards.len();
            check_table(&conn, table);
        }
    }
    assert!(guards > 40, "the fixtures declare guards in many tables: {guards}");
}

fn check_table(conn: &Connection, table: &Table) {
    let name = &table.name;
    for guard in &table.guards {
        let active = condition(guard);
        // Inactive: the controlling column differs or is null (an enclosing guard is inactive).
        let inactive = row_where(conn, table, &format!("{} IS NOT {}", quote_ident(&guard.column), guard.value))
            .unwrap_or_else(|| panic!("{name}: no row leaves `{active}` inactive"));
        for column in &guard.scope {
            let def = table.columns.iter().find(|def| &def.name == column).expect("scope columns are declared");
            assert!(
                refused_by_check(conn, table, &inactive, column, valid_value(conn, def)),
                "{name}: setting {column} while `{active}` does not hold was accepted"
            );
        }
        let holding = row_where(conn, table, &active).unwrap_or_else(|| panic!("{name}: the covering values never make `{active}` hold"));
        for column in &guard.required {
            assert!(
                refused_by_check(conn, table, &holding, column, SqlValue::Null),
                "{name}: nulling required {column} while `{active}` holds was accepted"
            );
        }
        // Control: rewriting the active row's own values is accepted, so the refusals above are
        // the rule and not the statement.
        for column in &guard.scope {
            let sql = format!("UPDATE {} SET {1} = {1} WHERE {active}", quote_ident(name), quote_ident(column));
            assert!(conn.execute(&sql, []).expect("unchanged rows satisfy every CHECK") >= 1, "{sql}");
        }
    }

    for column in &table.columns {
        let ColumnType::Code(codes) = &column.ty else { continue };
        let key = row_where(conn, table, &format!("{} IS NOT NULL", quote_ident(&column.name)))
            .unwrap_or_else(|| panic!("{name}: no row sets code column {}", column.name));
        for unknown in [codes.iter().min().expect("codes") - 1, codes.iter().max().expect("codes") + 1] {
            assert!(
                refused_by_check(conn, table, &key, &column.name, SqlValue::Integer(unknown)),
                "{name}.{} accepted code {unknown}",
                column.name
            );
        }
    }
}

// Behaviour (#3320 ruling 3): every `records` and `history` table holds its rows' version stamp
// as a `NOT NULL` integer, so a raw write cannot leave a row unstamped.
#[hegel::test(test_cases = 5)]
fn record_stamps_are_never_null(tc: TestCase) {
    let mut store = store();
    let mut write = store.write().expect("write");
    insert_covering::<Message>(&mut write, &tc);
    insert_covering::<Convoy>(&mut write, &tc);
    insert_covering::<TerminalSession>(&mut write, &tc);
    write.commit().expect("commit");
    let conn = store.into_connection();
    let mut checked = 0;
    for set in registry().iter() {
        for table in set.tables.iter().filter(|table| matches!(table.role, TableRole::Records | TableRole::History)) {
            let sql = format!("UPDATE {} SET {} = NULL", quote_ident(&table.name), quote_ident(RECORD_VERSION));
            match conn.execute(&sql, []) {
                Err(rusqlite::Error::SqliteFailure(error, Some(message))) => {
                    assert_eq!(error.code, ErrorCode::ConstraintViolation, "{sql}: {message}");
                    assert!(message.starts_with("NOT NULL constraint failed"), "{sql}: {message}");
                }
                other => panic!("{sql} was not refused: {other:?}"),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 7, "edges, attempts, deliverables, evidence, convoy edges, events and attempts");
}
