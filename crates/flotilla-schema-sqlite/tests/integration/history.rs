//! History in the per-home log (#3392 G8, G9, G11): every edge's entry holds its event, a delete
//! keeps the resource's history and identity, an expunge removes them, entries about a master
//! outlive it, edge codes are the kind's, and renamed fields still read old entries.

use flotilla_schema::{
    Cause, Change, Code, CodeEnum, Edge, EdgeEvent, Event, Identity, Kind, KindRegistry, Origin, Records, State, Stored, Timestamp,
};
use flotilla_schema_sqlite::{Applied, Malformed, ResourceId, Store, StoreError};
use flotilla_state_machines::{
    machines::{
        agent_session::{MemoryAgentSessionWorld, Receive},
        message::{
            AppliedThrough, Authority, Binding, Condition, Deliver, DeliveryFailure, MemoryMessageWorld, MessageEdge, ReceiptLog, Receipts,
            Receiver, Reply, Supersession,
        },
    },
    EdgePayload, Home, Kind as ReadKind, Ref, Source, Transport, Version,
};
use hegel::TestCase;
use rusqlite::Connection;

use crate::fixtures::{
    addr, agent_session, engine, message, origin, registry, store,
    synthetic::{DossierEdge, DossierEvent, DossierState},
    AgentSession, Dossier, Fixture, GateRead, Message, Payload, Pick, Referral, Resource, LOCAL,
};

const REPLICA: &str = "replica";

fn replica() -> Store {
    Store::open(Connection::open_in_memory().expect("db"), REPLICA, registry()).expect("open replica")
}

fn dossier_event(args: DossierEdge, from: DossierState, to: DossierState, at: i64, reads: Vec<GateRead>) -> DossierEvent {
    EdgeEvent { edge: args.name(), from, to, writer: format!("writer-{at}"), at: Timestamp(at), payload: Payload { args, reads } }
}

fn read(observed: &str, version: u64) -> GateRead {
    GateRead { observed: Identity { kind: Resource::<Message>::CODE, name: observed.into(), home: LOCAL.into() }, version }
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
    let row = Resource::<Message>::generate(&tc, Pick::Draw);
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
// the resource (its entries were gone before it read the log) logs the expunge alone. A late copy
// of an old entry, at a replica or arriving after the expunge at a fresh one, meets the expunge
// entry as a tombstone and is a duplicate: an expunged resource never returns.
#[test]
fn a_delete_keeps_history_and_an_expunge_removes_it() {
    let mut source = store();
    let mut write = source.write().expect("write");
    let stamp = write.insert("m", &message::sample(), &origin()).expect("insert");
    let current = write.load::<Resource<Message>>(stamp.id).expect("load").expect("present");
    let edited = write.update(&current, &message::sample_in(addr("edited")), &origin()).expect("edit");
    write.delete::<Resource<Message>>(edited, &Origin::new("deleter", Timestamp(9))).expect("delete");
    write.insert("other", &message::sample(), &origin()).expect("a resource that stays");
    write.commit().expect("commit");
    let history = source.read().expect("read").changes(stamp.id, 0, 10).expect("history");
    let causes: Vec<Cause> = history.iter().map(|entry| entry.cause).collect();
    assert_eq!(causes, [Cause::Create, Cause::Edit, Cause::Delete], "the history survives the delete");
    assert_eq!((history[2].writer.as_str(), history[2].at), ("deleter", Timestamp(9)));
    assert_eq!(source.read().expect("read").resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve"), Some(stamp.id));

    let mut target = replica();
    replay(&mut source, &mut target, 0);
    let seen = source.read().expect("read").last_sequence(LOCAL).expect("last");

    let mut write = source.write().expect("write");
    let expunged = write.expunge::<Resource<Message>>(stamp.id, &Origin::new("expunger", Timestamp(10))).expect("expunge");
    write.commit().expect("commit");
    let identity = Identity { kind: Resource::<Message>::CODE, name: "m".into(), home: LOCAL.into() };
    assert_eq!(source.read().expect("read").resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve"), None, "the identity is gone");
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
    assert_eq!(target.read().expect("read").resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve"), None);
    assert_eq!(log(&mut target, LOCAL), log(&mut source, LOCAL), "the replica's log is the home's");
    assert_eq!(replay(&mut source, &mut target, seen), [Applied::AlreadyExpunged], "a replayed expunge is a duplicate");
    // Late duplicates of the expunged resource's old entries meet its expunge entry, the tombstone
    // that outlives its identity: each is a duplicate, and nothing is recreated.
    let mut write = target.write().expect("write");
    for entry in &history {
        assert_eq!(write.apply(entry).expect("a late duplicate"), Applied::AlreadyExpunged, "{entry:?}");
    }
    assert_eq!(write.resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve"), None, "no expunged resource returns");
    write.commit().expect("commit");

    let mut fresh = replica();
    let applied = replay(&mut source, &mut fresh, 0);
    assert_eq!(applied.last(), Some(&Applied::Expunged), "never held, the expunge is logged alone");
    assert_eq!(log(&mut fresh, LOCAL), log(&mut source, LOCAL));
    let mut write = fresh.write().expect("write");
    assert_eq!(write.apply(&history[0]).expect("an old create after the expunge"), Applied::AlreadyExpunged);
    assert_eq!(write.resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve"), None);
    write.commit().expect("commit");
}

// Behaviour (#3392 A1): an expunge removes only a deleted resource's history (`Live` otherwise),
// only at its home (`NotHome`), and not while another resource's row references it
// (`Referenced`); a refused expunge writes nothing and leaves the transaction usable.
#[test]
fn an_expunge_refuses_live_and_referenced_resources() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let live = write.insert("live", &message::sample(), &origin()).expect("insert");
    assert!(matches!(write.expunge::<Resource<Message>>(live.id, &origin()), Err(StoreError::Live { id }) if id == live.id));
    let session = write.insert("session", &agent_session::sample(), &origin()).expect("insert");
    write.insert("referral", &Referral::sample(), &origin()).expect("a referral to the session");
    write.delete::<Resource<AgentSession>>(session, &origin()).expect("delete");
    let before = write.changes_since(LOCAL, 0, 100).expect("log");
    let refused = write.expunge::<Resource<AgentSession>>(session.id, &origin());
    assert!(matches!(refused, Err(StoreError::Referenced { id }) if id == session.id), "{refused:?}");
    assert_eq!(write.changes_since(LOCAL, 0, 100).expect("log"), before, "a refused expunge writes nothing");
    assert_eq!(write.resolve(Resource::<AgentSession>::CODE, "session", LOCAL).expect("resolve"), Some(session.id));
    write.commit().expect("commit");

    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open remote");
    let mut write = remote.write().expect("write");
    let far = write.insert("far", &message::sample(), &origin()).expect("insert");
    write.delete::<Resource<Message>>(far, &origin()).expect("delete");
    write.commit().expect("commit");
    let mut write = store.write().expect("write");
    for entry in remote.read().expect("read").changes_since("remote", 0, 10).expect("log") {
        write.apply(&entry).expect("apply");
    }
    let id = write.resolve(Resource::<Message>::CODE, "far", "remote").expect("resolve").expect("present");
    assert!(matches!(write.expunge::<Resource<Message>>(id, &origin()), Err(StoreError::NotHome { .. })), "only the home expunges");
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
    let identity = Identity { kind: Resource::<Message>::CODE, name: "m".into(), home: LOCAL.into() };
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
    write.delete::<Resource<Message>>(master, &origin()).expect("delete");
    write.commit().expect("commit");
    let mut target = replica();
    replay(&mut source, &mut target, 0);

    let mut write = source.write().expect("write");
    write.expunge::<Resource<Message>>(master.id, &origin()).expect("expunge");
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

// Behaviour (#3392 G11, `restore`): a replica seeded from a snapshot holds no entries before it,
// since a snapshot is not an event and those entries stay with their home (and its archive). The
// entries about a master on that replica are therefore the ones applied after the snapshot, paged
// in time order like the home's, which pages all of them.
#[test]
fn entries_about_a_master_on_a_restored_replica_start_at_its_snapshot() {
    let mut source = store();
    let mut write = source.write().expect("write");
    write.insert("m", &message::sample(), &origin()).expect("insert");
    let identity = Identity { kind: Resource::<Message>::CODE, name: "m".into(), home: LOCAL.into() };
    let mut dossier = write.insert("d", &Dossier::sample(0), &origin()).expect("insert");
    let open = Dossier::sample(0).state;
    let promise = |at: i64| {
        let event = dossier_event(DossierEdge::Promise { deliverable: 0 }, DossierState::Open, DossierState::Open, at, vec![]);
        Edge::new(Dossier::STATE, &open, &event).expect("edge").about(identity.clone())
    };
    for at in [5, 6] {
        dossier = write.apply_edge(dossier, promise(at)).expect("edge");
    }
    write.commit().expect("commit");
    let taken = source.read().expect("read").load_snapshot(dossier.id).expect("snapshot").expect("present");
    let mut write = source.write().expect("write");
    for at in [8, 7, 9] {
        dossier = write.apply_edge(dossier, promise(at)).expect("edge");
    }
    write.commit().expect("commit");

    let mut target = replica();
    let mut write = target.write().expect("write");
    write.restore(&taken).expect("seed");
    for entry in source.read().expect("read").changes(dossier.id, taken.version, 10).expect("tail") {
        assert!(matches!(write.apply(&entry).expect("apply"), Applied::Applied(_)));
    }
    write.commit().expect("commit");
    let times = |store: &mut Store| {
        let mut read = store.read().expect("read");
        let master = read.resolve(Resource::<Message>::CODE, "m", LOCAL).expect("resolve").expect("the master's identity");
        let (mut times, mut after) = (Vec::new(), None);
        loop {
            let page = read.about(master, after.as_ref(), 2).expect("about");
            times.extend(page.events.iter().map(|entry| entry.at.micros()));
            match page.next {
                Some(next) => after = Some(next),
                None => break times,
            }
        }
    };
    assert_eq!(times(&mut source), [5, 6, 7, 8, 9], "the home holds every entry about the master");
    assert_eq!(times(&mut target), [7, 8, 9], "the replica holds those after its snapshot, paged in time order");
}

/// A code enum no kind declares as its edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Stray {
    #[store(code = "zz")]
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
    assert!(matches!(refused, Err(StoreError::UnknownEdge { edge, .. }) if edge == Code::new("zz")), "{refused:?}");
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
        (Cause::Edge { edge: Code::new("zz"), from, to }, "UnknownEdge"),
        (Cause::Edge { edge, from: Code::new("zz"), to }, "UnknownLabel"),
        (Cause::Edge { edge, from, to: Code::new("zz") }, "UnknownLabel"),
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
#[store(kind = "Note", code = "nt")]
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
#[store(kind = "Note", code = "nt")]
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
    notes.append(0, OldNote { text: "first".into() }).expect("room");
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

/// A message world whose receipts are the store's entries about each message (#3392 D1), the
/// rest of its evidence in memory.
struct LogWorld {
    receipts: ReceiptLog,
    memory: MemoryMessageWorld,
}

impl Source<Receipts> for LogWorld {
    fn lookup(&self, reference: &Ref<Receipts>) -> Option<(Version, &<Receipts as ReadKind>::Value)> {
        self.receipts.lookup(reference)
    }
}

macro_rules! from_memory {
    ($($kind:ty),+ $(,)?) => {
        $(impl Source<$kind> for LogWorld {
            fn lookup(&self, reference: &Ref<$kind>) -> Option<(Version, &<$kind as ReadKind>::Value)> {
                self.memory.lookup(reference)
            }
        })+
    };
}

from_memory!(Binding, AppliedThrough, Reply, Condition, DeliveryFailure, Supersession, Authority);

/// `store`'s message world: the receipts about message `m`, paged from the log.
fn log_world(store: &mut Store, message: ResourceId) -> LogWorld {
    let mut read = store.read().expect("read");
    let (mut entries, mut after) = (Vec::new(), None);
    loop {
        let page = read.about(message, after.as_ref(), 1).expect("about");
        entries.extend(page.events);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let mut receipts = ReceiptLog::new();
    receipts.insert("m", &entries).expect("receipts");
    LogWorld { receipts, memory: MemoryMessageWorld::default() }
}

// Behaviour (#3392 D1, G11): a session's `Receive` is stored with the message as its master, so
// the log holds it as an entry about the message, and the message's Deliver gate reads those
// entries: a receiver with no receipt by the transport is refused, and the receiver whose session
// received it is delivered, the read recorded with the receipts' version (their count). Entries
// about the message that are not receipts for it (here a dossier edge) are no receipts.
#[test]
fn a_messages_delivery_gate_reads_the_receipts_about_it() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stored = write.insert("m", &message::sample(), &origin()).expect("insert");
    let session = agent_session::sample();
    let stamp = write.insert("s", &session, &origin()).expect("insert");
    let dossier = write.insert("d", &Dossier::sample(0), &origin()).expect("insert");
    let identity = Identity { kind: Resource::<Message>::CODE, name: "m".into(), home: LOCAL.into() };
    let event = dossier_event(DossierEdge::Promise { deliverable: 0 }, DossierState::Open, DossierState::Open, 2, vec![]);
    write.apply_edge(dossier, Edge::new(Dossier::STATE, &Dossier::sample(0).state, &event).expect("edge").about(identity)).expect("edge");
    write.commit().expect("commit");

    let mut home = Home::<Message>::new(LOCAL).expect("valid");
    home.restore(message::sample().with_identity("m", stored.version)).unwrap_or_else(|_| panic!("an empty home"));
    let deliver = |session: &str, transport| Deliver { receiver: Receiver { session: session.into(), host: "h".into() }, transport };
    let world = log_world(&mut store, stored.id);
    assert_eq!(world.receipts.lookup(&Ref::new("m")), None, "no receipt yet: the dossier's entry is not one");
    assert!(home.apply("m", deliver("s", Transport::Turn), &world, Timestamp(3)).is_err(), "no receipt, no delivery");

    let receive = Receive { message: "m".into(), transport: Transport::Turn };
    let (edge, _) = engine::step(&session, "consumer", receive, &MemoryAgentSessionWorld::default(), Timestamp(3)).expect("received");
    let mut write = store.write().expect("write");
    let received = write.apply_edge(stamp, edge).expect("receipt");
    let entry = write.changes(received.id, stamp.version, 1).expect("log").remove(0);
    write.commit().expect("commit");
    assert_eq!(entry.about.as_ref().map(|about| about.name.as_str()), Some("m"), "the receipt is about the message");

    let world = log_world(&mut store, stored.id);
    assert!(home.apply("m", deliver("t", Transport::Turn), &world, Timestamp(4)).is_err(), "another session's receipt does not count");
    assert!(home.apply("m", deliver("s", Transport::Notify), &world, Timestamp(4)).is_err(), "nor another transport");
    let event = home.apply("m", deliver("s", Transport::Turn), &world, Timestamp(4)).expect("delivered");
    let read = event.payload.reads.iter().find(|read| read.kind == Receipts::NAME).expect("the receipts read");
    assert_eq!((read.name.as_str(), read.version), ("m", Some(Version(1))));
}
