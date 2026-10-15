use flotilla_schema::Kind;
use flotilla_schema_sqlite::Store;
use hegel::TestCase;

use crate::fixtures::{
    message::{Pick, COVERING},
    store, Message, LOCAL,
};

/// Inserts `first`, checks that the snapshot and `load` decode to it, updates to `second` and
/// checks the result again.
fn insert_then_update(store: &mut Store, name: &str, first: &Message, second: &Message) {
    let mut write = store.write().expect("write");
    let inserted = write.insert(name, first).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let snapshot = read.load_snapshot(inserted.id).expect("load").expect("present");
    assert_eq!((snapshot.kind, snapshot.name.as_str(), snapshot.home.as_str()), (Message::CODE, name, LOCAL));
    assert_eq!(snapshot.version, inserted.version);
    assert_eq!(snapshot.decode::<Message>().expect("decode"), first.stored());
    let loaded = read.load::<Message>(inserted.id).expect("load").expect("present");
    assert_eq!(loaded.value, first.stored());
    drop(read);

    let mut write = store.write().expect("write");
    let updated = write.update(&loaded, second).expect("update");
    write.commit().expect("commit");
    if second.stored() == first.stored() {
        assert_eq!(updated, inserted, "an update with no row changes is a no-op");
    } else {
        assert!(updated.version > inserted.version);
    }

    let mut read = store.read().expect("read");
    let snapshot = read.load_snapshot(inserted.id).expect("load").expect("present");
    assert_eq!(snapshot.version, updated.version);
    assert_eq!(snapshot.decode::<Message>().expect("decode"), second.stored());
}

// Behaviour (#3320 round trip): insert, then load_snapshot and decode, gives back the value with
// times truncated to microseconds; the same holds after an update to another generated value.
// `load` agrees with the snapshot, and an update that changes rows takes a later version.
#[hegel::test]
fn message_round_trips_through_insert_and_update(tc: TestCase) {
    let first = Message::generate(&tc, Pick::Draw);
    let second = Message::generate(&tc, Pick::Draw);
    insert_then_update(&mut store(), "m", &first, &second);
}

// Behaviour (#3320 round trip): the same law holds for every variant of every enum, nested and
// flattened ones included: the covering indexes reach each of them (see `COVERING`), and each
// update moves to the next index, so every enum also switches variant through an update.
#[hegel::test]
fn every_variant_round_trips_and_switches_through_updates(tc: TestCase) {
    let mut store = store();
    for index in 0..COVERING {
        let first = Message::generate(&tc, Pick::Cover(index));
        let second = Message::generate(&tc, Pick::Cover((index + 1) % COVERING));
        insert_then_update(&mut store, &format!("m{index}"), &first, &second);
    }
}
