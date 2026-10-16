use flotilla_schema::Kind;

use crate::fixtures::{store, Message, LOCAL};

fn message() -> Message {
    Message::sample()
}

// Behaviour (DESIGN interning): ids created in a transaction that rolls back are never served
// from the cache. The rolled-back transaction interns "first"; the next transaction creates
// "second", which SQLite gives the freed id. A cache that kept the rolled-back id would resolve
// "first" to "second"'s row, and the later insert of "first" would load under the wrong name.
#[test]
fn ids_from_a_rolled_back_transaction_are_evicted() {
    let mut store = store();
    let mut write = store.write().expect("write");
    write.insert("first", &message()).expect("insert");
    drop(write);

    let mut write = store.write().expect("write");
    let second = write.insert("second", &message()).expect("insert");
    let first = write.insert("first", &message()).expect("insert the rolled-back name again");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.resolve(Message::CODE, "first", LOCAL).expect("resolve"), Some(first.id));
    assert_eq!(read.resolve(Message::CODE, "second", LOCAL).expect("resolve"), Some(second.id));
    assert_eq!(read.load::<Message>(first.id).expect("load").expect("present").name, "first");
    assert_eq!(read.load::<Message>(second.id).expect("load").expect("present").name, "second");
}

// Behaviour: a write refused inside a transaction rewinds the ids it created with its savepoint,
// so the same transaction can still commit names that the refused write interned.
#[test]
fn ids_from_a_refused_write_are_evicted() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let refused = Message { accepted_seq: u64::MAX, ..message() };
    assert!(write.insert("first", &refused).is_err());
    let first = write.insert("first", &message()).expect("insert");
    write.commit().expect("commit");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(first.id).expect("load").expect("present").name, "first");
}
