use flotilla_schema::Kind;
use flotilla_schema_sqlite::{Stamp, Store, StoreError};

use rusqlite::Connection;

use crate::fixtures::{registry, Message, LOCAL};

// Behaviour (#3320 ownership): the generated write path refuses rows whose identity is homed
// elsewhere; only the replication applier writes them. The foreign row is written by a store
// homed at `remote` over the same database, as a replica would hold it.
#[test]
fn writes_to_resources_homed_elsewhere_are_refused() {
    let conn = Connection::open_in_memory().expect("in-memory database");
    let mut remote = Store::open(conn, "remote", registry()).expect("open as remote");
    let mut write = remote.write().expect("write");
    write.insert("far", &Message { body: "replicated".into(), ..Message::sample() }).expect("replica row");
    write.commit().expect("commit");
    let conn = remote.into_connection();
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");

    let mut read = store.read().expect("read");
    let id = read.resolve(Message::CODE, "far", "remote").expect("resolve").expect("present");
    assert_eq!(read.resolve(Message::CODE, "far", LOCAL).expect("resolve"), None);
    let current = read.load::<Message>(id).expect("load").expect("present");
    assert_eq!((current.home.as_str(), current.version, current.value.body.as_str()), ("remote", 1, "replicated"));
    drop(read);

    let mut write = store.write().expect("write");
    let next = Message { body: "local edit".into(), ..current.value.clone() };
    assert!(matches!(write.update(&current, &next), Err(StoreError::NotHome { home, .. }) if home == "remote"));
    assert!(matches!(write.delete::<Message>(Stamp { id, version: 1 }), Err(StoreError::NotHome { .. })));
    // A local resource of the same name is a different identity.
    let local = write.insert("far", &next).expect("local insert");
    assert_ne!(local.id, id);
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(id).expect("load").expect("present").value, current.value);
}
