use flotilla_schema_sqlite::Store;
use hegel::TestCase;

use crate::fixtures::{store, Convoy, Fixture, Message, Pick, TerminalSession, LOCAL};

/// Inserts `first`, checks that the snapshot and `load` decode to it, updates to `second` and
/// checks the result again.
fn insert_then_update<K: Fixture>(store: &mut Store, name: &str, first: &K, second: &K) {
    let mut write = store.write().expect("write");
    let inserted = write.insert(name, first).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let snapshot = read.load_snapshot(inserted.id).expect("load").expect("present");
    assert_eq!((snapshot.kind, snapshot.name.as_str(), snapshot.home.as_str()), (K::CODE, name, LOCAL));
    assert_eq!(snapshot.version, inserted.version);
    assert_eq!(snapshot.decode::<K>().expect("decode"), first.stored());
    let loaded = read.load::<K>(inserted.id).expect("load").expect("present");
    assert_eq!(loaded.value, first.stored());
    drop(read);

    let mut write = store.write().expect("write");
    let updated = write.update(&loaded, second).expect("an evolved value is an allowed update");
    write.commit().expect("commit");
    if second.stored() == first.stored() {
        assert_eq!(updated, inserted, "an update with no row changes is a no-op");
    } else {
        assert!(updated.version > inserted.version);
    }

    let mut read = store.read().expect("read");
    let snapshot = read.load_snapshot(inserted.id).expect("load").expect("present");
    assert_eq!(snapshot.version, updated.version);
    assert_eq!(snapshot.decode::<K>().expect("decode"), second.stored());
}

/// Drawn values: insert, then an allowed update (spec and child tables drawn afresh, records
/// replaced in place and appended, history appended).
fn drawn<K: Fixture>(tc: &TestCase) {
    let first = K::generate(tc, Pick::Draw);
    let second = first.evolve(tc, Pick::Draw);
    insert_then_update(&mut store(), "r", &first, &second);
}

/// Covering values: each index's value, updated to the next index's shapes, so every enum also
/// switches variant through an update and every collection grows and shrinks.
fn covering<K: Fixture>(tc: &TestCase) {
    let mut store = store();
    for index in 0..K::COVERING {
        let first = K::generate(tc, Pick::Cover(index));
        let second = first.evolve(tc, Pick::Cover((index + 1) % K::COVERING));
        insert_then_update(&mut store, &format!("r{index}"), &first, &second);
    }
}

// Behaviour (#3320 round trip): insert, then load_snapshot and decode, gives back the value with
// times truncated to microseconds; the same holds after a generated update with spec child
// changes, records appends and in-place replaces, and history appends. `load` agrees with the
// snapshot, and an update that changes rows takes a later version.
#[hegel::test]
fn message_round_trips_through_insert_and_update(tc: TestCase) {
    drawn::<Message>(&tc);
}

#[hegel::test]
fn convoy_round_trips_through_insert_and_update(tc: TestCase) {
    drawn::<Convoy>(&tc);
}

#[hegel::test]
fn terminal_session_round_trips_through_insert_and_update(tc: TestCase) {
    drawn::<TerminalSession>(&tc);
}

// Behaviour (#3320 round trip): the same law holds for every variant of every enum, nested,
// flattened and in child tables, and for empty and non-empty collections at every depth: the
// covering indexes reach each of them (see each fixture's `COVERING`).
#[hegel::test]
fn every_variant_round_trips_and_switches_through_updates(tc: TestCase) {
    covering::<Message>(&tc);
    covering::<Convoy>(&tc);
    covering::<TerminalSession>(&tc);
}
