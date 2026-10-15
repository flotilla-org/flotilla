//! Child, records and history tables and frozen objects through SQLite.

use std::collections::BTreeMap;

use flotilla_schema::{quote_ident, Hash, Identity, Kind, Records, Ref, State, Table, Timestamp};
use flotilla_schema_sqlite::{Loaded, Store, StoreError, MEDIA_CBOR};
use hegel::TestCase;
use rusqlite::Connection;

use crate::fixtures::{
    attempt::{Attempt, Outcome},
    convoy::{workflow, ConvoyRepository, Deliverable, DeliverableState, Evidence, InputValue, PromiseKind, PromiseSource},
    message::{GateInput, MessageEdge, MessageEdgeEvent, Settlement},
    registry, store, Convoy, Fixture, Message, Pick, TerminalSession, LOCAL,
};

fn event(writer: &str) -> MessageEdgeEvent {
    MessageEdgeEvent {
        edge: MessageEdge::Settle { by: Settlement::Answered { reply: Ref::new("r", LOCAL) } },
        at: Timestamp(1),
        writer: writer.into(),
        inputs: Vec::new(),
    }
}

fn attempt(target: &str) -> Attempt<String> {
    Attempt { what: "deliver".into(), target: target.into(), at: Timestamp(2), outcome: Outcome::Success }
}

fn evidence(reference: &str) -> Evidence {
    Evidence { reference: reference.into(), submitted_at: Timestamp(3), verdict: None, metadata: BTreeMap::new() }
}

fn deliverable(evidence: Vec<Evidence>) -> Deliverable {
    Deliverable {
        label: "pr".into(),
        kind: PromiseKind::ChangeRequest,
        role: "coder".into(),
        source: PromiseSource::Dispatcher,
        evidence: evidence.into(),
        state: State::new(DeliverableState::Open, Timestamp(4)),
    }
}

fn insert<K: Kind>(store: &mut Store, name: &str, value: &K) -> Loaded<K> {
    let mut write = store.write().expect("write");
    let stamp = write.insert(name, value).expect("insert");
    let loaded = write.load::<K>(stamp.id).expect("load").expect("present");
    write.commit().expect("commit");
    loaded
}

fn load<K: Kind>(store: &mut Store, current: &Loaded<K>) -> Loaded<K> {
    store.read().expect("read").load::<K>(current.id).expect("load").expect("present")
}

// Behaviour (DESIGN writes): through the owner's write, a `records` row may be replaced or
// appended but not removed (`AppendOnly`); a `history` row may only be appended (`Immutable` when
// changed or removed); records nested in records keep the rule when their parent row is replaced.
// A refused write changes nothing, and the transaction stays usable for later writes.
#[test]
fn records_and_history_refusals_reach_the_caller() {
    let mut store = store();
    let message = Message { edges: vec![event("alice"), event("bob")].into(), attempts: vec![attempt("a")].into(), ..Message::sample() };
    let current = insert(&mut store, "m", &message);
    let convoy = Convoy { deliverables: vec![deliverable(vec![evidence("e0"), evidence("e1")])].into(), ..Convoy::sample(0) };
    let convoy_current = insert(&mut store, "c", &convoy);

    let mut write = store.write().expect("write");
    let fewer_edges = Message { edges: vec![event("alice")].into(), ..message.clone() };
    assert!(matches!(write.update(&current, &fewer_edges), Err(StoreError::AppendOnly { table }) if table == "message_edges"));
    let changed_attempt = Message { attempts: vec![attempt("b")].into(), ..message.clone() };
    assert!(matches!(write.update(&current, &changed_attempt), Err(StoreError::Immutable { table }) if table == "message_attempts"));
    let fewer_attempts = Message { attempts: Records::new(), ..message.clone() };
    assert!(matches!(write.update(&current, &fewer_attempts), Err(StoreError::Immutable { table }) if table == "message_attempts"));
    let mut replaced = convoy.deliverables.clone();
    replaced.replace(0, deliverable(vec![evidence("e0")]));
    let fewer_evidence = Convoy { deliverables: replaced, ..convoy.clone() };
    assert!(matches!(
        write.update(&convoy_current, &fewer_evidence),
        Err(StoreError::AppendOnly { table }) if table == "convoy_deliverables_evidence"
    ));

    // Allowed in the same transaction: replace an edge in place, append one, append an attempt.
    let mut edges = message.edges.clone();
    edges.replace(1, event("carol"));
    edges.push(event("dave"));
    let mut attempts = message.attempts.clone();
    attempts.push(attempt("b"));
    let next = Message { edges, attempts, ..message.clone() };
    write.update(&current, &next).expect("replace and append");
    write.commit().expect("commit");
    assert_eq!(load(&mut store, &current).value, next);
    assert_eq!(load(&mut store, &convoy_current).value, convoy, "the refused convoy write changed nothing");
}

/// Rows written by `f`: inserted, updated and deleted rows of every table, counted by SQLite.
fn rows_written(store: Store, f: impl FnOnce(&mut Store)) -> (Store, u64) {
    let conn = store.into_connection();
    let before = conn.total_changes();
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    f(&mut store);
    let conn = store.into_connection();
    let written = conn.total_changes() - before;
    (Store::open(conn, LOCAL, registry()).expect("reopen"), written)
}

fn update<K: Kind>(store: &mut Store, current: &Loaded<K>, next: &K) {
    let mut write = store.write().expect("write");
    write.update(current, next).expect("update");
    write.commit().expect("commit");
}

// Behaviour (#3320 production shape): an update writes only the rows that changed, plus the root
// row's version and the home sequence. Appending one record is one insert; replacing one record
// with one new gate input is one update and one insert; changing one of three repositories is
// one update; an unchanged collection writes nothing.
#[test]
fn updates_write_only_changed_rows() {
    let store = store();
    let message = Message { edges: vec![event("alice"), event("bob")].into(), attempts: vec![attempt("a")].into(), ..Message::sample() };
    let repository = |slug: &str| ConvoyRepository {
        url: "u".into(),
        repo_key: "k".into(),
        source_ref: "s".into(),
        target_ref: "t".into(),
        workspace_slug: slug.into(),
    };
    let convoy = Convoy { repositories: vec![repository("a"), repository("b"), repository("c")], ..Convoy::sample(0) };
    let (mut store, _) = rows_written(store, |_| {});
    let current = insert(&mut store, "m", &message);
    let convoy_current = insert(&mut store, "c", &convoy);

    let mut appended = message.clone();
    appended.edges.push(event("alice"));
    let (mut store, written) = rows_written(store, |store| update(store, &current, &appended));
    assert_eq!(written, 3, "one record row, the root version and the home sequence");

    let current = load(&mut store, &current);
    let mut replaced = appended.clone();
    // "alice" is already interned, so no `strings` row is written.
    let mut with_input = event("alice");
    with_input.inputs.push(GateInput { observed: Identity { kind: Message::CODE, name: "m".into(), home: LOCAL.into() }, version: 1 });
    replaced.edges.replace(1, with_input);
    let (store, written) = rows_written(store, |store| update(store, &current, &replaced));
    assert_eq!(written, 4, "the record row, its new gate input, the root version and the home sequence");

    let changed = Convoy { repositories: vec![repository("a"), repository("B"), repository("c")], ..convoy.clone() };
    let (mut store, written) = rows_written(store, |store| update(store, &convoy_current, &changed));
    assert_eq!(written, 3, "one repository row, the root version and the home sequence");

    let convoy_current = load(&mut store, &convoy_current);
    let (_, written) = rows_written(store, |store| update(store, &convoy_current, &changed));
    assert_eq!(written, 0, "an unchanged value writes nothing");
}

// Behaviour (#3320 representation): equal frozen content is one `objects` row (media 1, keyed by
// the SHA-256 of its bytes) shared by every resource naming it; different content is another row.
#[test]
fn equal_frozen_snapshots_share_one_object_row() {
    let mut store = store();
    let first = insert(&mut store, "a", &Convoy::sample(0));
    insert(&mut store, "b", &Convoy::sample(0));
    let conn = store.into_connection();
    let objects: Vec<(Vec<u8>, i64, Vec<u8>)> = conn
        .prepare("SELECT hash, media, bytes FROM objects")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(objects.len(), 1, "two convoys with equal snapshots share one object");
    let (hash, media, bytes) = &objects[0];
    assert_eq!((hash.as_slice(), *media), (&workflow(0).hash().0[..], MEDIA_CBOR));
    assert_eq!(Hash::of(bytes), workflow(0).hash());
    let distinct: i64 = conn.query_row("SELECT count(DISTINCT workflow_snapshot) FROM convoy", [], |row| row.get(0)).expect("count");
    assert_eq!(distinct, 1);

    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    insert(&mut store, "c", &Convoy::sample(1));
    assert_eq!(load(&mut store, &first).value.workflow_snapshot.value(), workflow(0).value());
    let conn = store.into_connection();
    let count: i64 = conn.query_row("SELECT count(*) FROM objects", [], |row| row.get(0)).expect("count");
    assert_eq!(count, 2, "different snapshots make two objects");
}

fn foreign_key_violations(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0)).expect("check")
}

/// Rows of every table of `K` owned by resource `id`.
fn owned_rows<K: Kind>(conn: &Connection, id: i64) -> Vec<(String, i64)> {
    K::table_set()
        .tables
        .iter()
        .map(|table| {
            let owner = if table.parent.is_none() { "id" } else { "owner" };
            let sql = format!("SELECT count(*) FROM {} WHERE {owner} = ?1", quote_ident(&table.name));
            (table.name.clone(), conn.query_row(&sql, [id], |row| row.get(0)).expect("count"))
        })
        .collect()
}

// Behaviour (DESIGN keys): removing a child row removes the rows nested under it (here an input
// whose list variant holds items, and a variant switch away from the list), leaving no orphans;
// deleting a resource removes its rows from every one of its tables.
#[test]
fn nested_rows_go_with_their_parent() {
    let mut store = store();
    let list = |items: &[&str]| InputValue::List { items: items.iter().map(|item| item.to_string()).collect() };
    let convoy = Convoy {
        inputs: [("a".to_owned(), list(&["x", "y"])), ("b".to_owned(), list(&["z"]))].into(),
        deliverables: vec![deliverable(vec![Evidence { metadata: [("k".to_owned(), "v".to_owned())].into(), ..evidence("e") }])].into(),
        ..Convoy::sample(0)
    };
    let current = insert(&mut store, "c", &convoy);
    let next = Convoy { inputs: [("b".to_owned(), InputValue::Text { value: "t".into() })].into(), ..convoy.clone() };
    update(&mut store, &current, &next);
    let conn = store.into_connection();
    let items: i64 = conn.query_row("SELECT count(*) FROM convoy_inputs_list_items", [], |row| row.get(0)).expect("count");
    assert_eq!(items, 0, "a removed input's items and a switched variant's items are gone");
    assert_eq!(foreign_key_violations(&conn), 0);

    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let current = load(&mut store, &current);
    assert_eq!(current.value, next);
    let mut write = store.write().expect("write");
    write.delete::<Convoy>(current.stamp()).expect("delete");
    write.commit().expect("commit");
    let conn = store.into_connection();
    for (table, rows) in owned_rows::<Convoy>(&conn, current.id.0) {
        assert_eq!(rows, 0, "{table} kept rows of a deleted convoy");
    }
    assert_eq!(foreign_key_violations(&conn), 0);
}

fn delete_cascades<K: Fixture>(tc: &TestCase) {
    let mut store = store();
    let mut loaded = Vec::new();
    for index in 0..K::COVERING {
        loaded.push(insert(&mut store, &format!("r{index}"), &K::generate(tc, Pick::Cover(index))));
    }
    let conn = store.into_connection();
    let before: i64 = loaded.iter().flat_map(|current| owned_rows::<K>(&conn, current.id.0)).map(|(_, rows)| rows).sum();
    let tables_with_rows = K::table_set().tables.iter().filter(|table| owned_rows_in(&conn, table) > 0).count();
    assert_eq!(tables_with_rows, K::table_set().tables.len(), "the covering values fill every table");
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let mut write = store.write().expect("write");
    for current in &loaded {
        write.delete::<K>(current.stamp()).expect("delete");
    }
    write.commit().expect("commit");
    let conn = store.into_connection();
    assert!(before > 0);
    for table in &K::table_set().tables {
        assert_eq!(owned_rows_in(&conn, table), 0, "{} kept rows of deleted resources", table.name);
    }
    assert_eq!(foreign_key_violations(&conn), 0);
}

fn owned_rows_in(conn: &Connection, table: &Table) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {}", quote_ident(&table.name)), [], |row| row.get(0)).expect("count")
}

// Behaviour (DESIGN keys): deleting a resource cascades through every table of its kind, at every
// depth, for every fixture; the covering values fill every table first.
#[hegel::test]
fn deleting_a_resource_empties_every_table(tc: TestCase) {
    delete_cascades::<Message>(&tc);
    delete_cascades::<Convoy>(&tc);
    delete_cascades::<TerminalSession>(&tc);
}
