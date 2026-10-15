//! Edge writes (#3320 ruling 1): `Write::apply_edge` against the fixtures.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use flotilla_schema::{Edge, Frozen, Kind, Position, Records, Row, RowChange, RowOp, State, TableRole, Timestamp, Value};
use flotilla_schema_sqlite::{Applied, ResourceId, Snapshot, Stamp, Store, StoreError};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

use crate::fixtures::{
    convoy::{
        ConvoyEdge, ConvoyEdgeEvent, ConvoyState, Deliverable, DeliverableState, Evidence, PlacementSnapshot, PromiseKind, PromiseSource,
    },
    registry, store, Convoy, Fixture, Message, Pick, TerminalSession, LOCAL,
};

fn snapshot(store: &mut Store, id: ResourceId) -> Snapshot {
    store.read().expect("read").load_snapshot(id).expect("snapshot").expect("present")
}

/// Rows of every table by key.
fn by_key(snapshot: &Snapshot) -> Vec<BTreeMap<Vec<Value>, Vec<Value>>> {
    snapshot.rows.tables.iter().map(|rows| rows.iter().map(|Row { key, values }| (key.clone(), values.clone())).collect()).collect()
}

/// Rows of the shared `strings` and `resource_identity` tables, which writes add to when a value
/// names a new string or resource.
fn interned(conn: &Connection) -> u64 {
    conn.query_row("SELECT (SELECT count(*) FROM strings) + (SELECT count(*) FROM resource_identity)", [], |row| row.get(0)).expect("count")
}

/// Applies `edge` and returns the change it logged and the rows SQLite counted as written,
/// excluding new `strings` and `resource_identity` rows.
fn apply_counted<K: Kind>(store: Store, current: Stamp, edge: Edge<K>) -> (Store, Stamp, RowChange, u64) {
    let conn = store.into_connection();
    let before = (conn.total_changes(), interned(&conn));
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let mut write = store.write().expect("write");
    let stamp = write.apply_edge(current, edge).expect("edge");
    let mut changes = write.changes(current.id, current.version, 10).expect("changes");
    write.commit().expect("commit");
    let conn = store.into_connection();
    let written = conn.total_changes() - before.0 - (interned(&conn) - before.1);
    assert_eq!(changes.len(), 1, "one change per edge");
    (Store::open(conn, LOCAL, registry()).expect("reopen"), stamp, changes.remove(0), written)
}

fn touches_only_declared_rows<K: Fixture>(tc: &TestCase) {
    let set = K::table_set();
    let table = |name: &str| set.tables.iter().position(|table| table.name == name).expect("a table of the kind");
    let mut store = store();
    let mut write = store.write().expect("write");
    let mut stamp = write.insert("r", &K::generate(tc, Pick::Draw)).expect("insert");
    write.commit().expect("commit");
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(4)) {
        let current = store.read().expect("read").load::<K>(stamp.id).expect("load").expect("present");
        let before = snapshot(&mut store, stamp.id);
        let (edge, _) = current.value.edge(tc, Pick::Draw);
        let (next_store, next, change, written) = apply_counted(store, stamp, edge);
        store = next_store;
        let after = snapshot(&mut store, stamp.id);
        assert_eq!((change.home_seq, after.version), (next.version, next.version));
        let stamped = Value::Int(i64::try_from(next.version).expect("stored"));

        // The logged ops name exactly the rows that changed, and hold their new values.
        let (old, new) = (by_key(&before), by_key(&after));
        let mut inserted = BTreeSet::new();
        let mut set_rows: BTreeMap<(usize, Vec<Value>), Vec<Range<usize>>> = BTreeMap::new();
        let mut root_set = false;
        for op in &change.ops {
            match op {
                RowOp::PutObject { .. } => {}
                RowOp::Insert { table: name, key, values } => {
                    let index = table(name);
                    assert!(!old[index].contains_key(key), "{name} {key:?} existed before its insert");
                    assert_eq!(new[index].get(key), Some(values), "{name} {key:?} holds the logged values");
                    if matches!(set.tables[index].role, TableRole::Records | TableRole::History) {
                        assert_eq!(values[0], stamped, "{name} row stamped with the edge's version");
                    }
                    inserted.insert((index, key.clone()));
                }
                RowOp::Set { table: name, key, field, values } => {
                    let index = table(name);
                    root_set |= index == 0;
                    let columns =
                        set.tables[index].fields.iter().find(|candidate| &candidate.name == field).expect("declared").columns.clone();
                    assert_eq!(&new[index][key][columns.clone()], &values[..], "{name}.{field} holds the logged values");
                    set_rows.entry((index, key.clone())).or_default().push(columns);
                }
                other => panic!("an edge only appends and sets: {other:?}"),
            }
        }
        for (index, (old, new)) in old.iter().zip(&new).enumerate() {
            assert!(old.keys().all(|key| new.contains_key(key)), "{}: an edge removes no row", set.tables[index].name);
            for (key, values) in new {
                let Some(before) = old.get(key) else {
                    assert!(inserted.contains(&(index, key.clone())), "{} {key:?} appeared without an insert", set.tables[index].name);
                    continue;
                };
                let declared = set_rows.get(&(index, key.clone())).cloned().unwrap_or_default();
                for (column, (before, after)) in before.iter().zip(values).enumerate() {
                    if before != after {
                        assert!(
                            declared.iter().any(|columns| columns.contains(&column)),
                            "{} {key:?} column {column} changed outside the edge's fields",
                            set.tables[index].name
                        );
                    }
                }
            }
        }
        // One statement per appended row and per set field; the root's version moves with its
        // first field, or by itself; then the home and identity sequences and the change log.
        let statements = inserted.len() + set_rows.values().map(Vec::len).sum::<usize>() + usize::from(!root_set) + 3;
        assert_eq!(written, statements as u64, "rows written by {change:?}");
        stamp = next;
    }
}

// Behaviour (#3320 ruling 1): an edge write touches exactly its declared rows: every row it appends
// (event, declared appends and the rows nested in them) is new, holds the logged values and, in a
// records or history table, the edge's version; every other row that changed is one a logged `Set`
// names, and only within that field's columns, which hold the logged values; no row disappears.
// SQLite counts one row write per op, plus the root version when no root field is set, plus the
// home sequence, identity sequence and change-log rows.
#[hegel::test(test_cases = 60)]
fn an_edge_touches_exactly_its_declared_rows(tc: TestCase) {
    touches_only_declared_rows::<Message>(&tc);
    touches_only_declared_rows::<Convoy>(&tc);
    touches_only_declared_rows::<TerminalSession>(&tc);
}

fn evidence(reference: &str) -> Evidence {
    Evidence { reference: reference.into(), submitted_at: Timestamp(3), verdict: None, metadata: [("k".to_owned(), "v".to_owned())].into() }
}

fn deliverable() -> Deliverable {
    Deliverable {
        label: "pr".into(),
        kind: PromiseKind::ChangeRequest,
        role: "coder".into(),
        source: PromiseSource::Dispatcher,
        evidence: Records::new(),
        state: State::new(DeliverableState::Open, Timestamp(4)),
    }
}

fn event(edge: ConvoyEdge) -> ConvoyEdgeEvent {
    ConvoyEdgeEvent { edge, at: Timestamp(6), writer: "alice".into() }
}

fn load(store: &mut Store, id: ResourceId) -> Convoy {
    store.read().expect("read").load::<Convoy>(id).expect("load").expect("present").value
}

// Behaviour (#3320 ruling 1, #3208 §4.1): a deliverable's state moves through an edge on its
// records row (`set_in`), and Submit appends evidence under a deliverable (`append_in`) at the next
// position under that deliverable, with the rows nested in it; each edge appends its event at the
// convoy's next event position. Every appended row carries its edge's version while the rows it
// sits beside keep theirs. An edge on a deliverable that does not exist is `NoRecord` and writes
// nothing; the transaction stays usable.
#[test]
fn deliverables_move_and_take_evidence_through_edges() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let mut deliverables = Records::new();
    let mut first = deliverable();
    first.evidence.append(0, evidence("e0"));
    deliverables.append(0, first);
    deliverables.append(0, deliverable());
    let inserted = write.insert("c", &Convoy { deliverables, ..Convoy::sample(0) }).expect("insert");

    let submit = Edge::new(Convoy::EDGES, &event(ConvoyEdge::Submit { deliverable: 0 }))
        .and_then(|edge| edge.append_in(Convoy::DELIVERABLES, Position(0), Deliverable::EVIDENCE, &evidence("e1")))
        .expect("edge");
    let submitted = write.apply_edge(inserted, submit).expect("submit");
    let retracted_state = State::new(DeliverableState::Retracted, Timestamp(7));
    let retract = Edge::new(Convoy::EDGES, &event(ConvoyEdge::Retract { deliverable: 1, reason: "dropped".into() }))
        .and_then(|edge| edge.set_in(Convoy::DELIVERABLES, Position(1), Deliverable::STATE, &retracted_state))
        .expect("edge");
    let retracted = write.apply_edge(submitted, retract).expect("retract");

    let missing = |position| {
        Edge::new(Convoy::EDGES, &event(ConvoyEdge::Submit { deliverable: position }))
            .and_then(|edge| edge.append_in(Convoy::DELIVERABLES, Position(position), Deliverable::EVIDENCE, &evidence("e9")))
            .expect("edge")
    };
    let refused = write.apply_edge(retracted, missing(2));
    assert!(
        matches!(&refused, Err(StoreError::NoRecord { table, key }) if table == "convoy_deliverables" && key == &[Value::Int(2)]),
        "{refused:?}"
    );
    let retract_missing = Edge::new(Convoy::EDGES, &event(ConvoyEdge::Retract { deliverable: 5, reason: String::new() }))
        .and_then(|edge| edge.set_in(Convoy::DELIVERABLES, Position(5), Deliverable::STATE, &retracted_state))
        .expect("edge");
    let refused = write.apply_edge(retracted, retract_missing);
    assert!(matches!(&refused, Err(StoreError::NoRecord { table, .. }) if table == "convoy_deliverables"), "{refused:?}");
    let delivered = Edge::new(Convoy::EDGES, &event(ConvoyEdge::Deliver))
        .and_then(|edge| edge.set(Convoy::STATE, &State::new(ConvoyState::Delivered, Timestamp(8))))
        .expect("edge");
    let last = write.apply_edge(retracted, delivered).expect("a refused edge leaves the transaction usable");
    write.commit().expect("commit");

    let convoy = load(&mut store, inserted.id);
    let versions: Vec<u64> = convoy.edges.iter().map(|row| row.version).collect();
    assert_eq!(versions, [submitted.version, retracted.version, last.version], "one event per edge, in order");
    let evidence_rows: Vec<(u32, u64, &str)> = convoy.deliverables.rows()[0]
        .record
        .evidence
        .iter()
        .map(|row| (row.position.0, row.version, row.record.reference.as_str()))
        .collect();
    assert_eq!(evidence_rows, [(0, inserted.version, "e0"), (1, submitted.version, "e1")]);
    assert_eq!(convoy.deliverables.rows()[0].record.evidence.rows()[1].record.metadata, evidence("e1").metadata, "nested rows come along");
    assert_eq!(convoy.deliverables.rows()[1].record.state, retracted_state);
    assert_eq!(convoy.deliverables.rows()[1].version, inserted.version, "a record keeps its stamp when an edge sets its fields");
    assert_eq!(convoy.deliverables.rows()[0].record.state, deliverable().state);
    assert_eq!(convoy.state.value, ConvoyState::Delivered);
}

// Behaviour (DESIGN writes, #3320 ruling 1): an edge is owner-checked and optimistic like a spec
// edit: a stale stamp is `Conflict`, a deleted resource `NotFound`, a resource homed elsewhere
// `NotHome`; none of them writes anything.
#[test]
fn edges_are_owner_checked_and_optimistic() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("c", &Convoy::sample(0)).expect("insert");
    let edge = || Edge::new(Convoy::EDGES, &event(ConvoyEdge::Deliver)).expect("edge");
    let moved = write.apply_edge(stamp, edge()).expect("edge");
    assert!(matches!(write.apply_edge(stamp, edge()), Err(StoreError::Conflict { .. })));
    write.delete::<Convoy>(moved).expect("delete");
    assert!(matches!(write.apply_edge(moved, edge()), Err(StoreError::NotFound { .. })));
    write.commit().expect("commit");

    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open");
    let mut write = remote.write().expect("write");
    write.insert("far", &Convoy::sample(0)).expect("insert");
    write.commit().expect("commit");
    let change = remote.read().expect("read").changes_since("remote", 0, 1).expect("log").remove(0);
    let mut write = store.write().expect("write");
    let Applied::Applied(far) = write.apply(&change).expect("apply") else { panic!("applied") };
    // A stale stamp is `Conflict` even when the edge is also wrong in another way (here, an
    // append under a records row that does not exist): retrying callers re-read on `Conflict`.
    let current = write.insert("stale", &Convoy::sample(0)).expect("insert");
    let both_wrong = || {
        Edge::new(Convoy::EDGES, &event(ConvoyEdge::Submit { deliverable: 9 }))
            .and_then(|edge| edge.append_in(Convoy::DELIVERABLES, Position(9), Deliverable::EVIDENCE, &evidence("e")))
            .expect("edge")
    };
    write.apply_edge(current, edge()).expect("edge");
    assert!(matches!(write.apply_edge(current, both_wrong()), Err(StoreError::Conflict { .. })));
    assert!(matches!(write.apply_edge(far, edge()), Err(StoreError::NotHome { home, .. }) if home == "remote"));
}

// Behaviour (ruling 1, both version branches): every edge appends its event, so the root row is
// touched either by a declared root field (whose statement also moves the version) or, for an
// edge that sets no root field, by the version move alone. Each branch moves the version to the
// write's sequence, logs a root `Set` only when it set a root field, and refuses a stale stamp.
#[test]
fn edges_move_the_version_with_and_without_a_root_field() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let inserted = write.insert("c", &Convoy::sample(0)).expect("insert");
    write.commit().expect("commit");
    let with_root = || {
        Edge::new(Convoy::EDGES, &event(ConvoyEdge::Deliver))
            .and_then(|edge| edge.set(Convoy::STATE, &State::new(ConvoyState::Delivered, Timestamp(3))))
            .expect("edge")
    };
    let without_root = || Edge::new(Convoy::EDGES, &event(ConvoyEdge::Submit { deliverable: 0 })).expect("edge");
    let mut current = inserted;
    for (root_set, edge) in [(true, with_root as fn() -> Edge<Convoy>), (false, without_root)] {
        let mut write = store.write().expect("write");
        let moved = write.apply_edge(current, edge()).expect("edge");
        assert!(moved.version > current.version);
        assert!(matches!(write.apply_edge(current, edge()), Err(StoreError::Conflict { .. })), "stale stamp, root set {root_set}");
        write.commit().expect("commit");
        assert_eq!(snapshot(&mut store, moved.id).version, moved.version);
        let change = store.read().expect("read").changes(moved.id, current.version, 10).expect("log").remove(0);
        let logged_root_set = change.ops.iter().any(|op| matches!(op, RowOp::Set { table, .. } if table == "convoy"));
        assert_eq!(logged_root_set, root_set);
        current = moved;
    }
}

// Behaviour (DESIGN change log, self-contained changes): an edge that sets a frozen field carries
// the object as a `PutObject`, even one the store already holds, so its change applies to a
// replica holding only the resource's earlier state; an edge setting it to `None` carries none.
#[test]
fn edges_carry_the_objects_they_set() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("a", &Convoy { placement_snapshot: Some(placement()), ..Convoy::sample(0) }).expect("insert a");
    let b = write.insert("b", &Convoy::sample(0)).expect("insert b");
    let placed = Edge::new(Convoy::EDGES, &event(ConvoyEdge::Deliver))
        .and_then(|edge| edge.set(Convoy::PLACEMENT_SNAPSHOT, &Some(placement())))
        .expect("edge");
    let b = write.apply_edge(b, placed).expect("edge");
    let cleared =
        Edge::new(Convoy::EDGES, &event(ConvoyEdge::Deliver)).and_then(|edge| edge.set(Convoy::PLACEMENT_SNAPSHOT, &None)).expect("edge");
    let b = write.apply_edge(b, cleared).expect("edge");
    write.commit().expect("commit");
    let changes = source.read().expect("read").changes(b.id, 0, 10).expect("log");
    let puts: Vec<usize> =
        changes.iter().map(|change| change.ops.iter().filter(|op| matches!(op, RowOp::PutObject { .. })).count()).collect();
    assert_eq!(puts, [1, 1, 0], "the insert's workflow, the edge's placement, none for clearing it");
    assert!(matches!(changes[1].ops[0], RowOp::PutObject { hash, .. } if hash == placement().hash()), "objects first");

    let mut replica = Store::open(Connection::open_in_memory().expect("db"), "replica", registry()).expect("open");
    let mut write = replica.write().expect("write");
    for change in &changes[..2] {
        assert!(matches!(write.apply(change).expect("apply"), Applied::Applied(_)));
    }
    let id = write.resolve(Convoy::CODE, "b", LOCAL).expect("resolve").expect("present");
    let value = write.load::<Convoy>(id).expect("load").expect("present").value;
    assert_eq!(value.placement_snapshot.map(|frozen| frozen.hash()), Some(placement().hash()));
}

fn placement() -> Frozen<PlacementSnapshot> {
    Frozen::new(PlacementSnapshot { host: "h".into(), labels: BTreeMap::new() }).expect("freeze")
}
