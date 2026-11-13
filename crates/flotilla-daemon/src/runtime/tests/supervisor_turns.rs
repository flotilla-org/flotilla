use super::*;
use flotilla_store_testkit::{ReadCountsBackendExt, ReadOperation};

// Startup adopts old queues. Watch notifications reconcile current Message evidence
// only, even when replica/session events arrive in separate batches, and no timer
// wakes the task.
#[tokio::test]
async fn legacy_adoption_runs_only_at_startup() {
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
    let intent = flotilla_resources::MessageSpec::builder()
        .sender("system:turn-rules".into())
        .receiver("flotilla/c/work/coder".into())
        .relation(flotilla_resources::MessageRelation::System)
        .body("turn".into())
        .build();
    // Admission reads back the message it admits; what one admission costs, with nothing listening.
    let rows = primitives.rows_read();
    primitives.admit(NAMESPACE, "first", &intent).await.expect("admit");
    let admission = primitives.rows_read();
    let admission = (admission.loaded - rows.loaded, admission.converted - rows.converted);

    let (task, ready) = super::super::tasks::spawn_pending_supervisor_turn_task(Arc::clone(&daemon), NAMESPACE.into());
    let task = tokio_util::task::AbortOnDropHandle::new(task);
    ready.await.expect("startup ready");

    let (before, rows) = (backend.read_snapshot(), primitives.rows_read());
    primitives.admit(NAMESPACE, "turn", &intent).await.expect("admit");
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.read_snapshot().since(&before).calls("Convoy", ReadOperation::Query) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the commit runs a pass");
    let after = primitives.rows_read();
    assert_eq!(
        (after.loaded - rows.loaded, after.converted - rows.converted),
        admission,
        "beyond its admission, hearing a Message commit loads and converts no message"
    );
    drop(task);
}
