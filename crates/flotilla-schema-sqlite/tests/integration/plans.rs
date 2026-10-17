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
        "log_append",
        "log_by_home",
        "log_by_resource",
        "log_about_first",
        "log_about_after",
        "log_expunge",
        "log_expunged_after",
        "identity_delete",
        "message select",
        "message update version",
        "message set state",
        "message set receiver",
        "message set state state",
        "message label state",
        "agent_session_receipts insert",
        "agent_session_receipts last position",
        "convoy_resolution upsert",
        "convoy_quota insert",
        "convoy_quota delete",
        "convoy label state",
        "ensure_recorded insert",
        "ensure set bound",
        "ensure_attempts last position",
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
        "ticket_notes upsert",
        "ticket_notes delete",
        "index ticket_open_by_queue first",
    ] {
        assert!(labels.contains(&expected), "missing {expected}");
    }
    assert_eq!(labels.iter().filter(|label| label.starts_with("index ")).count(), 2 * 9);
    assert_eq!(labels.iter().filter(|label| label.ends_with(" scan")).count(), 9);
    assert!(!labels.iter().any(|label| label.starts_with("agent_session_receipts set ")), "history rows are never set");
    assert!(!labels.iter().any(|label| label.contains("_edges")), "no kind has an edge-log table");
    assert!(
        !labels.iter().any(|label| label.starts_with("convoy_promises ") && label.ends_with(" last position")),
        "spec collections take no positions from edges"
    );
    assert!(
        !labels.iter().any(|label| label.ends_with(" update")),
        "rows change field by field (`set`) or are put by key (`upsert`), never updated whole"
    );
}

// Behaviour (#3392 A1, lane 3 ruling 1): removing an identity (an expunge) checks each foreign key
// referencing it by a search, never a scan: root rows and root reference columns by key or declared
// index, child tables' reference columns by their generated `<column>_ref` indexes, and the log by
// its own indexes. Expunges come about as often as creations, so a scan would grow with the store.
#[test]
fn identity_removal_checks_references() {
    let (conn, statements) = statements();
    let sql = &statements.iter().find(|(label, _)| label == "identity_delete").expect("a shared statement").1;
    let rows = plan(&conn, sql);
    assert!(!rows.iter().any(|row| row.contains("SCAN")), "{rows:?}");
    assert!(
        rows.iter().any(|row| row.contains("log_about (about=?)")) && rows.iter().any(|row| row.contains("log_by_resource (resource=?)"))
    );
    for column in ["artifact_resource", "commit_repository", "control_record_resource", "ref_repository"] {
        let index = format!("referral_references_{column}_ref ({column}=?)");
        assert!(rows.iter().any(|row| row.contains(&index)), "{index}: {rows:?}");
    }
}

// Behaviour (#3392 G8, G11): the log's reads each search their own index in its order, with no scan
// and no temporary sort: a resource's history by `log_by_resource`, a home's entries by the primary
// key, the entries about a master by the partial `log_about`, the expunge's removal of a
// resource's entries by `log_by_resource`, and the tombstone check of an unknown identity by the
// partial `log_expunges`, which holds expunge entries only.
#[test]
fn log_reads_search_their_own_index() {
    let (conn, statements) = statements();
    let expected = [
        ("log_by_resource", "USING INDEX log_by_resource (resource=? AND home_seq>?)"),
        ("log_by_home", "USING INDEX sqlite_autoindex_log_1 (home=? AND home_seq>?)"),
        ("log_about_first", "USING INDEX log_about (about=?)"),
        ("log_about_after", "USING INDEX log_about (about=? AND at>?)"),
        ("log_expunge", "USING COVERING INDEX log_by_resource (resource=?)"),
        ("log_expunged_after", "USING INDEX log_expunges (home=? AND home_seq>?)"),
    ];
    for (label, index) in expected {
        let sql = &statements.iter().find(|(candidate, _)| candidate == label).expect("a shared statement").1;
        let rows = plan(&conn, sql);
        assert!(rows.iter().any(|row| row.starts_with("SEARCH") && row.contains(index)), "{label}: {rows:?}");
        assert!(!rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")), "{label}: {rows:?}");
    }
}
