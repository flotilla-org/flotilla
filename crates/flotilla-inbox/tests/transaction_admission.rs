//! Storage assigns Message admission in the create transaction (approved owner seam).
use std::sync::{Arc, Mutex};

use flotilla_inbox::sqlite::MessageLog;
use flotilla_inbox_testkit::{consumer, inbox, spec, SessionPort};
use flotilla_schema::{Kind, Origin, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::message::{MemoryMessageWorld, Message},
    Home, MemoryLog, Ref, Resource,
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
    // Several consumer creates share one write transaction, so each insert
    // must consume its own sequence before Message's transaction begins.
    let template = SessionPort::new("template", MemoryLog::new());
    {
        let mut locked = store.lock().expect("store");
        let mut write = locked.write().expect("write");
        for n in 0..competitors {
            write.insert(&format!("other-{n}"), template.resource(), &Origin::new("test", Timestamp(5))).expect("consumer insert");
        }
        write.commit().expect("batch commit");
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
    let entries = read.changes(id, 0, 1).expect("create log entry");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].home_seq, resident.spec().admitted_seq, "actual log sequence, not just two copies of the spec");
}

// Behaviour: admission metadata is not an input to admission or a Message gate.
// Compare admission read sets, pending judgement and Undeliverable gate results
// across zero, ordinary and maximum stamps with the same declared evidence.
#[hegel::test(test_cases = 8)]
fn stamp_does_not_change_gate_inputs(tc: TestCase) {
    use flotilla_state_machines::machines::message::*;
    let stamped = tc.draw(gs::sampled_from(vec![0u64, 1, u64::MAX]));
    let run = |stamp| {
        let mut home = Home::<Message>::new("test").unwrap();
        let mut world = MemoryMessageWorld::default();
        world.binding.put(&Ref::new(inbox().to_string()), InboxBinding::Unbound { since: Timestamp(0) });
        let mut value = spec();
        value.admitted_seq = stamp;
        let reads = home.create("m", value, &world, Timestamp(10)).unwrap().reads;
        let pending = home.judge("m", &world, Timestamp(11), |h| pending_for(h, &consumer("a"))).unwrap();
        let dropped = home.apply("m", DropMessage { reason: DropReason::Undeliverable }, &world, Timestamp(12)).map(|_| ());
        (reads, pending.value, pending.reads, dropped)
    };
    assert_eq!(run(stamped), run(0));
}

// Behaviour: eviction forgets a cache value, not its durable identity. Both
// inbox logs reject re-creating that name, leave Home empty, and preserve the
// original resource and sequence. This also exercises the new owned hook.
#[test]
fn evicted_names_remain_unique_in_the_source() {
    fn check<L: flotilla_inbox::InboxLog>(log: L) {
        let mut home = Home::<Message, L>::with_log("test", log).unwrap();
        let world = MemoryMessageWorld::default();
        let name = "project:inbox/role:duplicate";
        home.create(name, spec(), &world, Timestamp(10)).unwrap();
        let original = home.evict(name).unwrap();
        let sequence = home.log_mut().sequence().unwrap();
        let result = home.create(name, spec(), &world, Timestamp(11));
        assert!(
            matches!(result, Err(flotilla_state_machines::WriteError::Log(_))),
            "source must reject duplicate after eviction: {result:?}"
        );
        assert!(home.get(name).is_none());
        assert_eq!(home.log_mut().sequence().unwrap(), sequence);
        let stored = home.log_mut().load(name).unwrap().unwrap();
        assert_eq!(stored.spec(), original.spec());
        assert_eq!(stored.version(), original.version());
    }
    check(flotilla_inbox::MemoryInboxLog::default());
    let store = Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().unwrap(), "test", flotilla_state_machines::kinds().unwrap()).unwrap(),
    ));
    check(MessageLog::new(store));
}
