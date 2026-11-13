//! A resource's creation time (#3108): its create's change time widened to nanoseconds, or the
//! old creation time an import's create carries, held on its identity and read by `Read::created`.

use flotilla_schema::{ChangeItem, CreatedAt, Edge, EdgeEvent, Identity, Kind, NewChange, State, Timestamp};
use flotilla_schema_sqlite::{stream, ResourceId, Store, StoreError};
use rusqlite::Connection;

use crate::fixtures::{
    registry, store,
    synthetic::{DossierEdge, DossierState},
    Dossier, Payload, LOCAL,
};

/// An import's old creation time: nanoseconds a change's microsecond time cannot hold.
const IMPORTED: CreatedAt = CreatedAt(100_123_456_789);

/// A store homed at `home`.
fn store_at(home: &str) -> Store {
    Store::open(Connection::open_in_memory().expect("memory"), home, registry()).expect("store")
}

/// Streams `source`'s home to `target`: a snapshot when `after` is none, else the entries after it.
fn sync(source: &mut Store, target: &mut Store, after: Option<u64>) {
    let home = source.home().to_owned();
    stream::Follower::new(target).receive(&home, |sink| stream::serve(source, after, Timestamp(9_999), sink).map(|_| ())).expect("sync");
}

fn created_at(store: &mut Store, name: &str, home: &str) -> Option<CreatedAt> {
    let id = store.read().expect("read").resolve(Dossier::CODE, name, home).expect("resolve").expect("held");
    created(store, id)
}

fn at(time: i64) -> NewChange {
    NewChange::new("test", Timestamp(time))
}

/// The creation time of a create at change time `micros`.
fn widened(micros: i64) -> Option<CreatedAt> {
    Some(CreatedAt(micros * 1_000))
}

fn created(store: &mut Store, id: ResourceId) -> Option<CreatedAt> {
    store.read().expect("read").created(id).expect("created")
}

fn deliver(time: i64) -> Edge<Dossier> {
    let event = EdgeEvent {
        edge: DossierEdge::Deliver.name(),
        from: DossierState::Open,
        to: DossierState::Delivered,
        at: Timestamp(time),
        arguments: Payload { args: DossierEdge::Deliver, reads: Vec::new() },
    };
    Edge::new(Dossier::STATE, &State::new(DossierState::Delivered, Timestamp(time)), &event).expect("edge")
}

// Behaviour: a resource reads back the time of the change that created it, in nanoseconds. An
// edit, an edge and a delete keep it; a create under the same identity after a delete is a new
// creation and replaces it.
#[test]
fn a_resource_reads_the_time_of_the_change_that_created_it() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("d", &Dossier::sample(0), &at(100)).expect("insert");
    write.commit().expect("commit");
    assert_eq!(created(&mut store, stamp.id), widened(100), "the creating change's time");

    let mut write = store.write().expect("write");
    let current = write.load::<Dossier>(stamp.id).expect("load").expect("present");
    let edited = write.update(&current, &Dossier::sample(1), &at(200)).expect("edit");
    let delivered = write.apply_edge(edited, deliver(300), &at(300)).expect("edge");
    write.commit().expect("commit");
    assert_eq!(created(&mut store, stamp.id), widened(100), "an edit or an edge is not a creation");

    let mut write = store.write().expect("write");
    write.delete::<Dossier>(delivered, &at(400)).expect("delete");
    write.commit().expect("commit");
    assert_eq!(created(&mut store, stamp.id), widened(100), "a delete keeps the identity's creation time");

    let mut write = store.write().expect("write");
    let again = write.insert("d", &Dossier::sample(0), &at(500)).expect("recreate");
    write.commit().expect("commit");
    assert_eq!(again.id, stamp.id, "a recreate takes the same identity");
    assert_eq!(created(&mut store, again.id), widened(500), "a recreate is a new creation");
}

// Behaviour: an identity first written as a change's item, before its resource is created, has no
// creation time; the resource's create then gives it that create's time.
#[test]
fn a_referenced_identity_has_no_creation_time_until_its_create() {
    let mut store = store();
    let named = Identity { kind: Dossier::CODE, name: "later".into(), home: LOCAL.into() };
    let mut write = store.write().expect("write");
    write
        .insert("first", &Dossier::sample(0), &at(100).with_items([ChangeItem { record: named.clone(), revision: None }]))
        .expect("insert");
    write.commit().expect("commit");
    let id = store.read().expect("read").resolve(Dossier::CODE, "later", LOCAL).expect("resolve").expect("the item's identity");
    assert_eq!(created(&mut store, id), None, "named, not created");
    let error = store.read().expect("read").creation_time(id).expect_err("a held object's creation time is never made up");
    assert!(matches!(error, StoreError::NotCreated { identity } if identity == named), "names the record");

    let mut write = store.write().expect("write");
    let stamp = write.insert("later", &Dossier::sample(0), &at(200)).expect("insert");
    write.commit().expect("commit");
    assert_eq!(stamp.id, id);
    assert_eq!(created(&mut store, id), widened(200));
}

// Behaviour (import): a create given an old creation time reads it to the nanosecond, whether its
// new change carries it or it is given beside a change opened before; the change keeps its own time.
#[test]
fn an_imported_create_reads_its_old_creation_time_exactly() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let carried = write.insert("carried", &Dossier::sample(0), &at(100).created_at(IMPORTED)).expect("insert");
    let opened = write.open_change(&at(100)).expect("open");
    let given = write.insert_created("given", &Dossier::sample(0), opened, IMPORTED).expect("insert");
    let local = write.insert("local", &Dossier::sample(0), opened).expect("insert");
    write.commit().expect("commit");
    assert_eq!(created(&mut store, carried.id), Some(IMPORTED), "carried by the new change");
    assert_eq!(created(&mut store, given.id), Some(IMPORTED), "given beside an opened change");
    assert_eq!(created(&mut store, local.id), widened(100), "the opened change's other creates are at its time");
}

// Behaviour (replication): a replica reads the home's creation time, an imported one to the
// nanosecond included, whether it installed the resource from the home's snapshot or applied the
// home's create event, and an edit it applies keeps it.
#[test]
fn a_replica_reads_the_homes_creation_time() {
    let mut home = store_at("home");
    let mut replica = store_at("replica");
    let mut write = home.write().expect("write");
    let first = write.insert("first", &Dossier::sample(0), &at(100)).expect("insert");
    write.insert("imported", &Dossier::sample(0), &at(100).created_at(IMPORTED)).expect("insert");
    write.commit().expect("commit");
    sync(&mut home, &mut replica, None);
    assert_eq!(created_at(&mut replica, "first", "home"), widened(100), "installed from the snapshot");
    assert_eq!(created_at(&mut replica, "imported", "home"), Some(IMPORTED), "an import's, installed from the snapshot");

    let after = home.read().expect("read").last_sequence("home").expect("last");
    let mut write = home.write().expect("write");
    write.insert("second", &Dossier::sample(0), &at(300)).expect("insert");
    write.insert("imported-later", &Dossier::sample(0), &at(300).created_at(IMPORTED)).expect("insert");
    let current = write.load::<Dossier>(first.id).expect("load").expect("present");
    write.update(&current, &Dossier::sample(1), &at(400)).expect("edit");
    write.commit().expect("commit");
    sync(&mut home, &mut replica, Some(after));
    assert_eq!(created_at(&mut replica, "second", "home"), widened(300), "applied from the create event");
    assert_eq!(created_at(&mut replica, "imported-later", "home"), Some(IMPORTED), "an import's, applied from the create event");
    assert_eq!(created_at(&mut replica, "first", "home"), widened(100), "an applied edit keeps it");
}
