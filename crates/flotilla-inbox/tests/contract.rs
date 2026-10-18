use std::sync::{Arc, Mutex};

use flotilla_inbox::{
    sqlite::{SqliteInbox, StoreLog},
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
        let backend = EngineInbox::open("test", StoreLog::new(store.clone()), evidence.clone(), Arc::new(Supervisors)).expect("open inbox");
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
// a receipt remains deliverable after reopening, and cursors never repeat.
#[tokio::test]
async fn sqlite_recovers_receipts_and_watch_positions() {
    use flotilla_inbox::{Cursor, InboxBackend};
    use flotilla_inbox_testkit::{inbox, spec};
    use flotilla_state_machines::{machines::message::Deliver, Transport};
    use futures::StreamExt;
    let store = Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().expect("SQLite"), "test", flotilla_state_machines::kinds().expect("registry"))
            .expect("store"),
    ));
    let evidence = Arc::new(Evidence::default());
    let backend = SqliteInbox::open("test", StoreLog::new(store.clone()), evidence.clone(), Arc::new(Supervisors)).expect("open inbox");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store.clone())));
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    let created = watch.next().await.expect("create").expect("event");
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
    drop(watch);
    drop(backend);
    let backend = SqliteInbox::open("test", StoreLog::new(store), evidence, Arc::new(Supervisors)).expect("recover");
    let mut resumed = backend.watch(&inbox(), Some(created.cursor)).await.expect("resume");
    let receipt = resumed.next().await.expect("receipt").expect("event");
    assert!(receipt.cursor > created.cursor);
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("recovered receipt");
    assert!(resumed.next().await.expect("delivery").expect("event").cursor > receipt.cursor);
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

// Behaviour: a lagging watcher gets an explicit terminal cursor-expired error,
// and can establish a fresh live tail before obtaining a pending baseline.
#[tokio::test]
async fn watch_expiry_and_live_restart() {
    use flotilla_inbox::{Cursor, Error, InboxBackend, WATCH_HISTORY};
    use flotilla_inbox_testkit::{inbox, spec};
    use futures::StreamExt;
    let harness = Memory.create();
    let mut lagging = harness.backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    for _ in 0..=WATCH_HISTORY {
        harness.backend.enqueue(spec()).await.expect("enqueue");
    }
    assert!(matches!(lagging.next().await, Some(Err(Error::CursorExpired))));
    assert!(lagging.next().await.is_none(), "expired stream terminates");
    assert!(matches!(harness.backend.watch(&inbox(), Some(Cursor(0))).await, Err(Error::CursorExpired)));
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
    let backend =
        SqliteInbox::open("test", StoreLog::new(local.clone()), Arc::new(Evidence::default()), Arc::new(Supervisors)).expect("open inbox");
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

// Behaviour: reopening preserves each historical state at its own watch cursor,
// rather than replaying the final state or interpreting Message edges again.
#[tokio::test]
async fn sqlite_recovers_historical_message_states() {
    use flotilla_inbox::{ChangeKind, Cursor, InboxBackend};
    use flotilla_inbox_testkit::{inbox, spec};
    use flotilla_state_machines::{
        machines::message::{Deliver, Expectation},
        Transport,
    };
    use futures::StreamExt;
    let store = Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().expect("SQLite"), "test", flotilla_state_machines::kinds().expect("registry"))
            .expect("store"),
    ));
    let evidence = Arc::new(Evidence::default());
    let backend = SqliteInbox::open("test", StoreLog::new(store.clone()), evidence.clone(), Arc::new(Supervisors)).expect("open");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store.clone())));
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut envelope = spec();
    envelope.expectation = Expectation::Reply;
    let message = backend.enqueue(envelope).await.expect("enqueue");
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receipt");
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
    let mut live = backend.watch(&inbox(), Some(Cursor(0))).await.expect("history");
    let mut expected = Vec::new();
    for _ in 0..3 {
        expected.push(live.next().await.expect("event").expect("change"));
    }
    assert!(matches!(expected[0].kind, ChangeKind::Message(flotilla_state_machines::machines::message::MessageState::Pending)));
    assert!(matches!(expected[2].kind, ChangeKind::Message(flotilla_state_machines::machines::message::MessageState::Awaiting { .. })));
    drop(live);
    drop(backend);
    let reopened = SqliteInbox::open("test", StoreLog::new(store), evidence, Arc::new(Supervisors)).expect("recover");
    let mut recovered = reopened.watch(&inbox(), Some(Cursor(0))).await.expect("history");
    for before in expected {
        let after = recovered.next().await.expect("event").expect("change");
        assert_eq!(after, before);
    }
}
