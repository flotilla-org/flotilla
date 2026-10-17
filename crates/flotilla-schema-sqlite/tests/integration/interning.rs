use flotilla_schema::Kind;

use crate::fixtures::{message, origin, store, Message, Referral, Resource, LOCAL};

fn message() -> Resource<Message> {
    message::sample()
}

// Behaviour (DESIGN interning): ids created in a transaction that rolls back are never served
// from the cache. The rolled-back transaction interns "first"; the next transaction creates
// "second", which SQLite gives the freed id. A cache that kept the rolled-back id would resolve
// "first" to "second"'s row, and the later insert of "first" would load under the wrong name.
#[test]
fn ids_from_a_rolled_back_transaction_are_evicted() {
    let mut store = store();
    let mut write = store.write().expect("write");
    write.insert("first", &message(), &origin()).expect("insert");
    drop(write);

    let mut write = store.write().expect("write");
    let second = write.insert("second", &message(), &origin()).expect("insert");
    let first = write.insert("first", &message(), &origin()).expect("insert the rolled-back name again");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.resolve(Resource::<Message>::CODE, "first", LOCAL).expect("resolve"), Some(first.id));
    assert_eq!(read.resolve(Resource::<Message>::CODE, "second", LOCAL).expect("resolve"), Some(second.id));
    assert_eq!(read.load::<Resource<Message>>(first.id).expect("load").expect("present").name, "first");
    assert_eq!(read.load::<Resource<Message>>(second.id).expect("load").expect("present").name, "second");
}

// Behaviour: a write refused inside a transaction rewinds the ids it created with its savepoint,
// so the same transaction can still commit names that the refused write interned.
#[test]
fn ids_from_a_refused_write_are_evicted() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let refused = Referral { seq: u64::MAX, ..Referral::sample() };
    assert!(write.insert("first", &refused, &origin()).is_err());
    let first = write.insert("first", &Referral::sample(), &origin()).expect("insert");
    write.commit().expect("commit");
    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Referral>(first.id).expect("load").expect("present").name, "first");
}
