use std::sync::{Arc, Mutex};

use flotilla_inbox::{
    sqlite::{SqliteInbox, StoreLog},
    Cursor, EngineInbox, Error, InboxBackend, InboxLog, MemoryInbox, MemoryInboxLog, Rules, RESIDENT_BUDGET,
};
use flotilla_inbox_testkit::{address, consumer, inbox, spec, Evidence, SessionPort, Supervisors};
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::{agent_session::AgentSession, message::*},
    MemoryLog, Ref, Transport,
};
use futures::{StreamExt, TryStreamExt};
use hegel::{generators as gs, TestCase};
use rusqlite::Connection;

fn database() -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(
        Store::open(Connection::open_in_memory().expect("db"), "test", flotilla_state_machines::kinds().expect("registry")).expect("store"),
    ))
}

fn backend(store: Arc<Mutex<Store>>, evidence: Arc<Evidence>, budget: usize) -> (SqliteInbox, StoreLog) {
    let log = StoreLog::new(store.clone());
    let backend = SqliteInbox::open_with_budget("test", log.clone(), evidence, Arc::new(Supervisors), budget).expect("open");
    backend.register_blocking(consumer("a"), SessionPort::new("a", StoreLog::new(store)));
    (backend, log)
}

// Behaviour: eviction changes query routing, never pending completeness/order or gate results.
// Body sizes cross the cache budget, and counts include empty and multi-page pending sets.
#[hegel::test(test_cases = 16)]
fn eviction_preserves_pending_and_receipts(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().max_value(65));
    let budget = tc.draw(gs::sampled_from(vec![2048usize, 8192, 32768]));
    let body_len = tc.draw(gs::sampled_from(vec![0usize, 2048, 8192]));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let evidence = Arc::new(Evidence::default());
        let (backend, log) = backend(database(), evidence.clone(), budget);
        exercise_eviction(&backend, &evidence, count, budget, body_len).await;
        assert_eq!(log.read_counts().terminal_loads, 0, "pending and delivery never load terminal rows");
        let memory_evidence = Arc::new(Evidence::default());
        let memory =
            MemoryInbox::open_with_budget("test", MemoryInboxLog::default(), memory_evidence.clone(), Arc::new(Supervisors), budget)
                .expect("memory");
        memory.register_blocking(consumer("a"), SessionPort::new("a", MemoryLog::<AgentSession>::new()));
        exercise_eviction(&memory, &memory_evidence, count, budget, body_len).await;
    });
}

async fn exercise_eviction<L: InboxLog + 'static>(
    backend: &EngineInbox<L>,
    evidence: &Evidence,
    count: usize,
    budget: usize,
    body_len: usize,
) {
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut expected = Vec::new();
    for n in 0..count {
        evidence.clock.store(10 + n as i64, std::sync::atomic::Ordering::SeqCst);
        let mut value = spec();
        value.body = MessageBody::text("x".repeat(body_len)).expect("body");
        expected.push(backend.enqueue(value).await.expect("enqueue"));
        let residency = backend.residency_blocking();
        assert!(residency.accounted_bytes <= budget, "{residency:?}");
        if residency.open_messages > 0 {
            assert!(residency.oldest_state_since <= residency.newest_state_since);
        }
    }
    let pending =
        backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending").try_collect::<Vec<_>>().await.expect("stream");
    assert_eq!(pending.iter().map(|p| p.message.clone()).collect::<Vec<_>>(), expected);
    for message in &expected {
        backend.receive(&consumer("a"), message, Transport::Turn).await.expect("receive");
        backend.deliver(message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
        assert!(backend.residency_blocking().accounted_bytes <= budget);
    }
    assert_eq!(backend.residency_blocking().open_messages, 0);
    assert!(backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending").next().await.is_none());
}

// Behaviour: the amount of Settled/Dropped history does not add reads to steady-state delivery,
// even after eviction forces Pending-index and keyed open-state reads. Warm delivery has none.
#[tokio::test]
async fn delivery_reads_no_terminal_history() {
    for budget in [2048, RESIDENT_BUDGET] {
        let evidence = Arc::new(Evidence::default());
        let (backend, log) = backend(database(), evidence.clone(), budget);
        backend.bind(inbox(), consumer("a")).await.expect("bind");
        evidence.world.lock().expect("world").authority.put(&Ref::new(address("project:test/role:owner").to_string()), true);
        for n in 0..96 {
            let message = backend.enqueue(spec()).await.expect("history message");
            if n % 2 == 0 {
                backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
                backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("settled");
            } else {
                backend
                    .drop(
                        &message,
                        DropReason::Withdrawn { by: address("project:test/role:sender"), authority: address("project:test/role:owner") },
                    )
                    .await
                    .expect("dropped");
            }
        }
        assert_eq!(backend.residency_blocking().open_messages, 0);
        log.reset_read_counts();
        for _ in 0..24 {
            let message = backend.enqueue(spec()).await.expect("enqueue");
            let pending = backend
                .pending(&inbox(), &consumer("a"), &Rules::default())
                .await
                .expect("pending")
                .try_collect::<Vec<_>>()
                .await
                .expect("stream");
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].message, message);
            backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
            backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
        }
        let reads = log.read_counts();
        assert_eq!(reads.terminal_loads, 0, "{reads:?}");
        assert_eq!(reads.resource_history_pages, 0, "{reads:?}");
        assert_eq!(reads.scan_pages, 0, "{reads:?}");
        assert_eq!(reads.watch_pages, 0, "{reads:?}");
        if budget == RESIDENT_BUDGET {
            assert_eq!(reads.resource_loads, 0, "warm delivery reads no stored resources");
            assert_eq!(reads.about_pages, 0, "warm delivery reads no receipt history");
        } else {
            assert!(reads.pending_pages > 0 && reads.about_pages > 0, "exercise the cold route");
        }
    }
}

// Behaviour: bounded restart recovers open receipts and finishes ordering from the declared
// Pending index. Terminal resources are available by key without becoming resident.
#[tokio::test]
async fn restart_rebuilds_bounded_open_state_and_cold_terminal_reads() {
    let store = database();
    let evidence = Arc::new(Evidence::default());
    let (first, _) = backend(store.clone(), evidence.clone(), 2048);
    first.bind(inbox(), consumer("a")).await.expect("bind");
    let mut messages = Vec::new();
    for _ in 0..70 {
        messages.push(first.enqueue(spec()).await.expect("enqueue"));
    }
    first.receive(&consumer("a"), &messages[0], Transport::Turn).await.expect("receive");
    first.deliver(&messages[0], Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
    first.receive(&consumer("a"), &messages[1], Transport::Turn).await.expect("receipt survives restart");
    drop(first);
    let log = StoreLog::new(store.clone());
    let reopened = SqliteInbox::open_with_budget("test", log.clone(), evidence, Arc::new(Supervisors), 2048).expect("restart");
    assert!(log.read_counts().scan_pages >= 3);
    assert!(reopened.residency_blocking().accounted_bytes <= 2048);
    reopened.deliver(&messages[1], Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("recovered receipt");
    let terminal = reopened.get_async(&messages[0]).await.expect("keyed read").expect("stored terminal");
    assert!(matches!(terminal.state(), MessageState::Settled { .. }));
    assert_eq!(reopened.residency_blocking().open_messages, 0, "oversized open records stay cold");
    assert!(matches!(
        reopened.deliver(&messages[0], Deliver { receiver: consumer("a"), transport: Transport::Turn }).await,
        Err(Error::Message(_))
    ));
    reopened.bind(inbox(), consumer("a")).await.expect("rebind after receipt drain");
    let pending = reopened
        .pending(&inbox(), &consumer("a"), &Rules::default())
        .await
        .expect("pending")
        .try_collect::<Vec<_>>()
        .await
        .expect("stream");
    assert_eq!(pending.into_iter().map(|p| p.message).collect::<Vec<_>>(), messages[2..]);
}

// Behaviour: the row's admission is the actual create home sequence, not zero or a caller
// supplied value. Consumer log writes create gaps between consecutive message admissions.
#[tokio::test]
async fn admission_is_stamped_before_create_and_never_changes() {
    let evidence = Arc::new(Evidence::default());
    let (backend, _) = backend(database(), evidence, RESIDENT_BUDGET);
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut value = spec();
    value.admitted_seq = 999;
    let message = backend.enqueue(value).await.expect("enqueue");
    let admitted = backend.get_async(&message).await.expect("get").expect("present");
    assert_ne!(admitted.spec().admitted_seq, 0);
    assert_ne!(admitted.spec().admitted_seq, 999);
    assert_eq!(admitted.spec().admitted_seq, admitted.version().0);
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
    let terminal = backend.get_async(&message).await.expect("get").expect("present");
    assert_eq!(terminal.spec().admitted_seq, admitted.spec().admitted_seq);
    assert!(terminal.version() > admitted.version());
    let next = backend.enqueue(spec()).await.expect("next");
    assert!(backend.get_async(&next).await.expect("get").expect("present").spec().admitted_seq > admitted.spec().admitted_seq + 1);
}

// Behaviour: watches decode committed state effects from the log, including the full terminal
// state, rather than loading the resource's latest state for an older cursor.
#[tokio::test]
async fn watch_replays_historical_states_after_eviction() {
    let evidence = Arc::new(Evidence::default());
    let (backend, _) = backend(database(), evidence.clone(), 2048);
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut value = spec();
    value.expectation = Expectation::Reply;
    let message = backend.enqueue(value).await.expect("enqueue");
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receipt");
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("awaiting");
    evidence.world.lock().expect("world").reply.put(&Ref::new(&message), ());
    backend.settle(&message, Settle { by: Settlement::Answered }).await.expect("settle");
    let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    let mut states = Vec::new();
    while states.len() < 3 {
        if let flotilla_inbox::ChangeKind::Message(state) = watch.next().await.expect("event").expect("change").kind {
            states.push(state);
        }
    }
    assert_eq!(
        states,
        vec![
            MessageState::Pending,
            MessageState::Awaiting { receiver: consumer("a") },
            MessageState::Settled { receiver: Some(consumer("a")) }
        ]
    );
}

// Behaviour: only a real gap in the authoritative log expires a cursor. The
// backend itself never trims history; this exercises the store's existing expunge.
#[tokio::test]
async fn watch_reports_expired_when_source_history_was_expunged() {
    use flotilla_schema::{Kind, Origin, Timestamp};
    use flotilla_state_machines::Resource;
    let store = database();
    let (backend, _) = backend(store.clone(), Arc::new(Evidence::default()), RESIDENT_BUDGET);
    let message = backend.enqueue(spec()).await.expect("enqueue");
    {
        let mut store = store.lock().expect("store");
        let mut write = store.write().expect("write");
        let id = write.resolve(Resource::<Message>::CODE, &message, "test").expect("resolve").expect("present");
        let current = write.load::<Resource<Message>>(id).expect("load").expect("present");
        let origin = Origin::new("test", Timestamp(100));
        write.delete::<Resource<Message>>(current.stamp(), &origin).expect("delete");
        write.expunge::<Resource<Message>>(id, &origin).expect("expunge");
        write.commit().expect("commit");
    }
    assert!(matches!(backend.watch(&inbox(), Some(Cursor(0))).await, Err(Error::CursorExpired)));
}

// Behaviour: a restart that repairs a committed Undeliverable drop may encounter
// its new dead letter in a later scan page; restore must not duplicate admission.
#[test]
fn paged_restart_finishes_interrupted_dead_letter_once() {
    use flotilla_inbox::InboxLog;
    use flotilla_state_machines::Home;
    let store = database();
    let mut home = Home::<Message, StoreLog>::with_log("test", StoreLog::new(store.clone())).expect("home");
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(inbox().to_string()), InboxBinding::Unbound { since: flotilla_schema::Timestamp(0) });
    for n in 0..40 {
        let mut value = spec();
        value.admitted_seq = home.log_mut().sequence().expect("sequence") + 1;
        let name = format!("project:inbox/role:message-{n}");
        home.create(&name, value, &world, flotilla_schema::Timestamp(0)).expect("create");
        if n == 0 {
            home.apply(&name, DropMessage { reason: DropReason::Undeliverable }, &world, flotilla_schema::Timestamp(1))
                .expect("committed drop without reaction");
        }
    }
    drop(home);
    for _ in 0..2 {
        let reopened = SqliteInbox::open("test", StoreLog::new(store.clone()), Arc::new(Evidence::default()), Arc::new(Supervisors))
            .expect("restart repairs reaction");
        assert!(matches!(
            reopened.get_blocking("project:inbox/role:dead-letter-message-0").expect("one dead letter").state(),
            MessageState::Pending
        ));
        assert_eq!(reopened.residency_blocking().open_messages, 40);
    }
}

// Behaviour: byte-bounded watch batches make progress for a first oversized
// change and never skip its cursor or the following smaller changes.
#[tokio::test]
async fn watch_pages_preserve_oversized_changes_and_followers() {
    let evidence = Arc::new(Evidence::default());
    let authority = address(&format!("project:test/role:{}", "a".repeat(4096)));
    evidence.world.lock().expect("world").authority.put(&Ref::new(authority.to_string()), true);
    let (backend, _) = backend(database(), evidence, 2048);
    let mut expected = Vec::new();
    for _ in 0..4 {
        let message = backend.enqueue(spec()).await.expect("enqueue");
        expected.push((message.clone(), MessageState::Pending));
        let reason = DropReason::Withdrawn { by: address("project:test/role:sender"), authority: authority.clone() };
        backend.drop(&message, reason.clone()).await.expect("drop");
        expected.push((message, MessageState::Dropped { reason, receiver: None, watermark: None }));
    }
    let events =
        backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch").take(expected.len()).try_collect::<Vec<_>>().await.expect("stream");
    assert!(events.windows(2).all(|pair| pair[0].cursor < pair[1].cursor));
    assert_eq!(
        events.into_iter().map(|event| (event.message, event.kind)).collect::<Vec<_>>(),
        expected.into_iter().map(|(name, state)| (name, flotilla_inbox::ChangeKind::Message(state))).collect::<Vec<_>>()
    );
}

// Behaviour: irreducible binding observations refuse budget overflow before
// mutation, while an existing inbox keeps its binding and complete cold result.
#[tokio::test]
async fn control_budget_refusal_preserves_existing_binding() {
    let (backend, _) = backend(database(), Arc::new(Evidence::default()), 2048);
    backend.bind(inbox(), consumer("a")).await.expect("initial binding");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let mut refused = false;
    for n in 0..100 {
        match backend.bind(address(&format!("project:test/role:extra-{n}")), consumer("a")).await {
            Ok(()) => {}
            Err(Error::Storage(_)) => {
                refused = true;
                break;
            }
            Err(error) => panic!("unexpected refusal {error}"),
        }
    }
    assert!(refused, "live controls cannot grow past the byte budget");
    assert!(backend.residency_blocking().accounted_bytes <= 2048);
    let pending =
        backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending").try_collect::<Vec<_>>().await.expect("stream");
    assert_eq!(pending.into_iter().map(|p| p.message).collect::<Vec<_>>(), vec![message]);
}

// Behaviour: watch attribution reuses a message's create within the same source page,
// including its receipt and edge, without loading terminal rows or resource history.
// One lifecycle suffices for this page-local routing seam; paging/oversize is covered above.
#[tokio::test]
async fn watch_attributes_dense_lifecycle_once_per_page() {
    let (backend, mut log) = backend(database(), Arc::new(Evidence::default()), RESIDENT_BUDGET);
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receive");
    backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("deliver");
    log.reset_read_counts();
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        let page = log.changes_since(Cursor(0)).expect("page");
        assert_eq!(page.changes.len(), 3);
    }
    let reads = log.read_counts();
    eprintln!("dense watch: 1000 pages / 3000 changes in {:?}; {reads:?}", started.elapsed());
    assert_eq!(reads.watch_pages, 1000);
    assert_eq!(reads.resource_history_pages, 0);
    assert_eq!(reads.terminal_loads, 0);
}
