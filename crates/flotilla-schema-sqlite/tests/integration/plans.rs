//! Query plans of every statement the runtime prepares (#3320 production shape).
//!
//! `Store::statements` lists the shared statements and every registered kind's rendered ones, and
//! the runtime prepares nothing else (a unit test in `store.rs` refuses literal SQL), so a new
//! statement is planned here without editing this test. `EXPLAIN QUERY PLAN` does not show
//! foreign-key enforcement; every foreign key the runtime can trigger points at a primary key or
//! at a child table whose primary key starts with the referencing columns.

use rusqlite::{types::Value as SqlValue, Connection};

use crate::fixtures::store;

/// The detail column of each plan row.
fn plan(conn: &Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap_or_else(|error| panic!("{sql}: {error}"));
    // Plans do not depend on parameter values; bind nulls.
    let nulls = vec![SqlValue::Null; statement.parameter_count()];
    let rows = statement.query_map(rusqlite::params_from_iter(nulls), |row| row.get::<_, String>(3)).expect("plan");
    rows.collect::<Result<_, _>>().expect("plan rows")
}

fn statements() -> (Connection, Vec<(String, String)>) {
    let store = store();
    let statements = store.statements();
    (store.into_connection(), statements)
}

const KEYED: [&str; 4] = ["USING INTEGER PRIMARY KEY", "USING PRIMARY KEY", "USING INDEX", "USING COVERING INDEX"];

// Behaviour (#3320 production shape): every statement the runtime prepares reads by a primary
// key, a unique key or a declared index: no plan row is a SCAN, every SEARCH names a key or index,
// no result is sorted through a temporary b-tree, and every statement other than a plain insert
// searches something. Each index query uses its own partial index, and the startup scan searches
// `resource_identity_by_home`.
#[test]
fn every_prepared_statement_searches_by_key() {
    let (conn, statements) = statements();
    let mut failures = Vec::new();
    for (label, sql) in &statements {
        let rows = plan(&conn, sql);
        let mut problems = Vec::new();
        if rows.iter().any(|row| row.split_whitespace().any(|word| word == "SCAN")) {
            problems.push("scans");
        }
        if rows.iter().any(|row| row.contains("USE TEMP B-TREE")) {
            problems.push("sorts through a temporary b-tree");
        }
        if rows.iter().any(|row| row.starts_with("SEARCH") && !KEYED.iter().any(|keyed| row.contains(keyed))) {
            problems.push("searches without a key");
        }
        let inserts = sql.starts_with("INSERT");
        if !inserts && !rows.iter().any(|row| row.starts_with("SEARCH")) {
            problems.push("searches nothing");
        }
        if let Some(object) = label.strip_prefix("index ").and_then(|rest| rest.split_whitespace().next()) {
            let own = |row: &String| row.contains(&format!("INDEX {object} (")) || row.ends_with(&format!("INDEX {object}"));
            if !rows.iter().any(own) {
                problems.push("does not use its own index");
            }
        }
        if label.ends_with(" scan")
            && !rows.iter().any(|row| row.contains("INDEX resource_identity_by_home (home=? AND kind=? AND rowid>?)"))
        {
            problems.push("does not page resource_identity_by_home by id");
        }
        if !problems.is_empty() {
            failures.push(format!("{label}: {}\n  {sql}\n  {rows:?}", problems.join(", ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Behaviour: the enumeration covers every kind's tables, indexes and scan, and the shared
// statements, so the plan test cannot pass by seeing too little.
#[test]
fn the_enumeration_covers_every_kind_and_index() {
    let (_, statements) = statements();
    let labels: Vec<&str> = statements.iter().map(|(label, _)| label.as_str()).collect();
    for expected in [
        "string_id",
        "log_by_home",
        "message select",
        "message_edges_inputs delete",
        "message_edges insert",
        "convoy_deliverables_evidence_metadata insert",
        "terminal_session_labels select",
        "message update version",
        "index message_open_by_inbox first",
        "index message_open_by_inbox after",
        "index message_by_reply after",
        "index message_deadline first",
        "index convoy_open_by_project first",
        "index terminal_session_by_vessel after",
        "index terminal_session_live_by_pool first",
        "vessel scan",
    ] {
        assert!(labels.contains(&expected), "missing {expected}");
    }
    assert_eq!(labels.iter().filter(|label| label.starts_with("index ")).count(), 2 * 6);
    assert_eq!(labels.iter().filter(|label| label.ends_with(" scan")).count(), 7);
}
