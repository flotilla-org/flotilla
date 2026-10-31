//! Child, records and history tables and frozen objects through SQLite.

use std::collections::BTreeMap;

use flotilla_schema::{Edge, Hash, Kind, Records, State, Table, Timestamp};
use flotilla_schema_sqlite::quote_ident;
use flotilla_schema_sqlite::{Loaded, Stamp, Store, StoreError};
use flotilla_state_machines::{
    machines::{
        ensure::{EnsureSpec, EnsureState, MemoryEnsureWorld, Record},
        message::{Deliver, MemoryMessageWorld, MessageState, MessageWorld, Settle, Settlement},
    },
    Ref, Transport,
};
use hegel::TestCase;
use rusqlite::Connection;

use crate::fixtures::{
    addr,
    engine::{step, NAME},
    ensure, message, origin, registry, store,
    synthetic::{workflow, Deliverable, DeliverableState, Evidence, InputValue, PromiseKind, PromiseSource},
    AgentSession, AwaitingReply, Convoy, Dossier, Ensure, Fixture, Message, Person, Pick, Referral, Resource, TerminalSession, Vessel,
    LOCAL,
};

fn evidence(reference: &str) -> Evidence {
    Evidence { reference: reference.into(), submitted_at: Timestamp(3), verdict: None, metadata: BTreeMap::new() }
}

fn deliverable(evidence: Vec<Evidence>) -> Deliverable {
    Deliverable {
        label: "pr".into(),
        kind: PromiseKind::ChangeRequest,
        source: PromiseSource::Dispatcher,
        evidence: records(evidence),
        state: State::new(DeliverableState::Open, Timestamp(4)),
    }
}

/// Records of `rows` with placeholder stamps: the store stamps every row it appends.
fn records<T>(rows: Vec<T>) -> Records<T> {
    let mut records = Records::new();
    for row in rows {
        records.append(0, row).expect("room");
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
        rebuilt.append(version, record).expect("room");
    }
    rebuilt
}

fn insert<K: Kind>(store: &mut Store, name: &str, value: &K) -> Loaded<K> {
    let mut write = store.write().expect("write");
    let stamp = write.insert(name, value, &origin()).expect("insert");
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
// of its records row), and any append to, change of or removal from a records table (deliverables,
// evidence under a kept
// deliverable). A refused write changes nothing and the transaction stays usable: a spec edit and
// an engine edge then commit, the edge's event stamped with its version.
#[test]
fn spec_edits_refuse_what_only_edges_write() {
    let mut store = store();
    let ensure = ensure::sample();
    let current = insert(&mut store, "e", &ensure);
    let dossier = Dossier { deliverables: records(vec![deliverable(vec![evidence("e0"), evidence("e1")])]), ..Dossier::sample(0) };
    let dossier_current = insert(&mut store, "d", &dossier);
    let (ensure, dossier) = (&current.value, &dossier_current.value);
    let stamps = dossier.deliverables.iter().map(|row| row.version);
    assert_eq!(stamps.collect::<Vec<_>>(), [dossier_current.version], "the insert stamps every row");

    let mut write = store.write().expect("write");
    let held = EnsureState::Held { recorded: Default::default(), created: Default::default(), failures: 0, last_failure: None };
    let moved = ensure.clone().with_state(State::new(held, ensure.since()));
    assert_eq!(refused(write.update(&current, &moved, &origin())), "ensure.state");
    let later = ensure
        .clone()
        .with_state(State::new(ensure.state().clone(), Timestamp(9)))
        .with_spec(EnsureSpec { inbox: addr("edited"), ..ensure.spec().clone() });
    assert_eq!(refused(write.update(&current, &later, &origin())), "ensure.state_since", "a spec field beside it does not excuse it");
    let deliverables = |f: Edit<Deliverable>| Dossier { deliverables: with_rows(&dossier.deliverables, f), ..dossier.clone() };
    let retracted = deliverables(&|rows| rows[0].1.state = State::new(DeliverableState::Retracted, Timestamp(5)));
    assert_eq!(refused(write.update(&dossier_current, &retracted, &origin())), "dossier_deliverables", "only an edge moves a record");
    assert_eq!(refused(write.update(&dossier_current, &deliverables(&|rows| rows.clear()), &origin())), "dossier_deliverables");
    let promised = deliverables(&|rows| rows.push((0, deliverable(Vec::new()))));
    assert_eq!(refused(write.update(&dossier_current, &promised, &origin())), "dossier_deliverables");
    let submitted =
        deliverables(&|rows| rows[0].1.evidence = with_rows(&rows[0].1.evidence, |evidence| evidence.push((0, evidence_row()))));
    assert_eq!(refused(write.update(&dossier_current, &submitted, &origin())), "dossier_deliverables_evidence");
    let fewer_evidence = deliverables(&|rows| rows[0].1.evidence = with_rows(&rows[0].1.evidence, |evidence| drop(evidence.pop())));
    assert_eq!(refused(write.update(&dossier_current, &fewer_evidence, &origin())), "dossier_deliverables_evidence");

    // Allowed in the same transaction: a spec edit, then the engine's record edge appending its
    // event and growing the state's recorded set, whatever stamps the caller's rows hold.
    let edited = ensure.clone().with_spec(EnsureSpec { inbox: addr("edited"), ..ensure.spec().clone() });
    let stamp = write.update(&current, &edited, &origin()).expect("spec edit");
    let (edge, next) =
        step(&edited, "dave", Record { fulfiller: Ref::new("f") }, &MemoryEnsureWorld::default(), Timestamp(8)).expect("recorded");
    let stamp = write.apply_edge(stamp, edge.clone(), &flotilla_schema::NewChange::new("test", edge.at())).expect("edge");
    write.commit().expect("commit");
    let expected = next.stamped(Some(&edited), stamp.version).with_identity("e", stamp.version);
    assert_eq!(load(&mut store, &current).value, expected);
    assert!(expected.state().recorded().contains(&Ref::new("f")));
    assert_eq!(load(&mut store, &dossier_current).value, *dossier, "the refused writes changed nothing");
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
    write.update(current, next, &origin()).expect("update");
    write.commit().expect("commit");
}

fn apply_edge<K: Kind>(store: &mut Store, current: Stamp, edge: Edge<K>) {
    let mut write = store.write().expect("write");
    write.apply_edge(current, edge.clone(), &flotilla_schema::NewChange::new("w", edge.at())).expect("edge");
    write.commit().expect("commit");
}

// Behaviour (#3320 production shape, #3392 G8): a write touches only its rows, plus the home
// sequence, the identity's sequence and one log row, and the change it is part of (#4024). An engine edge moving the state is one row
// write (the one state statement that also moves the version and writes the label's data),
// whatever gate reads its document holds: the event is the log row. A spec edit
// changing one of three inputs is one update and the root version; an unchanged value writes
// nothing.
#[test]
fn writes_touch_only_their_rows() {
    let store = store();
    let dossier = Dossier {
        inputs: ["a", "b", "c"].into_iter().map(|key| (key.to_owned(), InputValue::Text { value: key.into() })).collect(),
        ..Dossier::sample(0)
    };
    let (mut store, _) = rows_written(store, |_| {});
    // The strings the edges write are interned first, so no `strings` row is counted below.
    for (n, (session, host)) in [("s", "h"), ("w", "w")].into_iter().enumerate() {
        let awaiting = MessageState::Awaiting { receiver: message::agent(session, host) };
        let mut write = store.write().expect("pre-intern");
        write
            .insert(
                &format!("pre{n}"),
                &message::sample().with_state(State::new(awaiting, Timestamp(0))),
                &flotilla_schema::NewChange::new("w", Timestamp(0)),
            )
            .expect("pre-intern writer and receiver domains");
        write.commit().expect("commit");
    }
    let current = insert(&mut store, "m", &message::sample());
    let dossier_current = insert(&mut store, "d", &dossier);

    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&Ref::new(NAME), [(message::agent("s", "h"), Transport::Turn)].into());
    world.reply.put(&Ref::new(NAME), ());
    let world: Box<dyn MessageWorld> = Box::new(world);
    let deliver = Deliver { receiver: message::agent("s", "h"), transport: Transport::Turn };
    let (edge, delivered) = step(&current.value, "w", deliver, &*world, Timestamp(5)).expect("delivered");
    let (mut store, written) = rows_written(store, |store| apply_edge(store, current.stamp(), edge));
    assert_eq!(
        written, 6,
        "the state with the version, the home and identity sequences, the log entry, and its change with the sequence it takes"
    );

    let current = load(&mut store, &current);
    let (edge, _) = step(&delivered, "w", Settle { by: Settlement::Answered }, &*world, Timestamp(6)).expect("settled");
    let (store, written) = rows_written(store, |store| apply_edge(store, current.stamp(), edge));
    assert_eq!(written, 6, "gate reads add no rows, and leaving a label's data is the same statement");

    let mut changed = dossier.clone();
    changed.inputs.insert("b".into(), InputValue::Text { value: "B".into() });
    let (mut store, written) = rows_written(store, |store| update(store, &dossier_current, &changed));
    assert_eq!(written, 7, "one input row, the root version, the home sequence, the identity sequence, the log entry, and its change with the sequence it takes");

    let dossier_current = load(&mut store, &dossier_current);
    let (_, written) = rows_written(store, |store| update(store, &dossier_current, &changed));
    assert_eq!(written, 0, "an unchanged value writes nothing");
}

// Behaviour (#3320 representation): equal frozen content is one `objects` row (keyed by the
// SHA-256 of its bytes, which are canonical CBOR, so no media column) shared by every resource naming it; different content is another row.
#[test]
fn equal_frozen_snapshots_share_one_object_row() {
    let mut store = store();
    let first = insert(&mut store, "a", &Dossier::sample(0));
    insert(&mut store, "b", &Dossier::sample(0));
    let conn = store.into_connection();
    let objects: Vec<(Vec<u8>, Vec<u8>)> = conn
        .prepare("SELECT hash, bytes FROM objects")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(objects.len(), 1, "two dossiers with equal snapshots share one object");
    let (hash, bytes) = &objects[0];
    assert_eq!(hash.as_slice(), &workflow(0).hash().0[..]);
    assert_eq!(Hash::of(bytes), workflow(0).hash());
    let distinct: i64 = conn.query_row("SELECT count(DISTINCT workflow) FROM dossier", [], |row| row.get(0)).expect("count");
    assert_eq!(distinct, 1);

    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    insert(&mut store, "c", &Dossier::sample(1));
    assert_eq!(load(&mut store, &first).value.workflow.value(), workflow(0).value());
    let conn = store.into_connection();
    let count: i64 = conn.query_row("SELECT count(*) FROM objects", [], |row| row.get(0)).expect("count");
    assert_eq!(count, 2, "different snapshots make two objects");
}

fn foreign_key_violations(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0)).expect("check")
}

/// Rows of every table of `K` owned by resource `id`.
fn owned_rows<K: Kind>(conn: &Connection, id: i64) -> Vec<(String, i64)> {
    let set = K::table_set();
    set.tables
        .iter()
        .map(|table| {
            let owner = if table.parent.is_none() { "id" } else { set.parent_key_column() };
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
    let dossier = Dossier {
        inputs: [("a".to_owned(), list(&["x", "y"])), ("b".to_owned(), list(&["z"]))].into(),
        deliverables: records(vec![deliverable(vec![Evidence { metadata: [("k".to_owned(), "v".to_owned())].into(), ..evidence("e") }])]),
        ..Dossier::sample(0)
    };
    let current = insert(&mut store, "c", &dossier);
    let next = Dossier { inputs: [("b".to_owned(), InputValue::Text { value: "t".into() })].into(), ..current.value.clone() };
    update(&mut store, &current, &next);
    let conn = store.into_connection();
    let items: i64 = conn.query_row("SELECT count(*) FROM dossier_inputs_items", [], |row| row.get(0)).expect("count");
    assert_eq!(items, 0, "a removed input's items and a switched variant's items are gone");
    assert_eq!(foreign_key_violations(&conn), 0);

    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let current = load(&mut store, &current);
    assert_eq!(current.value, next);
    let mut write = store.write().expect("write");
    write.delete::<Dossier>(current.stamp(), &origin()).expect("delete");
    write.commit().expect("commit");
    let conn = store.into_connection();
    for (table, rows) in owned_rows::<Dossier>(&conn, current.id.0) {
        assert_eq!(rows, 0, "{table} kept rows of a deleted dossier");
    }
    assert_eq!(foreign_key_violations(&conn), 0);
}

fn delete_cascades<K: Fixture>(tc: &TestCase) {
    let mut store = store();
    let mut loaded = Vec::new();
    for index in 0..K::COVERING {
        loaded.push(insert(&mut store, &format!("r{index}"), &K::generate(tc, Pick::Cover(index))));
    }
    let conn = store.connection();
    let before: i64 = loaded.iter().flat_map(|current| owned_rows::<K>(conn, current.id.0)).map(|(_, rows)| rows).sum();
    let tables_with_rows = K::table_set().tables.iter().filter(|table| owned_rows_in(conn, table) > 0).count();
    assert_eq!(tables_with_rows, K::table_set().tables.len(), "the covering values fill every table");
    let mut write = store.write().expect("write");
    for current in &loaded {
        write.delete::<K>(current.stamp(), &origin()).expect("delete");
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
// depth, for every fixture; the covering values fill every table first. One test per kind, so
// they run in parallel.
#[hegel::test]
fn deleting_a_resource_empties_every_table_message(tc: TestCase) {
    delete_cascades::<Resource<Message>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_agent_session(tc: TestCase) {
    delete_cascades::<Resource<AgentSession>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_person(tc: TestCase) {
    delete_cascades::<Resource<Person>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_awaiting_reply(tc: TestCase) {
    delete_cascades::<Resource<AwaitingReply>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_convoy(tc: TestCase) {
    delete_cascades::<Resource<Convoy>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_ensure(tc: TestCase) {
    delete_cascades::<Resource<Ensure>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_terminal_session(tc: TestCase) {
    delete_cascades::<TerminalSession>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_vessel(tc: TestCase) {
    delete_cascades::<Resource<Vessel>>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_referral(tc: TestCase) {
    delete_cascades::<Referral>(&tc);
}

#[hegel::test]
fn deleting_a_resource_empties_every_table_dossier(tc: TestCase) {
    delete_cascades::<Dossier>(&tc);
}
