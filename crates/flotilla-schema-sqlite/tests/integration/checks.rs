//! The database refuses rows that break a tag's or an optional value's column rule, whoever writes
//! them.

use flotilla_schema::{ColumnDef, ColumnType, Guard, Table, TableRole, When, RECORD_VERSION};
use flotilla_schema_sqlite::quote_ident;
use flotilla_schema_sqlite::Write;
use hegel::TestCase;
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, ErrorCode};

use crate::fixtures::{
    origin, registry, store, AgentSession, Convoy, Dossier, Ensure, Fixture, Message, Pick, Referral, Resource, TerminalSession, Ticket,
    Vessel,
};

/// A stored value of the column's type that satisfies its own column constraint and foreign key,
/// so a refusal can only come from a guard CHECK.
fn valid_value(conn: &Connection, column: &ColumnDef) -> SqlValue {
    let any = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, SqlValue>(0)).expect("a stored row");
    match &column.ty {
        ColumnType::Integer | ColumnType::Bool | ColumnType::Timestamp => SqlValue::Integer(1),
        ColumnType::Code(codes) => SqlValue::Text(codes[0].to_string()),
        ColumnType::Text => SqlValue::Text("x".into()),
        ColumnType::Number => SqlValue::Real(1.0),
        ColumnType::Blob | ColumnType::Cbor => SqlValue::Blob(vec![0]),
        ColumnType::Frozen => any("SELECT hash FROM objects LIMIT 1"),
        ColumnType::Interned(_) => any("SELECT min(id) FROM strings"),
        ColumnType::Ref => any("SELECT min(id) FROM resource_identity"),
    }
}

fn condition(guard: &Guard) -> String {
    match &guard.when {
        When::Is(code) => format!("{} = '{}'", quote_ident(&guard.column), code.replace('\'', "''")),
        When::Present => format!("{} IS NOT NULL", quote_ident(&guard.column)),
    }
}

/// A row's primary key columns: `id` on the root; the generated parent key and the logical key on
/// child tables.
fn primary_key(table: &Table) -> Vec<String> {
    match table.parent {
        None => vec![quote_ident("id")],
        Some(_) => {
            std::iter::once(quote_ident(&table.generated[0].name)).chain(table.key.iter().map(|column| quote_ident(&column.name))).collect()
        }
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
        write.insert(&format!("r{index}"), &K::generate(tc, Pick::Cover(index)), &origin()).expect("insert");
    }
}

/// The covering values of every fixture kind.
fn insert_all(write: &mut Write<'_>, tc: &TestCase) {
    insert_covering::<Resource<Message>>(write, tc);
    insert_covering::<Resource<AgentSession>>(write, tc);
    insert_covering::<Resource<Convoy>>(write, tc);
    insert_covering::<Resource<Ensure>>(write, tc);
    insert_covering::<TerminalSession>(write, tc);
    insert_covering::<Resource<Vessel>>(write, tc);
    insert_covering::<Ticket>(write, tc);
    insert_covering::<Referral>(write, tc);
    insert_covering::<Dossier>(write, tc);
}

// Behaviour (#3320 CHECK constraints, #3373 ruling 1, #3392 G5 and G6): for every guard every
// fixture kind declares, in every table (every data enum variant and every label of a state, by
// mapping B, and every optional value of several columns, nested, flattened and in child tables),
// a raw SQL write that nulls a required column while the guard holds, sets a datum column the
// holding code lacks, or sets a column of an absent optional value, is refused by a CHECK; so is
// an unknown code in any code column. Cases come from the descriptors, not a hand list, so a new
// variant or table is covered automatically; the covering generators must reach a holding row for
// every guard, and an absent one for every optional value, or the test fails naming the guard.
#[hegel::test]
fn raw_writes_breaking_a_variant_rule_are_refused(tc: TestCase) {
    let mut store = store();
    let mut write = store.write().expect("write");
    insert_all(&mut write, &tc);
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
        // An optional value: while absent (its first required column null), its other columns are.
        if !guard.otherwise.is_empty() {
            let absent = row_where(conn, table, &format!("{} IS NULL", quote_ident(&guard.column)))
                .unwrap_or_else(|| panic!("{name}: no row leaves `{active}` absent"));
            for column in &guard.otherwise {
                let def = table.columns.iter().find(|def| &def.name == column).expect("guarded columns are declared");
                assert!(
                    refused_by_check(conn, table, &absent, column, valid_value(conn, def)),
                    "{name}: setting {column} while `{active}` does not hold was accepted"
                );
            }
        }
        let holding = row_where(conn, table, &active).unwrap_or_else(|| panic!("{name}: the covering values never make `{active}` hold"));
        for column in &guard.required {
            assert!(
                refused_by_check(conn, table, &holding, column, SqlValue::Null),
                "{name}: nulling required {column} while `{active}` holds was accepted"
            );
        }
        // A tag guard: the data its code lacks stays null while it holds.
        for column in &guard.absent {
            let def = table.columns.iter().find(|def| &def.name == column).expect("absent columns are declared");
            assert!(
                refused_by_check(conn, table, &holding, column, valid_value(conn, def)),
                "{name}: setting {column}, which the label of `{active}` lacks, was accepted"
            );
        }
        // Control: rewriting the active row's own values is accepted, so the refusals above are
        // the rule and not the statement.
        for column in guard.otherwise.iter().chain(&guard.absent) {
            let sql = format!("UPDATE {} SET {1} = {1} WHERE {active}", quote_ident(name), quote_ident(column));
            assert!(conn.execute(&sql, []).expect("unchanged rows satisfy every CHECK") >= 1, "{sql}");
        }
    }

    for column in &table.columns {
        let ColumnType::Code(codes) = &column.ty else { continue };
        let key = row_where(conn, table, &format!("{} IS NOT NULL", quote_ident(&column.name)))
            .unwrap_or_else(|| panic!("{name}: no row sets code column {}", column.name));
        let first = codes.iter().min().expect("codes");
        for unknown in [format!("{first}0"), "zz_none".to_owned(), first.as_str().to_uppercase()] {
            assert!(!codes.iter().any(|code| *code == unknown.as_str()), "{unknown} is a code");
            assert!(
                refused_by_check(conn, table, &key, &column.name, SqlValue::Text(unknown.clone())),
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
    insert_all(&mut write, &tc);
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
    assert_eq!(checked, 2, "the dossier's deliverables and evidence: every engine kind's history is its log entries");
}
