use flotilla_schema::Kind;
use flotilla_schema_sqlite::{Stamp, Store, StoreError};

use crate::fixtures::{registry, store, Message, LOCAL};

// Behaviour (#3320 ownership): the generated write path refuses rows whose identity is homed
// elsewhere; only the replication applier writes them. The foreign row is created with raw SQL,
// as a replica would hold it.
#[test]
fn writes_to_resources_homed_elsewhere_are_refused() {
    let conn = store().into_connection();
    conn.execute_batch(
        "INSERT INTO strings (s) VALUES ('remote'), ('far');
         INSERT INTO resource_identity (kind, name, home)
           VALUES (1, (SELECT id FROM strings WHERE s = 'far'), (SELECT id FROM strings WHERE s = 'remote'));
         INSERT INTO message (id, version, body, body_digest, interrupting, accepted_seq, deadline, ordinal)
           VALUES (last_insert_rowid(), 5, 'replicated', NULL, 0, 9, NULL, 2);",
    )
    .expect("replica rows");
    let mut store = Store::open(conn, LOCAL, registry()).expect("reopen");

    let mut read = store.read().expect("read");
    let id = read.resolve(Message::CODE, "far", "remote").expect("resolve").expect("present");
    assert_eq!(read.resolve(Message::CODE, "far", LOCAL).expect("resolve"), None);
    let current = read.load::<Message>(id).expect("load").expect("present");
    assert_eq!((current.home.as_str(), current.version, current.value.body.as_str()), ("remote", 5, "replicated"));
    drop(read);

    let mut write = store.write().expect("write");
    let next = Message { body: "local edit".into(), ..current.value.clone() };
    assert!(matches!(write.update(&current, &next), Err(StoreError::NotHome { home, .. }) if home == "remote"));
    assert!(matches!(write.delete::<Message>(Stamp { id, version: 5 }), Err(StoreError::NotHome { .. })));
    // A local resource of the same name is a different identity.
    let local = write.insert("far", &next).expect("local insert");
    assert_ne!(local.id, id);
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(id).expect("load").expect("present").value, current.value);
}
