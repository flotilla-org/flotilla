use super::*;
use crate::testkits::engine::engine;
use flotilla_store::load_metrics::CallerReads;

/// The convoy listings `reads` has made, by index or the whole kind.
fn convoy_listings(reads: &CallerReads) -> u64 {
    reads.snapshot().get("Convoy port").map_or(0, |convoys| convoys.index + convoys.whole_kind)
}

/// A capture of this test's spans as they close, as the daemon writes its jsonl.
fn closed_spans() -> (tracing::Dispatch, flotilla_log_testkit::Logs) {
    let logs = flotilla_log_testkit::Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_ansi(false)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(move || writer.clone())
        .finish();
    (flotilla_log_testkit::dispatch(subscriber), logs)
}

/// The supervisor turn passes `logs` saw end.
fn passes(logs: &flotilla_log_testkit::Logs) -> usize {
    logs.lines()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|line| line["fields"]["message"] == "close" && line["span"]["name"] == "supervisor_turns.pass")
        .count()
}

// Startup runs one pass. Watch notifications reconcile current Message evidence
// only, even when replica/session events arrive in separate batches, and no timer
// wakes the task. A pass takes its candidates from this home's follow-up notes: it lists no
// convoy.
#[tokio::test(start_paused = true)]
async fn startup_runs_one_pass_and_watches_drive_the_rest() {
    let temp = tempfile::tempdir().expect("config");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"supervisor-turns-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let _engine = engine(&daemon);
    // The test's runtime is current-thread, so the scoped dispatcher sees the task's spans.
    let (logging, logs) = closed_spans();
    let _logging = tracing::dispatcher::set_default(&logging);
    let (convoy_sender, convoy_receiver) = futures::channel::mpsc::unbounded();
    let (session_sender, session_receiver) = futures::channel::mpsc::unbounded();
    let receivers = Arc::new(StdMutex::new(Some((convoy_receiver, session_receiver))));
    // The task's reads count for one caller.
    let reads = Arc::new(CallerReads::default());
    let (task, ready) = within(&reads, || {
        super::super::tasks::spawn_pending_supervisor_turn_task_with_watches(daemon, NAMESPACE.into(), move || {
            let (convoys, sessions) = receivers.lock().expect("receivers").take().expect("one subscription");
            async move { Ok((convoys.boxed(), sessions.boxed())) }
        })
    });
    let task = tokio_util::task::AbortOnDropHandle::new(task);
    ready.await.expect("startup ready");
    assert_eq!(passes(&logs), 1);
    for sender in [&convoy_sender, &session_sender] {
        let before = passes(&logs);
        sender.unbounded_send(Ok(())).expect("watch event");
        // The pass may read on the blocking pool, which runs on wall time while the test's clock
        // is paused: wait on wall time for it, up to a bound.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while passes(&logs) == before && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        assert_eq!(passes(&logs) - before, 1);
    }
    assert_eq!(convoy_listings(&reads), 0, "a pass lists no convoy");
    // No timer wakes the task: over ten idle minutes it passes and reads nothing at all.
    let (before, passed) = (reads.snapshot(), passes(&logs));
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(reads.snapshot(), before);
    assert_eq!(passes(&logs), passed);
    drop(task);
}

// A Message committed on the engine's Message path runs a pass, heard from the store's commits:
// with no convoy to reconcile, nothing beyond the admission loads or converts a message.
#[tokio::test(start_paused = true)]
async fn a_message_commit_runs_a_pass_without_listing_messages() {
    let temp = tempfile::tempdir().expect("config");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"supervisor-turn-message-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp.path()))).await;
    let _engine = engine(&daemon);
    let (logging, logs) = closed_spans();
    let _logging = tracing::dispatcher::set_default(&logging);
    let primitives = daemon.message_primitives();
    designate_fleet(&daemon).await;
    // The designated fleet here is `fleet`, so turn-rules sends as `fleet:fleet/role:turn-rules`.
    let intent = flotilla_engine_messages_testkit::intent(
        "fleet:fleet/role:turn-rules",
        "flotilla/c/work/coder",
        flotilla_resources::MessageRelation::System,
        "turn",
    );
    // The test's and the task's reads count for one caller.
    let reads = Arc::new(CallerReads::default());
    let rows = || reads.snapshot().get("Message").map_or((0, 0), |counts| (counts.decoded, counts.converted));
    // Admission reads back the message it admits; what one admission costs, with nothing listening.
    attributed(Arc::clone(&reads), primitives.admit("first", &intent)).await.expect("admit");
    let admission = rows();

    let (task, ready) = within(&reads, || super::super::tasks::spawn_pending_supervisor_turn_task(Arc::clone(&daemon), NAMESPACE.into()));
    let task = tokio_util::task::AbortOnDropHandle::new(task);
    ready.await.expect("startup ready");

    let (before, read) = (passes(&logs), rows());
    attributed(Arc::clone(&reads), primitives.admit("turn", &intent)).await.expect("admit");
    // The commit is heard through the store's log, read on the blocking pool on wall time while
    // the test's clock is paused: wait on wall time for the pass, up to a bound.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while passes(&logs) == before && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert!(passes(&logs) > before, "the commit runs a pass");
    let after = rows();
    assert_eq!(
        (after.0 - read.0, after.1 - read.1),
        admission,
        "beyond its admission, hearing a Message commit loads and converts no message"
    );
    drop(task);
}
