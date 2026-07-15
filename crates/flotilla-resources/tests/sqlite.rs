mod common;

use std::{sync::mpsc as std_mpsc, thread, time::Instant};

use chrono::Utc;
use common::{
    contract::{
        assert_consumer_relists_after_expired_watch_and_converges_with_backend, assert_create_get_list_roundtrip_with_backend,
        assert_delete_emits_event_with_backend, assert_identical_status_update_is_noop_with_backend,
        assert_identical_update_is_noop_with_backend, assert_metadata_roundtrip_with_backend, assert_namespace_isolation_with_backend,
        assert_repeated_delete_with_pending_finalizers_is_noop_with_backend, assert_stale_resource_version_conflicts_with_backend,
        assert_store_diagnostics_report_retained_events_with_backend, assert_watch_from_version_replays_with_backend,
        assert_watch_now_semantics_with_backend, assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend,
        assert_watch_retention_expires_only_versions_below_floor_with_backend, ConvoyFixture,
    },
    convoy_meta, convoy_spec, convoy_status, pending_task_state, resource_meta, TestLoopHarness,
};
use flotilla_controllers::reconcilers::VesselReconciler;
use flotilla_resources::{
    controller::{Actuation, ControllerLoop, Reconciler},
    ApiPaths, Convoy, ConvoyPhase, ConvoyReconciler, EventRetention, NoStatusPatch, Resource, ResourceBackend, ResourceError,
    SqliteBackend, TerminalSession, TerminalSessionSource, TerminalSessionSpec, Vessel, VesselSpec, WatchEvent, WatchStart, WorkPhase,
    WorkflowTemplate, CONVOY_LABEL, VESSEL_REF_LABEL,
};
use futures::StreamExt;
use rstest::rstest;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use tempfile::tempdir;
use tokio::time::{timeout, Duration};

fn backend() -> ResourceBackend {
    ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend should open"))
}

#[derive(Debug, Clone, Copy)]
struct SlowResource;

#[derive(Debug, Clone, Deserialize)]
struct SlowSpec {
    value: String,
    #[serde(skip)]
    serialization_delay: Duration,
}

impl Serialize for SlowSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        thread::sleep(self.serialization_delay);
        let mut state = serializer.serialize_struct("SlowSpec", 1)?;
        state.serialize_field("value", &self.value)?;
        state.end()
    }
}

impl Resource for SlowResource {
    type Spec = SlowSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.test", version: "v1", plural: "slowresources", kind: "SlowResource" };
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn create_get_list_roundtrip(#[case] _fixture: ConvoyFixture) {
    assert_create_get_list_roundtrip_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn update_requires_current_resource_version(#[case] _fixture: ConvoyFixture) {
    assert_stale_resource_version_conflicts_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_update_is_noop_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_status_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_status_update_is_noop_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn delete_emits_deleted_event(#[case] _fixture: ConvoyFixture) {
    assert_delete_emits_event_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn repeated_delete_is_noop_while_finalizers_are_pending(#[case] _fixture: ConvoyFixture) {
    assert_repeated_delete_with_pending_finalizers_is_noop_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_from_version_replays_gaplessly_after_list(#[case] _fixture: ConvoyFixture) {
    assert_watch_from_version_replays_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_now_only_sees_future_events(#[case] _fixture: ConvoyFixture) {
    assert_watch_now_semantics_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_below_retention_floor_expires(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_watch_retention_expires_only_versions_below_floor_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn expired_watch_consumer_relists_and_converges(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_consumer_relists_after_expired_watch_and_converges_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn diagnostics_report_bounded_event_log(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_store_diagnostics_report_retained_events_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_only_diagnostics_match_mutation_based_stream_semantics(#[case] _fixture: ConvoyFixture) {
    assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn namespaces_are_isolated(#[case] _fixture: ConvoyFixture) {
    assert_namespace_isolation_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn owner_references_roundtrip_through_sqlite_backend(#[case] _fixture: ConvoyFixture) {
    assert_metadata_roundtrip_with_backend::<ConvoyFixture>(backend()).await;
}

#[tokio::test]
async fn objects_and_resource_versions_survive_restart() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");
    assert_eq!(created.metadata.resource_version, "1");
    drop(resolver);
    drop(backend);

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should reopen"));
    let resolver = backend.using::<Convoy>("flotilla");
    let fetched = resolver.get("alpha").await.expect("object should survive restart");
    assert_eq!(fetched.metadata.resource_version, "1");
    assert_eq!(fetched.spec.workflow_ref, "template-a");

    let updated = resolver
        .update(&convoy_meta("alpha"), &fetched.metadata.resource_version, &convoy_spec("template-b"))
        .await
        .expect("update should succeed after restart");
    assert_eq!(updated.metadata.resource_version, "2");
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_sqlite_open_does_not_stall_tokio_executor() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let blocker_path = path.clone();
    let (locked_tx, locked_rx) = std_mpsc::channel();
    let blocker = thread::spawn(move || {
        let connection = rusqlite::Connection::open(blocker_path).expect("blocking connection should open");
        connection.execute_batch("CREATE TABLE startup_lock (value INTEGER); BEGIN EXCLUSIVE").expect("exclusive transaction should begin");
        locked_tx.send(()).expect("lock acquisition should be reported");
        thread::sleep(Duration::from_millis(400));
        connection.execute_batch("ROLLBACK").expect("exclusive transaction should roll back");
    });
    locked_rx.recv().expect("blocking transaction should start");

    let started = Instant::now();
    let heartbeat = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        started.elapsed()
    });
    SqliteBackend::open_async(&path).await.expect("sqlite backend should open after lock release");

    let heartbeat_delay = heartbeat.await.expect("heartbeat should complete");
    assert!(heartbeat_delay < Duration::from_millis(150), "Tokio heartbeat was delayed by {heartbeat_delay:?}");
    blocker.join().expect("blocking connection thread should finish");
}

#[tokio::test(flavor = "current_thread")]
async fn slow_sqlite_crud_does_not_stall_tokio_executor() {
    let resolver = backend().using::<SlowResource>("flotilla");
    let started = Instant::now();
    let heartbeat = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        started.elapsed()
    });

    resolver
        .create(&resource_meta().name("alpha").call(), &SlowSpec {
            value: "alpha".to_string(),
            serialization_delay: Duration::from_millis(200),
        })
        .await
        .expect("slow create should succeed");

    let heartbeat_delay = heartbeat.await.expect("heartbeat should complete");
    assert!(heartbeat_delay < Duration::from_millis(150), "Tokio heartbeat was delayed by {heartbeat_delay:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_create_and_watch_delivers_the_committed_version_once() {
    let resolver = backend().using::<SlowResource>("flotilla");
    let create_resolver = resolver.clone();
    let create = tokio::spawn(async move {
        create_resolver
            .create(&resource_meta().name("alpha").call(), &SlowSpec {
                value: "alpha".to_string(),
                serialization_delay: Duration::from_millis(200),
            })
            .await
    });

    tokio::time::sleep(Duration::from_millis(25)).await;
    let watch_resolver = resolver.clone();
    let watch = tokio::spawn(async move { watch_resolver.watch(WatchStart::FromVersion("0".to_string())).await });
    tokio::task::yield_now().await;
    thread::sleep(Duration::from_millis(450));

    create.await.expect("create task should complete").expect("create should succeed");
    let mut watch = watch.await.expect("watch task should complete").expect("watch should start");
    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should replay the create")
        .expect("watch should remain open")
        .expect("event should decode");
    assert!(matches!(event, WatchEvent::Added(object) if object.metadata.name == "alpha"));
    assert!(
        timeout(Duration::from_millis(50), watch.next()).await.is_err(),
        "the committed version must not be emitted by both replay and live notification"
    );
}

#[tokio::test]
async fn completed_convoy_cleanup_converges_after_sqlite_restart_with_pending_vessel_finalizer() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    let terminals = backend.clone().using::<TerminalSession>("flotilla");

    let convoy =
        convoys.create(&convoy_meta("convoy-restart"), &convoy_spec("workflow-restart")).await.expect("convoy create should succeed");
    let mut completed_status = convoy_status(ConvoyPhase::Completed);
    completed_status.observed_workflow_ref = Some("workflow-restart".to_string());
    let mut completed_work = pending_task_state();
    completed_work.phase = WorkPhase::Complete;
    completed_work.finished_at = Some(Utc::now());
    completed_work.message = Some("done".to_string());
    completed_status.work.insert("implement".to_string(), completed_work);
    convoys
        .update_status("convoy-restart", &convoy.metadata.resource_version, &completed_status)
        .await
        .expect("convoy completion should be recorded");

    vessels
        .create(
            &resource_meta()
                .name("convoy-restart-implement")
                .labels([(CONVOY_LABEL.to_string(), "convoy-restart".to_string())].into_iter().collect())
                .finalizers(vec!["flotilla.work/vessel-workspace-teardown".to_string()])
                .call(),
            &restart_vessel_spec(),
        )
        .await
        .expect("vessel create should succeed");
    terminals
        .create(
            &resource_meta()
                .name("terminal-convoy-restart-implement-coder")
                .labels([(VESSEL_REF_LABEL.to_string(), "convoy-restart-implement".to_string())].into_iter().collect())
                .call(),
            &restart_terminal_session_spec(),
        )
        .await
        .expect("terminal child should be created");

    let convoy_reconciler = ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla")).with_vessels(vessels.clone());
    let completed_convoy = convoys.get("convoy-restart").await.expect("completed convoy should exist");
    let initial_dependencies =
        convoy_reconciler.fetch_dependencies(&completed_convoy).await.expect("initial cleanup dependencies should load");
    let initial_cleanup = convoy_reconciler.reconcile(&completed_convoy, &initial_dependencies, Utc::now());
    assert!(initial_cleanup
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-restart-implement")));

    vessels.delete("convoy-restart-implement").await.expect("initial convoy cleanup should mark the vessel");
    assert!(terminals.get("terminal-convoy-restart-implement-coder").await.is_ok(), "delayed finalizer should retain terminal child");

    drop(convoy_reconciler);
    drop(terminals);
    drop(vessels);
    drop(convoys);
    drop(backend);

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should reopen"));
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    let terminals = backend.clone().using::<TerminalSession>("flotilla");
    let convoy_reconciler = ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla")).with_vessels(vessels.clone());
    let restarted_convoy = convoys.get("convoy-restart").await.expect("completed convoy should survive restart");
    let restart_dependencies =
        convoy_reconciler.fetch_dependencies(&restarted_convoy).await.expect("restart cleanup dependencies should load");
    let restart_cleanup = convoy_reconciler.reconcile(&restarted_convoy, &restart_dependencies, Utc::now());
    assert!(
        !restart_cleanup
            .actuations
            .iter()
            .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-restart-implement")),
        "restart cleanup must leave a persisted pending vessel to its finalizer"
    );

    vessels
        .delete("convoy-restart-implement")
        .await
        .expect("a repeated queued cleanup after restart should not hard-delete the pending vessel");
    let pending = vessels.get("convoy-restart-implement").await.expect("pending vessel should survive the repeated delete");
    assert!(pending.metadata.deletion_timestamp.is_some());
    assert_eq!(pending.metadata.finalizers, vec!["flotilla.work/vessel-workspace-teardown".to_string()]);

    let mut harness = TestLoopHarness::new();
    harness.spawn(
        ControllerLoop {
            primary: vessels.clone(),
            secondaries: Vec::new(),
            reconciler: VesselReconciler::new(backend.clone(), "flotilla"),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );
    harness
        .wait_until(Duration::from_secs(1), || {
            let vessels = vessels.clone();
            let terminals = terminals.clone();
            async move {
                matches!(vessels.get("convoy-restart-implement").await, Err(ResourceError::NotFound { .. }))
                    && matches!(terminals.get("terminal-convoy-restart-implement-coder").await, Err(ResourceError::NotFound { .. }))
            }
        })
        .await;
    harness.shutdown().await;
}

fn restart_vessel_spec() -> VesselSpec {
    VesselSpec {
        convoy_ref: "convoy-restart".to_string(),
        vessel_name: "implement".to_string(),
        placement_policy_ref: "policy-restart".to_string(),
        adopted_checkout_refs: Default::default(),
    }
}

fn restart_terminal_session_spec() -> TerminalSessionSpec {
    TerminalSessionSpec {
        env_ref: "host-direct-01HXYZ".to_string(),
        role: "coder".to_string(),
        source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/workspace".to_string(),
        pool: "cleat".to_string(),
    }
}

#[test]
fn file_backend_enables_wal_and_busy_timeout() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");
    let backend = SqliteBackend::open(&path).expect("sqlite backend should open");

    let connection = rusqlite::Connection::open(&path).expect("inspection connection should open");
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).expect("read journal mode");
    let busy_timeout_ms: u64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0)).expect("read busy timeout");

    assert_eq!(journal_mode, "wal");
    assert_eq!(busy_timeout_ms, 5_000);
    drop(backend);
}

#[tokio::test]
async fn watch_from_version_replays_events_persisted_before_restart() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");
    let updated = resolver
        .update(&convoy_meta("alpha"), &created.metadata.resource_version, &convoy_spec("template-b"))
        .await
        .expect("update should succeed");
    drop(resolver);
    drop(backend);

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should reopen"));
    let resolver = backend.using::<Convoy>("flotilla");
    let mut watch = resolver.watch(WatchStart::FromVersion(created.metadata.resource_version)).await.expect("watch should succeed");

    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should replay persisted event")
        .expect("stream should yield item")
        .expect("event should decode");
    match event {
        WatchEvent::Modified(object) => assert_eq!(object.metadata.resource_version, updated.metadata.resource_version),
        other => panic!("expected modified event, got {other:?}"),
    }
}

#[tokio::test]
async fn reopening_with_smaller_retention_compacts_existing_events_and_persists_floor() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let first = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create");
    let second =
        resolver.update(&convoy_meta("alpha"), &first.metadata.resource_version, &convoy_spec("template-b")).await.expect("first update");
    let third =
        resolver.update(&convoy_meta("alpha"), &second.metadata.resource_version, &convoy_spec("template-c")).await.expect("second update");
    resolver.update(&convoy_meta("alpha"), &third.metadata.resource_version, &convoy_spec("template-d")).await.expect("third update");
    drop(resolver);
    drop(backend);

    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::Sqlite(
        SqliteBackend::open_with_event_retention(&path, retention).expect("sqlite backend should reopen with smaller retention"),
    );
    let resolver = backend.using::<Convoy>("flotilla");
    let expired = resolver
        .watch(WatchStart::FromVersion(first.metadata.resource_version.clone()))
        .await
        .expect_err("startup compaction should expire old version");
    assert_eq!(expired, flotilla_resources::ResourceError::WatchExpired {
        requested_version: first.metadata.resource_version,
        compacted_through: Some(second.metadata.resource_version),
    });

    drop(resolver);
    drop(backend);
    let backend = ResourceBackend::Sqlite(
        SqliteBackend::open_with_event_retention(&path, retention).expect("sqlite backend should reopen after compaction"),
    );
    let resolver = backend.using::<Convoy>("flotilla");
    assert!(matches!(
        resolver.watch(WatchStart::FromVersion("1".to_string())).await,
        Err(flotilla_resources::ResourceError::WatchExpired { compacted_through, .. }) if compacted_through.as_deref() == Some("2")
    ));
}
