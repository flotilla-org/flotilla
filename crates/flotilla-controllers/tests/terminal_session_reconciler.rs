use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    TerminalDeliveryFailure, TerminalDeliveryOutcome, TerminalDeliveryReadiness, TerminalObservation, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler,
};
use flotilla_resources::{
    controller::{Actuation, ControllerLoop, Reconciler},
    test_support::{
        run_transition_sequence, FixpointPredicate, LivenessEnrollment, LivenessScenario, LivenessStep, ReconcileStep, Transition,
        TransitionDriver, TransitionSequence, WorldBuilder,
    },
    Convoy, ConvoyPhase, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, HostDirectEnvironmentSpec, InputMeta,
    LifecycleAuthority, ResourceBackend, ResourceError, ResourceObject, StatusPatch, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalOccupancy, TerminalSession, TerminalSessionPhase, TerminalSessionSpec, TerminalSessionStatus,
    TerminalSessionStatusPatch, VirtualClock, ACTUATOR_HOST_REF_ANNOTATION, CONVOY_LABEL, CREDENTIAL_SCOPES_ANNOTATION,
    CREDENTIAL_SCOPES_SESSION_TAG, VESSEL_REF_LABEL,
};

mod common;
use common::{create_convoy_with_single_task, meta};

async fn create_ready_environment(backend: &ResourceBackend, name: &str) {
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let environment = environments
        .create(&meta(name), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "01HXYZ".to_string(), repo_default_dir: "/workspace".to_string() }),
            docker: None,
        })
        .await
        .expect("create environment");
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None }.apply(&mut status);
    environments.update_status(name, &environment.metadata.resource_version, &status).await.expect("mark environment ready");
}

#[tokio::test]
async fn terminal_session_failure_uses_injected_now_for_stopped_at() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let env = environments
        .create(&meta("env-a"), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: "01HXYZ".to_string(),
                repo_default_dir: "/Users/alice/dev/flotilla-repos".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("env create should succeed");
    environments
        .update_status("env-a", &env.metadata.resource_version, &{
            let mut status = EnvironmentStatus::default();
            EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None }.apply(&mut status);
            status
        })
        .await
        .expect("env ready update should succeed");

    let session = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session create should succeed");
    let reconciler = TerminalSessionReconciler::new(Arc::new(FailingTerminalRuntime), backend, "flotilla");
    let deps = reconciler.prepare(&session).await.expect("deps should load");
    let now = Utc::now();
    let outcome = reconciler.reconcile(&session, &deps, now);

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkFailed { stopped_at: Some(stopped_at), .. })
            if stopped_at == now
    ));
}

struct FailingTerminalRuntime;

#[async_trait]
impl TerminalRuntime for FailingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        Err("boom".to_string())
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn terminal_session_is_reclaimed_when_its_environment_is_gone() {
    let backend = ResourceBackend::InMemory(Default::default());
    let session = backend
        .clone()
        .using::<TerminalSession>("flotilla")
        .create(&meta("terminal-orphan").with_lifecycle_authority(LifecycleAuthority::Managed), &TerminalSessionSpec {
            env_ref: "deleted-environment".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("create orphaned terminal session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(RecordingTerminalRuntime::default()), backend, "flotilla");

    let deps = reconciler.prepare(&session).await.expect("missing environment should be lifecycle state");
    let outcome = reconciler.reconcile(&session, &deps, Utc::now());

    assert!(matches!(
        outcome.actuations.as_slice(),
        [Actuation::DeleteTerminalSession { name }, Actuation::DeleteDemand { name: demand_name }]
            if name == "terminal-orphan" && demand_name == "terminal-attention-terminal-orphan"
    ));
}

#[tokio::test]
async fn abandoned_convoy_reaps_terminal_without_calling_its_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(
        &backend,
        "flotilla",
        "abandoned-convoy",
        "work",
        "https://github.com/flotilla-org/flotilla",
        "main",
    )
    .await;
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let mut status = convoy.status.expect("convoy should have status");
    status.phase = ConvoyPhase::Abandoned;
    convoys.update_status("abandoned-convoy", &convoy.metadata.resource_version, &status).await.expect("convoy should be abandoned");

    let session = backend
        .clone()
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("terminal-abandoned-convoy-work-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "abandoned-convoy".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "missing-environment".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal should exist");
    let reconciler = TerminalSessionReconciler::new(Arc::new(RecordingTerminalRuntime::default()), backend, "flotilla");

    let deps = reconciler.prepare(&session).await.expect("abandoned owner should be handled as lifecycle state");
    let outcome = reconciler.reconcile(&session, &deps, Utc::now());

    assert!(matches!(
        outcome.actuations.as_slice(),
        [Actuation::DeleteTerminalSession { name }, Actuation::DeleteDemand { name: demand_name }]
            if name == "terminal-abandoned-convoy-work-coder"
                && demand_name == "terminal-attention-terminal-abandoned-convoy-work-coder"
    ));
}

#[tokio::test]
async fn failed_convoy_terminal_stops_without_probing_its_gone_environment_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-failed-convoy").await;
    let convoy =
        create_convoy_with_single_task(&backend, "flotilla", "failed-convoy", "work", "https://github.com/flotilla-org/flotilla", "main")
            .await;
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let mut convoy_status = convoy.status.expect("convoy status");
    convoy_status.phase = ConvoyPhase::Failed;
    convoys.update_status("failed-convoy", &convoy.metadata.resource_version, &convoy_status).await.expect("fail convoy");

    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let session = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-failed-convoy-work-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "failed-convoy".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "env-failed-convoy".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("create terminal");
    let mut running = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-failed-convoy".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut running);
    let session =
        sessions.update_status(&session.metadata.name, &session.metadata.resource_version, &running).await.expect("mark terminal running");
    let runtime = Arc::new(UnavailableRunningRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    let prepared = reconciler.prepare(&session).await.expect("terminal owner state");
    let outcome = reconciler.reconcile(&session, &prepared, Utc::now());

    assert!(matches!(outcome.patch, Some(TerminalSessionStatusPatch::MarkFailed { .. })));
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 0, "terminal owner state must short-circuit the unavailable runtime");
}

#[derive(Default)]
struct UnavailableRunningRuntime {
    probes: AtomicUsize,
    available: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl TerminalRuntime for UnavailableRunningRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("a running session must be probed, not ensured")
    }

    async fn session_is_running(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<bool, String> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        if self.available.load(Ordering::SeqCst) {
            Ok(true)
        } else {
            Err("provider registry unavailable for environment env-a".to_string())
        }
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn transient_runtime_probe_failure_holds_and_recovers_automatically() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-demo-work-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "env-a".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal should be created");
    let mut running = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-demo-work-coder".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut running);
    sessions.update_status(&created.metadata.name, &created.metadata.resource_version, &running).await.expect("terminal should be running");

    let runtime = Arc::new(UnavailableRunningRuntime::default());
    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: sessions.clone(),
            secondaries: Vec::new(),
            reconciler: TerminalSessionReconciler::new(Arc::clone(&runtime), backend.clone(), "flotilla"),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );

    for delay in [0, 60, 120, 240, 480] {
        if delay > 0 {
            tokio::time::advance(Duration::from_secs(delay)).await;
        }
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    let status = sessions
        .get("terminal-demo-work-coder")
        .await
        .expect("terminal should remain inspectable")
        .status
        .expect("terminal should retain status");
    let degraded = status.degraded.expect("retry budget should produce a degraded condition");
    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(degraded.reason, "ReconcileBackoff");
    assert_eq!(degraded.consecutive_failures, 5);
    assert!(degraded.message.contains("provider registry unavailable"));
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 5);

    runtime.available.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(15 * 60)).await;
    for _ in 0..40 {
        tokio::task::yield_now().await;
        if sessions
            .get("terminal-demo-work-coder")
            .await
            .ok()
            .and_then(|session| session.status)
            .is_some_and(|status| status.degraded.is_none())
        {
            break;
        }
    }
    let recovered =
        sessions.get("terminal-demo-work-coder").await.expect("terminal should recover").status.expect("terminal should retain status");
    assert_eq!(recovered.phase, TerminalSessionPhase::Running);
    assert_eq!(recovered.degraded, None);
    assert!(runtime.probes.load(Ordering::SeqCst) > 5, "degraded terminal must keep probing with backoff");

    let active_convoy = convoys.get("demo").await.expect("owning convoy should remain");
    assert_ne!(active_convoy.status.as_ref().map(|status| status.phase), Some(ConvoyPhase::Failed));

    let convoy = active_convoy;
    let mut convoy_status = convoy.status.expect("owning convoy should have status");
    convoy_status.phase = ConvoyPhase::Abandoned;
    convoys.update_status("demo", &convoy.metadata.resource_version, &convoy_status).await.expect("owning convoy should be abandoned");
    tokio::time::advance(Duration::from_secs(3600)).await;
    for _ in 0..40 {
        tokio::task::yield_now().await;
        if matches!(sessions.get("terminal-demo-work-coder").await, Err(ResourceError::NotFound { .. })) {
            break;
        }
    }
    assert!(
        matches!(sessions.get("terminal-demo-work-coder").await, Err(ResourceError::NotFound { .. })),
        "abandonment must wake and reap a previously degraded terminal"
    );

    loop_task.abort();
    let _ = loop_task.await;
}

#[tokio::test(start_paused = true)]
async fn foreign_actuator_runtime_failure_is_skipped_and_convoy_stays_active() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let convoy = convoys.get("demo").await.expect("convoy");
    let mut convoy_status = convoy.status.expect("convoy status");
    convoy_status.phase = ConvoyPhase::Active;
    convoys.update_status("demo", &convoy.metadata.resource_version, &convoy_status).await.expect("mark convoy active");

    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-demo-work-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string())]))
                .annotations(BTreeMap::from([(ACTUATOR_HOST_REF_ANNOTATION.to_string(), "udder".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "env-a".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal should be created");
    let mut running = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-demo-work-coder".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut running);
    sessions.update_status(&created.metadata.name, &created.metadata.resource_version, &running).await.expect("terminal should be running");

    let runtime = Arc::new(UnavailableRunningRuntime::default());
    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: sessions.clone(),
            secondaries: Vec::new(),
            reconciler: TerminalSessionReconciler::new(Arc::clone(&runtime), backend.clone(), "flotilla")
                .with_local_host_ref(flotilla_protocol::CanonicalHostId::resolved("kiwi")),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    for _ in 0..6 {
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    let terminal_status = sessions.get("terminal-demo-work-coder").await.expect("terminal remains").status.expect("terminal status");
    assert_eq!(terminal_status.phase, TerminalSessionPhase::Running);
    assert!(terminal_status.degraded.is_none());
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 0, "a non-actuator must never consult its local provider registry");
    assert_eq!(convoys.get("demo").await.expect("convoy remains").status.expect("convoy status").phase, ConvoyPhase::Active);

    loop_task.abort();
}

const GHOST_SESSION_NAME: &str = "terminal-deleted-convoy-work-coder";

struct GhostRecoveryWorld {
    backend: ResourceBackend,
    stale_session: ResourceObject<TerminalSession>,
    runtime: Arc<GhostRecoveryRuntime>,
    reconciler: TerminalSessionReconciler<GhostRecoveryRuntime>,
    durable_record_deleted: bool,
    ownerless_recovery_rejected: bool,
}

struct GhostRecoveryWorldBuilder;

#[async_trait]
impl WorldBuilder for GhostRecoveryWorldBuilder {
    type World = GhostRecoveryWorld;

    async fn build(&self, _scenario: LivenessScenario) -> Result<Self::World, String> {
        let backend = ResourceBackend::InMemory(Default::default());
        let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
        let env = environments
            .create(&meta("host-direct-feta"), &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "feta".to_string(), repo_default_dir: "/worktrees".to_string() }),
                docker: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        let mut env_status = EnvironmentStatus::default();
        EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None }.apply(&mut env_status);
        environments
            .update_status("host-direct-feta", &env.metadata.resource_version, &env_status)
            .await
            .map_err(|error| error.to_string())?;

        let stale_session = backend
            .clone()
            .using::<TerminalSession>("flotilla")
            .create(
                &InputMeta::builder()
                    .name(GHOST_SESSION_NAME.to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "deleted-convoy".to_string())]))
                    .build(),
                &TerminalSessionSpec {
                    env_ref: "host-direct-feta".to_string(),
                    role: "coder".to_string(),
                    source: flotilla_resources::TerminalSessionSource::Agent {
                        selector: flotilla_resources::Selector::for_capability("coding"),
                        brief: flotilla_resources::TerminalBrief {
                            path: ".flotilla/briefs/coder.md".to_string(),
                            content: "brief".to_string(),
                            copies: Vec::new(),
                        },
                        context: Box::new(flotilla_resources::TerminalCrewContext {
                            namespace: "flotilla".to_string(),
                            convoy: "deleted-convoy".to_string(),
                            vessel_ref: "deleted-convoy-work".to_string(),
                        }),
                        message: None,
                    },
                    cwd: "/workspace".to_string(),
                    pool: "cleat".to_string(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let runtime = Arc::new(GhostRecoveryRuntime::default());
        let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend.clone(), "flotilla");
        Ok(GhostRecoveryWorld {
            backend,
            stale_session,
            runtime,
            reconciler,
            durable_record_deleted: false,
            ownerless_recovery_rejected: false,
        })
    }
}

#[derive(Default)]
struct GhostRecoveryRuntime {
    ensure_calls: AtomicUsize,
}

#[async_trait]
impl TerminalRuntime for GhostRecoveryRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        self.ensure_calls.fetch_add(1, Ordering::SeqCst);
        Ok(TerminalRuntimeState {
            session_id: name.to_string(),
            pid: None,
            started_at: Utc::now(),
            crew: None,
            launch_command: "codex".to_string(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

struct GhostRecoveryStep;

#[async_trait]
impl ReconcileStep<GhostRecoveryWorld> for GhostRecoveryStep {
    type Patch = TerminalSessionStatusPatch;
    type Actuation = Actuation;

    async fn reconcile_step(&self, world: &mut GhostRecoveryWorld) -> Result<LivenessStep<Self::Patch, Self::Actuation>, String> {
        let deps = world.reconciler.prepare(&world.stale_session).await.map_err(|error| error.to_string())?;
        let outcome = world.reconciler.reconcile(&world.stale_session, &deps, Utc::now());
        world.ownerless_recovery_rejected = outcome.patch.is_none()
            && matches!(
                outcome.actuations.as_slice(),
                [Actuation::DeleteTerminalSession { name }, Actuation::DeleteDemand { name: demand_name }]
                    if name == GHOST_SESSION_NAME && demand_name == "terminal-attention-terminal-deleted-convoy-work-coder"
            );
        Ok(LivenessStep::new(outcome.patch, outcome.actuations))
    }

    async fn apply_patch(&self, world: &mut GhostRecoveryWorld, patch: Self::Patch) -> Result<(), String> {
        flotilla_resources::apply_status_patch(&world.backend.clone().using::<TerminalSession>("flotilla"), GHOST_SESSION_NAME, &patch)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn apply_actuation(&self, world: &mut GhostRecoveryWorld, actuation: Self::Actuation) -> Result<(), String> {
        match actuation {
            Actuation::DeleteTerminalSession { name } => {
                match world.backend.clone().using::<TerminalSession>("flotilla").delete(&name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error.to_string()),
                }
            }
            Actuation::DeleteDemand { name } => {
                match world.backend.clone().using::<flotilla_resources::Demand>("flotilla").delete(&name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error.to_string()),
                }
            }
            other => Err(format!("ghost recovery unexpectedly emitted {other:?}")),
        }
    }
}

#[async_trait]
impl TransitionDriver<GhostRecoveryWorld> for GhostRecoveryStep {
    type Field = ();
    type Value = ();
    type OriginRoot = String;

    async fn external_spec_write(&self, _world: &mut GhostRecoveryWorld, _field: &Self::Field, _value: &Self::Value) -> Result<(), String> {
        Err("external spec writes are not part of the ghost recovery property".to_string())
    }

    async fn delete(&self, world: &mut GhostRecoveryWorld) -> Result<(), String> {
        let sessions = world.backend.clone().using::<TerminalSession>("flotilla");
        sessions.delete(GHOST_SESSION_NAME).await.map_err(|error| error.to_string())?;
        world.durable_record_deleted = matches!(sessions.get(GHOST_SESSION_NAME).await, Err(ResourceError::NotFound { .. }));
        Ok(())
    }

    async fn restart_controller(&self, world: &mut GhostRecoveryWorld) -> Result<(), String> {
        world.reconciler = TerminalSessionReconciler::new(Arc::clone(&world.runtime), world.backend.clone(), "flotilla");
        Ok(())
    }

    async fn partition_store(&self, _world: &mut GhostRecoveryWorld, _origin_root: &Self::OriginRoot) -> Result<(), String> {
        Err("store partition is not part of the ghost recovery property".to_string())
    }
}

struct GhostRecoveryFixpoint;

impl FixpointPredicate<GhostRecoveryWorld> for GhostRecoveryFixpoint {
    fn at_fixpoint(&self, _world: &GhostRecoveryWorld) -> bool {
        false
    }
}

/// Regression property for #1202: a stale TerminalSession snapshot surviving
/// teardown and controller restart must not recreate the external session once
/// its owning convoy and durable record are gone.
#[tokio::test]
async fn deleted_terminal_session_is_not_resurrected_from_stale_state_after_restart() {
    let clock = Arc::new(VirtualClock::new(Utc::now()));
    let enrollment = LivenessEnrollment::new(GhostRecoveryWorldBuilder, GhostRecoveryStep, GhostRecoveryFixpoint, clock);
    let sequence: TransitionSequence<GhostRecoveryWorld, (), (), String> =
        TransitionSequence::new([Transition::Delete, Transition::RestartController, Transition::Reconcile, Transition::DeliverActuation])
            .sometimes("terminal record was absent before recovery", |world: &GhostRecoveryWorld| world.durable_record_deleted)
            .sometimes("ownerless stale recovery was rejected", |world: &GhostRecoveryWorld| world.ownerless_recovery_rejected);

    let world =
        run_transition_sequence(&enrollment, LivenessScenario::Normal, &sequence).await.expect("terminal-session ghost recovery sequence");

    assert_eq!(world.runtime.ensure_calls.load(Ordering::SeqCst), 0, "recovery resurrected an ownerless external terminal session");
    assert!(matches!(
        world.backend.clone().using::<TerminalSession>("flotilla").get(GHOST_SESSION_NAME).await,
        Err(ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn terminal_finalizer_kills_the_persisted_session_using_its_spec() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let spec = TerminalSessionSpec {
        env_ref: "host-direct-feta".to_string(),
        role: "coder".to_string(),
        source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
        cwd: "/workspace".to_string(),
        pool: "cleat".to_string(),
    };
    let created = sessions.create(&meta("terminal-convoy-work-coder"), &spec).await.expect("session create");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "terminal-convoy-work-coder".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions
        .update_status(&created.metadata.name, &created.metadata.resource_version, &status)
        .await
        .expect("session should be running");
    let demands = backend.clone().using::<flotilla_resources::Demand>("flotilla");
    demands
        .create(
            &meta("terminal-attention-terminal-convoy-work-coder").with_lifecycle_authority(LifecycleAuthority::Managed),
            &flotilla_resources::DemandSpec::for_dispatching_principal(
                flotilla_protocol::ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", "terminal-convoy-work-coder"),
                flotilla_resources::DemandKind::HumanGate,
                flotilla_protocol::PrincipalRef::implicit_for_namespace("flotilla"),
            ),
        )
        .await
        .expect("attention demand");
    let runtime = Arc::new(RecordingTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&session).await.expect("terminal finalizer should kill the session");

    assert_eq!(runtime.killed.lock().expect("killed mutex").as_slice(), &[("terminal-convoy-work-coder".to_string(), spec)]);
    assert!(matches!(demands.get("terminal-attention-terminal-convoy-work-coder").await, Err(ResourceError::NotFound { .. })));
}

#[derive(Default)]
struct RecordingTerminalRuntime {
    killed: Mutex<Vec<(String, TerminalSessionSpec)>>,
}

#[async_trait]
impl TerminalRuntime for RecordingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("terminal finalization must not ensure a new session")
    }

    async fn kill_session(&self, session_id: &str, spec: &TerminalSessionSpec) -> Result<(), String> {
        self.killed.lock().expect("killed mutex").push((session_id.to_string(), spec.clone()));
        Ok(())
    }
}

#[tokio::test]
async fn session_provisioning_passes_convoy_and_vessel_tags_to_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let env = environments
        .create(&meta("env-a"), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host-a".into(), repo_default_dir: "/repos".into() }),
            docker: None,
        })
        .await
        .expect("environment");
    let mut env_status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: None, image_ref: None, image_digest: None }.apply(&mut env_status);
    environments.update_status("env-a", &env.metadata.resource_version, &env_status).await.expect("ready environment");
    let input = InputMeta::builder()
        .name("term-a".to_string())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string()), (VESSEL_REF_LABEL.to_string(), "demo-work".to_string())]))
        .annotations(BTreeMap::from([(
            CREDENTIAL_SCOPES_ANNOTATION.to_string(),
            r#"{"github-app":["github.com-flotilla-org-flotilla"]}"#.to_string(),
        )]))
        .build();
    let session = sessions
        .create(&input, &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "watcher".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "tail -f log".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("terminal");
    let runtime = Arc::new(TagRecordingRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.prepare(&session).await.expect("provisioning dependencies");

    assert_eq!(runtime.tags.lock().expect("tags mutex").as_slice(), &[
        flotilla_resources::TerminalSessionTag::new("convoy", "demo"),
        flotilla_resources::TerminalSessionTag::new("vessel", "demo-work"),
        flotilla_resources::TerminalSessionTag::new(
            CREDENTIAL_SCOPES_SESSION_TAG,
            r#"{"github-app":["github.com-flotilla-org-flotilla"]}"#,
        ),
    ]);
}

#[derive(Default)]
struct TagRecordingRuntime {
    tags: Mutex<Vec<flotilla_resources::TerminalSessionTag>>,
}

#[async_trait]
impl TerminalRuntime for TagRecordingRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        _spec: &TerminalSessionSpec,
        tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        *self.tags.lock().expect("tags mutex") = tags.to_vec();
        Ok(TerminalRuntimeState {
            session_id: name.to_string(),
            pid: None,
            started_at: Utc::now(),
            crew: None,
            launch_command: "tail -f log".into(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_disappeared_running_session_is_observed_as_stopped() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "implement", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "brief".into(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-implement".into(),
                }),
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(MissingTerminalRuntime), backend, "flotilla");

    let deps = reconciler.prepare(&session).await.expect("observe session");
    let now = Utc::now();
    let outcome = reconciler.reconcile(&session, &deps, now);

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkStopped { stopped_at, .. }) if stopped_at == now
    ));
    assert!(matches!(
        outcome.actuations.as_slice(),
        [Actuation::DeleteDemand { name }] if name == "terminal-attention-term-a"
    ));
}

struct MissingTerminalRuntime;

#[async_trait]
impl TerminalRuntime for MissingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should be observed, not ensured")
    }

    async fn session_is_running(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<bool, String> {
        Ok(false)
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_message_queued_during_startup_is_delivered_before_attention_observation() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "review", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "reviewer".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("review"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/reviewer.md".into(),
                    content: "brief".into(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-review".into(),
                }),
                message: Some(flotilla_resources::TerminalCrewMessage {
                    id: "message-new".into(),
                    text: "Review the amended commit".into(),
                }),
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "claude".into(),
        delivered_message_id: Some("message-old".into()),
    }
    .apply(&mut status);
    flotilla_resources::TerminalSessionStatusPatch::MarkReconcileDegraded {
        message: "terminal pool temporarily unavailable".into(),
        consecutive_failures: 5,
        observed_at: Utc::now(),
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(DeliveringTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    let deps = reconciler.prepare(&session).await.expect("observe pending message");
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").as_slice(), &[(
        "cleat-session".to_string(),
        "Review the amended commit".to_string(),
        TerminalDeliveryReadiness::TurnBoundary,
    )]);
    let outcome = reconciler.reconcile(&session, &deps, Utc::now());
    assert!(matches!(
        &outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkMessageDelivered { message_id }) if message_id == "message-new"
    ));
    let mut acknowledged_status = session.status.clone().expect("status");
    outcome.patch.expect("acknowledgement patch").apply(&mut acknowledged_status);
    assert_eq!(acknowledged_status.degraded, None, "message acknowledgement should clear the recovered provider condition");
    let acknowledged =
        sessions.update_status("term-a", &session.metadata.resource_version, &acknowledged_status).await.expect("acknowledge message");

    let deps = reconciler.prepare(&acknowledged).await.expect("observe acknowledged message");
    assert!(matches!(deps, flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(_)));
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").len(), 1);
}

#[tokio::test]
async fn unconfirmed_delivery_is_named_and_not_repeated_by_reconciliation() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "review", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "reviewer".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("review"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/reviewer.md".into(),
                    content: "brief".into(),
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-review".into(),
                }),
                message: Some(flotilla_resources::TerminalCrewMessage {
                    id: "message-new".into(),
                    text: "Review the amended commit".into(),
                }),
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "claude".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(DeliveringTerminalRuntime { delivered: Mutex::default(), unconfirmed: true });
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    let pending = reconciler.reconcile(
        &session,
        &flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::MessageDeliveryPending,
        Utc::now(),
    );
    assert!(pending.patch.is_none());
    assert_eq!(pending.requeue_after, Some(Duration::from_millis(200)));

    let prepared = reconciler.prepare(&session).await.expect("attempt delivery");
    assert_eq!(runtime.delivered.lock().expect("delivered mutex")[0].2, TerminalDeliveryReadiness::Startup);
    let outcome = reconciler.reconcile(&session, &prepared, Utc::now());
    let mut flagged_status = session.status.clone().expect("status");
    outcome.patch.expect("delivery condition patch").apply(&mut flagged_status);
    assert_eq!(flagged_status.degraded.as_ref().map(|condition| condition.reason.as_str()), Some("DeliveryUnconfirmed"));
    assert_eq!(flagged_status.degraded.as_ref().and_then(|condition| condition.message_id.as_deref()), Some("message-new"));
    assert_eq!(
        flagged_status.degraded.as_ref().map(|condition| condition.message.as_str()),
        Some("agent session remained idle after submit and one retry")
    );
    let flagged = sessions.update_status("term-a", &session.metadata.resource_version, &flagged_status).await.expect("flag session");

    let prepared = reconciler.prepare(&flagged).await.expect("observe flag");
    assert!(matches!(prepared, flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::None));
    assert!(reconciler.reconcile(&flagged, &prepared, Utc::now()).patch.is_none());
    assert_eq!(runtime.delivered.lock().expect("delivered mutex").len(), 1);
}

#[tokio::test]
async fn attached_session_suppresses_input_demand_and_detach_surfaces_it_while_still_true() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(HooklessTerminalRuntime), backend, "flotilla");
    let attention =
        TerminalAttention { state: TerminalAttentionState::NeedsInput, as_of: Utc::now(), source: TerminalAttentionSource::Hook };

    let attached = reconciler.reconcile(
        &session,
        &flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
            attention: Some(attention.clone()),
            occupancy: TerminalOccupancy::Occupied,
        }),
        Utc::now(),
    );
    assert!(!attached.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateDemand { .. })));

    let detached = reconciler.reconcile(
        &session,
        &flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
            attention: Some(attention),
            occupancy: TerminalOccupancy::Vacant,
        }),
        Utc::now(),
    );
    assert!(matches!(detached.actuations.as_slice(), [Actuation::CreateDemand { spec, .. }] if spec.originating_work_ref.name == "term-a"));
}

#[tokio::test]
async fn terminal_finalizer_cleans_agent_artifacts() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "brief".into(),
                    copies: vec!["/workspace/repo-a".into()],
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-implement".into(),
                }),
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(CleanupRecordingTerminalRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    reconciler.run_finalizer(&session).await.expect("finalizer");

    assert_eq!(runtime.killed.lock().expect("killed mutex").as_slice(), &["cleat-session".to_string()]);
    assert_eq!(runtime.cleaned.lock().expect("cleaned mutex").as_slice(), &[".flotilla/briefs/coder.md".to_string()]);
}

#[derive(Default)]
struct DeliveringTerminalRuntime {
    delivered: Mutex<Vec<(String, String, TerminalDeliveryReadiness)>>,
    unconfirmed: bool,
}

#[derive(Default)]
struct CleanupRecordingTerminalRuntime {
    killed: Mutex<Vec<String>>,
    cleaned: Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalRuntime for CleanupRecordingTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("finalizer should not ensure sessions")
    }

    async fn kill_session(&self, session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        self.killed.lock().expect("killed mutex").push(session_id.to_string());
        Ok(())
    }

    async fn cleanup_session_artifacts(&self, spec: &TerminalSessionSpec) -> Result<(), String> {
        if let flotilla_resources::TerminalSessionSource::Agent { brief, .. } = &spec.source {
            self.cleaned.lock().expect("cleaned mutex").push(brief.path.clone());
        }
        Ok(())
    }
}

#[async_trait]
impl TerminalRuntime for DeliveringTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn deliver_message(
        &self,
        session_id: &str,
        _spec: &TerminalSessionSpec,
        message: &str,
        readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        self.delivered.lock().expect("delivered mutex").push((session_id.to_string(), message.to_string(), readiness));
        Ok(if self.unconfirmed {
            TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)
        } else {
            TerminalDeliveryOutcome::Confirmed
        })
    }

    async fn observe_attention(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<Option<TerminalObservation>, String> {
        Ok(Some(TerminalObservation {
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                as_of: Utc::now(),
                source: TerminalAttentionSource::Screen,
            }),
            occupancy: TerminalOccupancy::Vacant,
        }))
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn stale_hook_attention_decays_to_unobservable_without_changing_phase() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "hookless".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        session_id: "session-a".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    status.attention = Some(TerminalAttention {
        state: TerminalAttentionState::Working,
        as_of: Utc::now() - chrono::Duration::seconds(31),
        source: TerminalAttentionSource::Hook,
    });
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(HooklessTerminalRuntime), backend, "flotilla");

    let deps = reconciler.prepare(&session).await.expect("observe stale attention");
    let now = Utc::now();
    let patch = reconciler.reconcile(&session, &deps, now).patch.expect("decay patch");
    patch.apply(&mut status);

    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(status.attention.expect("attention").state, TerminalAttentionState::Unobservable);
}

struct HooklessTerminalRuntime;

#[async_trait]
impl TerminalRuntime for HooklessTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}
