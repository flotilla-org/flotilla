//! Owner-ruling proposal: storage assigns Message admission in the create transaction.
use std::sync::{Arc, Mutex};

use flotilla_inbox::sqlite::{MessageLog, StoreLog};
use flotilla_inbox_testkit::{spec, SessionPort};
use flotilla_schema::{Kind, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::message::{MemoryMessageWorld, Message},
    Home, Resource,
};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

// Behaviour: intervening writers may advance the home after request preparation;
// create atomically assigns admission and Home retains exactly what was persisted.
// Counts include no competitors and gaps spanning a digit boundary. Caller-supplied
// sequence values are deliberately wrong, including zero and the maximum value.
#[hegel::test(test_cases = 8)]
fn transaction_assigns_admission_to_home_and_store(tc: TestCase) {
    let competitors = tc.draw(gs::sampled_from(vec![0usize, 1, 10]));
    let supplied = tc.draw(gs::sampled_from(vec![0u64, 1, u64::MAX]));
    let store = Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().expect("db"), "test", flotilla_state_machines::kinds().expect("registry")).expect("store"),
    ));
    let mut request = spec();
    request.admitted_seq = supplied;
    let mut home = Home::<Message, MessageLog>::with_log("test", MessageLog::new(store.clone())).expect("home");
    for n in 0..competitors {
        let _other = SessionPort::new(&format!("other-{n}"), StoreLog::new(store.clone()));
    }
    home.create("message", request, &MemoryMessageWorld::default(), Timestamp(10)).expect("transaction admission");
    let resident = home.get("message").expect("resident");
    assert_eq!(resident.spec().admitted_seq, competitors as u64 + 1);
    assert_eq!(resident.version().0, resident.spec().admitted_seq);
    let mut locked = store.lock().expect("store");
    let mut read = locked.read().expect("read");
    let id = read.resolve(Resource::<Message>::CODE, "message", "test").expect("resolve").expect("id");
    let persisted = read.load::<Resource<Message>>(id).expect("load").expect("resource");
    assert_eq!(persisted.value.spec(), resident.spec());
    assert_eq!(persisted.value.version(), resident.version());
}
