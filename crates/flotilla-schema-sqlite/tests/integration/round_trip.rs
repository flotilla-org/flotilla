use flotilla_schema::Kind;
use hegel::TestCase;

use crate::fixtures::{store, Message, LOCAL};

// Behaviour (#3320 round trip): insert, then load_snapshot and decode, gives back the value with
// times truncated to microseconds; the same holds after an update to another generated value.
// `load` agrees with the snapshot, and an update that changes rows takes a later version.
#[hegel::test]
fn message_round_trips_through_insert_and_update(tc: TestCase) {
    let first = Message::generate(&tc);
    let second = Message::generate(&tc);
    let mut store = store();

    let mut write = store.write().expect("write");
    let inserted = write.insert("m", &first).expect("insert");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let snapshot = read.load_snapshot(inserted.id).expect("load").expect("present");
    assert_eq!((snapshot.kind, snapshot.name.as_str(), snapshot.home.as_str()), (Message::CODE, "m", LOCAL));
    assert_eq!(snapshot.version, inserted.version);
    assert_eq!(snapshot.decode::<Message>().expect("decode"), first.stored());
    let loaded = read.load::<Message>(inserted.id).expect("load").expect("present");
    assert_eq!(loaded.value, first.stored());
    drop(read);

    let mut write = store.write().expect("write");
    let updated = write.update(&loaded, &second).expect("update");
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
