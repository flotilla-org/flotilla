//! A convoy's seats grow within the cap; retired seats keep their names.
use flotilla_engine_store::{convoy_registry, StoreLog};
use flotilla_schema::{Address, Frozen, Kind, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::convoy::*, Home, Ref, Resource, WriteError};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[test]
fn retired_seats_keep_their_names_and_replicas_follow_seat_edges() {
    let name = "project:flotilla/convoy:governor";
    let address: Address = name.parse().unwrap();
    let store =
        Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().unwrap(), "home", convoy_registry().unwrap()).unwrap()));
    let mut home = Home::<Convoy, _>::with_log("home", StoreLog::new(Arc::clone(&store))).unwrap();
    let project: Address = "project:flotilla".parse().unwrap();
    let authority = Ref::new("ensure");
    let by = SeatName::new("governor", "");
    let a = by.clone();
    let b = SeatName::new("temporary", "");
    let child = SeatName::new("child", "");
    let mut world = MemoryConvoyWorld::default().with_project(&project).with_limits(3, 1);
    world.authority.put(&authority, true);
    let seat = |name: &SeatName, parent| Seat {
        role: name.role.clone(),
        seat: name.seat.clone(),
        parent,
        supervised_by: address.clone(),
        failed: None,
        retired: false,
        replacements: 0,
    };
    home.create(
        name,
        ConvoyRequest {
            project,
            priority: Priority(0),
            kinds: Frozen::new(PromiseKinds::default()).unwrap(),
            seats: vec![seat(&a, None), seat(&b, Some(a.clone())), seat(&child, Some(b.clone()))],
            promises: BTreeMap::from([(
                PromiseId(9),
                AdmittedPromise {
                    label: "old".into(),
                    kind: "opaque".into(),
                    target: address.clone(),
                    source: PromiseSource::Crew,
                    owner: b.clone(),
                },
            )]),
            duties: BTreeMap::from([
                (DutyId(0), Duty { owner: a.clone(), scope: DutyScope::Lifetime, terms: Terms::default() }),
                (
                    DutyId(1),
                    Duty {
                        owner: b.clone(),
                        scope: DutyScope::WhileOpen { promises: BTreeSet::from([PromiseId(9)]) },
                        terms: Terms::default(),
                    },
                ),
            ]),
            quota: BTreeMap::new(),
        },
        &world,
        Timestamp(0),
    )
    .unwrap();
    home.apply(name, ReplaceSeat(a.clone()), &world, Timestamp(1)).unwrap();
    home.apply(
        name,
        Keep::Verdict { promise: PromiseId(9), by: by.clone(), authority: Authority::Operator, reason: "done".into() },
        &world,
        Timestamp(2),
    )
    .unwrap();
    home.apply(name, RetireSeat { seat: b.clone(), by: by.clone(), authority: authority.clone() }, &world, Timestamp(3)).unwrap();
    // A retired name remains reserved, and it still occupies the cap.
    let add =
        |name| AddSeat { seat: name, parent: a.clone(), supervised_by: address.clone(), by: by.clone(), authority: authority.clone() };
    assert!(matches!(home.apply(name, add(b.clone()), &world, Timestamp(4)), Err(WriteError::Refused(ConvoyRefusal::SeatExists))));
    assert!(matches!(
        home.apply(name, add(SeatName::new("extra", "")), &world, Timestamp(4)),
        Err(WriteError::Refused(ConvoyRefusal::SeatsFull))
    ));
    // Repointing a retired seat is allowed, replacing it is refused.
    home.apply(
        name,
        Resupervise { seat: b.clone(), supervised_by: "project:other".parse().unwrap(), by: by.clone(), authority: authority.clone() },
        &world,
        Timestamp(4),
    )
    .unwrap();
    assert!(matches!(home.apply(name, ReplaceSeat(b.clone()), &world, Timestamp(4)), Err(WriteError::Refused(ConvoyRefusal::SeatRetired))));
    // A Fail verdict and a subject's link, unlink and relink write their rows in place.
    let fail = FailSeat { seat: child.clone(), reason: "crashed".into(), by: by.clone(), authority: authority.clone() };
    home.apply(name, fail, &world, Timestamp(5)).unwrap();
    let change: Address = "project:change".parse().unwrap();
    let link = || Link { subject: change.clone(), source: LinkSource::Claim, by: by.clone(), authority: authority.clone() };
    home.apply(name, link(), &world, Timestamp(5)).unwrap();
    home.apply(name, Unlink { subject: change.clone(), by: by.clone(), authority: authority.clone() }, &world, Timestamp(5)).unwrap();
    home.apply(name, link(), &world, Timestamp(5)).unwrap();
    let held = home.get(name).unwrap();
    assert_eq!(held.spec().seats.len(), 3);
    assert_eq!(held.spec().seat(&child).unwrap().1.failed.as_deref(), Some("crashed"));
    assert_eq!(held.spec().subjects.iter().map(|row| row.record.unlinked).collect::<Vec<_>>(), [true, false]);
    // The home's copy carries the rows' create stamps, as storage does, and every entry
    // replays on a replica to the same convoy.
    {
        let mut database = store.lock().unwrap();
        let mut read = database.read().unwrap();
        let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
        let loaded = read.load::<Resource<Convoy>>(id).unwrap().unwrap();
        assert_eq!(&loaded.value, held);
        let changes = read.changes(id, 0, 100).unwrap();
        let headers = read.changes_of(&changes).unwrap();
        let mut replica = Store::open(rusqlite::Connection::open_in_memory().unwrap(), "replica", convoy_registry().unwrap()).unwrap();
        let mut write = replica.write().unwrap();
        // Each entry's change first, as the stream sends it (#4024).
        headers.iter().for_each(|header| write.record_change(header).unwrap());
        for change in &changes {
            write.apply(change).unwrap();
        }
        let peer_id = write.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
        let after = write.load::<Resource<Convoy>>(peer_id).unwrap().unwrap();
        assert_eq!(&after.value, held);
        write.commit().unwrap();
    }
    assert!(matches!(
        home.apply(name, ReplaceSeat(a.clone()), &world, Timestamp(6)),
        Err(WriteError::Refused(ConvoyRefusal::ReplacementsExhausted))
    ));
    let held = home.get(name).unwrap();
    let mut database = store.lock().unwrap();
    let mut read = database.read().unwrap();
    let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
    let loaded = read.load::<Resource<Convoy>>(id).unwrap().unwrap();
    assert_eq!(&loaded.value, held);
}

#[test]
fn previous_database_and_create_event_decode_with_growth_defaults() {
    let name = "standing";
    let project: Address = "project:fleet".parse().unwrap();
    let owner = SeatName::new("governor", "");
    let store =
        Arc::new(Mutex::new(Store::open(rusqlite::Connection::open_in_memory().unwrap(), "home", convoy_registry().unwrap()).unwrap()));
    let mut home = Home::<Convoy, _>::with_log("home", StoreLog::new(Arc::clone(&store))).unwrap();
    let world = MemoryConvoyWorld::default().with_project(&project);
    home.create(
        name,
        ConvoyRequest {
            project: project.clone(),
            priority: Priority(0),
            kinds: Frozen::new(PromiseKinds::default()).unwrap(),
            seats: vec![Seat {
                role: owner.role.clone(),
                seat: owner.seat.clone(),
                parent: None,
                supervised_by: project,
                failed: None,
                retired: false,
                replacements: 0,
            }],
            promises: BTreeMap::new(),
            duties: BTreeMap::from([(DutyId(0), Duty { owner, scope: DutyScope::Lifetime, terms: Terms::default() })]),
            quota: BTreeMap::new(),
        },
        &world,
        Timestamp(0),
    )
    .unwrap();
    let (mut event, header) = {
        let mut database = store.lock().unwrap();
        let mut read = database.read().unwrap();
        let event = read.changes_since("home", 0, 1).unwrap().remove(0);
        let header = read.changes_of(std::slice::from_ref(&event)).unwrap().remove(0);
        (event, header)
    };
    // Previous-generation create events did not name the two seat growth fields.
    event.effect.changes.retain_mut(|change| match change {
        flotilla_schema::Change::Append { records, values, .. } if records == "seats" => {
            values.retain(|name, _| name != "retired" && name != "replacements");
            true
        }
        _ => true,
    });
    let mut follower = Store::open(rusqlite::Connection::open_in_memory().unwrap(), "follower", convoy_registry().unwrap()).unwrap();
    let mut write = follower.write().unwrap();
    write.record_change(&header).unwrap();
    write.apply(&event).unwrap();
    write.commit().unwrap();
    let mut read = follower.read().unwrap();
    let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
    let spec = read.load::<Resource<Convoy>>(id).unwrap().unwrap().value.spec().clone();
    assert!(!spec.seats.iter().next().unwrap().record.retired);
    assert_eq!(spec.seats.iter().next().unwrap().record.replacements, 0);
    drop(read);
    drop(home);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().unwrap().into_connection();
    // Recreate the exact old table shape, with the prior-generation rows still present.
    conn.execute_batch("DROP VIEW convoy_view; DROP VIEW convoy_seats_view; ALTER TABLE convoy_seats DROP COLUMN retired; ALTER TABLE convoy_seats DROP COLUMN replacements;").unwrap();
    let mut reopened = Store::open(conn, "home", convoy_registry().unwrap()).unwrap();
    let mut read = reopened.read().unwrap();
    let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
    let loaded = read.load::<Resource<Convoy>>(id).unwrap().unwrap();
    assert_eq!(loaded.value.spec(), &spec);
}

#[test]
fn failed_old_shape_upgrade_rolls_back_eligible_columns() {
    // Keep a second connection alive so the database survives Store::open's
    // failed, owning connection and can be inspected after its transaction drops.
    let uri = "file:seat_growth_failed_upgrade?mode=memory&cache=shared";
    let conn = rusqlite::Connection::open(uri).unwrap();
    let store = Store::open(conn, "home", convoy_registry().unwrap()).unwrap();
    let observer = store.into_connection();
    // `retired` is eligible to be added; `failed` has no previous-generation default, so its absence is drift.
    observer.execute_batch("DROP VIEW convoy_view; DROP VIEW convoy_seats_view; ALTER TABLE convoy_seats DROP COLUMN retired; ALTER TABLE convoy_seats DROP COLUMN failed;").unwrap();
    let definition =
        || observer.query_row("SELECT sql FROM sqlite_master WHERE name = 'convoy_seats'", [], |row| row.get::<_, String>(0)).unwrap();
    let before = definition();
    assert!(!before.contains("retired"));
    let result = Store::open(rusqlite::Connection::open(uri).unwrap(), "home", convoy_registry().unwrap());
    assert!(matches!(result, Err(flotilla_schema_sqlite::StoreError::KindTableDrift { table, .. }) if table == "convoy_seats"));
    assert_eq!(definition(), before, "the attempted ADD COLUMN must roll back on unrelated drift");
}
