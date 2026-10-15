use std::{env, fs, path::Path};

use flotilla_schema::DdlMode;
use flotilla_schema_sqlite::{schema_ddl, Store};

use crate::fixtures::{registry, store, Message, LOCAL};

// Behaviour (#3320 DDL snapshot): the fixtures' DDL is deterministic and checked in. Any diff
// fails; `UPDATE_DDL_SNAPSHOT=1` rewrites the snapshot for review.
#[test]
fn fixture_ddl_matches_the_snapshot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/snapshots/fixtures.sql");
    let ddl = schema_ddl(&registry(), DdlMode::Create);
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(&path, &ddl).expect("write snapshot");
        return;
    }
    let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
    assert_eq!(ddl, snapshot, "fixture DDL changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
}

// Behaviour (DESIGN DDL): every shared and fixture table is STRICT, and the stored rows satisfy
// every foreign key after writes.
#[test]
fn every_table_is_strict_and_foreign_keys_hold() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let message = Message { body_digest: Some("d".into()), interrupting: true, accepted_seq: 3, ordinal: 4, ..Message::sample() };
    let stamp = write.insert("m", &message).expect("insert");
    let current = write.load::<Message>(stamp.id).expect("load").expect("present");
    write.update(&current, &Message { body: "c".into(), ..message }).expect("update");
    write.commit().expect("commit");

    let conn = store.into_connection();
    let mut statement = conn
        .prepare("SELECT name, strict FROM pragma_table_list WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .expect("table list");
    let tables: Vec<(String, bool)> =
        statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).expect("query").collect::<Result<_, _>>().expect("rows");
    let names: Vec<&str> = tables.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "agent_session",
            "change_log",
            "convoy",
            "convoy_deliverables",
            "convoy_deliverables_evidence",
            "convoy_deliverables_evidence_metadata",
            "convoy_inputs",
            "convoy_inputs_list_items",
            "convoy_repositories",
            "home_sequence",
            "inbox",
            "message",
            "message_attempts",
            "message_edges",
            "message_edges_inputs",
            "message_references",
            "objects",
            "project",
            "resource_identity",
            "strings",
            "terminal_session",
            "terminal_session_attempts",
            "terminal_session_env",
            "terminal_session_events",
            "terminal_session_labels",
            "vessel",
        ]
    );
    for (name, strict) in &tables {
        assert!(strict, "{name} is not STRICT");
    }
    let violations: i64 = conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0)).expect("check");
    assert_eq!(violations, 0);
    let enabled: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0)).expect("pragma");
    assert!(enabled);
}

// Behaviour: opening an existing database is idempotent and keeps its rows.
#[test]
fn reopening_keeps_tables_and_rows() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let message = Message { body: "kept".into(), ..Message::sample() };
    let stamp = write.insert("m", &message).expect("insert");
    write.commit().expect("commit");
    let mut store = Store::open(store.into_connection(), LOCAL, registry()).expect("reopen");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(stamp.id).expect("load").expect("present").value, message);
}
