//! History in the per-home log (#3392 G8, G9, G11): every edge's entry holds its event, a delete
//! keeps the resource's history and identity, an expunge removes them, entries about a master
//! outlive it, edge codes are the kind's, and renamed fields still read old entries.

use flotilla_schema::{
    Cause, Change, CodeEnum, Edge, EdgeEvent, Event, Identity, Kind, KindRegistry, Origin, Records, State, Stored, Timestamp,
};
use flotilla_schema_sqlite::{Applied, Malformed, Store, StoreError};
use flotilla_state_machines::{machines::message::MessageEdge, EdgePayload};
use hegel::TestCase;
use rusqlite::Connection;

use crate::fixtures::{
    agent_session, engine, message, origin, registry, store,
    synthetic::{DossierEdge, DossierEvent, DossierState},
    AgentSessionRow, Dossier, Fixture, GateRead, MessageRow, Payload, Pick, Referral, LOCAL,
};

const REPLICA: &str = "replica";

fn replica() -> Store {
    Store::open(Connection::open_in_memory().expect("db"), REPLICA, registry()).expect("open replica")
}

fn dossier_event(args: DossierEdge, from: DossierState, to: DossierState, at: i64, reads: Vec<GateRead>) -> DossierEvent {
    EdgeEvent { edge: args.name(), from, to, writer: format!("writer-{at}"), at: Timestamp(at), payload: Payload { args, reads } }
}

fn read(observed: &str, version: u64) -> GateRead {
    GateRead { observed: Identity { kind: MessageRow::CODE, name: observed.into(), home: LOCAL.into() }, version }
}

/// Every entry of `home` in `store`, in home order.
fn log(store: &mut Store, home: &str) -> Vec<Event> {
    store.read().expect("read").changes_since(home, 0, 100).expect("log")
}

/// Applies every entry of `home` in `source` after `after` to `target`, returning what each did.
fn replay(source: &mut Store, target: &mut Store, after: u64) -> Vec<Applied> {
    let entries: Vec<Event> = log(source, LOCAL).into_iter().filter(|entry| entry.home_seq > after).collect();
    let mut write = target.write().expect("write");
    let applied = entries.iter().map(|entry| write.apply(entry).expect("apply")).collect();
    write.commit().expect("commit");
    applied
}

// Behaviour (#3392 G8, the edge log's coverage restated on the log): every edge appends exactly one
// entry, in the order applied, whose header is its event: the edge code, the labels it leaves and
// enters, its writer and time; and whose payload is the event's document, decoding to the
// arguments and recorded reads it was built with. A replica relays the same entries.
#[test]
fn every_edge_logs_one_entry_holding_its_event() {
    let mut source = store();
    let mut write = source.write().expect("write");
    let mut stamp = write.insert("d", &Dossier::sample(0), &origin()).expect("insert");
    let open = Dossier::sample(0).state;
    let events = [
        dossier_event(DossierEdge::Promise { deliverable: 0 }, DossierState::Open, DossierState::Open, 3, vec![read("m", 2)]),
        dossier_event(DossierEdge::Submit { deliverable: 0 }, DossierState::Open, DossierState::Open, 4, vec![]),
        dossier_event(DossierEdge::Deliver, DossierState::Open, DossierState::Delivered, 5, vec![read("m", 3), read("n", 1)]),
    ];
    for event in &events {
        let next = if event.to == event.from { open } else { State::new(event.to, event.at) };
        stamp = write.apply_edge(stamp, Edge::new(Dossier::STATE, &next, event).expect("edge")).expect("apply");
    }
    write.commit().expect("commit");
    let entries = source.read().expect("read").changes(stamp.id, 0, 10).expect("history");
    assert_eq!(entries.len(), 1 + events.len(), "the create, then one entry per edge");
    assert_eq!(entries[0].cause, Cause::Create);
    for (entry, event) in entries[1..].iter().zip(&events) {
        let header = (entry.cause, entry.writer.as_str(), entry.at);
        let cause = Cause::Edge { edge: event.edge.code(), from: event.from.code(), to: event.to.code() };
        assert_eq!(header, (cause, event.writer.as_str(), event.at));
        let payload: Payload<DossierEdge> = entry.payload().expect("a document").expect("decodes");
        assert_eq!(payload, event.payload);
    }
    let sequences: Vec<u64> = entries.iter().map(|entry| entry.home_seq).collect();
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]), "in the order applied: {sequences:?}");

    let mut target = replica();
    replay(&mut source, &mut target, 0);
    let id = target.read().expect("read").resolve(Dossier::CODE, "d", LOCAL).expect("resolve").expect("present");
    assert_eq!(target.read().expect("read").changes(id, 0, 10).expect("history"), entries);
}

// Behaviour (#3392 G8): an engine edge's entry holds the engine's document: its arguments and the
// reads its gate recorded, which decode back with their kinds as owned text.
#[hegel::test(test_cases = 30)]
fn an_engine_edge_entry_holds_its_recorded_reads(tc: TestCase) {
    let row = MessageRow::generate(&tc, Pick::Draw);
    let Some((edge, _)) = engine::drive(&tc, Pick::Draw, &row, message::candidates(&tc, &row)) else { return };
    let header = edge.clone().into_parts().event;
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert(engine::NAME, &row, &origin()).expect("insert");
    let stamp = write.apply_edge(stamp, edge).expect("edge");
    let entry = write.changes(stamp.id, stamp.version - 1, 1).expect("history").remove(0);
    write.commit().expect("commit");
    assert_eq!(entry.payload.as_ref(), Some(&header.payload));
    let decoded: EdgePayload<MessageEdge> = entry.payload().expect("a document").expect("decodes");
    assert_eq!(decoded, header.payload::<EdgePayload<MessageEdge>>().expect("the event's document"));
    assert_eq!((entry.writer.as_str(), entry.at), (header.writer.as_str(), header.at));
}

// Behaviour (#3392 G9, A1): a delete is an event. The resource's rows go; its entries (the delete
// last) and identity stay, so `changes` from 0 is its whole history. An expunge removes the
// history: its entries, then its identity, in one transaction, leaving one `expunge` entry that
// names the identity by kind, name and home and references no identity row. A replica applying
// the log does the same, an expunge applied again is a duplicate, and a replica that never held
// the resource (its entries were gone before it read the log) logs the expunge alone.
#[test]
fn a_delete_keeps_history_and_an_expunge_removes_it() {
    let mut source = store();
    let mut write = source.write().expect("write");
    let stamp = write.insert("m", &message::sample(), &origin()).expect("insert");
    let current = write.load::<MessageRow>(stamp.id).expect("load").expect("present");
    let edited = write.update(&current, &MessageRow { inbox: "edited".into(), ..message::sample() }, &origin()).expect("edit");
    write.delete::<MessageRow>(edited, &Origin::new("deleter", Timestamp(9))).expect("delete");
    write.insert("other", &message::sample(), &origin()).expect("a resource that stays");
    write.commit().expect("commit");
    let history = source.read().expect("read").changes(stamp.id, 0, 10).expect("history");
    let causes: Vec<Cause> = history.iter().map(|entry| entry.cause).collect();
    assert_eq!(causes, [Cause::Create, Cause::Edit, Cause::Delete], "the history survives the delete");
    assert_eq!((history[2].writer.as_str(), history[2].at), ("deleter", Timestamp(9)));
    assert_eq!(source.read().expect("read").resolve(MessageRow::CODE, "m", LOCAL).expect("resolve"), Some(stamp.id));

    let mut target = replica();
    replay(&mut source, &mut target, 0);
    let seen = source.read().expect("read").last_sequence(LOCAL).expect("last");

    let mut write = source.write().expect("write");
    let expunged = write.expunge::<MessageRow>(stamp.id, &Origin::new("expunger", Timestamp(10))).expect("expunge");
    write.commit().expect("commit");
    let identity = Identity { kind: MessageRow::CODE, name: "m".into(), home: LOCAL.into() };
    assert_eq!(source.read().expect("read").resolve(MessageRow::CODE, "m", LOCAL).expect("resolve"), None, "the identity is gone");
    let entries = log(&mut source, LOCAL);
    let last = entries.last().expect("the expunge entry");
    assert_eq!(
        (last.cause, last.home_seq, last.identity(), last.prev_seq),
        (Cause::Expunge, expunged, identity.clone(), history[2].home_seq)
    );
    assert_eq!(last.effect.expunged.as_ref(), Some(&identity));
    assert!(entries.iter().all(|entry| entry.name != "m" || entry.cause == Cause::Expunge), "the resource's other entries are gone");
    let conn = source.into_connection();
    let orphaned: i64 =
        conn.query_row("SELECT count(*) FROM log WHERE resource IS NULL AND cause = 'expunge'", [], |row| row.get(0)).expect("count");
    assert_eq!(orphaned, 1, "the expunge entry references no identity row");
    let mut source = Store::open(conn, LOCAL, registry()).expect("reopen");

    assert_eq!(replay(&mut source, &mut target, seen), [Applied::Expunged]);
    assert_eq!(target.read().expect("read").resolve(MessageRow::CODE, "m", LOCAL).expect("resolve"), None);
    assert_eq!(log(&mut target, LOCAL), log(&mut source, LOCAL), "the replica's log is the home's");
    assert_eq!(replay(&mut source, &mut target, seen), [Applied::Duplicate(None)], "a replayed expunge is a duplicate");

    let mut fresh = replica();
    let applied = replay(&mut source, &mut fresh, 0);
    assert_eq!(applied.last(), Some(&Applied::Expunged));
    assert_eq!(log(&mut fresh, LOCAL), log(&mut source, LOCAL));
}

// Behaviour (#3392 A1): an expunge removes only a deleted resource's history (`Live` otherwise),
// and not while another resource's row references it (`Referenced`); a refused expunge writes
// nothing and leaves the transaction usable.
#[test]
fn an_expunge_refuses_live_and_referenced_resources() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let live = write.insert("live", &message::sample(), &origin()).expect("insert");
    assert!(matches!(write.expunge::<MessageRow>(live.id, &origin()), Err(StoreError::Live { id }) if id == live.id));
    let session = write.insert("session", &agent_session::sample(), &origin()).expect("insert");
    write.insert("referral", &Referral::sample(), &origin()).expect("a referral to the session");
    write.delete::<AgentSessionRow>(session, &origin()).expect("delete");
    let before = write.changes_since(LOCAL, 0, 100).expect("log");
    let refused = write.expunge::<AgentSessionRow>(session.id, &origin());
    assert!(matches!(refused, Err(StoreError::Referenced { id }) if id == session.id), "{refused:?}");
    assert_eq!(write.changes_since(LOCAL, 0, 100).expect("log"), before, "a refused expunge writes nothing");
    assert_eq!(write.resolve(AgentSessionRow::CODE, "session", LOCAL).expect("resolve"), Some(session.id));
    write.commit().expect("commit");
}

// Behaviour (#3392 G11, A2): an edge naming its master is read back among the master's entries, in
// time order and paged. Expunging the master keeps every entry about it, `about` set to null, which
// the log's CHECK admits; the entry still reads, as does a replica's copy.
#[test]
fn entries_about_a_master_outlive_its_expunge() {
    let mut source = store();
    let mut write = source.write().expect("write");
    let master = write.insert("m", &message::sample(), &origin()).expect("insert");
    let identity = Identity { kind: MessageRow::CODE, name: "m".into(), home: LOCAL.into() };
    let mut dossier = write.insert("d", &Dossier::sample(0), &origin()).expect("insert");
    let open = Dossier::sample(0).state;
    for at in [7, 5, 6] {
        let event = dossier_event(DossierEdge::Promise { deliverable: 0 }, DossierState::Open, DossierState::Open, at, vec![]);
        let edge = Edge::new(Dossier::STATE, &open, &event).expect("edge").about(identity.clone());
        dossier = write.apply_edge(dossier, edge).expect("edge");
    }
    let first = write.about(master.id, None, 2).expect("about");
    let times: Vec<Timestamp> = first.events.iter().map(|entry| entry.at).collect();
    assert_eq!(times, [Timestamp(5), Timestamp(6)], "in time order");
    let rest = write.about(master.id, first.next.as_ref(), 2).expect("about");
    assert_eq!(rest.events.iter().map(|entry| entry.at).collect::<Vec<_>>(), [Timestamp(7)]);
    assert!(first.events.iter().chain(&rest.events).all(|entry| entry.about.as_ref() == Some(&identity) && entry.name == "d"));
    write.delete::<MessageRow>(master, &origin()).expect("delete");
    write.commit().expect("commit");
    let mut target = replica();
    replay(&mut source, &mut target, 0);

    let mut write = source.write().expect("write");
    write.expunge::<MessageRow>(master.id, &origin()).expect("expunge");
    write.commit().expect("commit");
    replay(&mut source, &mut target, 0);
    for store in [&mut source, &mut target] {
        let mut read = store.read().expect("read");
        let id = read.resolve(Dossier::CODE, "d", LOCAL).expect("resolve").expect("present");
        let entries = read.changes(id, 0, 10).expect("history");
        let edges: Vec<&Event> = entries.iter().filter(|entry| matches!(entry.cause, Cause::Edge { .. })).collect();
        assert_eq!(edges.len(), 3, "every entry about the master stays");
        assert!(edges.iter().all(|entry| entry.about.is_none()), "about is null once the master is expunged");
    }
}

/// A code enum no kind declares as its edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Stray {
    #[store(code = 99)]
    Stray,
}

// Behaviour (#3392 A7, edge-code checks): the writer refuses an edge whose code the kind does not
// declare (`#[store(edges = E)]`), and `apply` refuses another home's entry whose edge code, or
// whose `from` or `to` label, is not the kind's; each writes nothing.
#[test]
fn edge_codes_are_checked_by_the_writer_and_by_apply() {
    let mut source = store();
    let mut write = source.write().expect("write");
    let stamp = write.insert("d", &Dossier::sample(0), &origin()).expect("insert");
    let open = Dossier::sample(0).state;
    let stray = EdgeEvent {
        edge: Stray::Stray,
        from: DossierState::Open,
        to: DossierState::Open,
        writer: "w".into(),
        at: Timestamp(3),
        payload: Payload { args: DossierEdge::Deliver, reads: Vec::new() },
    };
    let refused = write.apply_edge(stamp, Edge::new(Dossier::STATE, &open, &stray).expect("edge"));
    assert!(matches!(refused, Err(StoreError::UnknownEdge { edge: 99, .. })), "{refused:?}");
    let event = dossier_event(DossierEdge::Promise { deliverable: 0 }, DossierState::Open, DossierState::Open, 3, vec![]);
    write.apply_edge(stamp, Edge::new(Dossier::STATE, &open, &event).expect("edge")).expect("a declared edge");
    write.commit().expect("commit");
    let entries = log(&mut source, LOCAL);
    assert_eq!(entries.len(), 2, "the refused edge logged nothing");

    let mut target = replica();
    let mut write = target.write().expect("write");
    write.apply(&entries[0]).expect("create");
    let Cause::Edge { edge, from, to } = entries[1].cause else { panic!("an edge") };
    for (cause, refusal) in [
        (Cause::Edge { edge: 99, from, to }, "UnknownEdge"),
        (Cause::Edge { edge, from: 99, to }, "UnknownLabel"),
        (Cause::Edge { edge, from, to: 99 }, "UnknownLabel"),
    ] {
        let tampered = Event { cause, ..entries[1].clone() };
        let refused = format!("{:?}", write.apply(&tampered));
        assert!(refused.contains(refusal), "{refused}");
    }
    assert!(matches!(write.apply(&entries[1]).expect("the real edge"), Applied::Applied(_)));
    let about = Event { about: entries[0].about.clone().or(Some(entries[0].identity())), ..entries[0].clone() };
    let refused = write.apply(&Event { home_seq: 99, prev_seq: entries[1].home_seq, cause: Cause::Edit, ..about });
    assert!(matches!(refused, Err(StoreError::MalformedEvent(Malformed::Shape { .. }))), "only an edge names a master: {refused:?}");
    write.commit().expect("commit");
}

/// A part whose fields the renamed kind keeps under new names.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Person {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct OldNote {
    text: String,
}

/// A note as an earlier generation stored it.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Note", code = 40)]
struct Before {
    title: String,
    #[store(flatten)]
    who: Person,
    #[store(child)]
    tags: Vec<String>,
    #[store(records)]
    notes: Records<OldNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct NewNote {
    #[store(alias = "text")]
    body: String,
}

/// The same kind after renaming every field: each keeps its earlier name as an alias.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Note", code = 40)]
struct After {
    #[store(alias = "title")]
    heading: String,
    #[store(flatten, alias = "who")]
    by: Person,
    #[store(child, alias = "tags")]
    labels: Vec<String>,
    #[store(records, alias = "notes")]
    comments: Records<NewNote>,
}

// Behaviour (#3392 A7, `#[store(alias)]`): entries written under a kind's earlier field names (a
// column, a flattened part's columns, a child table, a records table and a field of its rows)
// lower into the renamed kind's tables, so the next generation still applies them (N→N+1).
#[test]
fn renamed_fields_read_entries_written_under_their_aliases() {
    let open = |home: &str, register: fn(&mut KindRegistry)| {
        let mut kinds = KindRegistry::new();
        register(&mut kinds);
        Store::open(Connection::open_in_memory().expect("db"), home, kinds).expect("open")
    };
    let mut old = open(LOCAL, |kinds| kinds.register::<Before>().expect("register"));
    let mut notes = Records::new();
    notes.append(0, OldNote { text: "first".into() });
    let value = Before { title: "t".into(), who: Person { name: "ann".into() }, tags: vec!["a".into(), "b".into()], notes };
    let mut write = old.write().expect("write");
    write.insert("n", &value, &origin()).expect("insert");
    write.commit().expect("commit");
    let entries = log(&mut old, LOCAL);
    let names: Vec<String> = entries[0]
        .effect
        .changes
        .iter()
        .map(|change| match change {
            Change::Set { field, .. } => field.clone(),
            Change::Put { collection, .. } | Change::Append { records: collection, .. } => collection.clone(),
            other => panic!("a create sets and adds: {other:?}"),
        })
        .collect();
    assert_eq!(names, ["title", "who", "tags", "tags", "notes"], "written under the old names");

    let mut new = open(REPLICA, |kinds| kinds.register::<After>().expect("register"));
    let mut write = new.write().expect("write");
    let Applied::Applied(stamp) = write.apply(&entries[0]).expect("apply") else { panic!("applied") };
    let loaded = write.load::<After>(stamp.id).expect("load").expect("present").value;
    assert_eq!(
        (loaded.heading.as_str(), loaded.by.name.as_str(), loaded.labels.clone()),
        ("t", "ann", vec!["a".to_owned(), "b".to_owned()])
    );
    let comments: Vec<(&str, u64)> = loaded.comments.iter().map(|row| (row.record.body.as_str(), row.version)).collect();
    assert_eq!(comments, [("first", stamp.version)]);
    write.commit().expect("commit");
}
