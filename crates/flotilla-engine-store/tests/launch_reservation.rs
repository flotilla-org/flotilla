//! Reserved launch numbers (#3108, team-lead ruling TL-2): admission takes a launch's number
//! before it writes anything keyed by the launch's name, then creates the launch with it.

use std::sync::Arc;

use flotilla_engine_store::{registry, StoreLog};
use flotilla_schema::{Address, Frozen, Timestamp};
use flotilla_schema_sqlite::{
    stream::{self, Follower, Frame},
    Store, StoreHandle,
};
use flotilla_state_machines::{
    kinds::{stock, Catalog},
    machines::convoy::{
        AdmissionBytes, AdmittedPromise, Convoy, ConvoyRequest, MemoryConvoyWorld, Priority, PromiseId, PromiseSource, Seat, SeatName,
    },
    Home, Log, Ref, Resource, WriteError,
};
use rusqlite::Connection;

const HOME: &str = "alpha";

fn store() -> Arc<StoreHandle> {
    Arc::new(StoreHandle::new(
        Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry().expect("registry")).expect("store"),
    ))
}

fn project() -> Address {
    Address::project("fleet").expect("project")
}

fn request(of: &str) -> ConvoyRequest {
    ConvoyRequest {
        launch_of: of.into(),
        workflow_ref: "w".into(),
        admission: Frozen::new(AdmissionBytes::new(Vec::new())).expect("freeze"),
        project: project(),
        promises: [(
            PromiseId(0),
            AdmittedPromise {
                label: "template/pr".into(),
                kind: "pr".into(),
                target: project(),
                source: PromiseSource::Template,
                owner: SeatName::new("coder", ""),
            },
        )]
        .into(),
        duties: Default::default(),
        seats: vec![Seat {
            failed: None,
            retired: false,
            replacements: 0,
            role: "coder".into(),
            seat: String::new(),
            parent: None,
            supervised_by: project(),
        }],
        quota: Default::default(),
        priority: Priority(0),
        kinds: Catalog::resolve(&stock(), &[]).expect("stock").freeze(["pr"]).expect("freeze"),
    }
}

fn world() -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default();
    world.projects.put(&Ref::new(project().as_str()), ());
    world
}

/// Follows `source`'s home from `after` into `target`; whether the round sent a fresh snapshot.
fn follow(source: &Arc<StoreHandle>, target: &mut Store, after: Option<u64>) -> bool {
    let mut snapshot = false;
    Follower::new(target)
        .receive(HOME, |sink| {
            let mut source = source.lock().expect("source");
            stream::serve(&mut source, after, Timestamp(1), |bytes| {
                snapshot |= matches!(Frame::decode(bytes)?, Frame::Begin { .. });
                sink(bytes)
            })
            .map(|_| ())
        })
        .expect("follow");
    snapshot
}

// Behaviour: a reserved number names the launch before it exists; the create takes it under its
// own later version, and a follower tails the reservation and the create without a snapshot.
#[test]
fn a_reserved_launch_is_created_under_its_number_and_followed() {
    let source = store();
    let mut home: Home<Convoy, StoreLog> = Home::with_log(HOME, StoreLog::new(Arc::clone(&source))).expect("home");
    let mut follower = Store::open(Connection::open_in_memory().expect("SQLite"), "beta", registry().expect("registry")).expect("store");
    home.create("first", request("first"), &world(), Timestamp(1)).expect("first launch");
    assert!(follow(&source, &mut follower, None), "the first round is a snapshot");
    let after = source.lock().expect("source").read().expect("read").last_sequence(HOME).expect("last");

    let n = home.reserve_launch("fix-it", Timestamp(2)).expect("reserved");
    // Something else written between the reservation and the create, as admission's artifacts are.
    home.create("other", request("other"), &world(), Timestamp(3)).expect("other launch");
    let created = home.create_launch(request("fix-it"), &world(), Timestamp(4), n).expect("created").resource.clone();
    assert_eq!(created.name(), format!("fix-it#{n}"));
    assert_eq!(created.spec().launch, n);
    assert!(created.version().0 > n, "the create's version follows the reservation");

    assert!(!follow(&source, &mut follower, Some(after)), "the reservation's empty change keeps the prefix whole");
    let held =
        follower.read().expect("read").resolve(<Resource<Convoy> as flotilla_schema::Kind>::CODE, created.name(), HOME).expect("resolve");
    assert!(held.is_some(), "the follower holds the launch");
}

// Behaviour: a launch is created only with a number reserved at its home for its name, once.
#[test]
fn an_unreserved_or_used_number_is_refused() {
    let source = store();
    let mut home: Home<Convoy, StoreLog> = Home::with_log(HOME, StoreLog::new(Arc::clone(&source))).expect("home");
    let unreserved = home.create_launch(request("fix-it"), &world(), Timestamp(1), 99).map(|created| created.resource.clone());
    assert_eq!(unreserved.expect_err("never reserved"), WriteError::Unreserved("fix-it".into(), 99));

    let n = home.reserve_launch("fix-it", Timestamp(2)).expect("reserved");
    let other = home.create_launch(request("other"), &world(), Timestamp(3), n).map(|created| created.resource.clone());
    assert_eq!(other.expect_err("reserved for another name"), WriteError::Unreserved("other".into(), n));

    home.create_launch(request("fix-it"), &world(), Timestamp(4), n).expect("created");
    let again = home.create_launch(request("fix-it"), &world(), Timestamp(5), n).map(|created| created.resource.clone());
    assert_eq!(again.expect_err("used"), WriteError::Unreserved("fix-it".into(), n));
}

// Behaviour: the store log itself refuses a number that is not an empty change at its home, one
// another write took, whatever the caller's home believes.
#[test]
fn the_store_refuses_a_number_another_write_took() {
    let source = store();
    let mut home: Home<Convoy, StoreLog> = Home::with_log(HOME, StoreLog::new(Arc::clone(&source))).expect("home");
    let taken = home.create("first", request("first"), &world(), Timestamp(1)).expect("first launch").resource.version().0;
    let mut log = StoreLog::new(Arc::clone(&source));
    let launch = home.get(&format!("first#{taken}")).expect("held").clone();
    assert!(Log::<Convoy>::create_reserved(&mut log, HOME, Timestamp(2), launch, taken, &[]).is_err(), "a number a create took");
}
