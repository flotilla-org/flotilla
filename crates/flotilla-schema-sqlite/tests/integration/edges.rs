//! Edge writes (#3320 ruling 1): `Write::apply_edge` against the fixtures: the engine kinds' edges
//! (transitions the engine applied) and the synthetic kinds' (code-enum states, records rows).

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use flotilla_schema::{
    Cause, Change, Edge, EdgeError, EdgeEvent, Event, Frozen, Kind, Named, Position, Records, Row, State, Step, TableRole, TableSet,
    Timestamp, Value,
};
use flotilla_schema_sqlite::{Applied, ResourceId, Snapshot, Stamp, Store, StoreError};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

use crate::fixtures::{
    origin, registry, store,
    synthetic::{
        Deliverable, DeliverableState, DossierEdge, DossierEvent, DossierState, Evidence, PlacementSnapshot, PromiseKind, PromiseSource,
    },
    AgentSession, Convoy, Dossier, Ensure, Fixture, Message, Payload, Pick, Referral, Resource, TerminalSession, LOCAL,
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

/// What one change of an effect addresses, in the kind's table numbering: the table, the row's full
/// key, and the columns it writes with their values (all of a put or appended row's, none for a
/// removal).
#[derive(Debug)]
struct Touch {
    table: usize,
    key: Vec<Value>,
    columns: Vec<(usize, Value)>,
}

/// The table and full key a row path addresses.
fn row_of(set: &TableSet, path: &[Step]) -> (usize, Vec<Value>) {
    let (mut table, mut key) = (0, Vec::new());
    for step in path {
        table = set.tables.iter().position(|candidate| candidate.parent == Some(table) && candidate.field == step.field).expect("a table");
        key.extend(own_key(set, table, &step.key));
    }
    (table, key)
}

fn own_key(set: &TableSet, table: usize, named: &Named) -> Vec<Value> {
    let table = &set.tables[table];
    table.key[table.key.len() - table.own_key..].iter().map(|column| named.get(&column.name).cloned().expect("an own key column")).collect()
}

/// `named` as (column index, value) of `table`'s data columns, nulls filled in over `range`.
fn columns(set: &TableSet, table: usize, range: Range<usize>, named: &Named) -> Vec<(usize, Value)> {
    let columns = &set.tables[table].columns;
    for name in named.keys() {
        assert!(columns[range.clone()].iter().any(|column| &column.name == name), "{name} is a column of the change");
    }
    range.map(|index| (index, named.get(&columns[index].name).cloned().unwrap_or(Value::Null))).collect()
}

/// What `change` writes, by the descriptor's names: the test's own reading of an effect.
fn touch(set: &TableSet, change: &Change, sequence: i64) -> Touch {
    let child = |parent: usize, field: &str| {
        set.tables.iter().position(|table| table.parent == Some(parent) && table.field == field).expect("a collection")
    };
    match change {
        Change::Set { row, field, values } | Change::State { row, field, values } => {
            let (table, key) = row_of(set, row);
            let fields = &set.tables[table].fields;
            let range = |name: &str| fields.iter().find(|candidate| candidate.name == name).expect("a field").columns.clone();
            let mut span = range(field);
            if matches!(change, Change::State { .. }) {
                for datum in fields.iter().filter(|datum| datum.datum_of.as_deref() == Some(field.as_str())) {
                    span = span.start.min(datum.columns.start)..span.end.max(datum.columns.end);
                }
            }
            Touch { table, key, columns: columns(set, table, span, values) }
        }
        Change::Put { row, collection, key, values } => {
            let (parent, mut full) = row_of(set, row);
            let table = child(parent, collection);
            full.extend(own_key(set, table, key));
            Touch { table, key: full, columns: columns(set, table, 0..set.tables[table].columns.len(), values) }
        }
        Change::Remove { row, collection, key } => {
            let (parent, mut full) = row_of(set, row);
            let table = child(parent, collection);
            full.extend(own_key(set, table, key));
            Touch { table, key: full, columns: Vec::new() }
        }
        Change::Append { row, records, position, values } => {
            let (parent, mut full) = row_of(set, row);
            let table = child(parent, records);
            full.push(Value::Int(i64::from(*position)));
            let mut columns = columns(set, table, 1..set.tables[table].columns.len(), values);
            columns.insert(0, (0, Value::Int(sequence)));
            Touch { table, key: full, columns }
        }
    }
}

/// Applies `edge` and returns the event it logged and the rows SQLite counted as written,
/// excluding new `strings` and `resource_identity` rows.
fn apply_counted<K: Kind>(store: Store, current: Stamp, edge: Edge<K>) -> (Store, Stamp, Event, u64) {
    let conn = store.into_connection();
    let before = (conn.total_changes(), interned(&conn));
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let mut write = store.write().expect("write");
    let stamp = write.apply_edge(current, edge).expect("edge");
    let mut changes = write.changes(current.id, current.version, 10).expect("changes");
    write.commit().expect("commit");
    let conn = store.into_connection();
    let written = conn.total_changes() - before.0 - (interned(&conn) - before.1);
    assert_eq!(changes.len(), 1, "one entry per edge");
    (Store::open(conn, LOCAL, registry()).expect("reopen"), stamp, changes.remove(0), written)
}

fn touches_only_declared_rows<K: Fixture>(tc: &TestCase) {
    let set = K::table_set();
    let mut store = store();
    let mut write = store.write().expect("write");
    let mut stamp = write.insert("r", &K::generate(tc, Pick::Draw), &origin()).expect("insert");
    write.commit().expect("commit");
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(4)) {
        let current = store.read().expect("read").load::<K>(stamp.id).expect("load").expect("present");
        let before = snapshot(&mut store, stamp.id);
        let Some((edge, _)) = current.value.edge(tc, Pick::Draw) else { return };
        let (next_store, next, change, written) = apply_counted(store, stamp, edge);
        store = next_store;
        let after = snapshot(&mut store, stamp.id);
        assert_eq!((change.home_seq, after.version), (next.version, next.version));
        let stamped = Value::Int(i64::try_from(next.version).expect("stored"));

        // The logged changes name exactly the rows that changed, and hold their new values.
        let (old, new) = (by_key(&before), by_key(&after));
        let (mut inserted, mut updated, mut deleted) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        let mut set_rows: BTreeMap<(usize, Vec<Value>), Vec<Range<usize>>> = BTreeMap::new();
        let (mut root_state_set, mut root_other_sets) = (false, 0);
        let sequence = i64::try_from(next.version).expect("stored");
        assert!(matches!(change.cause, Cause::Edge { .. }));
        for logged in &change.effect.changes {
            let Touch { table: index, key, columns } = touch(&set, logged, sequence);
            let name = &set.tables[index].name;
            let values: Vec<Value> = columns.iter().map(|(_, value)| value.clone()).collect();
            match logged {
                Change::Append { .. } => {
                    assert!(matches!(set.tables[index].role, TableRole::Records | TableRole::History));
                    assert!(!old[index].contains_key(&key), "{name} {key:?} existed before its append");
                    assert_eq!(new[index].get(&key), Some(&values), "{name} {key:?} holds the logged values, stamped");
                    assert_eq!(values[0], stamped);
                    inserted.insert((index, key));
                }
                Change::Put { .. } => {
                    // A collection row of state data, or a spec collection's row nested in a row the
                    // edge appends.
                    let nested = set.tables[index].role == TableRole::Child && !old[index].contains_key(&key);
                    assert!(
                        set.tables[index].role == TableRole::State || nested,
                        "{name}: an edge puts rows of state data or appended rows"
                    );
                    assert_eq!(new[index].get(&key), Some(&values), "{name} {key:?} holds the logged values");
                    match old[index].get(&key) {
                        Some(before) => {
                            assert_ne!(before, &values, "{name} {key:?} changed");
                            updated.insert((index, key));
                        }
                        None => {
                            inserted.insert((index, key));
                        }
                    }
                }
                Change::Remove { .. } => {
                    assert_eq!(set.tables[index].role, TableRole::State, "{name}: an edge removes rows of state data only");
                    assert!(old[index].contains_key(&key) && !new[index].contains_key(&key), "{name} {key:?} went");
                    deleted.insert((index, key));
                }
                Change::Set { field, .. } | Change::State { field, .. } => {
                    let state = matches!(logged, Change::State { .. });
                    if index == 0 {
                        if state {
                            root_state_set = true;
                        } else {
                            root_other_sets += 1;
                        }
                    }
                    let range = columns.first().map_or(0, |(column, _)| *column)..columns.last().map_or(0, |(column, _)| column + 1);
                    assert_eq!(&new[index][&key][range.clone()], &values[..], "{name}.{field} holds the logged values");
                    set_rows.entry((index, key)).or_default().push(range);
                }
            }
        }
        for (index, (old, new)) in old.iter().zip(&new).enumerate() {
            for key in old.keys().filter(|key| !new.contains_key(*key)) {
                assert!(deleted.contains(&(index, key.clone())), "{} {key:?} disappeared without a delete", set.tables[index].name);
            }
            for (key, values) in new {
                let Some(before) = old.get(key) else {
                    assert!(inserted.contains(&(index, key.clone())), "{} {key:?} appeared without an insert", set.tables[index].name);
                    continue;
                };
                if updated.contains(&(index, key.clone())) {
                    continue;
                }
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
        // One statement per appended row, per row edit and per set field or state of a records
        // row; the root's state as one unit by one statement, and each of its other set fields by
        // one; then the home and identity sequences and the log entry.
        let root = usize::from(root_state_set) + root_other_sets;
        let record_sets = set_rows.iter().filter(|((index, _), _)| *index != 0).map(|(_, columns)| columns.len()).sum::<usize>();
        let statements = inserted.len() + updated.len() + deleted.len() + record_sets + root.max(1) + 3;
        assert_eq!(written, statements as u64, "rows written by {change:?}");
        stamp = next;
    }
}

// Behaviour (#3320 ruling 1, #3373 ruling 2, #3392 G8): an edge write touches exactly its declared
// rows, as its one log entry names them: every row it appends (declared appends and the rows nested
// in them) is new, holds the logged values and, in a records or history table, the edge's version;
// every row of state data it puts or removes is one that differs; every other row that changed is
// one a logged `Set` or `State` names, and only within those columns, which hold the logged values;
// no other row appears or disappears. SQLite counts one row write per change (the root's state as
// one statement), plus the home sequence, identity sequence and log rows. The engine kinds' edges
// are the engine's, drawn over every edge their stored states allow.
#[hegel::test(test_cases = 60)]
fn an_edge_touches_exactly_its_declared_rows(tc: TestCase) {
    touches_only_declared_rows::<Resource<Message>>(&tc);
    touches_only_declared_rows::<Resource<AgentSession>>(&tc);
    touches_only_declared_rows::<Resource<Convoy>>(&tc);
    touches_only_declared_rows::<Resource<Ensure>>(&tc);
    touches_only_declared_rows::<TerminalSession>(&tc);
    touches_only_declared_rows::<Referral>(&tc);
    touches_only_declared_rows::<Dossier>(&tc);
}

fn evidence(reference: &str) -> Evidence {
    Evidence { reference: reference.into(), submitted_at: Timestamp(3), verdict: None, metadata: [("k".to_owned(), "v".to_owned())].into() }
}

fn deliverable() -> Deliverable {
    Deliverable {
        label: "pr".into(),
        kind: PromiseKind::ChangeRequest,
        source: PromiseSource::Dispatcher,
        evidence: Records::new(),
        state: State::new(DeliverableState::Open, Timestamp(4)),
    }
}

fn event(args: DossierEdge, from: DossierState, to: DossierState) -> DossierEvent {
    EdgeEvent { edge: args.name(), from, to, writer: "alice".into(), at: Timestamp(6), payload: Payload { args, reads: Vec::new() } }
}

/// A deliverable edge on a sample dossier: it writes the dossier's state unchanged.
fn staying_open(args: DossierEdge) -> Result<Edge<Dossier>, EdgeError> {
    let open = Dossier::sample(0).state;
    Edge::new(Dossier::STATE, &open, &event(args, DossierState::Open, DossierState::Open))
}

/// The deliver edge on an open dossier.
fn deliver() -> Edge<Dossier> {
    let delivered = State::new(DossierState::Delivered, Timestamp(8));
    Edge::new(Dossier::STATE, &delivered, &event(DossierEdge::Deliver, DossierState::Open, DossierState::Delivered)).expect("edge")
}

fn load(store: &mut Store, id: ResourceId) -> Dossier {
    store.read().expect("read").load::<Dossier>(id).expect("load").expect("present").value
}

// Behaviour (#3320 ruling 1, #3208 §4.1, on the synthetic Dossier): a deliverable's state moves
// through an edge on its records row (`set_in`), and Submit appends evidence under a deliverable
// (`append_in`) at the next position under that deliverable, with the rows nested in it; each edge
// appends its event at the dossier's next event position. Every appended row carries its edge's version while the rows it
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
    let inserted = write.insert("c", &Dossier { deliverables, ..Dossier::sample(0) }, &origin()).expect("insert");

    let submit = staying_open(DossierEdge::Submit { deliverable: 0 })
        .and_then(|edge| edge.append_in(Dossier::DELIVERABLES, Position(0), Deliverable::EVIDENCE, &evidence("e1")))
        .expect("edge");
    let submitted = write.apply_edge(inserted, submit).expect("submit");
    let retracted_state = State::new(DeliverableState::Retracted, Timestamp(7));
    let retract = staying_open(DossierEdge::Retract { deliverable: 1, reason: "dropped".into() })
        .and_then(|edge| edge.set_in(Dossier::DELIVERABLES, Position(1), Deliverable::STATE, &retracted_state))
        .expect("edge");
    let retracted = write.apply_edge(submitted, retract).expect("retract");

    let missing = |position| {
        staying_open(DossierEdge::Submit { deliverable: position })
            .and_then(|edge| edge.append_in(Dossier::DELIVERABLES, Position(position), Deliverable::EVIDENCE, &evidence("e9")))
            .expect("edge")
    };
    let refused = write.apply_edge(retracted, missing(2));
    assert!(
        matches!(&refused, Err(StoreError::NoRecord { table, key }) if table == "dossier_deliverables" && key == &[Value::Int(2)]),
        "{refused:?}"
    );
    let retract_missing = staying_open(DossierEdge::Retract { deliverable: 5, reason: String::new() })
        .and_then(|edge| edge.set_in(Dossier::DELIVERABLES, Position(5), Deliverable::STATE, &retracted_state))
        .expect("edge");
    let refused = write.apply_edge(retracted, retract_missing);
    assert!(matches!(&refused, Err(StoreError::NoRecord { table, .. }) if table == "dossier_deliverables"), "{refused:?}");
    let _last = write.apply_edge(retracted, deliver()).expect("a refused edge leaves the transaction usable");
    write.commit().expect("commit");

    let dossier = load(&mut store, inserted.id);
    let evidence_rows: Vec<(u32, u64, &str)> = dossier.deliverables.rows()[0]
        .record
        .evidence
        .iter()
        .map(|row| (row.position.0, row.version, row.record.reference.as_str()))
        .collect();
    assert_eq!(evidence_rows, [(0, inserted.version, "e0"), (1, submitted.version, "e1")]);
    assert_eq!(dossier.deliverables.rows()[0].record.evidence.rows()[1].record.metadata, evidence("e1").metadata, "nested rows come along");
    assert_eq!(dossier.deliverables.rows()[1].record.state, retracted_state);
    assert_eq!(dossier.deliverables.rows()[1].version, inserted.version, "a record keeps its stamp when an edge sets its fields");
    assert_eq!(dossier.deliverables.rows()[0].record.state, deliverable().state);
    assert_eq!(dossier.state.value, DossierState::Delivered);
}

// Behaviour (DESIGN writes, #3320 ruling 1): an edge is owner-checked and optimistic like a spec
// edit: a stale stamp is `Conflict`, a deleted resource `NotFound`, a resource homed elsewhere
// `NotHome`; none of them writes anything.
#[test]
fn edges_are_owner_checked_and_optimistic() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("c", &Dossier::sample(0), &origin()).expect("insert");
    let edge = deliver;
    let moved = write.apply_edge(stamp, edge()).expect("edge");
    assert!(matches!(write.apply_edge(stamp, edge()), Err(StoreError::Conflict { .. })));
    write.delete::<Dossier>(moved, &origin()).expect("delete");
    assert!(matches!(write.apply_edge(moved, edge()), Err(StoreError::NotFound { .. })));
    write.commit().expect("commit");

    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open");
    let mut write = remote.write().expect("write");
    write.insert("far", &Dossier::sample(0), &origin()).expect("insert");
    write.commit().expect("commit");
    let change = remote.read().expect("read").changes_since("remote", 0, 1).expect("log").remove(0);
    let mut write = store.write().expect("write");
    let Applied::Applied(far) = write.apply(&change).expect("apply") else { panic!("applied") };
    // A stale stamp is `Conflict` even when the edge is also wrong in another way (here, an
    // append under a records row that does not exist): retrying callers re-read on `Conflict`.
    let current = write.insert("stale", &Dossier::sample(0), &origin()).expect("insert");
    let both_wrong = || {
        staying_open(DossierEdge::Submit { deliverable: 9 })
            .and_then(|edge| edge.append_in(Dossier::DELIVERABLES, Position(9), Deliverable::EVIDENCE, &evidence("e")))
            .expect("edge")
    };
    write.apply_edge(current, edge()).expect("edge");
    assert!(matches!(write.apply_edge(current, both_wrong()), Err(StoreError::Conflict { .. })));
    assert!(matches!(write.apply_edge(far, edge()), Err(StoreError::NotHome { home, .. }) if home == "remote"));
}

// Behaviour (ruling 1, #3319's state value): every edge writes the state value, so its root `Set`
// moves the version to the write's sequence under the optimistic check, whether the label changes
// (deliver) or not (a deliverable edge, writing the state unchanged); a stale stamp is refused.
#[test]
fn every_edge_writes_the_state_and_moves_the_version() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let inserted = write.insert("c", &Dossier::sample(0), &origin()).expect("insert");
    write.commit().expect("commit");
    let unchanged = || staying_open(DossierEdge::Promise { deliverable: 0 }).expect("edge");
    let mut current = inserted;
    for (label, edge) in [(DossierState::Open, unchanged as fn() -> Edge<Dossier>), (DossierState::Delivered, deliver)] {
        let mut write = store.write().expect("write");
        let moved = write.apply_edge(current, edge()).expect("edge");
        assert!(moved.version > current.version);
        assert!(matches!(write.apply_edge(current, edge()), Err(StoreError::Conflict { .. })), "stale stamp, {label:?}");
        write.commit().expect("commit");
        assert_eq!(snapshot(&mut store, moved.id).version, moved.version);
        let change = store.read().expect("read").changes(moved.id, current.version, 10).expect("log").remove(0);
        let state_set =
            matches!(change.effect.changes.first(), Some(Change::State { row, field, .. }) if row.is_empty() && field == "state");
        assert!(state_set, "{label:?}");
        assert_eq!(load(&mut store, moved.id).state.value, label);
        current = moved;
    }
}

// Behaviour (#3392 G8, self-contained events): an edge that sets a frozen field carries the object
// in its effect, even one the store already holds, so its entry applies to a replica holding only
// the resource's earlier state; an edge setting it to `None` carries none.
#[test]
fn edges_carry_the_objects_they_set() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("a", &Dossier { placement: Some(placement()), ..Dossier::sample(0) }, &origin()).expect("insert a");
    let b = write.insert("b", &Dossier::sample(0), &origin()).expect("insert b");
    let placed = staying_open(DossierEdge::Promise { deliverable: 0 })
        .and_then(|edge| edge.set(Dossier::PLACEMENT, &Some(placement())))
        .expect("edge");
    let b = write.apply_edge(b, placed).expect("edge");
    let cleared = staying_open(DossierEdge::Promise { deliverable: 0 }).and_then(|edge| edge.set(Dossier::PLACEMENT, &None)).expect("edge");
    let b = write.apply_edge(b, cleared).expect("edge");
    write.commit().expect("commit");
    let changes = source.read().expect("read").changes(b.id, 0, 10).expect("log");
    let puts: Vec<usize> = changes.iter().map(|change| change.effect.objects.len()).collect();
    assert_eq!(puts, [1, 1, 0], "the insert's workflow, the edge's placement, none for clearing it");
    assert!(changes[1].effect.objects.contains_key(&placement().hash()));

    let mut replica = Store::open(Connection::open_in_memory().expect("db"), "replica", registry()).expect("open");
    let mut write = replica.write().expect("write");
    for change in &changes[..2] {
        assert!(matches!(write.apply(change).expect("apply"), Applied::Applied(_)));
    }
    let id = write.resolve(Dossier::CODE, "b", LOCAL).expect("resolve").expect("present");
    let value = write.load::<Dossier>(id).expect("load").expect("present").value;
    assert_eq!(value.placement.map(|frozen| frozen.hash()), Some(placement().hash()));
}

fn placement() -> Frozen<PlacementSnapshot> {
    Frozen::new(PlacementSnapshot { host: "h".into(), labels: BTreeMap::new() }).expect("freeze")
}
