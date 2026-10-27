//! A home over the store (#3373 rulings R1 and R2): the engine's single writer decides each edge
//! and a [`Log`] over the store persists it, so the version every resource holds is the `home_seq`
//! the store gave its write, and its history is the store's log, read in pages as a range over
//! (resource, sequence) and decoded back into the events the home returned.

use std::fmt::Debug;

use flotilla_schema::{Cbor, EdgeEvent, Identity, Kind, KindSpec, Labelled, NewChange, StateData, Timestamp};
use flotilla_schema_sqlite::{Stamp, Store};
use flotilla_state_machines::{
    edge_event,
    machines::message::{Deliver, MemoryMessageWorld, Message, MessageBody, MessageSpec, MessageState, Settle, Settlement},
    EdgeEventOf, EdgePayload, Home, InputRead, Label, Log, LogError, LoggedEdgeOf, Machine, Ref, Resource, StoredState, Transport, Version,
    WriteError,
};

use crate::fixtures::{addr, message, store, LOCAL};

/// The storage port the engine leaves to a later layer, as these tests need it: each create is an
/// `insert` and each edge an `apply_edge` under the stamp the home holds, each in its own
/// transaction, and the store's stamp is the version the home adopts.
struct StoreLog {
    store: Store,
}

fn refused(error: impl std::error::Error + Send + Sync + 'static) -> LogError {
    LogError::new(error)
}

impl<M> Log<M> for StoreLog
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, _: &[InputRead]) -> Result<Version, LogError> {
        let mut write = self.store.write().map_err(refused)?;
        let stamp = write.insert(resource.name(), resource, &NewChange::new(writer, at)).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }

    fn edge(&mut self, writer: &str, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        // Every master a message's edge could name is a session at this home; none of these does.
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: LOCAL.to_owned() };
        let edge = next.stored_edge_about(previous, event, master).map_err(refused)?;
        let mut write = self.store.write().map_err(refused)?;
        let id = write.resolve(Resource::<M>::CODE, next.name(), LOCAL).map_err(refused)?.ok_or_else(|| LogError::new("not stored"))?;
        let stamp = write.apply_edge(Stamp { id, version: next.version().0 }, edge, &NewChange::new(writer, event.at)).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }
}

fn home() -> Home<Message, StoreLog> {
    Home::with_log("kiwi", StoreLog { store: store() }).expect("valid declaration")
}

fn at(micros: i64) -> Timestamp {
    Timestamp(micros)
}

/// A world in which `name` has a receipt from session `s`, so it may be delivered, and a reply, so
/// it may settle.
fn world(names: &[&str]) -> MemoryMessageWorld {
    let mut world = MemoryMessageWorld::default();
    for name in names {
        world.receipts.put(&Ref::new(*name), [(message::agent("s", "h"), Transport::Turn)].into());
        world.reply.put(&Ref::new(*name), ());
    }
    world
}

fn deliver() -> Deliver {
    Deliver { receiver: message::agent("s", "h"), transport: Transport::Turn }
}

/// `name` as the store holds it, with the store's name and version.
fn loaded<M>(home: &mut Home<M, StoreLog>, name: &str) -> Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + Debug,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    let mut read = home.log_mut().store.read().expect("read");
    let id = read.resolve(Resource::<M>::CODE, name, LOCAL).expect("resolve").expect("stored");
    read.load::<Resource<M>>(id).expect("load").expect("present").value
}

// Behaviour (R2): a resource's version is the sequence its home's log gave its last write, never
// one the home mints. The sequence is the home's, so writes to two resources interleave in it: each
// create and edge takes the next `home_seq`, and the home's resource equals the stored one.
#[test]
fn a_home_over_the_store_adopts_each_writes_home_seq() {
    let mut home = home();
    let world = world(&["a", "b"]);
    // Each write's change takes the sequence before the write's own (#4024).
    let created = home.create("a", message::sample_spec(), &world, at(1)).expect("created").resource.version();
    assert_eq!(created, Version(2));
    assert_eq!(home.create("b", message::sample_spec(), &world, at(1)).expect("created").resource.version(), Version(4));
    home.apply("a", deliver(), &world, at(2)).expect("delivered");
    home.apply("b", deliver(), &world, at(3)).expect("delivered");
    home.apply("a", Settle { by: Settlement::Answered }, &world, at(4)).expect("settled");
    let versions = [home.get("a").expect("a").version(), home.get("b").expect("b").version()];
    assert_eq!(versions, [Version(10), Version(8)], "a's writes took 2, 6 and 10, b's 4 and 8; their changes the odd ones");
    for name in ["a", "b"] {
        let held = home.get(name).expect("held").clone();
        assert_eq!(loaded(&mut home, name), held, "the home holds what the store holds, at its version");
    }
    assert_eq!(home.log_mut().store.read().expect("read").last_sequence(LOCAL).expect("sequence"), 10);
}

// Behaviour (R1): history is no part of the resource the home decides on: loading the current state
// reads only its rows. A resource's edges are the store's log entries, paged as a range over
// (resource, home_seq) (`changes`, by `log_by_resource`), and each entry decodes back into the
// event the home returned; its create is an entry but no edge.
#[test]
fn history_is_a_paged_range_over_the_log_that_decodes_to_the_homes_events() {
    let mut home = home();
    let world = world(&["a", "b"]);
    home.create("a", message::sample_spec(), &world, at(1)).expect("created");
    home.create("b", message::sample_spec(), &world, at(1)).expect("created");
    let mut events = vec![home.apply("a", deliver(), &world, at(2)).expect("delivered")];
    home.apply("b", deliver(), &world, at(3)).expect("delivered");
    events.push(home.apply("a", Settle { by: Settlement::Answered }, &world, at(4)).expect("settled"));

    let mut read = home.log_mut().store.read().expect("read");
    let id = read.resolve(Resource::<Message>::CODE, "a", LOCAL).expect("resolve").expect("stored");
    let (mut pages, mut after, mut entries) = (0, 0, Vec::new());
    loop {
        let page = read.changes(id, after, 2).expect("page");
        let Some(last) = page.last() else { break };
        after = last.home_seq;
        pages += 1;
        entries.extend(page);
    }
    assert_eq!((pages, entries.iter().map(|entry| entry.home_seq).collect::<Vec<_>>()), (2, vec![2, 6, 10]), "a's entries alone");
    let decoded: Vec<Option<LoggedEdgeOf<Message>>> = entries
        .iter()
        .map(|entry| {
            let at = read.change(LOCAL, entry.change).expect("read").expect("its change").at;
            edge_event::<Message>(entry, at).expect("an entry of the kind")
        })
        .collect();
    assert_eq!(decoded[0], None, "the create is no edge");
    // The entry keeps the edge; the reads the home recorded are its change's (#4024).
    let logged: Vec<_> = events
        .iter()
        .map(|event| EdgeEvent { edge: event.edge, from: event.from, to: event.to, at: event.at, arguments: event.arguments.edge.clone() })
        .collect();
    assert_eq!(decoded[1..].iter().flatten().cloned().collect::<Vec<_>>(), logged, "each edge's entry is the event the home returned");
    drop(read);
    let current = loaded::<Message>(&mut home, "a");
    assert_eq!(current.state(), &MessageState::Settled { receiver: Some(message::agent("s", "h")) });
}

// Behaviour (R2): the log's refusal is the write's. A write the store refuses (here, the stored
// version moved under the home: a spec edit by another writer) is `WriteError::Log`, and the home's
// resource keeps its state and version.
#[test]
fn a_write_the_store_refuses_leaves_the_homes_resource_as_it_was() {
    let mut home = home();
    let world = world(&["a"]);
    home.create("a", message::sample_spec(), &world, at(1)).expect("created");
    let before = home.get("a").expect("a").clone();
    {
        let mut write = home.log_mut().store.write().expect("write");
        let id = write.resolve(Resource::<Message>::CODE, "a", LOCAL).expect("resolve").expect("stored");
        let current = write.load::<Resource<Message>>(id).expect("load").expect("present");
        let spec = MessageSpec { inbox: addr("elsewhere"), ..current.value.spec().clone() };
        let edited = current.value.clone().with_spec(spec);
        write.update(&current, &edited, &NewChange::new("other", at(2))).expect("edit");
        write.commit().expect("commit");
    }
    let refused = home.apply("a", deliver(), &world, at(3));
    assert!(matches!(refused, Err(WriteError::Log(_))), "{refused:?}");
    assert_eq!(home.get("a"), Some(&before), "the refused write left the resource as it was");
}

/// How many times `needle` occurs in `bytes`.
fn occurrences(bytes: &[u8], needle: &[u8]) -> usize {
    bytes.windows(needle.len()).filter(|window| *window == needle).count()
}

// Behaviour (#3424 ruling 4): a message's body is stored at most twice, once in `objects` and once
// among its create entry's effect objects. The create's change names the records admission relied
// on, never the request, so neither it nor the entry copies the spec and its body.
#[test]
fn a_messages_body_is_stored_at_most_twice() {
    let mut home = home();
    let text = "a body worth storing once ".repeat(40);
    let spec = MessageSpec { body: MessageBody::text(text.clone()).expect("a small body"), ..message::sample_spec() };
    home.create("m", spec, &MemoryMessageWorld::default(), at(1)).expect("created");
    let needle = text.as_bytes();

    let mut store = std::mem::replace(&mut home.log_mut().store, store());
    let entries = store.read().expect("read").changes_since(LOCAL, 0, 10).expect("log");
    let [create] = entries.as_slice() else { panic!("one entry: {entries:?}") };
    assert_eq!(create.arguments, None, "a create has no arguments");
    assert_eq!(occurrences(&create.encode().expect("encodes"), needle), 1, "the effect carries the body once, as its object");

    let conn = store.into_connection();
    let objects: Vec<Vec<u8>> = conn
        .prepare("SELECT bytes FROM objects")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(objects.iter().map(|bytes| occurrences(bytes, needle)).sum::<usize>(), 1, "one object row holds it");
}
