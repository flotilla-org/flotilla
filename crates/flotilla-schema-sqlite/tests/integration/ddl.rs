use std::{env, fs, path::Path};

use flotilla_schema::{render_view, DdlMode, Kind, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, Store};
use flotilla_state_machines::{
    machines::message::{Deliver, MemoryMessageWorld, MessageSpec, MessageWorld, Receiver},
    Ref, Transport,
};

use crate::fixtures::{addr, engine, message, origin, registry, store, Message, Resource, LOCAL};

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

// Behaviour (CLAUDE.md schema review): the fixtures' schema view (widths, enum expansion, child
// tables and indexes per table; `flotilla_schema::render_view`) is checked in beside the DDL, so a
// shape change shows in review as a diff of both. `UPDATE_DDL_SNAPSHOT=1` rewrites it too.
#[test]
fn fixture_schema_view_matches_the_snapshot() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/integration/snapshots/schema_view.md");
    let view = format!(
        "# Fixture schema view\n\nKind tables only; the shared tables are in `fixtures.sql`.\n\n{}",
        render_view(registry().iter())
    );
    if env::var_os("UPDATE_DDL_SNAPSHOT").is_some_and(|value| value == "1") {
        fs::write(&path, &view).expect("write snapshot");
        return;
    }
    let snapshot = fs::read_to_string(&path).expect("read snapshot; run with UPDATE_DDL_SNAPSHOT=1 to create it");
    assert_eq!(view, snapshot, "fixture schema view changed; review and rerun with UPDATE_DDL_SNAPSHOT=1");
}

// Behaviour (DESIGN DDL): every shared and fixture table is STRICT, and the stored rows satisfy
// every foreign key after writes.
#[test]
fn every_table_is_strict_and_foreign_keys_hold() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let spec = MessageSpec { deadline: Some(flotilla_schema::Timestamp(3)), unbound_after: 4, ..message::sample_spec() };
    let message = message::sample().with_spec(spec.clone());
    let stamp = write.insert("m", &message, &origin()).expect("insert");
    let current = write.load::<Resource<Message>>(stamp.id).expect("load").expect("present");
    write.update(&current, &message.with_spec(MessageSpec { inbox: addr("c"), ..spec }), &origin()).expect("update");
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
            "codes",
            "convoy",
            "convoy_promises",
            "convoy_quota",
            "convoy_resolution",
            "dossier",
            "dossier_deliverables",
            "dossier_deliverables_evidence",
            "dossier_deliverables_evidence_metadata",
            "dossier_inputs",
            "dossier_inputs_items",
            "ensure",
            "ensure_created",
            "ensure_recorded",
            "home_sequence",
            "log",
            "message",
            "objects",
            "referral",
            "referral_references",
            "resource_identity",
            "strings",
            "terminal_session",
            "terminal_session_env",
            "terminal_session_labels",
            "ticket",
            "ticket_assignees",
            "ticket_notes",
            "vessel",
            "vessel_shape",
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
    let message = message::sample_in(addr("kept"));
    let stamp = write.insert("m", &message, &origin()).expect("insert");
    write.commit().expect("commit");
    let mut store = Store::open(store.into_connection(), LOCAL, registry()).expect("reopen");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Resource<Message>>(stamp.id).expect("load").expect("present").value, message.with_identity("m", stamp.version));
}

// Behaviour (#3392 ruling D4): codes stay short, and each kind table has a readable view naming
// every code by its variant and every interned column by its text, through `codes` and
// `strings`. Reopening replaces a kind's `codes` rows and recreates its views.
#[test]
fn readable_views_name_codes_and_strings() {
    let mut store = store();
    let mut write = store.write().expect("write");
    write.insert("m", &message::sample_in(addr("inbox-a")), &origin()).expect("insert");
    write.commit().expect("commit");
    let conn = Store::open(store.into_connection(), LOCAL, registry()).expect("reopen").into_connection();
    let raw: (String, String) =
        conn.query_row("SELECT state, expectation FROM message", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("raw");
    assert_eq!(raw, ("pe".to_owned(), "re".to_owned()), "the table holds codes");
    let view: (String, String, String) = conn
        .query_row("SELECT inbox, state, expectation FROM message_view", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("view");
    assert_eq!(view, ("project:inbox-a".to_owned(), "Pending".to_owned(), "Reply".to_owned()));
    let edge: String = conn
        .query_row("SELECT name FROM codes WHERE kind = 'ms' AND field = 'log:edge' AND code = 'dr'", [], |row| row.get(0))
        .expect("code");
    assert_eq!(edge, "Drop");
    let views: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema WHERE type = 'view'", [], |row| row.get(0)).expect("views");
    let tables: usize = registry().iter().map(|set| set.tables.len()).sum();
    assert_eq!(views, tables as i64 + 1, "one view per kind table, and the log's");
}

// Behaviour (#3392 D4, lane 3): `log_view` names each entry's home, kind, resource and writer, and
// decodes an edge's `edge`, `from` and `to` through the entry's kind (its `log:edge` and
// `log:label` rows of `codes`); the table itself holds the short codes.
#[test]
fn the_log_view_names_edges_and_labels_by_the_entrys_kind() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let resource = message::sample();
    let stamp = write.insert(engine::NAME, &resource, &origin()).expect("insert");
    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&Ref::new(engine::NAME), [("s".to_owned(), Transport::Turn)].into());
    let deliver = Deliver { receiver: Receiver { session: "s".into(), host: "h".into() }, transport: Transport::Turn };
    let world: Box<dyn MessageWorld> = Box::new(world);
    let (edge, _) = engine::step(&resource, "deliverer", deliver, &*world, Timestamp(4)).expect("delivered");
    write.apply_edge(stamp, edge).expect("edge");
    write.commit().expect("commit");
    let conn = store.into_connection();
    let raw: (String, String, String) = conn
        .query_row("SELECT edge, \"from\", \"to\" FROM log WHERE cause = 'edge'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("raw");
    assert_eq!(raw, ("de".to_owned(), "pe".to_owned(), "aw".to_owned()), "the log holds codes");
    let mut statement = conn
        .prepare("SELECT home, home_seq, kind, name, cause, edge, \"from\", \"to\", writer FROM log_view ORDER BY home_seq")
        .expect("view");
    let rows: Vec<Vec<Option<String>>> = statement
        .query_map([], |row| (0..9).map(|n| row.get::<_, rusqlite::types::Value>(n).map(text)).collect())
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    let row = |cells: &[Option<&str>]| cells.iter().map(|cell| cell.map(str::to_owned)).collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            row(&[Some(LOCAL), Some("1"), Some("ms"), Some(engine::NAME), Some("create"), None, None, None, Some("fixture")]),
            row(&[
                Some(LOCAL),
                Some("2"),
                Some("ms"),
                Some(engine::NAME),
                Some("edge"),
                Some("Deliver"),
                Some("Pending"),
                Some("Awaiting"),
                Some("deliverer")
            ]),
        ]
    );
}

fn text(value: rusqlite::types::Value) -> Option<String> {
    match value {
        rusqlite::types::Value::Null => None,
        rusqlite::types::Value::Integer(n) => Some(n.to_string()),
        rusqlite::types::Value::Text(text) => Some(text),
        other => Some(format!("{other:?}")),
    }
}

// Behaviour (#3392 D4, review of 2026-10-17): reopening an unchanged schema changes nothing: no
// row is written and the schema cookie stays, so other connections keep their prepared
// statements. A view that is missing is created again.
#[test]
fn reopening_an_unchanged_schema_changes_nothing() {
    let conn = store().into_connection();
    let cookie = |conn: &rusqlite::Connection| -> i64 { conn.query_row("PRAGMA schema_version", [], |row| row.get(0)).expect("cookie") };
    let before = (cookie(&conn), conn.total_changes());
    let conn = Store::open(conn, LOCAL, registry()).expect("reopen").into_connection();
    assert_eq!((cookie(&conn), conn.total_changes()), before);
    conn.execute_batch("DROP VIEW message_view").expect("drop");
    let conn = Store::open(conn, LOCAL, registry()).expect("reopen").into_connection();
    let views: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_schema WHERE type = 'view' AND name = 'message_view'", [], |row| row.get(0))
        .expect("count");
    assert_eq!(views, 1, "a missing view is recreated");
}
