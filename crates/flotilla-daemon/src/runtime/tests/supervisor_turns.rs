use super::*;
use flotilla_store::load_metrics::CallerReads;
use flotilla_store_testkit::{ReadCountsBackendExt, ReadOperation};

// Startup runs one pass. Watch notifications reconcile current Message evidence
// only, even when replica/session events arrive in separate batches, and no timer
// wakes the task.
#[tokio::test]
async fn startup_runs_one_pass_and_watches_drive_the_rest() {
    let temp = tempfile::tempdir().expect("config");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"legacy-adoption-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_read_counts();
    let daemon = daemon_with_backend(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), backend.clone()).await;
    tokio::time::pause();
    let (convoy_sender, convoy_receiver) = futures::channel::mpsc::unbounded();
    let (session_sender, session_receiver) = futures::channel::mpsc::unbounded();
    let receivers = Arc::new(StdMutex::new(Some((convoy_receiver, session_receiver))));
    let before = backend.read_snapshot();
    let (task, ready) = super::super::tasks::spawn_pending_supervisor_turn_task_with_watches(daemon, NAMESPACE.into(), move || {
        let (convoys, sessions) = receivers.lock().expect("receivers").take().expect("one subscription");
        async move { Ok((convoys.boxed(), sessions.boxed())) }
    });
    let task = tokio_util::task::AbortOnDropHandle::new(task);
    ready.await.expect("startup ready");
    assert_eq!(backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query), 1);
    for sender in [&convoy_sender, &session_sender] {
        let before = backend.read_snapshot();
        sender.unbounded_send(Ok(())).expect("watch event");
        for _ in 0..100 {
            tokio::task::yield_now().await;
            if backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query) > 0 {
                break;
            }
        }
        assert_eq!(backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query), 1);
    }
    let before = backend.read_snapshot();
    tokio::time::advance(Duration::from_secs(600)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query), 0);
    drop(task);
}

// A Message committed on the engine's Message path runs a pass, heard from the store's commits:
// with no convoy to reconcile, nothing beyond the admission loads or converts a message.
#[tokio::test]
async fn a_message_commit_runs_a_pass_without_listing_messages() {
    let temp = tempfile::tempdir().expect("config");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"supervisor-turn-message-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_read_counts();
    let daemon = daemon_with_backend(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), backend.clone()).await;
    let primitives = daemon.open_message_primitives().expect("message path");
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

    let (before, read) = (backend.read_snapshot(), rows());
    attributed(Arc::clone(&reads), primitives.admit("turn", &intent)).await.expect("admit");
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the commit runs a pass");
    let after = rows();
    assert_eq!(
        (after.0 - read.0, after.1 - read.1),
        admission,
        "beyond its admission, hearing a Message commit loads and converts no message"
    );
    drop(task);
}
