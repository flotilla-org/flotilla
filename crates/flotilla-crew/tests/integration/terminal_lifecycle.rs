//! Real SQLite contracts: an engine launch, never an old status copy, decides lifecycle behavior.
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use chrono::DateTime;
use flotilla_crew::reconcilers::terminal_session::{TerminalLiveness, TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler};
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::engine::{
    terminal_session::{EngineTerminalLaunches, TerminalSessionBinding},
    Engine, Lifecycles,
};
use flotilla_provisioning_testkit::{store_object, terminal_session, Ending, LifecycleFixture};
use flotilla_resources::{Clock, ResourceObject, TerminalSession, TerminalSessionPhase as Phase, TerminalSessionTarget};
use flotilla_store::{
    apply_status_patch,
    controller::{ReconcileOutcome, Reconciler},
    ResourceBackend,
};
use flotilla_store_testkit::VirtualClock;

// The terminal process boundary is controlled here; lifecycle storage remains real SQLite.
struct Runtime {
    starts: AtomicUsize,
    fail: AtomicBool,
    live: Mutex<TerminalLiveness>,
    probes: Mutex<Vec<String>>,
    probe_gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}
#[async_trait]
impl TerminalRuntime for Runtime {
    async fn ensure_session(
        &self,
        _: &str,
        _: TerminalSessionTarget<'_>,
        _: &[flotilla_terminal::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        let n = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail.load(Ordering::SeqCst) {
            return Err("launch refused".into());
        }
        Ok(TerminalRuntimeState::builder()
            .session_id(format!("new-{n}"))
            .started_at(DateTime::UNIX_EPOCH)
            .launch_command("bash".into())
            .build())
    }
    async fn session_liveness(&self, id: &str, _: TerminalSessionTarget<'_>) -> Result<TerminalLiveness, String> {
        let gate = self.probe_gate.lock().expect("gate").clone();
        if let Some((entered, resume)) = gate {
            entered.notify_one();
            resume.notified().await;
        }
        self.probes.lock().expect("probes").push(id.into());
        Ok(self.live.lock().expect("live").clone())
    }
    async fn kill_session(&self, _: &str, _: TerminalSessionTarget<'_>) -> Result<(), String> {
        Ok(())
    }
}

type Controller = Engine<TerminalSessionReconciler<Runtime>, TerminalSessionBinding>;
struct Fixture {
    backend: ResourceBackend,
    lifecycle: LifecycleFixture<TerminalSessionBinding>,
    obj: ResourceObject<TerminalSession>,
    runtime: Arc<Runtime>,
    clock: Arc<VirtualClock>,
}
impl Fixture {
    async fn new(phase: Phase) -> Self {
        Self::with_initial_launch(phase, true).await
    }
    async fn with_initial_launch(phase: Phase, requested: bool) -> Self {
        let backend = ResourceBackend::new(flotilla_store_sqlite::SqliteBackend::open_in_memory().expect("resources"));
        super::terminal_session_reconciler::create_ready_environment(&backend, "env").await;
        let obj = store_object(&backend, terminal_session(phase)).await;
        let lifecycle = LifecycleFixture::<TerminalSessionBinding>::new(false);
        if requested {
            lifecycle.request(&obj);
        }
        backend.install_terminal_launches(Arc::new(EngineTerminalLaunches { lifecycles: lifecycle.home.clone() }));
        backend.install_terminal_readings(lifecycle.reads.clone());
        Self {
            backend,
            lifecycle,
            obj,
            runtime: Arc::new(Runtime {
                starts: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
                live: Mutex::new(TerminalLiveness::Running),
                probes: Default::default(),
                probe_gate: Default::default(),
            }),
            clock: Arc::new(VirtualClock::new(DateTime::UNIX_EPOCH + chrono::Duration::seconds(10))),
        }
    }
    fn controller(&self) -> Controller {
        self.controller_with(self.lifecycle.home.clone())
    }
    fn controller_with(
        &self,
        home: Arc<dyn Lifecycles<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>>,
    ) -> Controller {
        Engine::new(
            TerminalSessionReconciler::new(self.runtime.clone(), self.backend.clone(), "flotilla")
                .with_clock(self.clock.clone())
                .with_lifecycle_reads(self.lifecycle.reads.clone()),
            Some(home),
        )
    }
    async fn tick(&self, controller: &Controller, publish: bool) -> ReconcileOutcome<TerminalSession> {
        let records = self.backend.using::<TerminalSession>("flotilla");
        let obj = records.get(&self.obj.metadata.name).await.expect("object");
        let frame = controller.prepare(&obj).await.expect("prepare");
        let outcome = controller.reconcile(&obj, &frame, self.clock.now());
        if publish {
            if let Some(patch) = &outcome.patch {
                apply_status_patch(&records, &obj.metadata.name, patch).await.expect("publish");
            }
        }
        outcome
    }
    fn reading(&self) -> flotilla_resources::terminal_launches::TerminalLaunch {
        self.lifecycle.reads.try_terminal_launch(&self.obj.metadata.name, None).expect("read").expect("held")
    }
    async fn replace(&self, predecessor: &str) -> Result<(), flotilla_resources::ResourceError> {
        self.backend.request_terminal_replacement(&self.obj, Some(predecessor), self.clock.now()).await
    }
}

// Requested successor is durable even if observation publication fails; duplicate and late requests cannot allocate n+2.
#[tokio::test]
async fn requested_replacement_survives_status_write_failure_and_refuses_n_plus_two() {
    let fixture = Fixture::new(Phase::Stopped).await;
    for invalid in ["terminal-a#0", "terminal-a#01", "different#1", "terminal-a#4294967295"] {
        assert!(fixture.replace(invalid).await.is_err(), "invalid predecessor {invalid}");
    }
    fixture.lifecycle.finish(&fixture.obj, Ending::Released);
    let first = fixture.reading().name;
    let (a, b) = tokio::join!(fixture.replace(&first), fixture.replace(&first));
    a.expect("first replacement");
    b.expect("idempotent retry");
    let next = fixture.reading();
    assert_eq!(next.name, "terminal-a#2");
    assert_eq!(next.reading, LifecycleReading::InProgress);
    let version = fixture.lifecycle.home.get(&next.name).expect("next").version();
    fixture.replace(&first).await.expect("already requested");
    assert_eq!(fixture.lifecycle.home.get(&next.name).expect("next").version(), version, "no extra durable write");
    assert_eq!(
        fixture.backend.using::<TerminalSession>("flotilla").get("terminal-a").await.expect("raw").status.expect("status").phase,
        Phase::Stopped
    );
    let controller = fixture.controller();
    assert!(fixture.tick(&controller, false).await.patch.is_some(), "model a failed status write after engine realization");
    fixture.tick(&controller, false).await;
    assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 1);
    assert!(fixture.runtime.probes.lock().expect("probes").iter().all(|id| id == "new-1"));
    assert!(fixture.replace(&first).await.expect_err("late predecessor").to_string().contains("no longer latest"));
    assert!(fixture.lifecycle.home.get("terminal-a#3").is_none());
    assert_eq!(fixture.reading().reading, LifecycleReading::Ready { identity: "new-1".into() });
}

// Observation-only loss can revive the same launch; only RecoverLost's explicit replacement ends it.
#[tokio::test]
async fn temporary_unobservability_revives_without_a_launch_then_absence_requests_replacement() {
    let fixture = Fixture::new(Phase::Running).await;
    fixture.lifecycle.finish(&fixture.obj, Ending::Ready("old"));
    let controller = fixture.controller();
    *fixture.runtime.live.lock().expect("live") = TerminalLiveness::Unavailable("temporarily unreachable".into());
    assert!(controller.prepare(&fixture.obj).await.err().expect("unavailable").to_string().contains("temporarily unreachable"));
    assert_eq!(fixture.reading().name, "terminal-a#1");
    *fixture.runtime.live.lock().expect("live") = TerminalLiveness::Lost("missing".into());
    fixture.tick(&controller, true).await;
    assert_eq!(fixture.reading().reading, LifecycleReading::Ready { identity: "old".into() });
    *fixture.runtime.live.lock().expect("live") = TerminalLiveness::Running;
    fixture.tick(&controller, true).await;
    assert_eq!(fixture.reading().name, "terminal-a#1");
    assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 0);
    *fixture.runtime.live.lock().expect("live") = TerminalLiveness::Lost("absent".into());
    fixture.tick(&controller, true).await;
    let lost_at = fixture
        .backend
        .using::<TerminalSession>("flotilla")
        .get("terminal-a")
        .await
        .expect("lost observation")
        .status
        .expect("status")
        .stopped_at
        .expect("loss time");
    fixture.clock.set(lost_at + chrono::Duration::seconds(6));
    let restart = fixture.tick(&controller, true).await;
    assert!(matches!(restart.patch, Some(flotilla_resources::TerminalSessionStatusPatch::MarkStarting)));
    let raw = fixture.backend.using::<TerminalSession>("flotilla").get("terminal-a").await.expect("observations").status.expect("status");
    assert_eq!(raw.phase, Phase::Starting);
    assert!(raw.session_id.is_none(), "reset old runtime identity after recording intent");
    assert_eq!(fixture.reading().name, "terminal-a#2");
    assert_eq!(fixture.reading().reading, LifecycleReading::InProgress);
    assert_eq!(
        flotilla_provisioning::engine::reads::judge::<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>(
            fixture.lifecycle.home.get("terminal-a#1").expect("first").state()
        )
        .reading(),
        LifecycleReading::Lost
    );
}

// After a restart between Attempted and GiveUp, the controller finishes that failure without provider work or retallying.
#[tokio::test]
async fn exhausted_launch_recovers_after_restart_without_copy_or_provider_work() {
    let fixture = Fixture::new(Phase::Starting).await;
    fixture.lifecycle.exhaust_before_give_up(&fixture.obj, "launch refused");
    let home = fixture.lifecycle.reopen();
    fixture.backend.install_terminal_launches(Arc::new(EngineTerminalLaunches { lifecycles: home.clone() }));
    let before = home.get("terminal-a#1").expect("durable attempt").version();
    let controller = fixture.controller_with(home.clone());
    assert!(fixture.tick(&controller, false).await.patch.is_none());
    assert_eq!(home.get("terminal-a#1").expect("gave up").version().0, before.0 + 1, "GiveUp only, no repeated Attempted");
    assert_eq!(fixture.reading().reading, LifecycleReading::GaveUp { cause: "launch refused".into() });
    fixture.tick(&controller, false).await;
    assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.reading().name, "terminal-a#1");
    assert_eq!(
        fixture.backend.using::<TerminalSession>("flotilla").get("terminal-a").await.expect("raw").status.expect("status").phase,
        Phase::Starting
    );
}

// Generate every old phase and engine ending, including empty/Unicode causes; one pure mapping contract complements the SQLite scenarios.
#[hegel::test]
fn phase_and_ending_views_do_not_reuse_old_phase(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let phase = [Phase::Starting, Phase::Running, Phase::Stopped, Phase::Lost, Phase::Failed]
        [tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
    let cause = ["", "launch failed", "失敗"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))].to_owned();
    let (reading, expected) = match tc.draw(gs::integers::<usize>().min_value(0).max_value(7)) {
        0 => (LifecycleReading::InProgress, Phase::Starting),
        1 => (LifecycleReading::Ready { identity: "different-current-session".into() }, Phase::Running),
        2 => (LifecycleReading::Released, Phase::Stopped),
        3 => (LifecycleReading::Lost, Phase::Lost),
        4 => (LifecycleReading::Failed { reason: cause }, Phase::Failed),
        5 => (LifecycleReading::GaveUp { cause }, Phase::Failed),
        6 => (LifecycleReading::Exhausted { cause }, Phase::Failed),
        _ => (LifecycleReading::Retrying { failures: 1, next: DateTime::UNIX_EPOCH, cause }, Phase::Starting),
    };
    let obj = terminal_session(phase);
    let old = obj.status.clone();
    // With no engine, selecting a view is immediate and retains observations: it must not
    // dispatch to the OS pool or change controller/watch ordering.
    use futures::FutureExt;
    let (off, key) = flotilla_store::read_terminal_session(Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads), &obj, None)
        .now_or_never()
        .expect("disabled lifecycle reads complete without a blocking task")
        .expect("off view");
    assert_eq!(off.status, old);
    assert!(key.is_none());
    let selected = flotilla_resources::terminal_launches::TerminalLaunch { name: "terminal-a#7".into(), reading }.project(&obj);
    assert_eq!(selected.status.expect("view").phase, expected);
    assert_eq!(obj.status, old, "a view does not write a status copy");
}

// A failed first attempt is GaveUp even when old observations claim Running; no copy or fresh launch follows a lost status write.
#[tokio::test]
async fn engine_failure_does_not_use_the_old_running_phase_or_restart_from_stale_status() {
    let fixture = Fixture::new(Phase::Running).await;
    fixture.runtime.fail.store(true, Ordering::SeqCst);
    let controller = fixture.controller();
    assert!(fixture.tick(&controller, false).await.patch.is_some());
    assert_eq!(fixture.reading().reading, LifecycleReading::GaveUp { cause: "launch refused".into() });
    assert!(fixture.tick(&controller, false).await.patch.is_none());
    assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.reading().name, "terminal-a#1");
    let (view, _) = flotilla_store::read_terminal_session(fixture.lifecycle.reads.clone(), &fixture.obj, None).await.expect("view");
    assert_eq!(view.status.expect("view status").phase, Phase::Failed);
    assert_eq!(fixture.obj.status.expect("stale observations").phase, Phase::Running);
}

// Local, replica and receiver-snapshot shapes share the same checked SQLite launch evidence.
#[tokio::test]
async fn terminal_reader_shapes_follow_engine_phase_identity_and_endings() {
    use flotilla_resources::{ReadResourceObject, ResourceProvenance};
    let fixture = Fixture::new(Phase::Failed).await;
    let sources = || {
        vec![
            ReadResourceObject { object: fixture.obj.clone(), provenance: ResourceProvenance::Local },
            ReadResourceObject {
                object: fixture.obj.clone(),
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("home"),
                    last_synced_at: DateTime::UNIX_EPOCH,
                },
            },
        ]
    };
    let replica_reads = Arc::new(flotilla_provisioning::engine::reads::LifecycleReadPort::new(
        flotilla_provisioning::engine::reads::StoreReads::new(fixture.lifecycle.store.clone()),
        "reader".into(),
    ));
    let check = |phase| {
        let fixture = &fixture;
        let sources = &sources;
        let replica_reads = replica_reads.clone();
        async move {
            let selected = flotilla_store::read_terminal_sources(fixture.lifecycle.reads.clone(), sources()).await.expect("views");
            assert!(selected.iter().all(|source| source.object.status.as_ref().expect("status").phase == phase));
            let replica = sources().pop().expect("replica");
            let foreign = flotilla_store::read_terminal_sources(replica_reads, vec![replica]).await.expect("foreign-origin reading");
            assert_eq!(foreign[0].object.status.as_ref().expect("foreign status").phase, phase);
            let receivers = flotilla_messaging::MessageReceiverSnapshot::load(&fixture.backend, "flotilla").await.expect("receivers");
            assert_eq!(receivers.terminals()[0].object.status.as_ref().expect("receiver status").phase, phase);
        }
    };
    check(Phase::Starting).await;
    fixture.lifecycle.finish(&fixture.obj, Ending::Ready("engine-session"));
    check(Phase::Running).await;
    let (view, key) = flotilla_store::read_terminal_session(fixture.lifecycle.reads.clone(), &fixture.obj, None).await.expect("view");
    assert_eq!(key.as_deref(), Some("terminal-a#1"));
    assert_eq!(view.status.expect("status").session_id.as_deref(), Some("engine-session"));
    fixture.lifecycle.finish(&fixture.obj, Ending::Failed("engine failure"));
    check(Phase::Failed).await;
    fixture.lifecycle.poison();
    assert!(flotilla_store::read_terminal_sources(fixture.lifecycle.reads.clone(), sources()).await.is_err());
    assert!(flotilla_messaging::MessageReceiverSnapshot::load(&fixture.backend, "flotilla").await.is_err());
}

// A failed create leaves the old launch realised; a committed request survives a later ending-write failure and the controller repairs it.
#[tokio::test]
async fn replacement_intent_and_predecessor_ending_recover_from_storage_failures() {
    for create_failure in [false, true] {
        let fixture = Fixture::new(Phase::Lost).await;
        fixture.lifecycle.finish(&fixture.obj, Ending::Ready("old"));
        let home = fixture.lifecycle.replacement_write_failures(&fixture.obj, create_failure, if create_failure { 1 } else { 2 });
        fixture.backend.install_terminal_launches(Arc::new(EngineTerminalLaunches { lifecycles: home.clone() }));
        let error = fixture.replace("terminal-a#1").await.expect_err("injected storage failure");
        assert!(error.to_string().contains("injected"));
        assert_eq!(
            flotilla_provisioning::engine::reads::judge::<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>(
                home.get("terminal-a#1").expect("predecessor").state()
            )
            .reading(),
            LifecycleReading::Ready { identity: "old".into() }
        );
        if create_failure {
            assert_eq!(fixture.reading().name, "terminal-a#1");
            fixture.replace("terminal-a#1").await.expect("retry request");
        } else {
            assert_eq!(fixture.reading().name, "terminal-a#2");
            assert_eq!(fixture.reading().reading, LifecycleReading::InProgress);
        }
        if !create_failure {
            assert!(fixture.controller_with(home.clone()).prepare(&fixture.obj).await.is_err());
            assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 0, "no provider work while predecessor ending still fails");
        }
        let home = fixture.lifecycle.reopen();
        fixture.backend.install_terminal_launches(Arc::new(EngineTerminalLaunches { lifecycles: home.clone() }));
        let controller = fixture.controller_with(home.clone());
        fixture.tick(&controller, true).await;
        fixture.tick(&controller, true).await;
        assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.reading().reading, LifecycleReading::Ready { identity: "new-1".into() });
        assert_eq!(
            flotilla_provisioning::engine::reads::judge::<flotilla_provisioning::stored::terminal_session::TerminalSessionKind>(
                home.get("terminal-a#1").expect("predecessor").state()
            )
            .reading(),
            LifecycleReading::Lost
        );
        assert!(home.get("terminal-a#3").is_none());
    }
}

// A report prepared for the old launch cannot end the successor requested while its probe was in flight.
#[tokio::test]
async fn in_flight_old_launch_report_cannot_end_the_requested_successor() {
    let fixture = Fixture::new(Phase::Running).await;
    fixture.lifecycle.finish(&fixture.obj, Ending::Ready("old"));
    *fixture.runtime.live.lock().expect("live") = TerminalLiveness::Stopped;
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *fixture.runtime.probe_gate.lock().expect("gate") = Some((entered.clone(), resume.clone()));
    let controller = Arc::new(fixture.controller());
    let probe = {
        let (controller, obj) = (controller.clone(), fixture.obj.clone());
        tokio::spawn(async move { controller.prepare(&obj).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.expect("probe entered");
    fixture.replace("terminal-a#1").await.expect("request while probing");
    resume.notify_one();
    let frame = probe.await.expect("probe task").expect("prepare");
    let outcome = controller.reconcile(&fixture.obj, &frame, fixture.clock.now());
    assert!(outcome.patch.is_none(), "a stale report has no observation write");
    assert!(outcome.actuations.is_empty());
    assert!(outcome.requeue_after.is_some(), "revisit the current launch");
    assert_eq!(fixture.reading().name, "terminal-a#2");
    assert_eq!(fixture.reading().reading, LifecycleReading::InProgress);
    assert!(fixture.lifecycle.home.get("terminal-a#3").is_none());
}

// A predecessorless restart is refused without observation changes; normal reconciliation
// claims the first launch even when a legacy observation says Stopped or Lost.
#[tokio::test]
async fn unheld_terminal_claims_first_launch_before_runtime_work() {
    for phase in [Phase::Stopped, Phase::Lost] {
        let fixture = Fixture::with_initial_launch(phase, false).await;
        let error = fixture.backend.request_terminal_replacement(&fixture.obj, None, fixture.clock.now()).await.unwrap_err();
        assert!(error.to_string().contains("captured engine launch"));
        assert!(fixture.lifecycle.home.get("terminal-a#1").is_none());
        let controller = fixture.controller();
        fixture.tick(&controller, true).await;
        assert_eq!(fixture.reading().name, "terminal-a#1");
        assert_eq!(fixture.reading().reading, LifecycleReading::Ready { identity: "new-1".into() });
        assert_eq!(fixture.runtime.starts.load(Ordering::SeqCst), 1);
        assert!(fixture.lifecycle.home.get("terminal-a#2").is_none());
    }
}

// The controller loop must record the captured predecessor before publishing the runtime reset.
// This plan-only reconciler supplies an ordinary outcome to the real actuation runner and SQLite homes.
struct RestartPlan {
    replaces: String,
}
impl Reconciler for RestartPlan {
    type Resource = TerminalSession;
    type Prepared = ();
    async fn prepare(&self, _: &ResourceObject<TerminalSession>) -> Result<(), flotilla_resources::ResourceError> {
        Ok(())
    }
    fn reconcile(&self, obj: &ResourceObject<TerminalSession>, _: &(), _: DateTime<chrono::Utc>) -> ReconcileOutcome<TerminalSession> {
        if obj.status.as_ref().is_some_and(|status| status.phase == Phase::Starting) {
            return ReconcileOutcome::new(None);
        }
        ReconcileOutcome::with_actuations(
            None,
            vec![flotilla_resources::Actuation::RestartTerminalSession {
                name: obj.metadata.name.clone(),
                replaces: Some(self.replaces.clone()),
            }],
        )
    }
    async fn run_finalizer(&self, _: &ResourceObject<TerminalSession>) -> Result<(), flotilla_resources::ResourceError> {
        Ok(())
    }
    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

#[tokio::test]
async fn controller_actuation_records_requested_successor_before_resetting_observations() {
    let fixture = Fixture::new(Phase::Stopped).await;
    fixture.lifecycle.finish(&fixture.obj, Ending::Released);
    let predecessor = fixture.reading().name;
    let sessions = fixture.backend.using::<TerminalSession>("flotilla");
    let task = tokio::spawn(
        flotilla_store::controller::ControllerLoop {
            primary: sessions.clone(),
            secondaries: vec![],
            reconciler: RestartPlan { replaces: predecessor.clone() },
            resync_interval: std::time::Duration::from_secs(3600),
            backend: fixture.backend.clone(),
        }
        .run(),
    );
    let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let raw = sessions.get("terminal-a").await.expect("raw");
            if raw.status.as_ref().is_some_and(|status| status.phase == Phase::Starting) {
                let launch = fixture.reading();
                assert_eq!(launch.name, "terminal-a#2");
                assert_eq!(launch.reading, LifecycleReading::InProgress);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    task.abort();
    let _ = task.await;
    observed.expect("controller published reset after request");
    fixture.replace(&predecessor).await.expect("retry same intent");
    assert!(fixture.lifecycle.home.get("terminal-a#3").is_none());
}
