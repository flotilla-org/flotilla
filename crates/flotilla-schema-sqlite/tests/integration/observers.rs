//! Observers of a kind's writes (#3108 open-stall set): told inside each write's transaction, of
//! this home's writes, another home's applied and a snapshot's restored, with what they stage run
//! only once the transaction commits.

use std::sync::{Arc, Mutex};

use flotilla_schema_sqlite::{Observed, Observer, Staged, Store};
use rusqlite::Connection;

use crate::fixtures::{carry_changes, message as messages, origin, registry, store, Message, Resource, LOCAL};

/// What the observer saw, in commit order: `name@home` and whether it holds a value, or a reset.
type Seen = Arc<Mutex<Vec<String>>>;

fn observing(store: &mut Store) -> Seen {
    let seen = Seen::default();
    let staged_to = Arc::clone(&seen);
    let observer: Arc<Observer<Resource<Message>>> = Arc::new(move |observed: Observed<'_, Resource<Message>>| {
        let line = match observed {
            Observed::Written { name, home, value } => format!("{name}@{home} {}", if value.is_some() { "held" } else { "gone" }),
            Observed::Reset { home } => format!("reset {home}"),
        };
        let seen = Arc::clone(&staged_to);
        let staged: Staged = Box::new(move || seen.lock().expect("seen").push(line));
        Some(staged)
    });
    store.observe(observer);
    seen
}

fn taken(seen: &Seen) -> Vec<String> {
    std::mem::take(&mut *seen.lock().expect("seen"))
}

// Behaviour: an observer is told of this home's insert and delete, another home's applied write,
// a snapshot restored and a replica reset; what it stages runs only when the write commits, and a
// write dropped uncommitted runs none of it.
#[test]
fn observers_see_every_lowering_once_committed() {
    let mut local = store();
    let seen = observing(&mut local);

    let mut write = local.write().expect("write");
    write.insert("dropped", &messages::sample(), &origin()).expect("insert");
    assert_eq!(taken(&seen), Vec::<String>::new(), "nothing runs before the commit");
    drop(write);
    assert_eq!(taken(&seen), Vec::<String>::new(), "an uncommitted write runs nothing");

    let mut write = local.write().expect("write");
    let stamp = write.insert("mine", &messages::sample(), &origin()).expect("insert");
    write.commit().expect("commit");
    let mut write = local.write().expect("write");
    write.delete::<Resource<Message>>(stamp, &origin()).expect("delete");
    write.commit().expect("commit");
    assert_eq!(taken(&seen), [format!("mine@{LOCAL} held"), format!("mine@{LOCAL} gone")]);

    let mut remote = Store::open(Connection::open_in_memory().expect("db"), "remote", registry()).expect("open remote");
    let mut write = remote.write().expect("write");
    write.insert("far", &messages::sample(), &origin()).expect("insert far");
    let snapped = write.insert("snapped", &messages::sample(), &origin()).expect("insert snapped");
    write.commit().expect("commit");
    let changes = remote.read().expect("read").changes_since("remote", 0, 1).expect("log");
    let snapshot = remote.read().expect("read").load_snapshot(snapped.id).expect("snapshot").expect("held");

    let mut write = local.write().expect("write");
    carry_changes(&mut remote, &mut write, &changes);
    write.apply(&changes[0]).expect("apply");
    write.restore(&snapshot).expect("restore");
    write.commit().expect("commit");
    assert_eq!(taken(&seen), ["far@remote held", "snapped@remote held"]);

    let mut write = local.write().expect("write");
    write.reset_replica("remote").expect("reset");
    write.commit().expect("commit");
    assert_eq!(taken(&seen), ["reset remote"]);
}
