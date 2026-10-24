//! A standing convoy compacts at the cap without changing its identity.
use flotilla_engine_store::{convoy_registry, StoreLog};
use flotilla_schema::{Address, Frozen, Kind, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::convoy::*, Home, Ref, Resource, WriteError};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[test]
fn standing_rollover_keeps_identity_live_seats_and_standing_duties() {
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
    home.apply(name, Rollover { by: by.clone(), authority: authority.clone() }, &world, Timestamp(5)).unwrap();
    let held = home.get(name).unwrap();
    assert_eq!(held.name(), address.as_str());
    assert_eq!(held.spec().seats.len(), 2);
    assert!(held.spec().seats.iter().all(|row| row.version == held.version().0));
    assert_eq!(held.spec().seat(&child).unwrap().1.parent, None);
    assert_eq!(held.spec().duties.keys().copied().collect::<Vec<_>>(), vec![DutyId(0)]);
    assert!(held.spec().promises.is_empty());
    assert_eq!(held.spec().promise_floor, 10);
    assert!(matches!(
        home.apply(name, ReplaceSeat(a.clone()), &world, Timestamp(6)),
        Err(WriteError::Refused(ConvoyRefusal::ReplacementsExhausted))
    ));
    home.apply(name, add(b.clone()), &world, Timestamp(7)).unwrap();
    assert_eq!(home.get(name).unwrap().spec().seat(&b).unwrap().1.replacements, 0);
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
    let mut event = store.lock().unwrap().read().unwrap().changes_since("home", 0, 1).unwrap().remove(0);
    // Previous-generation create events did not name the three new fields.
    event.effect.changes.retain_mut(|change| match change {
        flotilla_schema::Change::Set { row, field, .. } if row.is_empty() && field == "promise_floor" => false,
        flotilla_schema::Change::Append { records, values, .. } if records == "seats" => {
            values.retain(|name, _| name != "retired" && name != "replacements");
            true
        }
        _ => true,
    });
    let mut follower = Store::open(rusqlite::Connection::open_in_memory().unwrap(), "follower", convoy_registry().unwrap()).unwrap();
    let mut write = follower.write().unwrap();
    write.apply(&event).unwrap();
    write.commit().unwrap();
    let mut read = follower.read().unwrap();
    let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
    let spec = read.load::<Resource<Convoy>>(id).unwrap().unwrap().value.spec().clone();
    assert_eq!(spec.promise_floor, 0);
    assert!(!spec.seats.iter().next().unwrap().record.retired);
    assert_eq!(spec.seats.iter().next().unwrap().record.replacements, 0);
    drop(read);
    drop(home);
    let conn = Arc::try_unwrap(store).unwrap_or_else(|_| panic!("store still held")).into_inner().unwrap().into_connection();
    // Recreate the exact old table shape, with the prior-generation rows still present.
    conn.execute_batch("DROP VIEW convoy_view; DROP VIEW convoy_seats_view; ALTER TABLE convoy DROP COLUMN promise_floor; ALTER TABLE convoy_seats DROP COLUMN retired; ALTER TABLE convoy_seats DROP COLUMN replacements;").unwrap();
    let mut reopened = Store::open(conn, "home", convoy_registry().unwrap()).unwrap();
    let mut read = reopened.read().unwrap();
    let id = read.resolve(Resource::<Convoy>::CODE, name, "home").unwrap().unwrap();
    let loaded = read.load::<Resource<Convoy>>(id).unwrap().unwrap();
    assert_eq!(loaded.value.spec(), &spec);
}
