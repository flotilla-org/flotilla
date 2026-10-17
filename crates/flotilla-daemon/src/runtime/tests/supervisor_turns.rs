use super::*;
use flotilla_store_testkit::{ReadCountsBackendExt, ReadOperation};

// Startup and resync adopt old queues. Watch notifications reconcile current
// Message evidence only, even when replica/session events arrive in separate batches.
#[tokio::test]
async fn legacy_adoption_runs_only_at_startup_and_resync() {
    let temp = tempfile::tempdir().expect("config");
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"legacy-adoption-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_read_counts();
    let daemon = daemon_with_backend(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), backend.clone()).await;
    tokio::time::pause();
    let (convoy_sender, convoy_receiver) = futures::channel::mpsc::unbounded();
    let (session_sender, session_receiver) = futures::channel::mpsc::unbounded();
    let receivers = Arc::new(StdMutex::new(Some((convoy_receiver, session_receiver))));
    let before = backend.read_snapshot();
    let (task, ready) = super::super::tasks::spawn_pending_supervisor_turn_task_with_watches(
        daemon,
        NAMESPACE.into(),
        Duration::from_secs(60),
        move || {
            let (convoys, sessions) = receivers.lock().expect("receivers").take().expect("one subscription");
            async move { Ok((convoys.boxed(), sessions.boxed())) }
        },
    );
    let task = tokio_util::task::AbortOnDropHandle::new(task);
    ready.await.expect("startup ready");
    assert_eq!(backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query), 3);
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
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query) >= 3 {
            break;
        }
    }
    assert_eq!(backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query), 3);
    drop(task);
}
