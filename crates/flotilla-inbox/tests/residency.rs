mod support;
use std::sync::{Arc, Mutex};
use support::{CountedInbox as SqliteInbox, CountingLog};

use flotilla_inbox::{
    sqlite::{MessageLog, StoreLog},
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

fn backend(store: Arc<Mutex<Store>>, evidence: Arc<Evidence>, budget: usize) -> (SqliteInbox, CountingLog) {
    let log = CountingLog::new(store.clone());
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
        let mut parent = String::new();
        for n in 0..96 {
            let message = backend.enqueue(spec()).await.expect("history message");
            if n == 0 {
                parent = message.clone();
            }
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
            let mut reply = spec();
            reply.in_reply_to = Some(MessageRef::new(parent.clone(), "test"));
            let message = backend.enqueue(reply).await.expect("reply to settled parent");
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
        assert_eq!(reads.scan_pages, 0, "{reads:?}");
        assert_eq!(reads.watch_pages, 0, "{reads:?}");
        if budget == RESIDENT_BUDGET {
            assert_eq!(reads.resource_loads, 0, "warm delivery reads no stored resources");
            assert_eq!(reads.receipt_reads, 0, "warm delivery reads no receipt history");
        } else {
            assert!(reads.resource_loads > 0, "cold values are read by key");
        }
        assert_eq!(reads.exists, 24, "reply admission resolves its settled parent without decoding it");
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
    let log = CountingLog::new(store.clone());
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
async fn admission_is_transaction_assigned_and_never_changes() {
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

// Behaviour: expunging any resource at the home creates a sequence gap;
// older cursors expire even for unrelated inboxes. No backend trimming occurs.
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
    let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.unwrap();
    assert!(matches!(watch.next().await, Some(Err(Error::CursorExpired))));
    let mut unrelated = backend.watch(&address("project:test/role:unrelated"), Some(Cursor(0))).await.unwrap();
    assert!(matches!(unrelated.next().await, Some(Err(Error::CursorExpired))));
}

// Behaviour: a restart that repairs a committed Undeliverable drop may encounter
// its new dead letter in a later scan page; restore must not duplicate admission.
#[test]
fn paged_restart_finishes_interrupted_dead_letter_once() {
    use flotilla_state_machines::Home;
    let store = database();
    let mut home = Home::<Message, MessageLog>::with_log("test", MessageLog::new(store.clone())).expect("home");
    let mut world = MemoryMessageWorld::default();
    world.binding.put(&Ref::new(inbox().to_string()), InboxBinding::Unbound { since: flotilla_schema::Timestamp(0) });
    let mut first = String::new();
    for n in 0..40 {
        let value = spec();
        let name = flotilla_inbox::keyed_name(&inbox(), &value.sender, &n.to_string()).expect("message address");
        if n == 0 {
            first = name.clone();
        }
        home.create(&name, value, &world, flotilla_schema::Timestamp(0)).expect("create");
        if n == 0 {
            home.apply(&name, DropMessage { reason: DropReason::Undeliverable }, &world, flotilla_schema::Timestamp(1))
                .expect("committed drop without reaction");
        }
    }
    drop(home);
    for _ in 0..2 {
        let reopened = SqliteInbox::open("test", CountingLog::new(store.clone()), Arc::new(Evidence::default()), Arc::new(Supervisors))
            .expect("restart repairs reaction");
        assert!(matches!(
            reopened
                .get_blocking(&flotilla_inbox::dead_letter_name(&address("project:test/role:supervisor"), &first).expect("dead letter"))
                .expect("one dead letter")
                .state(),
            MessageState::Pending
        ));
        assert_eq!(reopened.residency_blocking().open_messages, 40);
    }
}

// Behaviour: store-paged watches preserve a large change and all its
// followers without a second decoded-page budget or cursor skipping.
#[tokio::test]
async fn watch_pages_preserve_large_changes_and_followers() {
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

// Behaviour: absent observation callbacks return an error without poisoning the
// writer. The next valid snapshot can still admit a message.
#[tokio::test]
async fn missing_snapshot_is_recoverable() {
    struct Intermittent(std::sync::atomic::AtomicBool);
    impl flotilla_inbox::Observations for Intermittent {
        fn now(&self) -> flotilla_schema::Timestamp {
            flotilla_schema::Timestamp(10)
        }
        fn with_snapshot(&self, read: &mut dyn FnMut(&MemoryMessageWorld)) {
            if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                read(&MemoryMessageWorld::default());
            }
        }
    }
    let backend =
        MemoryInbox::open("test", MemoryInboxLog::default(), Arc::new(Intermittent(false.into())), Arc::new(Supervisors)).expect("open");
    assert!(matches!(backend.enqueue(spec()).await, Err(Error::Storage(_))));
    backend.enqueue(spec()).await.expect("writer is not poisoned");
}

// Behaviour: value eviction during a Pending stream leaves the complete
// resident index intact, including a later enqueue, without skips or duplicates.
#[tokio::test]
async fn value_eviction_preserves_resident_pending_stream() {
    let (backend, _) = backend(database(), Arc::new(Evidence::default()), 32 * 1024);
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut expected = Vec::new();
    for _ in 0..2 {
        expected.push(backend.enqueue(spec()).await.expect("warm"));
    }
    let mut pending = backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.expect("pending");
    let mut actual = vec![pending.next().await.expect("first").expect("item").message];
    let mut large = spec();
    large.body = MessageBody::text("x".repeat(64 * 1024)).expect("body");
    expected.push(backend.enqueue(large).await.expect("overflow"));
    while let Some(item) = pending.next().await {
        actual.push(item.expect("item").message);
    }
    assert_eq!(actual, expected);
}

// Behaviour: cold bind examines the full pending backlog when none has receipts;
// this documents its linear indexed-read cost without terminal/history reads.
#[tokio::test]
async fn cold_bind_backlog_cost() {
    let (backend, log) = backend(database(), Arc::new(Evidence::default()), 2048);
    for _ in 0..3 {
        backend.enqueue(spec()).await.expect("backlog");
    }
    log.reset_read_counts();
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let reads = log.read_counts();
    assert_eq!(reads.resource_loads, 3);
    assert_eq!(reads.receipt_reads, 3);
    assert_eq!(reads.terminal_loads + reads.scan_pages, 0);
}

// Behaviour: restart scans terminal history in bounded pages but retains none
// of its bodies. Measure startup separately from the resident byte budget.
#[tokio::test]
async fn terminal_backlog_restart_cost() {
    let store = database();
    let evidence = Arc::new(Evidence::default());
    let (backend, _) = backend(store.clone(), evidence.clone(), 2048);
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    for _ in 0..3 {
        let message = backend.enqueue(spec()).await.expect("enqueue");
        backend.receive(&consumer("a"), &message, Transport::Turn).await.expect("receipt");
        backend.deliver(&message, Deliver { receiver: consumer("a"), transport: Transport::Turn }).await.expect("terminal");
    }
    drop(backend);
    let log = CountingLog::new(store);
    let reopened = SqliteInbox::open_with_budget("test", log.clone(), evidence, Arc::new(Supervisors), 2048).expect("restart");
    assert_eq!(log.read_counts().scan_pages, 1);
    assert_eq!(log.read_counts().receipt_reads, 0);
    assert_eq!(reopened.residency_blocking().open_messages, 0);
    assert!(reopened.residency_blocking().accounted_bytes <= 2048);
}

// Behaviour: the first enqueue starts one clock shared by later messages in
// that inbox, even when all message values are evicted between admissions.
#[tokio::test]
async fn unbound_clock_belongs_to_inbox_not_message() {
    let evidence = Arc::new(Evidence::default());
    let (backend, _) = backend(database(), evidence.clone(), 2048);
    let mut first = spec();
    first.unbound_after = 100;
    backend.enqueue(first).await.expect("first at 10");
    evidence.clock.store(50, std::sync::atomic::Ordering::SeqCst);
    let mut later = spec();
    later.unbound_after = 100;
    let later = backend.enqueue(later).await.expect("later at 50");
    evidence.clock.store(111, std::sync::atomic::Ordering::SeqCst);
    backend.drop(&later, DropReason::Undeliverable).await.expect("inbox unbound since 10");
}

// Behaviour: sharing an inbox address across homes never adds another home's
// Pending rows to this inbox's index or decodes their bodies during a pull.
#[tokio::test]
async fn pending_excludes_other_homes_before_value_reads() {
    use flotilla_state_machines::Resource;
    let mut donor = Store::open(Connection::open_in_memory().unwrap(), "other", flotilla_state_machines::kinds().unwrap()).unwrap();
    let mut snapshots = Vec::new();
    for n in 0..40 {
        let mut value = spec();
        let name = flotilla_inbox::keyed_name(&inbox(), &value.sender, &n.to_string()).expect("message address");
        value.admitted_seq = n + 1;
        let resource = Resource::<Message>::stored(
            &name,
            value,
            flotilla_schema::State::new(MessageState::Pending, flotilla_schema::Timestamp(10)),
            flotilla_state_machines::Version(0),
        );
        let mut write = donor.write().unwrap();
        let stamp = write.insert(&name, &resource, &flotilla_schema::Origin::new("other", flotilla_schema::Timestamp(10))).unwrap();
        write.commit().unwrap();
        snapshots.push(donor.read().unwrap().load_snapshot(stamp.id).unwrap().unwrap());
    }
    let store = database();
    for snapshot in snapshots {
        let mut locked = store.lock().unwrap();
        let mut write = locked.write().unwrap();
        write.restore(&snapshot).unwrap();
        write.commit().unwrap();
    }
    let log = CountingLog::new(store);
    let backend =
        EngineInbox::open_with_budgets("test", log.clone(), Arc::new(Evidence::default()), Arc::new(Supervisors), 2048, 0).unwrap();
    assert_eq!(backend.residency_blocking().pending_messages, 0);
    backend.bind(inbox(), consumer("a")).await.unwrap();
    let mut own = Vec::new();
    for _ in 0..3 {
        own.push(backend.enqueue(spec()).await.unwrap());
    }
    log.reset_read_counts();
    let pending = backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
    assert_eq!(pending.iter().map(|p| p.message.clone()).collect::<Vec<_>>(), own);
    assert_eq!(log.read_counts().resource_loads, 3, "one indexed value read per local candidate");
    assert!(log.read_counts().pending_pages > 0, "exercise the shared-home store fallback");
    assert_eq!(log.read_counts().terminal_loads, 0);
}

// Behaviour: a source watch may read while a gate holds the inbox writer lock.
// A blocked observation callback is a deterministic simulation of a long decision.
#[tokio::test]
async fn watch_reads_do_not_take_inbox_writer_lock() {
    struct Paused {
        ready: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl flotilla_inbox::Observations for Paused {
        fn now(&self) -> flotilla_schema::Timestamp {
            flotilla_schema::Timestamp(10)
        }
        fn with_snapshot(&self, read: &mut dyn FnMut(&MemoryMessageWorld)) {
            if let Some(ready) = self.ready.lock().unwrap().take() {
                ready.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
            read(&MemoryMessageWorld::default());
        }
    }
    let (ready_tx, ready) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let backend = flotilla_inbox::sqlite::SqliteInbox::open(
        "test",
        MessageLog::new(database()),
        Arc::new(Paused { ready: Mutex::new(Some(ready_tx)), release: Mutex::new(release_rx) }),
        Arc::new(Supervisors),
    )
    .unwrap();
    let writer = backend.clone();
    let task = tokio::spawn(async move { writer.enqueue(spec()).await });
    ready.await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), backend.watch(&inbox(), None)).await;
    release.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _watch = result.expect("watch does not wait for writer").expect("source watch");
}

// Behaviour: a whole inbox spills when its complete order does not fit; cached
// inboxes and spilled inboxes keep admission order, including mid-stream spill.
// Restart rebuilds cache order through key-only pending_keys(), never from scan order, and a
// zero cache/value allowance never refuses admission or recovery. Both stores.
#[tokio::test]
async fn pending_spill_and_restart_use_declared_index() {
    async fn run<L: InboxLog + Clone + 'static>(source: L) {
        let source = CountingLog::wrap(source);
        let evidence = Arc::new(Evidence::default());
        let backend = EngineInbox::open_with_budgets("test", source.clone(), evidence.clone(), Arc::new(Supervisors), 65536, 1100).unwrap(); // two message addresses fit, a third spills
        backend.bind(inbox(), consumer("a")).await.unwrap();
        let mut names = Vec::new();
        for _ in 0..2 {
            names.push(backend.enqueue(spec()).await.unwrap());
        }
        assert_eq!(backend.residency_blocking().pending_messages, 2);
        let mut stream = backend.pending(&inbox(), &consumer("a"), &Rules::default()).await.unwrap();
        let mut pulled = vec![stream.next().await.unwrap().unwrap().message, stream.next().await.unwrap().unwrap().message];
        source.reset_read_counts();
        names.push(backend.enqueue(spec()).await.unwrap());
        assert_eq!(backend.residency_blocking().pending_messages, 0, "inbox spills as a whole");
        for _ in pulled.len()..names.len() {
            pulled.push(stream.next().await.unwrap().unwrap().message);
        }
        assert_eq!(pulled, names);
        assert!(stream.next().await.is_none());
        assert_eq!(source.read_counts().resource_loads, 1, "spill decodes only the one un-emitted candidate");
        assert!(source.read_counts().pending_pages > 0);
        assert_eq!(source.read_counts().pending_key_pages, 0, "a read never recaches the spilled inbox");
        drop(stream);
        let other_inbox = address("project:test/role:small");
        backend.bind(other_inbox.clone(), consumer("a")).await.unwrap();
        let mut other = spec();
        other.inbox = other_inbox.clone();
        let other_name = backend.enqueue(other).await.unwrap();
        assert_eq!(backend.residency_blocking().pending_messages, 1, "small inbox fits alongside spilled inbox");
        source.reset_read_counts();
        let other_rows =
            backend.pending(&other_inbox, &consumer("a"), &Rules::default()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(other_rows[0].message, other_name);
        assert_eq!(source.read_counts().pending_pages, 0, "fitting inbox serves cached order");
        drop(backend);
        source.reset_read_counts();
        let recovered = EngineInbox::open_with_budgets("test", source.clone(), evidence.clone(), Arc::new(Supervisors), 0, 1100).unwrap();
        assert!(source.read_counts().pending_key_pages > 0, "restart reads key-only declared index to rebuild cache");
        assert_eq!(source.read_counts().scan_resource_loads, 4, "startup decodes each current body once");
        assert_eq!(source.read_counts().resource_loads, 0, "rebuilding Pending order decodes no bodies");
        assert_eq!(source.read_counts().pending_pages, 0);
        let actual = recovered.pending(&inbox(), &consumer("a"), &Rules::default()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        // Recovery is Unbound; provide binding evidence through the regular API
        // on a backend with a normal control allowance below instead.
        assert!(actual.is_empty());
        drop(recovered);
        let recovered =
            EngineInbox::open_with_budgets("test", source.clone(), evidence.clone(), Arc::new(Supervisors), 65536, 65536).unwrap();
        assert_eq!(recovered.residency_blocking().pending_messages, 4, "all orders fit on restart");
        recovered.bind(inbox(), consumer("a")).await.unwrap();
        source.reset_read_counts();
        let actual = recovered.pending(&inbox(), &consumer("a"), &Rules::default()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(actual.into_iter().map(|p| p.message).collect::<Vec<_>>(), names, "cached restart preserves admission order");
        assert_eq!(source.read_counts().pending_pages, 0, "cached restart serves order from memory");
        drop(recovered);
        let recovered = EngineInbox::open_with_budgets("test", source.clone(), evidence, Arc::new(Supervisors), 2048, 0).unwrap();
        recovered.bind(inbox(), consumer("a")).await.unwrap();
        let actual = recovered.pending(&inbox(), &consumer("a"), &Rules::default()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(actual.into_iter().map(|p| p.message).collect::<Vec<_>>(), names);
        let mut extra = spec();
        extra.inbox = address("project:test/role:another");
        recovered.enqueue(extra).await.expect("no cache budget admission failure");
        assert_eq!(recovered.residency_blocking().pending_bytes, 0);
    }
    run(MemoryInboxLog::default()).await;
    run(MessageLog::new(database())).await;
}

// Behaviour: only live unbound inboxes retain clocks. Emptying and refilling
// creates a fresh clock; historical-only inboxes get no clock on restart.
#[tokio::test]
async fn empty_inbox_refill_and_recovery_clock_lifetime() {
    async fn run<L: InboxLog + Clone + 'static>(log: L) {
        let evidence = Arc::new(Evidence::default());
        let backend = EngineInbox::open_with_budgets("test", log.clone(), evidence.clone(), Arc::new(Supervisors), 0, 0).unwrap();
        let value = || {
            let mut value = spec();
            value.sender = address("project:test/role:alone");
            value.unbound_after = 100;
            value
        };
        let first = backend.enqueue(value()).await.unwrap();
        evidence.clock.store(111, std::sync::atomic::Ordering::SeqCst);
        backend.drop(&first, DropReason::Undeliverable).await.unwrap();
        assert_eq!(backend.residency_blocking().control_bytes, 0);
        evidence.clock.store(200, std::sync::atomic::Ordering::SeqCst);
        let next = backend.enqueue(value()).await.unwrap();
        evidence.clock.store(250, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(backend.drop(&next, DropReason::Undeliverable).await, Err(Error::Message(_))), "fresh clock has not matured");
        evidence.clock.store(301, std::sync::atomic::Ordering::SeqCst);
        backend.drop(&next, DropReason::Undeliverable).await.unwrap();
        drop(backend);
        let recovered = EngineInbox::open_with_budgets("test", log, evidence, Arc::new(Supervisors), 0, 0).unwrap();
        assert_eq!(recovered.residency_blocking().control_bytes, 0, "terminal history has no inbox clock");
    }
    run(MemoryInboxLog::default()).await;
    run(MessageLog::new(database())).await;
}

// Behaviour: a lagging watch crosses the source's 16-entry page boundary with
// no extra validation page. A restart resumes from the last emitted cursor,
// including events buffered but not emitted before shutdown. Both stores.
#[tokio::test]
async fn watch_lag_crosses_pages_and_restart() {
    async fn run<L: InboxLog + Clone + 'static>(log: L, restart: bool) {
        let log = CountingLog::wrap(log);
        let evidence = Arc::new(Evidence::default());
        let backend = EngineInbox::open("test", log.clone(), evidence.clone(), Arc::new(Supervisors)).unwrap();
        log.reset_read_counts();
        let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.unwrap();
        assert_eq!(log.read_counts().watch_pages, 0, "construction does not read the first page");
        let mut names = Vec::new();
        for _ in 0..35 {
            names.push(backend.enqueue(spec()).await.unwrap());
        }
        if restart {
            let mut last = Cursor(0);
            for name in &names[..17] {
                let row = watch.next().await.unwrap().unwrap();
                assert_eq!(&row.message, name);
                last = row.cursor;
            }
            drop(watch);
            drop(backend);
            let backend = EngineInbox::open("test", log.clone(), evidence, Arc::new(Supervisors)).unwrap();
            log.reset_read_counts();
            let mut watch = backend.watch(&inbox(), Some(last)).await.unwrap();
            assert_eq!(log.read_counts().watch_pages, 0);
            for name in &names[17..] {
                assert_eq!(&watch.next().await.unwrap().unwrap().message, name);
            }
            assert_eq!(log.read_counts().watch_pages, 2);
        } else {
            for name in names {
                assert_eq!(watch.next().await.unwrap().unwrap().message, name);
            }
            assert_eq!(log.read_counts().watch_pages, 3);
        }
    }
    for restart in [false, true] {
        run(MemoryInboxLog::default(), restart).await;
        run(MessageLog::new(database()), restart).await;
    }
}

// Behaviour: lag survives closing the actual SQLite connection, including
// unconsumed buffered entries, and resumes through multiple source pages.
#[tokio::test]
async fn watch_lag_survives_sqlite_connection_restart() {
    struct File(std::path::PathBuf);
    impl Drop for File {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let file = File(std::env::temp_dir().join(format!("inbox-watch-{}-{unique}.sqlite", std::process::id())));
    let open = || {
        CountingLog::new(Arc::new(Mutex::new(
            Store::open(Connection::open(&file.0).unwrap(), "test", flotilla_state_machines::kinds().unwrap()).unwrap(),
        )))
    };
    let evidence = Arc::new(Evidence::default());
    let log = open();
    let backend = EngineInbox::open("test", log.clone(), evidence.clone(), Arc::new(Supervisors)).unwrap();
    let mut watch = backend.watch(&inbox(), Some(Cursor(0))).await.unwrap();
    let mut names = Vec::new();
    for _ in 0..35 {
        names.push(backend.enqueue(spec()).await.unwrap());
    }
    let mut last = Cursor(0);
    for name in &names[..7] {
        let row = watch.next().await.unwrap().unwrap();
        assert_eq!(&row.message, name);
        last = row.cursor;
    }
    drop(watch);
    drop(backend);
    drop(log);
    let log = open();
    let recovered = EngineInbox::open("test", log.clone(), evidence, Arc::new(Supervisors)).unwrap();
    log.reset_read_counts();
    let mut watch = recovered.watch(&inbox(), Some(last)).await.unwrap();
    for name in &names[7..] {
        assert_eq!(&watch.next().await.unwrap().unwrap().message, name);
    }
    assert_eq!(log.read_counts().watch_pages, 2);
}
