//! The storage port (#3317): the engine's Log over one Store, and receipts
//! served from `Read::about`.

use std::sync::{Arc, Mutex};

use flotilla_engine_store::{registry, MessageLog, StoreLog};
use flotilla_schema::{Address, Kind, Timestamp};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::{
        agent_session::{AgentSession, MemoryAgentSessionWorld, Receive},
        consumer::{ConsumerKind, Receiver},
        message::{
            Deliver, DropMessage, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageBody, MessageSpec,
            MessageState, Receipts, Relation, Settle, Settlement,
        },
    },
    Home, Ref, Resource, Source, Transport,
};
use rusqlite::Connection;

const HOME: &str = "alpha";

fn store() -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(Store::open(Connection::open_in_memory().expect("SQLite"), HOME, registry().expect("registry")).expect("store")))
}

fn inbox() -> Address {
    "project:fleet/convoy:demo/role:coder".parse().expect("inbox")
}

fn spec(expectation: Expectation) -> MessageSpec {
    MessageSpec {
        expectation,
        unbound_after: 10,
        ..MessageSpec::new(
            "project:fleet/role:turn-rules".parse().expect("sender"),
            inbox(),
            Relation::System,
            MessageBody::text("take a turn").expect("small body"),
        )
    }
}

fn name(id: &str) -> String {
    Address::message(&inbox(), id).expect("message address").as_str().to_owned()
}

fn session() -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: "terminal-demo-coder".into(), host: HOME.into() }
}

fn last_sequence(store: &Arc<Mutex<Store>>) -> u64 {
    store.lock().expect("store").read().expect("read").last_sequence(HOME).expect("sequence")
}

fn stored(store: &Arc<Mutex<Store>>, name: &str) -> Resource<Message> {
    let mut store = store.lock().expect("store");
    let mut read = store.read().expect("read");
    let id = read.resolve(Resource::<Message>::CODE, name, HOME).expect("resolve").expect("stored");
    read.load::<Resource<Message>>(id).expect("load").expect("body").value
}

// Behaviour: create, Deliver, Settle and Drop each commit one write whose
// version is the home's sequence; admission is the create's sequence; the
// stored resource reloads equal to the engine's. Deliver's gate reads the
// receipt a real AgentSession's Receive wrote, through Read::about.
#[test]
fn port_round_trips_every_message_edge_at_the_home_sequence() {
    let store = store();
    let log = MessageLog::new(store.clone());
    let mut messages = Home::<Message, MessageLog>::with_log(HOME, log.clone()).expect("message home");
    let mut sessions = Home::<AgentSession, StoreLog>::with_log(HOME, log.consumers()).expect("session home");
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(inbox().as_str()), InboxBinding::Bound { consumers: [session()].into() });

    let (answered, dropped) = (name("answered"), name("dropped"));
    for (message, expectation) in [(&answered, Expectation::Reply), (&dropped, Expectation::Nothing)] {
        messages.create(message, spec(expectation), &world, Timestamp(1)).expect("create");
        let resource = messages.get(message).expect("created");
        assert_eq!(resource.version().0, last_sequence(&store), "create version is the home sequence");
        assert_eq!(resource.spec().admitted_seq, resource.version().0, "admission is the create's sequence");
        assert_eq!(&stored(&store, message), resource);
    }

    sessions.create(&session().id, (), &MemoryAgentSessionWorld::default(), Timestamp(2)).expect("session");
    sessions
        .apply(
            &session().id,
            Receive { message: answered.clone(), transport: Transport::Turn },
            &MemoryAgentSessionWorld::default(),
            Timestamp(3),
        )
        .expect("receive");
    let receipts = {
        let mut locked = store.lock().expect("store");
        let mut read = locked.read().expect("read");
        log.receipt_log(&mut read, [answered.as_str(), dropped.as_str()]).expect("receipts from about")
    };
    assert_eq!(
        receipts.lookup(&Ref::<Receipts>::new(answered.as_str())).map(|(_, rows)| rows.clone()),
        Some([(session(), Transport::Turn)].into())
    );
    assert!(receipts.lookup(&Ref::<Receipts>::new(dropped.as_str())).is_none(), "no receipt about the other message");
    world.receipts = Default::default();
    world.receipts.put(&Ref::new(answered.as_str()), receipts.lookup(&Ref::new(answered.as_str())).expect("receipt").1.clone());

    messages.apply(&answered, Deliver { receiver: session(), transport: Transport::Turn }, &world, Timestamp(4)).expect("deliver");
    assert_eq!(messages.get(&answered).expect("message").version().0, last_sequence(&store));
    assert_eq!(*messages.get(&answered).expect("message").state(), MessageState::Awaiting { receiver: session() });

    world.reply.put(&Ref::new(answered.as_str()), ());
    messages.apply(&answered, Settle { by: Settlement::Answered }, &world, Timestamp(5)).expect("settle");
    assert_eq!(messages.get(&answered).expect("message").version().0, last_sequence(&store));
    assert_eq!(&stored(&store, &answered), messages.get(&answered).expect("message"));

    world.binding.put(&Ref::new(inbox().as_str()), InboxBinding::Unbound { since: Timestamp(0) });
    messages.apply(&dropped, DropMessage { reason: DropReason::Undeliverable }, &world, Timestamp(50)).expect("drop");
    let resource = messages.get(&dropped).expect("message");
    assert_eq!(resource.version().0, last_sequence(&store));
    assert!(matches!(resource.state(), MessageState::Dropped { reason: DropReason::Undeliverable, .. }));
    assert_eq!(&stored(&store, &dropped), resource);
}

// The documented one-roll additive upgrade preserves a six-column session row;
// the new observation stays absent until its first hook edge is written.
#[test]
fn legacy_session_rows_decode_after_the_documented_activity_upgrade() {
    use flotilla_schema::{Origin, State};
    use flotilla_state_machines::{
        machines::agent_session::{Activity, AgentSessionSpec, AgentSessionState, Channel, Observe},
        Version,
    };
    let mut store = Store::open(Connection::open_in_memory().unwrap(), HOME, registry().unwrap()).unwrap();
    let session = Resource::<AgentSession>::stored(
        "legacy",
        AgentSessionSpec {},
        State::new(AgentSessionState::live(Channel::Detached), Timestamp(0)),
        Version(0),
    );
    let mut write = store.write().unwrap();
    let stamp = write.insert("legacy", &session, &Origin::new("test", Timestamp(0))).unwrap();
    write.commit().unwrap();
    let connection = store.into_connection();
    // Reconstruct the previous generation's six-column root table around its row.
    connection
        .execute_batch(
            "DROP VIEW agent_session_view;
         ALTER TABLE agent_session DROP COLUMN activity;
         ALTER TABLE agent_session DROP COLUMN activity_since;
         ALTER TABLE agent_session DROP COLUMN activity_cause;",
        )
        .unwrap();
    assert_eq!(connection.query_row("SELECT count(*) FROM pragma_table_info('agent_session')", [], |row| row.get::<_, u32>(0)).unwrap(), 6);
    // This is the exact explicit upgrade from the PR rollout instructions.
    connection
        .execute_batch(
            "ALTER TABLE agent_session ADD COLUMN activity TEXT CHECK (activity IN ('id', 'in', 'wo'));
         ALTER TABLE agent_session ADD COLUMN activity_since INTEGER;
         ALTER TABLE agent_session ADD COLUMN activity_cause TEXT;",
        )
        .unwrap();
    let mut store = Store::open(connection, HOME, registry().unwrap()).unwrap();
    let loaded = store.read().unwrap().load::<Resource<AgentSession>>(stamp.id).unwrap().unwrap();
    assert!(loaded.value.state().observation().is_none());
    let store = Arc::new(Mutex::new(store));
    let mut home = Home::<AgentSession, StoreLog>::with_log(HOME, StoreLog::new(Arc::clone(&store))).unwrap();
    home.restore(loaded.value).unwrap();
    home.apply(
        "legacy",
        Observe { activity: Activity::Working, cause: "UserPromptSubmit".into() },
        &MemoryAgentSessionWorld::default(),
        Timestamp(2),
    )
    .unwrap();
    let mut store = store.lock().unwrap();
    let current = store.read().unwrap().load::<Resource<AgentSession>>(stamp.id).unwrap().unwrap();
    let observation = current.value.state().observation().unwrap();
    assert_eq!(observation.activity, Activity::Working);
    assert_eq!(observation.activity_since, Timestamp(2));
    assert_eq!(observation.activity_cause, "UserPromptSubmit");
}

// Behaviour (#3755): a panic in one writer rolls back its rows and cached IDs;
// another kind's home can then create and apply edges, and homes can reopen.
// This deterministic scenario uses real in-memory SQLite and unwinds while the
// shared mutex and a populated write transaction are both held.
#[test]
fn poisoned_writer_does_not_take_down_other_homes() {
    let store = store();
    let log = MessageLog::new(store.clone());
    let mut messages = Home::<Message, MessageLog>::with_log(HOME, log.clone()).unwrap();
    let mut sessions = Home::<AgentSession, StoreLog>::with_log(HOME, log.consumers()).unwrap();
    sessions.create("seed", (), &MemoryAgentSessionWorld::default(), Timestamp(1)).unwrap();
    let seed = sessions.get("seed").unwrap().clone();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut locked = store.lock().unwrap();
        let mut write = locked.write().unwrap();
        write.insert("rolled-back", &seed, &flotilla_schema::Origin::new("panicking-writer", Timestamp(2))).unwrap();
        panic!("writer panicked before commit");
    }));
    assert!(panic.is_err());
    assert!(store.is_poisoned());

    // The already-open Message home must survive another kind's writer panic.
    let message = name("after-panic");
    let mut world = MemoryMessageWorld::default();
    messages.create(&message, spec(Expectation::Nothing), &world, Timestamp(3)).unwrap();
    assert_eq!(messages.get(&message).unwrap().version().0, 2, "rolled-back sequence is reusable");
    world.binding.put(&Ref::new(inbox().as_str()), InboxBinding::Unbound { since: Timestamp(0) });
    messages.apply(&message, DropMessage { reason: DropReason::Undeliverable }, &world, Timestamp(50)).unwrap();

    // Reusing the rolled-back identity exercises the string and identity caches.
    sessions.create("rolled-back", (), &MemoryAgentSessionWorld::default(), Timestamp(4)).unwrap();
    sessions
        .apply(
            "rolled-back",
            Receive { message: message.clone(), transport: Transport::Turn },
            &MemoryAgentSessionWorld::default(),
            Timestamp(5),
        )
        .unwrap();
    let reopened = flotilla_engine_store::open_home::<AgentSession>(StoreLog::new(store.clone())).unwrap();
    assert_eq!(reopened.get("rolled-back"), sessions.get("rolled-back"));
    let mut locked = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut read = locked.read().unwrap();
    assert_eq!(read.last_sequence(HOME).unwrap(), 5);
    let receipts = log.receipt_log(&mut read, [message.as_str()]).unwrap();
    assert!(receipts.lookup(&Ref::<Receipts>::new(message.as_str())).is_some());
}
