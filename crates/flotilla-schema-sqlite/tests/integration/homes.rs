//! A home over the store (#3373 rulings R1 and R2): the engine's single writer decides each edge
//! and a [`Log`] over the store persists it, so the version every resource holds is the `home_seq`
//! the store gave its write, and its history is the store's log, read in pages as a range over
//! (resource, sequence) and decoded back into the events the home returned.

use std::fmt::Debug;

use flotilla_schema::{Cbor, Identity, Kind, KindSpec, Labelled, Origin, StateData, Timestamp};
use flotilla_schema_sqlite::{Stamp, Store};
use flotilla_state_machines::{
    edge_event,
    machines::message::{Deliver, MemoryMessageWorld, Message, MessageSpec, MessageState, Receiver, Settle, Settlement},
    EdgeEventOf, EdgePayload, Home, InputRead, Label, Log, LogError, Machine, Ref, Resource, StoredState, Transport, Version, WriteError,
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
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError> {
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut write = self.store.write().map_err(refused)?;
        let stamp = write.insert(resource.name(), resource, &origin).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }

    fn edge(&mut self, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        // Every master a message's edge could name is a session at this home; none of these does.
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: LOCAL.to_owned() };
        let edge = next.stored_edge_about(previous, event, master).map_err(refused)?;
        let mut write = self.store.write().map_err(refused)?;
        let id = write.resolve(Resource::<M>::CODE, next.name(), LOCAL).map_err(refused)?.ok_or_else(|| LogError::new("not stored"))?;
        let stamp = write.apply_edge(Stamp { id, version: next.version().0 }, edge).map_err(refused)?;
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
        world.receipts.put(&Ref::new(*name), [("s".to_owned(), Transport::Turn)].into());
        world.reply.put(&Ref::new(*name), ());
    }
    world
}

fn deliver() -> Deliver {
    Deliver { receiver: Receiver { session: "s".into(), host: "h".into() }, transport: Transport::Turn }
}

/// `name` as the store holds it, with the store's name and version.
fn loaded<M>(home: &mut Home<M, StoreLog>, name: &str) -> Resource<M>
where
    M: Machine,
    M::Spec: KindSpec<State = M::State> + Debug,
    M::State: StateData + Labelled<Label = Label<M>>,
    EdgePayload<M::Edge>: Cbor,
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
    let created = home.create("a", message::sample_spec(), &world, at(1)).expect("created").resource.version();
    assert_eq!(created, Version(1));
    assert_eq!(home.create("b", message::sample_spec(), &world, at(1)).expect("created").resource.version(), Version(2));
    home.apply("a", deliver(), &world, at(2)).expect("delivered");
    home.apply("b", deliver(), &world, at(3)).expect("delivered");
    home.apply("a", Settle { by: Settlement::Answered }, &world, at(4)).expect("settled");
    let versions = [home.get("a").expect("a").version(), home.get("b").expect("b").version()];
    assert_eq!(versions, [Version(5), Version(4)], "a's writes took 1, 3 and 5, b's 2 and 4");
    for name in ["a", "b"] {
        let held = home.get(name).expect("held").clone();
        assert_eq!(loaded(&mut home, name), held, "the home holds what the store holds, at its version");
    }
    assert_eq!(home.log_mut().store.read().expect("read").last_sequence(LOCAL).expect("sequence"), 5);
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
    assert_eq!((pages, entries.iter().map(|entry| entry.home_seq).collect::<Vec<_>>()), (2, vec![1, 3, 5]), "a's entries alone");
    let decoded: Vec<Option<EdgeEventOf<Message>>> =
        entries.iter().map(|entry| edge_event::<Message>(entry).expect("an entry of the kind")).collect();
    assert_eq!(decoded[0], None, "the create is no edge");
    assert_eq!(decoded[1..].iter().flatten().cloned().collect::<Vec<_>>(), events, "each edge's entry is the event the home returned");
    drop(read);
    let current = loaded::<Message>(&mut home, "a");
    assert_eq!(current.state(), &MessageState::Settled { receiver: Some(Receiver { session: "s".into(), host: "h".into() }) });
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
        write.update(&current, &edited, &Origin::new("other", at(2))).expect("edit");
        write.commit().expect("commit");
    }
    let refused = home.apply("a", deliver(), &world, at(3));
    assert!(matches!(refused, Err(WriteError::Log(_))), "{refused:?}");
    assert_eq!(home.get("a"), Some(&before), "the refused write left the resource as it was");
}
