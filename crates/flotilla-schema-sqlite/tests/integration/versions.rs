use flotilla_schema_sqlite::{Stamp, StoreError};
use hegel::{generators as gs, TestCase};

use chrono::{DateTime, Duration};

use crate::fixtures::{floor, store, Message};

fn message(body: &str) -> Message {
    Message { body: body.into(), accepted_seq: 1, ..Message::sample() }
}

// Behaviour (ADR 0064, #3320 sequencing): every write takes the home's next sequence, so the
// versions it returns strictly increase across resources, updates, deletes and transactions,
// even when a deleted name is inserted again. The generated sequence mixes inserts of up to three
// names, updates and deletes, and commits after random steps; each step checks the invariant.
#[hegel::test]
fn versions_strictly_increase_across_writes(tc: TestCase) {
    let mut store = store();
    let mut live: Vec<Option<Stamp>> = vec![None; 3];
    let mut last = 0;
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let mut write = store.write().expect("write");
    for step in 0..steps {
        let slot = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let name = format!("m{slot}");
        let version = match live[slot] {
            None => {
                let stamp = write.insert(&name, &message(&format!("{step}"))).expect("insert");
                live[slot] = Some(stamp);
                Some(stamp.version)
            }
            Some(stamp) if tc.draw(gs::booleans()) => {
                let current = write.load::<Message>(stamp.id).expect("load").expect("present");
                let next = write.update(&current, &message(&format!("{step}"))).expect("update");
                live[slot] = Some(next);
                Some(next.version)
            }
            Some(stamp) => {
                write.delete::<Message>(stamp).expect("delete");
                live[slot] = None;
                None
            }
        };
        if let Some(version) = version {
            assert!(version > last, "version {version} after {last}");
            last = version;
        }
        if tc.draw(gs::booleans()) {
            write.commit().expect("commit");
            write = store.write().expect("write");
        }
    }
    write.commit().expect("commit");
}

// Behaviour (DESIGN writes): an update with no row changes takes no sequence and leaves the
// version unchanged, including a change below the stored microsecond resolution.
#[test]
fn update_without_row_changes_is_a_no_op() {
    let mut store = store();
    let time = DateTime::from_timestamp(1_700_000_000, 123_456_000).expect("in range");
    let value = Message { deadline: Some(time), ..message("same") };
    let mut write = store.write().expect("write");
    let stamp = write.insert("m", &value).expect("insert");
    let current = write.load::<Message>(stamp.id).expect("load").expect("present");
    assert_eq!(write.update(&current, &value).expect("no-op"), stamp);
    let finer = Message { deadline: Some(time + Duration::nanoseconds(999)), ..value.clone() };
    assert_eq!(floor(finer.deadline.expect("set")), time);
    assert_eq!(write.update(&current, &finer).expect("no-op below resolution"), stamp);
    // The next real write takes the very next sequence: the no-ops consumed none.
    let next = write.update(&current, &message("changed")).expect("update");
    assert_eq!(next.version, stamp.version + 1);
    write.commit().expect("commit");
}

// Behaviour (DESIGN writes): updates and deletes are optimistic on the version; a stale one is
// `Conflict`, and the refusal leaves the transaction usable for later writes.
#[test]
fn stale_versions_conflict() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("m", &message("a")).expect("insert");
    let stale = write.load::<Message>(stamp.id).expect("load").expect("present");
    write.update(&stale, &message("b")).expect("update");
    assert!(
        matches!(write.update(&stale, &message("c")), Err(StoreError::Conflict { id, expected }) if id == stamp.id && expected == stamp.version)
    );
    assert!(matches!(write.update(&stale, &message("a")), Err(StoreError::Conflict { .. })), "a stale no-op is also a conflict");
    assert!(matches!(write.delete::<Message>(stamp), Err(StoreError::Conflict { .. })));
    let unstorable = Stamp { id: stamp.id, version: u64::MAX };
    assert!(matches!(write.delete::<Message>(unstorable), Err(StoreError::Conflict { .. })));
    let other = write.insert("n", &message("n")).expect("the transaction is still usable");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    assert_eq!(read.load::<Message>(stamp.id).expect("load").expect("present").value, message("b"));
    assert!(read.load::<Message>(other.id).expect("load").is_some());
}

// Behaviour: an inserted name cannot be inserted again while it exists; after a delete it can.
#[test]
fn names_are_unique_per_kind_and_home() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("m", &message("a")).expect("insert");
    assert!(matches!(write.insert("m", &message("b")), Err(StoreError::Exists { .. })));
    write.delete::<Message>(stamp).expect("delete");
    assert!(write.load::<Message>(stamp.id).expect("load").is_none());
    let again = write.insert("m", &message("b")).expect("insert after delete");
    assert_eq!(again.id, stamp.id, "the identity row is kept");
    assert!(again.version > stamp.version);
    write.commit().expect("commit");
}

// Behaviour (DESIGN writes, many writes per transaction): a refused write leaves the batch
// transaction usable; the writes around it commit and the refused one leaves no trace.
#[test]
fn a_refused_write_does_not_spoil_its_batch() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let first = write.insert("a", &message("first")).expect("insert a");
    let current = write.load::<Message>(first.id).expect("load").expect("present");
    let moved = write.update(&current, &message("second")).expect("update");
    let refused = write.update(&current, &message("stale"));
    assert!(matches!(refused, Err(StoreError::Conflict { .. })), "{refused:?}");
    let other = write.insert("b", &message("other")).expect("insert b");
    write.commit().expect("commit");

    let mut read = store.read().expect("read");
    let a = read.load::<Message>(first.id).expect("load a").expect("a present");
    assert_eq!((a.value.body.as_str(), a.version), ("second", moved.version));
    assert_eq!(read.load::<Message>(other.id).expect("load b").expect("b present").value.body, "other");
}

// Behaviour: identity rows outlive their resource, so a write to a deleted resource is
// `NotFound` rather than a version `Conflict`.
#[test]
fn writes_to_a_deleted_resource_are_not_found() {
    let mut store = store();
    let mut write = store.write().expect("write");
    let stamp = write.insert("gone", &message("x")).expect("insert");
    let current = write.load::<Message>(stamp.id).expect("load").expect("present");
    write.delete::<Message>(stamp).expect("delete");
    assert!(matches!(write.update(&current, &message("y")), Err(StoreError::NotFound { .. })));
    assert!(matches!(write.delete::<Message>(stamp), Err(StoreError::NotFound { .. })));
}
