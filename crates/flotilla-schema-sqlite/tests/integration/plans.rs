//! Query plans of every statement the runtime prepares (#3320 production shape).
//!
//! `Store::statements` lists the shared statements and every registered kind's rendered ones, and
//! the runtime prepares nothing else (a unit test in `store.rs` refuses literal SQL), so a new
//! statement is planned here without editing this test. `EXPLAIN QUERY PLAN` does not show
//! foreign-key enforcement; every foreign key the runtime can trigger points at a primary key or
//! at a child table whose primary key starts with the referencing columns.

use flotilla_schema::quote_ident;
use rusqlite::{types::Value as SqlValue, Connection};

use crate::fixtures::{registry, store};

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
        "message_edges select",
        "message_edges insert",
        "message update version",
        "message set state",
        "message set receiver",
        "message set state state",
        "message label state",
        "message_edges last position",
        "agent_session_edges insert",
        "convoy_resolution update",
        "convoy_quota insert",
        "convoy_quota delete",
        "convoy label state",
        "ensure_recorded insert",
        "ensure set bound",
        "ensure_edges last position",
        "terminal_session_labels select",
        "dossier_deliverables_evidence_metadata insert",
        "dossier_deliverables set state",
        "dossier_deliverables last position",
        "dossier_deliverables_evidence last position",
        "referral set subject",
        "index message_open_by_inbox first",
        "index message_open_by_inbox after",
        "index message_deadline first",
        "index convoy_open_by_project first",
        "index ensure_by_inbox after",
        "index terminal_session_by_vessel after",
        "index terminal_session_live_by_pool first",
        "index referral_by_reply after",
        "index referral_by_session first",
        "vessel scan",
        "ticket set state state",
        "ticket set reason",
        "ticket label state",
        "ticket_assignees insert",
        "ticket_notes update",
        "ticket_notes delete",
        "index ticket_open_by_queue first",
    ] {
        assert!(labels.contains(&expected), "missing {expected}");
    }
    assert_eq!(labels.iter().filter(|label| label.starts_with("index ")).count(), 2 * 9);
    assert_eq!(labels.iter().filter(|label| label.ends_with(" scan")).count(), 9);
    assert!(!labels.iter().any(|label| label.starts_with("message_edges set ")), "history rows are never set");
    assert!(
        !labels.iter().any(|label| label.starts_with("convoy_promises ") && label.ends_with(" last position")),
        "spec collections take no positions from edges"
    );
    assert!(
        !labels.iter().any(|label| label.starts_with("ensure_recorded ") && label.ends_with(" update")),
        "a set's rows are inserted and deleted, never updated"
    );
}

// Behaviour (#3320 ruling of 2026-10-15): an edge log's indexes serve the reads history answers.
// Events of one edge in a time range, and every event in a time range across resources, each
// search their own index in its order: no scan and no temporary sort. (One resource's log in order
// is its primary key, which loading already searches.) The reads have no query API yet, so the
// statements are rendered here from the descriptor.
#[test]
fn edge_log_indexes_serve_history_reads() {
    let (conn, _) = statements();
    let mut planned = Vec::new();
    for set in registry().iter() {
        for index in set.indexes.iter().filter(|index| index.table != set.tables[0].name) {
            let table = set.tables.iter().find(|table| table.name == index.table).expect("the index's table");
            let key: Vec<String> =
                ["owner"].into_iter().chain(table.key.iter().map(|column| column.name.as_str())).map(quote_ident).collect();
            let order: Vec<String> = index.order.iter().map(|column| quote_ident(column)).collect();
            let mut terms: Vec<String> = index.eq.iter().map(|column| format!("{} = ?", quote_ident(column))).collect();
            terms.push(format!("{} >= ? AND {} < ?", order[0], order[0]));
            let sql = format!(
                "SELECT {} FROM {} WHERE {} ORDER BY {}, {} LIMIT ?",
                key.join(", "),
                quote_ident(&table.name),
                terms.join(" AND "),
                order.join(", "),
                key.join(", ")
            );
            let rows = plan(&conn, &sql);
            let own = format!("USING COVERING INDEX {} (", index.object_name());
            assert!(
                rows.iter().any(|row| row.starts_with("SEARCH") && row.contains(&own))
                    && !rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")),
                "{sql}: {rows:?}"
            );
            planned.push(index.object_name());
        }
    }
    planned.sort();
    assert_eq!(
        planned,
        [
            "agent_session_edges_by_edge",
            "agent_session_edges_by_time",
            "convoy_edges_by_edge",
            "convoy_edges_by_time",
            "dossier_edges_by_edge",
            "dossier_edges_by_time",
            "ensure_edges_by_edge",
            "ensure_edges_by_time",
            "message_edges_by_edge",
            "message_edges_by_time",
            "referral_edges_by_edge",
            "referral_edges_by_time",
            "terminal_session_edges_by_edge",
            "terminal_session_edges_by_time",
            "ticket_edges_by_edge",
            "ticket_edges_by_time",
        ]
    );
}
