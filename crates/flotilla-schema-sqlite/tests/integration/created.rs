//! A resource's creation time (#3108): its create's change time widened to nanoseconds, held on
//! its identity and read by `Read::created`.

use flotilla_schema::{ChangeItem, CreatedAt, Edge, EdgeEvent, Identity, Kind, NewChange, State, Timestamp};
use flotilla_schema_sqlite::{ResourceId, Store, StoreError};

use crate::fixtures::{
    store,
    synthetic::{DossierEdge, DossierState},
    Dossier, Payload, LOCAL,
};

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
