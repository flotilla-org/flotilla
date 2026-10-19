use std::sync::{Arc, Mutex};

use flotilla_inbox::{
    sqlite::{MessageLog, SqliteInbox, StoreLog},
    EngineInbox, MemoryInbox, MemoryInboxLog,
};
use flotilla_inbox_testkit::{consumer, Evidence, Factory, Harness, SessionPort, Supervisors};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{machines::agent_session::AgentSession, MemoryLog};
use rusqlite::Connection;

#[derive(Default)]
struct Memory;
impl Factory for Memory {
    type Backend = MemoryInbox;
    fn inspect(
        &self,
        backend: &Self::Backend,
        message: &str,
    ) -> flotilla_state_machines::Resource<flotilla_state_machines::machines::message::Message> {
        backend.get_blocking(message).expect("message")
    }
    fn create(&self) -> Harness<Self::Backend> {
        let evidence = Arc::new(Evidence::default());
        let backend = EngineInbox::open("test", MemoryInboxLog::default(), evidence.clone(), Arc::new(Supervisors)).expect("open inbox");
        for name in ["a", "b"] {
            backend.register_blocking(consumer(name), SessionPort::new(name, MemoryLog::<AgentSession>::new()));
        }
        Harness { backend, evidence }
    }
}
#[derive(Default)]
struct Sqlite;
impl Factory for Sqlite {
    type Backend = SqliteInbox;
    fn inspect(
        &self,
        backend: &Self::Backend,
        message: &str,
    ) -> flotilla_state_machines::Resource<flotilla_state_machines::machines::message::Message> {
        backend.get_blocking(message).expect("message")
    }
    fn create(&self) -> Harness<Self::Backend> {
        let store = Arc::new(Mutex::new(
            Store::open(Connection::open_in_memory().expect("SQLite"), "test", flotilla_state_machines::kinds().expect("registry"))
                .expect("store"),
        ));
        let evidence = Arc::new(Evidence::default());
        let backend =
            EngineInbox::open("test", MessageLog::new(store.clone()), evidence.clone(), Arc::new(Supervisors)).expect("open inbox");
        for name in ["a", "b"] {
            backend.register_blocking(consumer(name), SessionPort::new(name, StoreLog::new(store.clone())));
        }
        Harness { backend, evidence }
    }
}
// Behaviour: both storage implementations satisfy the exact same observable contract.
#[tokio::test]
async fn memory_contract() {
    flotilla_inbox_testkit::suite(&Memory).await;
}
#[tokio::test]
async fn sqlite_contract() {
    flotilla_inbox_testkit::suite(&Sqlite).await;
}

// Behaviour: SQLite recovery reads the actual Receive entries about a message;
// a receipt remains deliverable after reopening, old cursors resume from the
// durable log, and a fresh live tail observes only post-restart changes.
#[tokio::test]
async fn sqlite_recovers_receipts_and_resumes_log_cursors() {
    use flotilla_inbox::{Cursor, InboxBackend};
    use flotilla_inbox_testkit::{inbox, spec};
    use flotilla_state_machines::{machines::message::Deliver, Transport};
    use futures::StreamExt;
    let store = Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().expect("SQLite"), "test", flotilla_state_machines::kinds().expect("registry"))
            .expect("store"),
    ));
    let evidence = Arc::new(Evidence::default());
    let backend = SqliteInbox::open("test", MessageLog::new(store.clone()), evidence.clone(), Arc::new(Supervisors)).expect("open inbox");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store.clone())));
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    let created = watch.next().await.expect("create").expect("event");
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
    let receipt = watch.next().await.expect("receipt").expect("event");
    let mut lagged = Vec::new();
    for _ in 0..3 {
        lagged.push(backend.enqueue(spec()).await.expect("lagged create"));
    }
    drop(watch);
    drop(backend);
    let backend = SqliteInbox::open("test", MessageLog::new(store), evidence, Arc::new(Supervisors)).expect("recover");
    let mut replay = backend.watch(&inbox(), Some(created.cursor)).await.expect("durable cursor survives restart");
    assert_eq!(replay.next().await.expect("receipt").expect("event"), receipt);
    for message in lagged {
        assert_eq!(replay.next().await.expect("lagged create").expect("event").message, message);
    }
    let mut live = backend.watch(&inbox(), None).await.expect("fresh live tail");
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("recovered receipt");
    let delivered = live.next().await.expect("delivery").expect("event");
    assert!(delivered.cursor > receipt.cursor);
    let mut resumed = backend.watch(&inbox(), Some(delivered.cursor)).await.expect("post-restart cursor remains valid");
    let next = backend.enqueue(spec()).await.expect("enqueue after cursor");
    assert_eq!(resumed.next().await.expect("new event").expect("change").message, next);
}

// Behaviour: the shared first-claim invariant holds after every step of a
// generated sequence, for both storage implementations.
#[hegel::test]
fn generated_interleavings(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Covers empty sequences, duplicate receipts, losing claims, and unbinding
    // either consumer before or after a receipt, up to 24 operations.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(24));
    let operations: Vec<_> = (0..count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(5))).collect();
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        flotilla_inbox_testkit::generated_claim(&Memory, &operations).await;
        flotilla_inbox_testkit::generated_claim(&Sqlite, &operations).await;
    });
}

// Behaviour: the Message race traces replay through both implementations.
#[test]
#[ignore = "generate traces with specs/primitives/check.sh"]
fn message_race_traces() {
    flotilla_inbox_testkit::quint::replay_message_race::<Memory>();
    flotilla_inbox_testkit::quint::replay_message_race::<Sqlite>();
}

// Behaviour: every specified selector must match, first match wins, and no
// matching rule defaults to Turn. The generator crosses every selector match
// combination and each transport mode.
#[hegel::test]
fn rule_selectors_and_precedence(tc: hegel::TestCase) {
    use flotilla_inbox::{Rule, Rules};
    use flotilla_inbox_testkit::{address, spec};
    use flotilla_state_machines::{
        machines::message::{Expectation, Relation},
        Transport,
    };
    use hegel::generators as gs;
    let sender = tc.draw(gs::booleans());
    let relation = tc.draw(gs::booleans());
    let expectation = tc.draw(gs::booleans());
    let mode = [Transport::Turn, Transport::Notify, Transport::Mute][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let rule = Rule::builder()
        .sender(if sender { spec().sender } else { address("project:another") })
        .relation(if relation { Relation::Peer } else { Relation::System })
        .expectation(if expectation { Expectation::Nothing } else { Expectation::Reply })
        .mode(mode)
        .build();
    assert_eq!(Rules(vec![rule.clone()]).choose(&spec()), if sender && relation && expectation { mode } else { Transport::Turn });
    let fallback = Rule::builder().mode(Transport::Mute).build();
    assert_eq!(Rules(vec![fallback, rule]).choose(&spec()), Transport::Mute);
}

// Behaviour: a lagging reader resumes from authoritative memory history; a fresh live tail starts at the current log.
#[tokio::test]
async fn log_watch_survives_lag() {
    use flotilla_inbox::{Cursor, Error, InboxBackend};
    use flotilla_inbox_testkit::{inbox, spec};
    use futures::StreamExt;
    let harness = Memory.create();
    let mut lagging = harness.backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    let first = harness.backend.enqueue(spec()).await.expect("first");
    for _ in 0..8 {
        harness.backend.enqueue(spec()).await.expect("enqueue");
    }
    assert_eq!(lagging.next().await.expect("retained event").expect("change").message, first);
    let mut replay = harness.backend.watch(&inbox(), Some(Cursor(0))).await.expect("old cursor retained");
    assert_eq!(replay.next().await.expect("first").expect("change").message, first);
    let mut live = harness.backend.watch(&inbox(), None).await.expect("fresh live tail");
    let next = harness.backend.enqueue(spec()).await.expect("new message");
    assert_eq!(live.next().await.expect("new event").expect("change").message, next);
    assert!(matches!(harness.backend.watch(&inbox(), Some(Cursor(u64::MAX))).await, Err(Error::CursorAhead)));
}

// Behaviour: SQLite refuses a port on another store before it commits Receive,
// preserving the original creation event and its exclusive watch cursor.
#[tokio::test]
async fn sqlite_rejects_consumer_in_another_store() {
    use flotilla_inbox::{Error, InboxBackend};
    use flotilla_inbox_testkit::spec;
    let make_store = || {
        Arc::new(Mutex::new(
            Store::open(Connection::open_in_memory().expect("SQLite"), "test", flotilla_state_machines::kinds().expect("registry"))
                .expect("store"),
        ))
    };
    let local = make_store();
    let foreign = make_store();
    let backend = SqliteInbox::open("test", MessageLog::new(local.clone()), Arc::new(Evidence::default()), Arc::new(Supervisors))
        .expect("open inbox");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(foreign.clone())));
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let before = foreign.lock().expect("store").read().expect("read").last_sequence("test").expect("sequence");
    assert!(matches!(backend.receive(&consumer("a"), &message, flotilla_state_machines::Transport::Turn).await, Err(Error::Consumer(_))));
    assert_eq!(
        foreign.lock().expect("store").read().expect("read").last_sequence("test").expect("sequence"),
        before,
        "refusal precedes the consumer write"
    );
    use futures::StreamExt;
    let mut watch = backend.watch(&flotilla_inbox_testkit::inbox(), Some(flotilla_inbox::Cursor(0))).await.expect("watch");
    let created = watch.next().await.expect("create").expect("change");
    backend.register_async(consumer("a"), SessionPort::new("a", StoreLog::new(local))).await.expect("runtime registration");
    assert!(backend.get_async(&message).await.expect("async snapshot").is_some());
    backend.receive(&consumer("a"), &message, flotilla_state_machines::Transport::Turn).await.expect("same store receipt");
    let receipt = watch.next().await.expect("receipt").expect("change");
    assert!(receipt.cursor > created.cursor, "a receipt never overwrites its creation event");
}
