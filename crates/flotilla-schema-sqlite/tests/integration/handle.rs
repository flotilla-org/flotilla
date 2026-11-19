//! A timed store handle reports each hold: as a write when it began a write transaction, as a
//! read otherwise, with its lock's wait when another hold had the store.

use std::{
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use flotilla_schema_sqlite::{Hold, StoreHandle};

use crate::fixtures::store;

fn timed() -> (Arc<StoreHandle>, Arc<Mutex<Vec<Hold>>>) {
    let handle = Arc::new(StoreHandle::new(store()));
    let holds = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&holds);
    handle.time_holds(Arc::new(move |hold| seen.lock().expect("holds").push(hold))).expect("one timer");
    (handle, holds)
}

#[test]
fn each_hold_reports_as_a_read_or_a_write() {
    let (handle, holds) = timed();
    drop(handle.lock().expect("lock").read().expect("read"));
    drop(handle.lock().expect("lock").write().expect("write"));
    let mut both = handle.lock().expect("lock");
    drop(both.read().expect("read"));
    drop(both.write().expect("write"));
    drop(both);
    let holds = holds.lock().expect("holds");
    assert_eq!(holds.iter().map(|hold| hold.write).collect::<Vec<_>>(), [false, true, true], "a hold that writes is a write");
    assert!(holds.iter().all(|hold| hold.wait == Duration::ZERO), "a free lock waits for nothing: {holds:?}");
}

#[test]
fn a_hold_behind_another_reports_its_wait() {
    let (handle, holds) = timed();
    let (taken, waiting) = mpsc::channel();
    let holder = {
        let handle = Arc::clone(&handle);
        std::thread::spawn(move || {
            let held = handle.lock().expect("lock");
            taken.send(()).expect("taken");
            std::thread::sleep(Duration::from_millis(20));
            drop(held);
        })
    };
    waiting.recv().expect("the holder has the store");
    drop(handle.lock().expect("lock"));
    holder.join().expect("holder");
    let holds = holds.lock().expect("holds");
    assert_eq!(holds.len(), 2);
    assert!(holds[0].held >= Duration::from_millis(20), "{holds:?}");
    assert!(holds[1].wait > Duration::ZERO, "the second hold waited for the first: {holds:?}");
    assert!(handle.time_holds(Arc::new(|_| {})).is_err(), "a handle takes one timer");
}
