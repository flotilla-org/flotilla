use flotilla_resources::Actuation;
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::VesselReconciler;
use flotilla_resources::{
    Convoy, ConvoyPhase, ConvoyTeardownRuntime, Host, ResourceError, TerminalSession, TerminalSessionSource, TerminalSessionSpec, Vessel,
    VesselSpec, WatchEvent, WatchStart, WorkPhase, WorkflowTemplate, CONVOY_LABEL, VESSEL_REF_LABEL,
};
use flotilla_store::{
    controller::{ControllerLoop, Reconciler},
    ConvoyReconciler, InMemoryBackend, ResourceBackend, SqliteBackend,
};
use futures::StreamExt;
use tempfile::tempdir;

use flotilla_store_testkit::fixtures::{
    convoy_meta, convoy_spec, convoy_status,
    owner_gc::{create, meta, start, NS},
    pending_task_state, resource_meta, TestLoopHarness,
};

async fn vessel_finalizer_contract(backend: ResourceBackend) {
    create(&backend, meta("convoy-surrogate", None)).await;
    let vessels = backend.using::<Vessel>(NS);
    vessels
        .create(
            &meta("vessel", Some("convoy-surrogate")).with_added_finalizer("flotilla.work/vessel-workspace-teardown"),
            &VesselSpec {
                convoy_ref: "convoy-surrogate".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "unused".to_string(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .expect("create vessel");
    let terminals = backend.using::<TerminalSession>(NS);
    // A legacy label-only child proves the Vessel's finalizer actually ran;
    // the generic collector cannot delete this terminal itself.
    let mut terminal_meta = meta("running-terminal", None);
    terminal_meta.labels.insert(VESSEL_REF_LABEL.to_string(), "vessel".to_string());
    terminals
        .create(
            &terminal_meta,
            &TerminalSessionSpec {
                env_ref: "unused".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "true".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "test".to_string(),
            },
        )
        .await
        .expect("create terminal");
    let gc = start(&backend).await;
    let mut vessel_watch = vessels.watch(WatchStart::Now).await.expect("watch vessel");
    backend.using::<Host>(NS).delete("convoy-surrogate").await.expect("delete owner");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = vessel_watch.next().await.expect("watch open").expect("event");
            if matches!(event, WatchEvent::Modified(vessel) if vessel.metadata.is_pending_finalization()) {
                break;
            }
        }
    })
    .await
    .expect("cascade requests finalization");
    assert!(terminals.get("running-terminal").await.is_ok());
    let controller = tokio::spawn(
        ControllerLoop {
            primary: vessels.clone(),
            secondaries: Vec::new(),
            reconciler: VesselReconciler::new(backend.clone(), NS),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(vessel_watch.next().await.expect("watch open").expect("event"), WatchEvent::Deleted(_)) {
                break;
            }
        }
    })
    .await
    .expect("vessel finalizes reactively");
    assert!(matches!(terminals.get("running-terminal").await, Err(ResourceError::NotFound { .. })));
    gc.abort();
    controller.abort();
    let _ = gc.await;
    let _ = controller.await;
}

#[tokio::test]
async fn memory_vessel_finalizer() {
    vessel_finalizer_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}
#[tokio::test]
async fn sqlite_vessel_finalizer() {
    vessel_finalizer_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}

struct AlwaysEligible;

#[async_trait]
impl ConvoyTeardownRuntime for AlwaysEligible {
    async fn verify_reclaim(
        &self,
        _convoy: &flotilla_resources::ResourceObject<Convoy>,
        _checkouts: &[flotilla_resources::ResourceObject<flotilla_resources::Checkout>],
    ) -> Result<(), String> {
        Ok(())
    }
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
    let mut completed_status = convoy_status(ConvoyPhase::Landed);
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

    let convoy_reconciler = ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
        .with_vessels(vessels.clone())
        .with_teardown_runtime(Arc::new(AlwaysEligible));
    let completed_convoy = convoys.get("convoy-restart").await.expect("completed convoy should exist");
    let initial_dependencies = convoy_reconciler.prepare(&completed_convoy).await.expect("initial cleanup dependencies should load");
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
    let convoy_reconciler = ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
        .with_vessels(vessels.clone())
        .with_teardown_runtime(Arc::new(AlwaysEligible));
    let restarted_convoy = convoys.get("convoy-restart").await.expect("completed convoy should survive restart");
    let restart_dependencies = convoy_reconciler.prepare(&restarted_convoy).await.expect("restart cleanup dependencies should load");
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
    // Restart replays the pending vessel from the primary list, then finalizes
    // its terminal child and vessel through separate SQLite writes. Give that
    // work a CI-load bound; the former one-second deadline measured scheduling
    // delay as much as cleanup progress.
    harness
        .wait_until(Duration::from_secs(10), || {
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
        env: Default::default(),
        pool: "cleat".to_string(),
    }
}
