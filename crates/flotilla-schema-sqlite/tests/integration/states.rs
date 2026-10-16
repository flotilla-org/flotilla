//! Label-plus-data states through SQLite (#3373 rulings 1 to 5), on the synthetic `Ticket` kind:
//! one statement per state write under the per-label CHECK, row-level collection edits, label-only
//! `since`, and the store-side `from` and version checks inside the writer's transaction.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{
    quote_ident, Edge, EdgeEvent, Kind, Labelled, Position, Records, Row, RowChange, RowOp, State, TableRole, Timestamp, Value,
};
use flotilla_schema_sqlite::{Applied, ResourceId, Snapshot, Stamp, Store, StoreError};
use hegel::{generators as gs, TestCase};
use rusqlite::{Connection, ErrorCode};

use crate::fixtures::{
    convoy::{ConvoyEdge, ConvoyEdgeEvent, ConvoyState, Deliverable, Evidence},
    registry, store,
    ticket::{next_value, state_value, transition, Ticket, TicketEdgeName, TicketLabel, TicketState},
    Convoy, Fixture, Payload, Pick, LOCAL,
};

fn snapshot(store: &mut Store, id: ResourceId) -> Snapshot {
    store.read().expect("read").load_snapshot(id).expect("snapshot").expect("present")
}

fn by_key(rows: &[Row]) -> BTreeMap<Vec<Value>, Vec<Value>> {
    rows.iter().map(|Row { key, values }| (key.clone(), values.clone())).collect()
}

/// Rows of the shared `strings` and `resource_identity` tables.
fn interned(conn: &Connection) -> u64 {
    conn.query_row("SELECT (SELECT count(*) FROM strings) + (SELECT count(*) FROM resource_identity)", [], |row| row.get(0)).expect("count")
}

/// Applies `edge` in its own transaction; returns the change it logged and the rows SQLite counted
/// as written, excluding new `strings` and `resource_identity` rows.
fn apply_counted(store: Store, current: Stamp, edge: Edge<Ticket>) -> (Store, Stamp, RowChange, u64) {
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

fn ticket(state: TicketState) -> Ticket {
    Ticket { queue: "q".into(), title: "t".into(), state: State::new(state, Timestamp(1)), edges: Records::new() }
}

fn open(assignees: &[&str], priority: u8) -> TicketState {
    TicketState::Open {
        assignees: assignees.iter().map(|value| (*value).to_owned()).collect(),
        notes: BTreeMap::new(),
        priority,
        owner: None,
    }
}

fn closed() -> TicketState {
    TicketState::Closed { assignees: BTreeSet::new(), notes: BTreeMap::new(), priority: 0, reason: "done".into(), closed_by: "ann".into() }
}

fn event(from: TicketLabel, to: TicketLabel, at: i64) -> EdgeEvent<TicketLabel, TicketEdgeName, Payload<String>> {
    let edge = if from == to { TicketEdgeName::Edit } else { TicketEdgeName::Move };
    EdgeEvent { edge, from, to, writer: "w".into(), at: Timestamp(at), payload: Payload { args: String::new(), reads: Vec::new() } }
}

fn edge(previous: &State<TicketState>, next: TicketState, at: i64) -> Edge<Ticket> {
    let (from, to) = (*previous.value.label(), *next.label());
    let next = State::new(next, if from == to { previous.since } else { Timestamp(at) });
    Edge::transition(Ticket::STATE, previous, &next, Ticket::EDGES, &event(from, to, at)).expect("edge")
}

// Behaviour (#3373 rulings 2 and 3, touched rows): every transition logs exactly the rows and
// columns that differ between the stored value before and after it, and the store writes nothing
// else. The root's state fields that are logged (`Set`) are exactly those with a differing column,
// holding the new values; the label field is among them exactly when the label changes, and
// `since` then becomes the event's `at` and otherwise stays. Each state data table's logged
// inserts, updates and deletes are exactly the keys added, changed and removed, never an unchanged
// row; the only other op is the event's insert, stamped with the edge's version. SQLite counts one
// row write per row op, one for the root (the one state statement, or the version alone), the
// event, and the home sequence, identity sequence and change log. Transitions are drawn on every
// label pair, collections growing, shrinking and changing.
#[hegel::test(test_cases = 60)]
fn a_transition_logs_exactly_the_rows_and_columns_that_differ(tc: TestCase) {
    let set = Ticket::table_set();
    let table = |name: &str| set.tables.iter().position(|table| table.name == name).expect("a table of the kind");
    let mut store = store();
    let mut write = store.write().expect("write");
    let mut stamp = write.insert("t", &Ticket::generate(&tc, Pick::Draw)).expect("insert");
    write.commit().expect("commit");
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(5)) {
        let current = store.read().expect("read").load::<Ticket>(stamp.id).expect("load").expect("present");
        let before = snapshot(&mut store, stamp.id);
        let (edge, _) = current.value.edge(&tc, Pick::Draw);
        let (next_store, next, change, written) = apply_counted(store, stamp, edge);
        store = next_store;
        let after = snapshot(&mut store, stamp.id);
        let root = &set.tables[0];
        let (old_root, new_root) = (&before.rows.tables[0][0].values, &after.rows.tables[0][0].values);
        let field = |name: &str| root.fields.iter().find(|field| field.name == name).expect("a field").columns.clone();

        let mut logged_fields = BTreeSet::new();
        let mut events = Vec::new();
        let mut logged_rows: BTreeMap<usize, Vec<&RowOp>> = BTreeMap::new();
        for op in &change.ops {
            match op {
                RowOp::Set { table: name, key, field: logged, values } => {
                    assert_eq!((name.as_str(), key.as_slice()), ("ticket", &[][..]), "only the root's fields are set");
                    assert_eq!(&new_root[field(logged)], &values[..], "{logged} holds the logged values");
                    assert!(logged_fields.insert(logged.clone()), "{logged} set once");
                }
                RowOp::Insert { table: name, values, .. } if name == "ticket_edges" => events.push(values.clone()),
                RowOp::Insert { table: name, .. } | RowOp::Update { table: name, .. } | RowOp::Delete { table: name, .. } => {
                    let index = table(name);
                    assert_eq!(set.tables[index].role, TableRole::State, "{name}");
                    logged_rows.entry(index).or_default().push(op);
                }
                other => panic!("a transition writes no {other:?}"),
            }
        }
        let differing: BTreeSet<String> = root
            .fields
            .iter()
            .filter(|field| old_root[field.columns.clone()] != new_root[field.columns.clone()])
            .map(|field| field.name.clone())
            .collect();
        assert_eq!(logged_fields, differing, "the logged fields are exactly those that differ");
        assert!(differing.iter().all(|name| root.fields.iter().any(|field| &field.name == name && field.state)), "only state moves");

        let stamped = Value::Int(i64::try_from(next.version).expect("stored"));
        assert_eq!(events.len(), 1, "one event");
        assert_eq!(events[0][0], stamped);
        let [_, _, Value::Int(from), Value::Int(to), _, Value::Int(at), _] = events[0][..] else { panic!("event header {:?}", events[0]) };
        let label = field("state").start;
        assert_eq!((old_root[label].clone(), new_root[label].clone()), (Value::Int(from), Value::Int(to)), "the event names the move");
        let since = new_root[label + 1].clone();
        assert_eq!(since, if from == to { old_root[label + 1].clone() } else { Value::Int(at) }, "since moves only with the label");
        assert_eq!(logged_fields.contains("state"), from != to);

        let mut row_ops = 0;
        for (index, table_set) in set.tables.iter().enumerate().filter(|(_, table)| table.role == TableRole::State) {
            let (old, new) = (by_key(&before.rows.tables[index]), by_key(&after.rows.tables[index]));
            let mut expected: Vec<String> = Vec::new();
            expected.extend(old.keys().filter(|key| !new.contains_key(*key)).map(|key| format!("delete {key:?}")));
            expected.extend(
                new.iter()
                    .filter(|(key, values)| old.get(*key).is_some_and(|old| old != *values))
                    .map(|(key, values)| format!("update {key:?} {values:?}")),
            );
            expected
                .extend(new.iter().filter(|(key, _)| !old.contains_key(*key)).map(|(key, values)| format!("insert {key:?} {values:?}")));
            let logged: Vec<String> = logged_rows
                .get(&index)
                .into_iter()
                .flatten()
                .map(|op| match op {
                    RowOp::Delete { key, .. } => format!("delete {key:?}"),
                    RowOp::Update { key, values, .. } => format!("update {key:?} {values:?}"),
                    RowOp::Insert { key, values, .. } => format!("insert {key:?} {values:?}"),
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(logged, expected, "{}: exactly the rows that differ, deletes, updates, then inserts", table_set.name);
            row_ops += logged.len();
        }
        assert_eq!(written, row_ops as u64 + 1 + 1 + 3, "rows written by {change:?}");
        stamp = next;
    }
}

// Behaviour (#3373 ruling 1, round trip): a ticket evolves by drawn transitions (every label pair,
// collections growing, shrinking and changing) and spec edits; after each, `load_snapshot`
// decodes to the model, and replaying the store's change log into another database reproduces the
// same rows (snapshots equal, although the databases number their strings differently).
#[hegel::test(test_cases = 60)]
fn transitions_round_trip_and_replay(tc: TestCase) {
    let mut source = store();
    let mut write = source.write().expect("write");
    let first = Ticket::generate(&tc, Pick::Draw);
    let mut stamp = write.insert("t", &first).expect("insert");
    write.commit().expect("commit");
    let mut model = first.stamped(None, stamp.version);
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(6)) {
        let mut write = source.write().expect("write");
        let current = write.load::<Ticket>(stamp.id).expect("load").expect("present");
        assert_eq!(current.value, model);
        if tc.draw(gs::integers::<u8>().max_value(3)) == 0 {
            let next = current.value.evolve(&tc, Pick::Draw);
            stamp = write.update(&current, &next).expect("a spec edit keeps the state");
            model = next;
        } else {
            let (edge, next) = current.value.edge(&tc, Pick::Draw);
            stamp = write.apply_edge(stamp, edge).expect("edge");
            model = next.stamped(Some(&current.value), stamp.version);
        }
        write.commit().expect("commit");
        assert_eq!(snapshot(&mut source, stamp.id).decode::<Ticket>().expect("decode"), model);
    }

    let mut replica = Store::open(Connection::open_in_memory().expect("db"), "replica", registry()).expect("open");
    let mut write = replica.write().expect("write");
    write.insert("pre", &ticket(closed())).expect("a local ticket interns its strings first");
    for change in source.read().expect("read").changes_since(LOCAL, 0, 100).expect("log") {
        assert!(matches!(write.apply(&change).expect("apply"), Applied::Applied(_)));
    }
    let id = write.resolve(Ticket::CODE, "t", LOCAL).expect("resolve").expect("present");
    let replayed = write.load_snapshot(id).expect("snapshot").expect("present");
    write.commit().expect("commit");
    assert_eq!(replayed, snapshot(&mut source, stamp.id));
}

fn convoy_event(args: ConvoyEdge, from: ConvoyState, to: ConvoyState) -> ConvoyEdgeEvent {
    EdgeEvent { edge: args.name(), from, to, writer: "w".into(), at: Timestamp(6), payload: Payload { args, reads: Vec::new() } }
}

fn evidence() -> Evidence {
    Evidence { reference: "e".into(), submitted_at: Timestamp(3), verdict: None, metadata: BTreeMap::new() }
}

// Behaviour (#3373 ruling 5, refusal order): inside its transaction, `apply_edge` refuses an edge
// on a resource it may not write (`NotHome`) first; then a stale stamp (`Conflict`), whatever else
// is wrong; then an event whose `from` is not the stored label (`WrongFrom`), before anything the
// edge addresses (a missing records row, `NoRecord`). Each holds for transitions and for edges
// built whole (`Edge::new`).
#[test]
fn edge_refusals_come_in_a_fixed_order() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let inserted = write.insert("t", &ticket(open(&["a"], 1))).expect("insert");
    let current = State::new(open(&["a"], 1), Timestamp(1));
    let moved = write.apply_edge(inserted, edge(&current, open(&["a", "b"], 1), 2)).expect("a data-only edge");

    // Leaves Closed, but the ticket is Open.
    let wrong_from = || {
        let previous = State::new(closed(), Timestamp(1));
        Edge::transition(
            Ticket::STATE,
            &previous,
            &State::new(TicketState::Draft, Timestamp(3)),
            Ticket::EDGES,
            &event(TicketLabel::Closed, TicketLabel::Draft, 3),
        )
        .expect("edge")
    };
    assert!(matches!(write.apply_edge(inserted, wrong_from()), Err(StoreError::Conflict { .. })), "a stale stamp first");
    let refused = write.apply_edge(moved, wrong_from());
    assert!(
        matches!(&refused, Err(StoreError::WrongFrom { id, field, from: 3, stored: 2 }) if *id == moved.id && field == "state"),
        "{refused:?}"
    );

    // An edge built whole, leaving the wrong label and appending under a missing deliverable.
    let convoy = write.insert("c", &Convoy::sample(0)).expect("insert");
    let delivered = State::new(ConvoyState::Delivered, Timestamp(6));
    let both = Edge::new(
        Convoy::STATE,
        &delivered,
        Convoy::EDGES,
        &convoy_event(ConvoyEdge::Deliver, ConvoyState::Delivered, ConvoyState::Delivered),
    )
    .and_then(|edge| edge.append_in(Convoy::DELIVERABLES, Position(9), Deliverable::EVIDENCE, &evidence()))
    .expect("edge");
    let refused = write.apply_edge(convoy, both.clone());
    assert!(matches!(&refused, Err(StoreError::WrongFrom { from: 2, stored: 1, .. })), "the label before the records row: {refused:?}");
    let missing =
        Edge::new(Convoy::STATE, &delivered, Convoy::EDGES, &convoy_event(ConvoyEdge::Deliver, ConvoyState::Open, ConvoyState::Delivered))
            .and_then(|edge| edge.append_in(Convoy::DELIVERABLES, Position(9), Deliverable::EVIDENCE, &evidence()))
            .expect("edge");
    assert!(matches!(write.apply_edge(convoy, missing), Err(StoreError::NoRecord { .. })), "then the records row");
    let stale = Stamp { version: convoy.version - 1, ..convoy };
    assert!(matches!(write.apply_edge(stale, both), Err(StoreError::Conflict { .. })), "a stale stamp before the label");
    write.commit().expect("commit");

    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open");
    let mut remote_write = remote.write().expect("write");
    remote_write.insert("far", &ticket(open(&[], 0))).expect("insert");
    remote_write.commit().expect("commit");
    let change = remote.read().expect("read").changes_since("remote", 0, 1).expect("log").remove(0);
    let mut write = store.write().expect("write");
    let Applied::Applied(far) = write.apply(&change).expect("apply") else { panic!("applied") };
    let stale_far = Stamp { version: far.version + 5, ..far };
    assert!(matches!(write.apply_edge(stale_far, wrong_from()), Err(StoreError::NotHome { .. })), "ownership before everything");
}

// Behaviour (#3373 ruling 5): a refused edge writes nothing (no row, no sequence, no change-log
// entry) and leaves the transaction usable: the next edge in it applies and commits.
#[test]
fn a_refused_edge_writes_nothing_and_the_transaction_goes_on() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let inserted = write.insert("t", &ticket(open(&["a"], 1))).expect("insert");
    write.commit().expect("commit");
    let before = snapshot(&mut store, inserted.id);

    let mut write = store.write().expect("write");
    let previous = State::new(closed(), Timestamp(1));
    let wrong = Edge::transition(
        Ticket::STATE,
        &previous,
        &State::new(open(&["z"], 0), Timestamp(4)),
        Ticket::EDGES,
        &event(TicketLabel::Closed, TicketLabel::Open, 4),
    )
    .expect("edge");
    assert!(matches!(write.apply_edge(inserted, wrong), Err(StoreError::WrongFrom { .. })));
    assert_eq!(write.last_sequence(LOCAL).expect("sequence"), inserted.version, "no sequence taken");
    assert_eq!(write.changes(inserted.id, inserted.version, 10).expect("log"), [], "nothing logged");
    assert_eq!(write.load_snapshot(inserted.id).expect("snapshot"), Some(before.clone()), "no row written");
    let current = State::new(open(&["a"], 1), Timestamp(1));
    let next = write.apply_edge(inserted, edge(&current, closed(), 5)).expect("the transaction is still usable");
    write.commit().expect("commit");
    let after = store.read().expect("read").load::<Ticket>(inserted.id).expect("load").expect("present");
    assert_eq!((after.version, after.value.state), (next.version, State::new(closed(), Timestamp(5))));
}

// Behaviour (#3373 ruling 2): state data moves only through edges: a spec edit that changes a
// state data row, or a column datum, is refused (`NotThroughEdge`), and writes nothing.
#[test]
fn spec_edits_never_move_state_data() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("t", &ticket(open(&["a"], 1))).expect("insert");
    let current = write.load::<Ticket>(stamp.id).expect("load").expect("present");
    for (next, table, column) in [
        (open(&["a", "b"], 1), "ticket_assignees", None),
        (open(&[], 1), "ticket_assignees", None),
        (open(&["a"], 0), "ticket", Some("priority".to_owned())),
    ] {
        let edited = Ticket { state: State::new(next, Timestamp(1)), ..current.value.clone() };
        let refused = write.update(&current, &edited);
        assert!(
            matches!(&refused, Err(StoreError::NotThroughEdge { table: refused, column: refused_column }) if refused == table && *refused_column == column),
            "{refused:?}"
        );
    }
    assert_eq!(write.load::<Ticket>(stamp.id).expect("load").expect("present"), current);
}

/// `sql` against the store's connection, refused by a CHECK.
fn refused_by_check(conn: &Connection, sql: &str) -> bool {
    match conn.execute(sql, []) {
        Err(rusqlite::Error::SqliteFailure(error, Some(message))) => {
            error.code == ErrorCode::ConstraintViolation && message.starts_with("CHECK constraint failed")
        }
        other => panic!("{sql}: {other:?}"),
    }
}

// Behaviour (#3373 ruling 1): the per-label CHECK holds after every statement, which is why one
// statement writes a label and its data: moving an Open ticket's label to Closed alone (its
// reason still null), or setting a reason while it is Open, is refused; both together pass. A
// datum table's rows under a label lacking it cannot be checked by the table, and loading refuses
// them.
#[test]
fn a_label_and_its_data_change_in_one_statement() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("t", &ticket(open(&[], 1))).expect("insert");
    let draft = write.insert("d", &ticket(TicketState::Draft)).expect("insert");
    write.commit().expect("commit");
    let conn = store.into_connection();
    let by = |id: ResourceId| format!("WHERE {} = {}", quote_ident("id"), id.0);
    let closed_by: i64 = conn.query_row("SELECT min(id) FROM strings", [], |row| row.get(0)).expect("a string");
    assert!(refused_by_check(&conn, &format!("UPDATE ticket SET state = 3, closed_by = {closed_by} {}", by(stamp.id))), "no reason");
    assert!(refused_by_check(&conn, &format!("UPDATE ticket SET reason = 'r' {}", by(stamp.id))), "Open lacks a reason");
    let together = format!("UPDATE ticket SET state = 3, reason = 'r', closed_by = {closed_by} {}", by(stamp.id));
    assert_eq!(conn.execute(&together, []).expect("one statement"), 1);

    conn.execute(&format!("INSERT INTO ticket_assignees (owner, value) VALUES ({}, 'x')", draft.id.0), []).expect("no table CHECK");
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");
    let refused = store.read().expect("read").load::<Ticket>(draft.id);
    assert!(matches!(refused, Err(StoreError::Decode(flotilla_schema::DecodeError::AbsentRows { .. }))), "{refused:?}");
}

// Behaviour (#3373 ruling 1, indexes): an index predicate on a label-plus-data state takes its
// codes from the labels enum: `open_by_queue` lists exactly the queue's Open tickets, and a
// transition out of Open leaves it.
#[test]
fn an_index_on_the_state_follows_transitions() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let a = write.insert("a", &ticket(open(&["x"], 1))).expect("insert");
    write.insert("b", &ticket(closed())).expect("insert");
    write.insert("c", &ticket(TicketState::Draft)).expect("insert");
    let d = write.insert("d", &ticket(open(&[], 0))).expect("insert");
    write.insert("e", &Ticket { queue: "other".into(), ..ticket(open(&[], 0)) }).expect("insert");
    let ids =
        |write: &mut flotilla_schema_sqlite::Write<'_>| write.page(&Ticket::open_by_queue(&"q".to_owned()), None, 10).expect("page").ids;
    assert_eq!(ids(&mut write), [a.id, d.id]);
    write.apply_edge(a, edge(&State::new(open(&["x"], 1), Timestamp(1)), closed(), 7)).expect("close");
    assert_eq!(ids(&mut write), [d.id]);
    write.commit().expect("commit");
}

// Behaviour (#3373 ruling 2): drawn transitions keep the in-memory model and the store in step
// across every label pair, including into and out of the label carrying no data, and the state
// generators reach every label.
#[hegel::test(test_cases = 30)]
fn every_label_pair_round_trips(tc: TestCase) {
    let mut store = store();
    let first = state_value(&tc, Pick::Draw);
    let mut write = store.write().expect("write");
    let mut stamp = write.insert("t", &ticket(first.clone())).expect("insert");
    let mut state = State::new(first, Timestamp(1));
    for _ in 0..4 {
        let (edge, next, _) = transition(&tc, Pick::Draw, &state, next_value(&tc, Pick::Draw, &state.value));
        stamp = write.apply_edge(stamp, edge).expect("edge");
        state = next;
        assert_eq!(write.load::<Ticket>(stamp.id).expect("load").expect("present").value.state, state);
    }
    write.commit().expect("commit");
}
