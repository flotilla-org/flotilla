//! Child, records and history tables and frozen objects through SQLite.

use std::collections::BTreeMap;

use flotilla_schema::{quote_ident, Edge, EdgeEvent, Hash, Identity, Kind, Records, Ref, State, Table, Timestamp};
use flotilla_schema_sqlite::{Loaded, Stamp, Store, StoreError};
use hegel::TestCase;
use rusqlite::Connection;

use crate::fixtures::{
    attempt::{Attempt, Outcome},
    convoy::{workflow, ConvoyRepository, Deliverable, DeliverableState, Evidence, InputValue, PromiseKind, PromiseSource},
    message::{MessageEdge, MessageEdgeEvent, MessageState, Settlement},
    registry, store, Convoy, Fixture, GateRead, Message, Payload, Pick, TerminalSession, LOCAL,
};

/// A settle edge's event, from a sample message's `Pending` to `Awaiting`.
fn event(writer: &str) -> MessageEdgeEvent {
    let args = MessageEdge::Settle { by: Settlement::Answered { reply: Ref::new("r", LOCAL) } };
    EdgeEvent {
        edge: args.name(),
        from: MessageState::Pending,
        to: MessageState::Awaiting,
        writer: writer.into(),
        at: Timestamp(1),
        payload: Payload { args, reads: Vec::new() },
    }
}

fn read() -> GateRead {
    GateRead { observed: Identity { kind: Message::CODE, name: "m".into(), home: LOCAL.into() }, version: 1 }
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
        evidence: records(evidence),
        state: State::new(DeliverableState::Open, Timestamp(4)),
    }
}

/// Records of `rows` with placeholder stamps: the store stamps every row it appends.
fn records<T>(rows: Vec<T>) -> Records<T> {
    let mut records = Records::new();
    for row in rows {
        records.append(0, row);
    }
    records
}

/// An edit of records as (stamp, record) pairs; see [`with_rows`].
type Edit<'a, T> = &'a dyn Fn(&mut Vec<(u64, T)>);

/// `records` rebuilt from its rows, as (stamp, record) pairs, after `f` edits them: how a test
/// writes a change the record type itself never makes, to see the store refuse it.
fn with_rows<T: Clone>(records: &Records<T>, f: impl FnOnce(&mut Vec<(u64, T)>)) -> Records<T> {
    let mut rows: Vec<(u64, T)> = records.iter().map(|row| (row.version, row.record.clone())).collect();
    f(&mut rows);
    let mut rebuilt = Records::new();
    for (version, record) in rows {
        rebuilt.append(version, record);
    }
    rebuilt
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

fn refused(result: Result<Stamp, StoreError>) -> String {
    match result {
        Err(StoreError::NotThroughEdge { table, column: Some(column) }) => format!("{table}.{column}"),
        Err(StoreError::NotThroughEdge { table, column: None }) => table,
        other => format!("{other:?}"),
    }
}

// Behaviour (#3320 ruling 1): a spec edit (`update`) refuses what only an edge writes, naming the
// table or column: a changed state column (`state`, `state_since`; a deliverable's state is part
// of its records row), and any append to, change of or removal from a history table (edge events,
// their documents' gate reads, attempts) or a records table (deliverables, evidence under a kept
// deliverable). A refused write changes nothing and the transaction stays usable: a spec edit and
// an edge then commit, the edge's rows stamped with its version.
#[test]
fn spec_edits_refuse_what_only_edges_write() {
    let mut store = store();
    let message =
        Message { edges: records(vec![event("alice"), event("bob")]), attempts: records(vec![attempt("a")]), ..Message::sample() };
    let current = insert(&mut store, "m", &message);
    let convoy = Convoy { deliverables: records(vec![deliverable(vec![evidence("e0"), evidence("e1")])]), ..Convoy::sample(0) };
    let convoy_current = insert(&mut store, "c", &convoy);
    let (message, convoy) = (&current.value, &convoy_current.value);
    let stamps = message.edges.iter().map(|row| row.version).chain(convoy.deliverables.iter().map(|row| row.version));
    assert_eq!(stamps.collect::<Vec<_>>(), [current.version, current.version, convoy_current.version], "the insert stamps every row");

    let mut write = store.write().expect("write");
    let edges = |f: Edit<MessageEdgeEvent>| Message { edges: with_rows(&message.edges, f), ..message.clone() };
    assert_eq!(refused(write.update(&current, &edges(&|rows| rows.push((0, event("carol")))))), "message_edges", "no append");
    assert_eq!(refused(write.update(&current, &edges(&|rows| drop(rows.pop())))), "message_edges");
    assert_eq!(refused(write.update(&current, &edges(&|rows| rows[1].1 = event("carol")))), "message_edges");
    assert_eq!(refused(write.update(&current, &edges(&|rows| rows[1].0 += 1))), "message_edges", "the stamp is part of the row");
    let with_read = edges(&|rows| rows[0].1.payload.reads.push(read()));
    assert_eq!(refused(write.update(&current, &with_read)), "message_edges", "gate reads are part of the edge row");
    let more_attempts = Message { attempts: with_rows(&message.attempts, |rows| rows.push((0, attempt("b")))), ..message.clone() };
    assert_eq!(refused(write.update(&current, &more_attempts)), "message_attempts");
    let fewer_attempts = Message { attempts: Records::new(), ..message.clone() };
    assert_eq!(refused(write.update(&current, &fewer_attempts)), "message_attempts");
    let settled = Message { state: State::new(MessageState::Settled, message.state.since), ..message.clone() };
    assert_eq!(refused(write.update(&current, &settled)), "message.state");
    let later = Message { state: State::new(message.state.value, Timestamp(9)), body: "edited".into(), ..message.clone() };
    assert_eq!(refused(write.update(&current, &later)), "message.state_since", "a spec field beside it does not excuse it");
    let deliverables = |f: Edit<Deliverable>| Convoy { deliverables: with_rows(&convoy.deliverables, f), ..convoy.clone() };
    let retracted = deliverables(&|rows| rows[0].1.state = State::new(DeliverableState::Retracted, Timestamp(5)));
    assert_eq!(refused(write.update(&convoy_current, &retracted)), "convoy_deliverables", "only an edge moves a record");
    assert_eq!(refused(write.update(&convoy_current, &deliverables(&|rows| rows.clear()))), "convoy_deliverables");
    let promised = deliverables(&|rows| rows.push((0, deliverable(Vec::new()))));
    assert_eq!(refused(write.update(&convoy_current, &promised)), "convoy_deliverables");
    let submitted =
        deliverables(&|rows| rows[0].1.evidence = with_rows(&rows[0].1.evidence, |evidence| evidence.push((0, evidence_row()))));
    assert_eq!(refused(write.update(&convoy_current, &submitted)), "convoy_deliverables_evidence");
    let fewer_evidence = deliverables(&|rows| rows[0].1.evidence = with_rows(&rows[0].1.evidence, |evidence| drop(evidence.pop())));
    assert_eq!(refused(write.update(&convoy_current, &fewer_evidence)), "convoy_deliverables_evidence");

    // Allowed in the same transaction: a spec edit, then an edge appending an event and an attempt
    // and moving the state, whatever stamps the caller's rows hold.
    let edited = Message { body: "edited".into(), ..message.clone() };
    let stamp = write.update(&current, &edited).expect("spec edit");
    let awaiting = State::new(MessageState::Awaiting, Timestamp(8));
    let edge = Edge::new(Message::STATE, &awaiting, Message::EDGES, &event("dave"))
        .and_then(|edge| edge.append(Message::ATTEMPTS, &attempt("b")))
        .expect("edge");
    let stamp = write.apply_edge(stamp, edge).expect("edge");
    write.commit().expect("commit");
    let expected = Message {
        edges: with_rows(&message.edges, |rows| rows.push((stamp.version, event("dave")))),
        attempts: with_rows(&message.attempts, |rows| rows.push((stamp.version, attempt("b")))),
        state: awaiting,
        ..edited
    };
    assert_eq!(load(&mut store, &current).value, expected);
    assert_eq!(load(&mut store, &convoy_current).value, *convoy, "the refused writes changed nothing");
}

fn evidence_row() -> Evidence {
    evidence("e2")
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

fn apply_edge<K: Kind>(store: &mut Store, current: Stamp, edge: Edge<K>) {
    let mut write = store.write().expect("write");
    write.apply_edge(current, edge).expect("edge");
    write.commit().expect("commit");
}

// Behaviour (#3320 production shape): a write touches only its rows, plus the home sequence, the
// identity's sequence and one change-log row. An edge appending its event and moving the state is
// two row writes (the event insert, and the state update that also moves the version), with or
// without gate reads, which are inside the event's document. A spec edit
// changing one of three repositories is one update and the root version; an unchanged value
// writes nothing.
#[test]
fn writes_touch_only_their_rows() {
    let store = store();
    let repository = |slug: &str| ConvoyRepository {
        url: "u".into(),
        repo_key: "k".into(),
        source_ref: "s".into(),
        target_ref: "t".into(),
        workspace_slug: slug.into(),
    };
    let convoy = Convoy { repositories: vec![repository("a"), repository("b"), repository("c")], ..Convoy::sample(0) };
    let (mut store, _) = rows_written(store, |_| {});
    let current = insert(&mut store, "m", &Message { edges: records(vec![event("alice")]), ..Message::sample() });
    let convoy_current = insert(&mut store, "c", &convoy);

    let moved = State::new(MessageState::Awaiting, Timestamp(5));
    let edge = Edge::new(Message::STATE, &moved, Message::EDGES, &event("alice")).expect("edge");
    let (mut store, written) = rows_written(store, |store| apply_edge(store, current.stamp(), edge));
    assert_eq!(written, 5, "the event row, the state with the version, the home and identity sequences and the change log");

    let current = load(&mut store, &current);
    // "alice" is already interned, so no `strings` row is written.
    let mut with_reads = event("alice");
    with_reads.payload.reads = vec![read(), read()];
    // The message is Awaiting now, and an edge leaves the stored label (#3373 ruling 5).
    with_reads.from = MessageState::Awaiting;
    let edge = Edge::new(Message::STATE, &moved, Message::EDGES, &with_reads).expect("edge");
    let (store, written) = rows_written(store, |store| apply_edge(store, current.stamp(), edge));
    assert_eq!(written, 5, "gate reads add no rows: they are in the event's document");

    let changed = Convoy { repositories: vec![repository("a"), repository("B"), repository("c")], ..convoy.clone() };
    let (mut store, written) = rows_written(store, |store| update(store, &convoy_current, &changed));
    assert_eq!(written, 5, "one repository row, the root version, the home sequence, the identity sequence and the change log");

    let convoy_current = load(&mut store, &convoy_current);
    let (_, written) = rows_written(store, |store| update(store, &convoy_current, &changed));
    assert_eq!(written, 0, "an unchanged value writes nothing");
}

// Behaviour (#3320 representation): equal frozen content is one `objects` row (keyed by the
// SHA-256 of its bytes, which are canonical CBOR, so no media column) shared by every resource naming it; different content is another row.
#[test]
fn equal_frozen_snapshots_share_one_object_row() {
    let mut store = store();
    let first = insert(&mut store, "a", &Convoy::sample(0));
    insert(&mut store, "b", &Convoy::sample(0));
    let conn = store.into_connection();
    let objects: Vec<(Vec<u8>, Vec<u8>)> = conn
        .prepare("SELECT hash, bytes FROM objects")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(objects.len(), 1, "two convoys with equal snapshots share one object");
    let (hash, bytes) = &objects[0];
    assert_eq!(hash.as_slice(), &workflow(0).hash().0[..]);
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
        deliverables: records(vec![deliverable(vec![Evidence { metadata: [("k".to_owned(), "v".to_owned())].into(), ..evidence("e") }])]),
        ..Convoy::sample(0)
    };
    let current = insert(&mut store, "c", &convoy);
    let next = Convoy { inputs: [("b".to_owned(), InputValue::Text { value: "t".into() })].into(), ..current.value.clone() };
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
