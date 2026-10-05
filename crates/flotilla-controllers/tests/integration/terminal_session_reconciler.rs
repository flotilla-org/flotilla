use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{create_convoy_with_single_task, meta};
use flotilla_controllers::reconcilers::{
    TerminalDeliveryFailure, TerminalDeliveryOutcome, TerminalDeliveryReadiness, TerminalLiveness, TerminalObservation, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler,
};
use flotilla_resources::{
    controller::{Actuation, ControllerLoop, Reconciler},
    test_support::{
        run_transition_sequence, FixpointPredicate, LivenessEnrollment, LivenessScenario, LivenessStep, ReconcileStep, Transition,
        TransitionDriver, TransitionSequence, WorldBuilder,
    },
    Checkout, Convoy, ConvoyPhase, ConvoyReconciler, ConvoyTeardownRuntime, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch,
    HostDirectEnvironmentSpec, InputMeta, LifecycleAuthority, OwnerReference, Resource, ResourceBackend, ResourceError, ResourceObject,
    StatusPatch, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalOccupancy, TerminalSession,
    TerminalSessionPhase, TerminalSessionSpec, TerminalSessionStatus, TerminalSessionStatusPatch, Vessel, VesselSpec, VirtualClock,
    ACTUATOR_HOST_REF_ANNOTATION, CONVOY_LABEL, CREDENTIAL_SCOPES_ANNOTATION, CREDENTIAL_SCOPES_SESSION_TAG, VESSEL_REF_LABEL,
};
use tracing::instrument::WithSubscriber;

use crate::common;

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
    EnvironmentStatusPatch::MarkReady { configured_limits: None, docker_container_id: None, image_ref: None, image_digest: None }
        .apply(&mut status);
    environments.update_status(name, &environment.metadata.resource_version, &status).await.expect("mark environment ready");
}

#[derive(Default)]
struct DeadThenRestoredRuntime {
    deliveries: Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalRuntime for DeadThenRestoredRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        Ok(TerminalRuntimeState::builder()
            .session_id("new-generation".to_string())
            .maybe_pid(None)
            .started_at(Utc::now())
            .maybe_crew(None)
            .launch_command("cargo test".to_string())
            .maybe_delivered_message_id(None)
            .build())
    }

    async fn session_liveness(&self, session_id: &str, _spec: &TerminalSessionSpec) -> Result<TerminalLiveness, String> {
        Ok(if session_id == "old-generation" {
            TerminalLiveness::Lost("daemon generation is dead; session is recreatable".into())
        } else {
            TerminalLiveness::Running
        })
    }

    async fn deliver_message(
        &self,
        _session_id: &str,
        _spec: &TerminalSessionSpec,
        message: &str,
        _readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        self.deliveries.lock().expect("deliveries").push(message.to_string());
        Ok(TerminalDeliveryOutcome::Confirmed)
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn dead_generation_is_lost_then_recreated() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "Original brief".into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-work".into(),
                }),
                message: Some(flotilla_resources::TerminalCrewMessage {
                    id: "conflicting-1".into(),
                    text: "PR #2185 is conflicting; rebase and rerun the gates".into(),
                    sender: Default::default(),
                    delivery: flotilla_resources::CrewMessageDelivery::Queued,
                    acknowledged: Default::default(),
                    following: Vec::new(),
                }),
            },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("create session");
    let mut running = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "old-generation".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".into(),
        delivered_message_id: None,
    }
    .apply(&mut running);
    let running = sessions.update_status("term-a", &created.metadata.resource_version, &running).await.expect("running status");
    let runtime = Arc::new(DeadThenRestoredRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");
    let prepared = reconciler.prepare(&running).await.expect("probe");
    let lost_at = Utc::now() - chrono::Duration::seconds(6);
    let outcome = reconciler.reconcile(&running, &prepared, lost_at);
    let patch = outcome.patch.expect("lost patch");
    assert!(matches!(&patch, TerminalSessionStatusPatch::MarkLost { reason, .. } if reason.contains("generation is dead")));
    let mut lost = running.status.expect("running status");
    patch.apply(&mut lost);
    assert_eq!(lost.phase, TerminalSessionPhase::Lost);
    assert_eq!(lost.message.as_deref(), Some("daemon generation is dead; session is recreatable"));
    let lost = sessions.update_status("term-a", &running.metadata.resource_version, &lost).await.expect("persist lost");
    let prepared = reconciler.prepare(&lost).await.expect("recovery preparation");
    let outcome = reconciler.reconcile(&lost, &prepared, Utc::now());
    let mut starting = lost.status.expect("lost status");
    outcome.patch.expect("restart patch").apply(&mut starting);
    assert_eq!(starting.phase, TerminalSessionPhase::Starting);
    let starting = sessions.update_status("term-a", &lost.metadata.resource_version, &starting).await.expect("persist restart");
    let prepared = reconciler.prepare(&starting).await.expect("recreate");
    let outcome = reconciler.reconcile(&starting, &prepared, Utc::now());
    let mut recreated = starting.status.expect("starting status");
    outcome.patch.expect("running patch").apply(&mut recreated);
    assert_eq!(recreated.phase, TerminalSessionPhase::Running);
    assert_eq!(recreated.session_id.as_deref(), Some("new-generation"));
    let recreated = sessions.update_status("term-a", &starting.metadata.resource_version, &recreated).await.expect("persist running");
    let prepared = reconciler.prepare(&recreated).await.expect("redeliver pending turn");
    let outcome = reconciler.reconcile(&recreated, &prepared, Utc::now());
    assert!(
        matches!(outcome.patch, Some(TerminalSessionStatusPatch::MarkMessageDelivered { ref message_id }) if message_id == "conflicting-1")
    );
    assert_eq!(runtime.deliveries.lock().expect("deliveries").as_slice(), ["PR #2185 is conflicting; rebase and rerun the gates"]);
}

#[derive(Default)]
struct BrieflyMissingRuntime {
    probes: AtomicUsize,
}

#[async_trait]
impl TerminalRuntime for BrieflyMissingRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("a revived session must not be launched again")
    }

    async fn session_liveness(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<TerminalLiveness, String> {
        Ok(if self.probes.fetch_add(1, Ordering::SeqCst) == 0 {
            TerminalLiveness::Lost("session absent from list".into())
        } else {
            TerminalLiveness::Running
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn a_briefly_missing_live_session_recovers_without_a_second_launch() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("create session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "session-a".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "cargo test".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let running = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running");
    let runtime = Arc::new(BrieflyMissingRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");
    let prepared = reconciler.prepare(&running).await.expect("first probe");
    let outcome = reconciler.reconcile(&running, &prepared, Utc::now());
    outcome.patch.expect("lost patch").apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Lost);
    let lost = sessions.update_status("term-a", &running.metadata.resource_version, &status).await.expect("persist loss");
    let prepared = reconciler.prepare(&lost).await.expect("recheck");
    let outcome = reconciler.reconcile(&lost, &prepared, Utc::now());
    outcome.patch.expect("revived patch").apply(&mut status);
    assert_eq!(status.phase, TerminalSessionPhase::Running);
    assert_eq!(status.session_id.as_deref(), Some("session-a"));
    assert_eq!(status.stopped_at, None);
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 2);
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
            EnvironmentStatusPatch::MarkReady { configured_limits: None, docker_container_id: None, image_ref: None, image_digest: None }
                .apply(&mut status);
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

struct GoneEnvironmentTerminalRuntime;

#[async_trait]
impl TerminalRuntime for GoneEnvironmentTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("deleted environments cannot launch sessions")
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Err("provider registry unavailable for deleted environment".to_string())
    }

    async fn cleanup_session_artifacts(&self, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Err("agent adapter unavailable for deleted environment".to_string())
    }
}

#[tokio::test]
async fn terminal_finalizer_drains_after_its_environment_is_deleted() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-orphan".to_string())
                .owner_references(vec![vessel_owner("deleted-vessel")])
                .finalizers(vec!["flotilla.work/terminal-teardown".to_string()])
                .build(),
            &TerminalSessionSpec {
                env_ref: "deleted-environment".to_string(),
                role: "governor".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("create terminal session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "terminal-orphan".to_string(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".to_string(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    sessions.update_status("terminal-orphan", &created.metadata.resource_version, &status).await.expect("running session");
    sessions.delete("terminal-orphan").await.expect("delete terminal session");
    let deleting = sessions.get("terminal-orphan").await.expect("finalizer holds the terminating record");
    assert!(deleting.metadata.deletion_timestamp.is_some());
    let demands = backend.clone().using::<flotilla_resources::Demand>("flotilla");
    demands
        .create(
            &meta("terminal-attention-terminal-orphan").with_lifecycle_authority(LifecycleAuthority::Managed),
            &flotilla_resources::DemandSpec::for_dispatching_principal(
                flotilla_protocol::ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", "terminal-orphan"),
                flotilla_resources::DemandKind::HumanGate,
                flotilla_protocol::PrincipalRef::implicit_for_namespace("flotilla"),
            ),
        )
        .await
        .expect("attention demand");
    let reconciler = TerminalSessionReconciler::new(Arc::new(GoneEnvironmentTerminalRuntime), backend, "flotilla");

    reconciler.run_finalizer(&deleting).await.expect("missing environment means there is no terminal process or artifact to tear down");
    assert!(matches!(demands.get("terminal-attention-terminal-orphan").await, Err(ResourceError::NotFound { .. })));
}

fn vessel_owner(name: &str) -> OwnerReference {
    OwnerReference {
        api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
        kind: Vessel::API_PATHS.kind.to_string(),
        name: name.to_string(),
        controller: true,
    }
}

fn vessel_spec() -> VesselSpec {
    VesselSpec {
        convoy_ref: "convoy-governor".to_string(),
        vessel_name: "work".to_string(),
        placement_policy_ref: "policy-a".to_string(),
        adopted_checkout_refs: BTreeMap::new(),
    }
}

async fn vessel_owned_session(backend: &ResourceBackend, session_name: &str, vessel_name: &str) -> ResourceObject<TerminalSession> {
    backend
        .clone()
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name(session_name.to_string())
                .owner_references(vec![vessel_owner(vessel_name)])
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Managed),
            &TerminalSessionSpec {
                env_ref: "env-a".to_string(),
                role: "governor".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("create vessel-owned terminal session")
}

#[tokio::test]
async fn managed_terminal_session_is_reclaimed_when_its_vessel_owner_is_gone() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    let session = vessel_owned_session(&backend, "terminal-orphan", "deleted-vessel").await;
    let reconciler = TerminalSessionReconciler::new(Arc::new(RecordingTerminalRuntime::default()), backend, "flotilla");

    let prepared = reconciler.prepare(&session).await.expect("missing vessel should be lifecycle state");
    let outcome = reconciler.reconcile(&session, &prepared, Utc::now());

    assert!(matches!(
        outcome.actuations.as_slice(),
        [Actuation::DeleteTerminalSession { name }, Actuation::DeleteDemand { .. }] if name == "terminal-orphan"
    ));
}

#[tokio::test]
async fn managed_terminal_session_is_preserved_while_its_vessel_owner_exists() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(&meta("convoy-governor-work"), &vessel_spec())
        .await
        .expect("create live governor vessel");
    let session = vessel_owned_session(&backend, "terminal-convoy-governor-work-governor", "convoy-governor-work").await;
    let reconciler = TerminalSessionReconciler::new(Arc::new(FailingTerminalRuntime), backend, "flotilla");

    let prepared = reconciler.prepare(&session).await.expect("live vessel should be readable");
    let outcome = reconciler.reconcile(&session, &prepared, Utc::now());

    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteTerminalSession { .. })));
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
        configured_limits: None,
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
    assert!(matches!(outcome.actuations.as_slice(), [Actuation::DeleteDemand { .. }]));
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 0, "terminal owner state must short-circuit the unavailable runtime");
}

#[tokio::test]
async fn failed_environment_terminal_stops_without_probing_its_runtime() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-failed").await;
    let environments = backend.clone().using::<flotilla_resources::Environment>("flotilla");
    let environment = environments.get("env-failed").await.expect("environment");
    let mut environment_status = environment.status.expect("environment status");
    EnvironmentStatusPatch::MarkFailed { message: "container is permanently gone".to_string() }.apply(&mut environment_status);
    environments.update_status("env-failed", &environment.metadata.resource_version, &environment_status).await.expect("fail environment");

    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let session = sessions
        .create(&meta("terminal-failed-environment"), &TerminalSessionSpec {
            env_ref: "env-failed".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("create terminal");
    let runtime = Arc::new(UnavailableRunningRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");

    let prepared = reconciler.prepare(&session).await.expect("terminal environment state");
    let outcome = reconciler.reconcile(&session, &prepared, Utc::now());

    assert!(matches!(outcome.patch, Some(TerminalSessionStatusPatch::MarkFailed { .. })));
    assert!(matches!(outcome.actuations.as_slice(), [Actuation::DeleteDemand { .. }]));
    assert_eq!(runtime.probes.load(Ordering::SeqCst), 0, "terminal environment state must short-circuit the unavailable runtime");
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
        configured_limits: None,
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
                .owner_references(vec![vessel_owner("missing-foreign-vessel")])
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
        configured_limits: None,
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
        EnvironmentStatusPatch::MarkReady { configured_limits: None, docker_container_id: None, image_ref: None, image_digest: None }
            .apply(&mut env_status);
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
                            artifact_digest: None,
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
            configured_limits: None,
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
    create_ready_environment(&backend, "host-direct-feta").await;
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
        configured_limits: None,
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
    EnvironmentStatusPatch::MarkReady { configured_limits: None, docker_container_id: None, image_ref: None, image_digest: None }
        .apply(&mut env_status);
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
struct BriefGateRuntime {
    available: AtomicBool,
    launches: AtomicUsize,
}

#[async_trait]
impl TerminalRuntime for BriefGateRuntime {
    async fn brief_ready(&self, _spec: &TerminalSessionSpec) -> Result<bool, String> {
        Ok(self.available.load(Ordering::SeqCst))
    }

    async fn ensure_session(
        &self,
        name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(TerminalRuntimeState {
            configured_limits: None,
            session_id: name.to_string(),
            pid: None,
            started_at: Utc::now(),
            crew: None,
            launch_command: "codex".into(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn digest_backed_session_waits_for_blob_then_launches() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    create_ready_environment(&backend, "env-a").await;
    let session = backend
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name("term-brief".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "demo".to_string()),
                    (VESSEL_REF_LABEL.to_string(), "demo-work".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "env-a".into(),
                role: "coder".into(),
                source: flotilla_resources::TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: ".flotilla/briefs/coder.md".into(),
                        content: String::new(),
                        artifact_digest: Some("a".repeat(64)),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "demo".into(),
                        vessel_ref: "demo-work".into(),
                    }),
                    message: None,
                },
                cwd: "/workspace".into(),
                pool: "cleat".into(),
            },
        )
        .await
        .expect("terminal");
    let runtime = Arc::new(BriefGateRuntime::default());
    let reconciler = TerminalSessionReconciler::new(Arc::clone(&runtime), backend, "flotilla");
    let waiting = reconciler.prepare(&session).await.expect("wait for blob");
    let outcome = reconciler.reconcile(&session, &waiting, Utc::now());
    assert!(outcome.patch.is_none());
    assert_eq!(outcome.requeue_after, Some(Duration::from_secs(5)));
    assert_eq!(runtime.launches.load(Ordering::SeqCst), 0);
    runtime.available.store(true, Ordering::SeqCst);
    let available = reconciler.prepare(&session).await.expect("blob fetched");
    assert!(reconciler.reconcile(&session, &available, Utc::now()).patch.is_some());
    assert_eq!(runtime.launches.load(Ordering::SeqCst), 1);
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
            configured_limits: None,
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
                    artifact_digest: None,
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
        configured_limits: None,
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

#[tokio::test]
async fn a_fresh_turn_launched_as_the_brief_is_not_delivered_again() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let text = "[flotilla · turn: conflicting]\n\nRebase the PR";
    let session = sessions
        .create(&meta("fresh-turn"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("code"),
                brief: flotilla_resources::TerminalBrief {
                    path: "brief.md".into(),
                    content: text.into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-work".into(),
                }),
                message: Some(flotilla_resources::TerminalCrewMessage {
                    id: "fresh-turn-message".into(),
                    text: text.into(),
                    sender: flotilla_resources::CrewMessageSender::FlotillaTurn { source: "conflicting".into() },
                    delivery: flotilla_resources::CrewMessageDelivery::LaunchBrief,
                    acknowledged: Default::default(),
                    following: Vec::new(),
                }),
            },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("create fresh turn session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(MissingTerminalRuntime), backend, "flotilla");
    let prepared = flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Running(TerminalRuntimeState {
        configured_limits: None,
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".into(),
        delivered_message_id: None,
    });
    let patch = reconciler.reconcile(&session, &prepared, Utc::now()).patch.expect("running patch");
    let mut status = TerminalSessionStatus::default();
    patch.apply(&mut status);
    assert_eq!(status.delivered_message_id.as_deref(), Some("fresh-turn-message"));

    let runtime_with_older_delivery =
        flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Running(TerminalRuntimeState {
            configured_limits: None,
            session_id: "cleat-session".into(),
            pid: None,
            started_at: Utc::now(),
            crew: None,
            launch_command: "codex".into(),
            delivered_message_id: Some("older-message".into()),
        });
    let patch = reconciler.reconcile(&session, &runtime_with_older_delivery, Utc::now()).patch.expect("running patch");
    patch.apply(&mut status);
    assert_eq!(status.delivered_message_id.as_deref(), Some("fresh-turn-message"));
}

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
                    artifact_digest: None,
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
                    sender: Default::default(),
                    delivery: Default::default(),
                    acknowledged: Default::default(),
                    following: vec![flotilla_resources::TerminalCrewMessage {
                        id: "nudge-after-brief".into(),
                        text: "Check the result".into(),
                        sender: flotilla_resources::CrewMessageSender::FlotillaNudge,
                        delivery: Default::default(),
                        acknowledged: Default::default(),
                        following: Vec::new(),
                    }],
                }),
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = flotilla_resources::TerminalSessionStatus::default();
    flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
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

    // #2599 review: optional observation failure must not back off or starve
    // a pending delivery. No queued message is acknowledged until it succeeds.
    runtime.pending.store(true, Ordering::SeqCst);
    runtime.observation_failed.store(true, Ordering::SeqCst);
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-04T15:45:11Z").expect("epoch").with_timezone(&Utc);
    let logs = ReclaimLogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    for step in 0..3 {
        let prepared =
            reconciler.prepare(&session).with_subscriber(dispatch.clone()).await.expect("observation failure must preserve delivery retry");
        let outcome = reconciler.reconcile(&session, &prepared, now + chrono::Duration::milliseconds(step * 200));
        assert!(!matches!(outcome.patch, Some(TerminalSessionStatusPatch::MarkMessageDelivered { .. })));
        assert_eq!(outcome.requeue_after, Some(Duration::from_millis(200)));
        assert!(runtime.delivered.lock().expect("delivered mutex").is_empty());
    }
    let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("logs");
    assert_eq!(output.matches("terminal crew turn delivery decision").count(), 1, "{output}");
    assert!(output.contains("wait_for_boundary_or_submission_evidence"));
    runtime.pending.store(false, Ordering::SeqCst);

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

    let deps = reconciler.prepare(&acknowledged).await.expect("deliver nudge after brief");
    let outcome = reconciler.reconcile(&acknowledged, &deps, Utc::now());
    assert!(matches!(
        &outcome.patch,
        Some(flotilla_resources::TerminalSessionStatusPatch::MarkMessageDelivered { message_id }) if message_id == "nudge-after-brief"
    ));
    {
        let delivered = runtime.delivered.lock().expect("delivered mutex");
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[1].1, "Check the result");
    }
    let mut status = acknowledged.status.clone().expect("status");
    outcome.patch.expect("nudge acknowledgment").apply(&mut status);
    let acknowledged =
        sessions.update_status("term-a", &acknowledged.metadata.resource_version, &status).await.expect("nudge acknowledged");
    runtime.observation_failed.store(false, Ordering::SeqCst);
    let deps = reconciler.prepare(&acknowledged).await.expect("observe completed queue");
    assert!(matches!(deps, flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(_)));
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
                    artifact_digest: None,
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
                    sender: Default::default(),
                    delivery: Default::default(),
                    acknowledged: Default::default(),
                    following: Vec::new(),
                }),
            },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "cleat-session".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "claude".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let runtime = Arc::new(
        DeliveringTerminalRuntime::builder()
            .delivered(Mutex::default())
            .unconfirmed(true)
            .pending(AtomicBool::new(false))
            .observation_failed(AtomicBool::new(false))
            .build(),
    );
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

// #2560: output progress is persisted even when attention is coalesced;
// repeating the same output is a no-op and cannot keep a hung turn alive.
#[tokio::test]
async fn meaningful_output_progress_survives_coalesced_attention() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("output-crew"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "test".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("session");
    let start = Utc::now();
    let status = TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() };
    sessions.update_status("output-crew", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(HooklessTerminalRuntime), backend, "flotilla");
    for (digest, second, expected, changed) in
        [("first", 0, 0, true), ("second", 10, 0, false), ("second", 29, 0, false), ("second", 30, 30, true), ("second", 60, 30, false)]
    {
        let session = sessions.get("output-crew").await.expect("session");
        let observation = flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
            output_digest: Some(digest.into()),
            attention: None,
            occupancy: TerminalOccupancy::Vacant,
        });
        let result = reconciler.reconcile(&session, &observation, start + chrono::Duration::seconds(second));
        assert_eq!(result.patch.is_some(), changed);
        if let Some(patch) = result.patch {
            flotilla_resources::apply_status_patch(&sessions, "output-crew", &patch).await.expect("persist output progress");
        }
        let status = sessions.get("output-crew").await.expect("session").status.expect("status");
        assert_eq!(status.last_output_activity_at, Some(start + chrono::Duration::seconds(expected)));
    }
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
        configured_limits: None,
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
            output_digest: None,
            attention: Some(attention.clone()),
            occupancy: TerminalOccupancy::Occupied,
        }),
        Utc::now(),
    );
    assert!(!attached.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateDemand { .. })));

    let detached = reconciler.reconcile(
        &session,
        &flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
            output_digest: None,
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
    create_ready_environment(&backend, "env-a").await;
    let sessions = backend.clone().using::<flotilla_resources::TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    artifact_digest: None,
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
        configured_limits: None,
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

// Fake for the external terminal process: pending delivery and observation
// failures are independent responses from that boundary.
#[derive(Default, bon::Builder)]
struct DeliveringTerminalRuntime {
    delivered: Mutex<Vec<(String, String, TerminalDeliveryReadiness)>>,
    unconfirmed: bool,
    pending: AtomicBool,
    observation_failed: AtomicBool,
}

#[derive(Default)]
struct CleanupRecordingTerminalRuntime {
    reclaim_refused: AtomicBool,
    killed: Mutex<Vec<String>>,
    cleaned: Mutex<Vec<String>>,
}

#[async_trait]
impl TerminalRuntime for CleanupRecordingTerminalRuntime {
    // The daemon's VCS/forge verification is the external boundary.
    async fn verify_reclaim(&self, _convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        if self.reclaim_refused.load(Ordering::SeqCst) {
            Err("checkout evidence is stale".to_string())
        } else {
            Ok(())
        }
    }

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
        if self.pending.load(Ordering::SeqCst) {
            return Ok(TerminalDeliveryOutcome::Pending);
        }
        self.delivered.lock().expect("delivered mutex").push((session_id.to_string(), message.to_string(), readiness));
        Ok(if self.unconfirmed {
            TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)
        } else {
            TerminalDeliveryOutcome::Confirmed
        })
    }

    async fn observe_attention(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<Option<TerminalObservation>, String> {
        if self.observation_failed.load(Ordering::SeqCst) {
            return Err("attention observation unavailable".into());
        }
        Ok(Some(TerminalObservation {
            output_digest: None,
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
async fn stale_attention_decays_to_unobservable_without_losing_a_live_session() {
    for source in [TerminalAttentionSource::Hook, TerminalAttentionSource::Screen] {
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
            configured_limits: None,
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
            as_of: Utc::now() - TerminalAttention::FRESH_FOR - chrono::Duration::seconds(1),
            source,
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

struct AuthFailedTerminalRuntime;

#[async_trait]
impl TerminalRuntime for AuthFailedTerminalRuntime {
    async fn ensure_session(
        &self,
        _name: &str,
        _spec: &TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running sessions should not be ensured")
    }

    async fn observe_failure(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<Option<String>, String> {
        Ok(Some(
            "Codex authentication failed for the central Codex credential /var/lib/flotilla/.config/flotilla/credentials/codex-central/auth.json in environment env-a: token_expired".into(),
        ))
    }

    async fn kill_session(&self, _session_id: &str, _spec: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn fatal_runtime_observation_fails_a_running_terminal_naming_its_credential() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("term-a"), &TerminalSessionSpec {
            env_ref: "env-a".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "codex".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning {
        configured_limits: None,
        session_id: "session-a".into(),
        pid: None,
        started_at: Utc::now(),
        crew: None,
        launch_command: "codex".into(),
        delivered_message_id: None,
    }
    .apply(&mut status);
    let session = sessions.update_status("term-a", &created.metadata.resource_version, &status).await.expect("running session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(AuthFailedTerminalRuntime), backend, "flotilla");

    let prepared = reconciler.prepare(&session).await.expect("observe auth failure");
    let patch = reconciler.reconcile(&session, &prepared, Utc::now()).patch.expect("failure patch");
    patch.apply(&mut status);

    assert_eq!(status.phase, TerminalSessionPhase::Failed);
    let message = status.message.expect("failure reason");
    assert!(message.contains("codex-central/auth.json"), "the failure must name the central credential: {message}");
    assert!(message.contains("token_expired"), "the failure must carry the observed reason: {message}");
}

// A real controller loop acknowledges and compacts a long queue without
// changing delivery order or leaving delivered bodies in the stored resource.
#[tokio::test]
async fn controller_loop_prunes_acknowledged_message_payloads() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let new_message = |id: usize| flotilla_resources::TerminalCrewMessage {
        id: format!("turn-{id}"),
        text: format!("operator brief {id}"),
        sender: Default::default(),
        delivery: Default::default(),
        following: Vec::new(),
        acknowledged: Default::default(),
    };
    let mut head = new_message(0);
    for id in 1..=128 {
        head.append(new_message(id));
    }
    let created = sessions
        .create(&InputMeta::builder().name("history-session".into()).build(), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            cwd: "/workspace".into(),
            pool: "cleat".into(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: "brief.md".into(),
                    content: "original brief".into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-work".into(),
                }),
                message: Some(head),
            },
        })
        .await
        .expect("session");
    sessions
        .update_status(&created.metadata.name, &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("history-session".into()),
            delivered_message_id: Some("turn-0".into()),
            ..Default::default()
        })
        .await
        .expect("running");
    let runtime = Arc::new(DeliveringTerminalRuntime::default());
    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: sessions.clone(),
            secondaries: Vec::new(),
            reconciler: TerminalSessionReconciler::new(runtime.clone(), backend.clone(), "flotilla"),
            resync_interval: Duration::from_secs(3600),
            backend,
        }
        .run(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = sessions.get("history-session").await.expect("stored session");
            if let flotilla_resources::TerminalSessionSource::Agent { message: Some(head), .. } = &current.spec.source {
                if head.acknowledged.contains("turn-128") {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller drains queue");
    loop_task.abort();
    let stored = sessions.get("history-session").await.expect("stored session");
    let flotilla_resources::TerminalSessionSource::Agent { message: Some(mut head), brief, .. } = stored.spec.source else {
        panic!("agent")
    };
    assert!(head.text.is_empty());
    assert!(head.following.is_empty());
    assert_eq!(brief.content, "original brief");
    assert_eq!(head.acknowledged.len(), 129);
    head.append(new_message(42));
    assert!(head.following.is_empty(), "replaying a pruned turn cannot redeliver it");
    let delivered = runtime.delivered.lock().expect("delivery log");
    assert_eq!(
        delivered.iter().map(|(_, text, _)| text.clone()).collect::<Vec<_>>(),
        (1..=128).map(|id| format!("operator brief {id}")).collect::<Vec<_>>()
    );
}

struct ExitedAgentRuntime(i32);

#[async_trait]
impl TerminalRuntime for ExitedAgentRuntime {
    async fn ensure_session(
        &self,
        _: &str,
        _: &TerminalSessionSpec,
        _: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        panic!("running shell must not provision a replacement before exit is observed")
    }
    async fn session_liveness(&self, _: &str, _: &TerminalSessionSpec) -> Result<TerminalLiveness, String> {
        assert_ne!(self.0, 2, "positive usage exit must win before liveness or relaunch");
        Ok(TerminalLiveness::Running)
    }
    // Inject the process boundary: the parent shell has observed the child exit.
    async fn agent_exit_code(&self, _: &TerminalSessionSpec, _: &flotilla_resources::CrewSessionStatus) -> Result<Option<i32>, String> {
        Ok(Some(self.0))
    }
    // Use the real adapter contract behind the injected process boundary.
    async fn agent_exit_failure(&self, _: &str, _: &TerminalSessionSpec, code: i32) -> Result<Option<String>, String> {
        use flotilla_core::{
            agent_adapter::AgentAdapterRegistry,
            providers::{
                discovery::{EnvironmentAssertion, EnvironmentBag},
                ProcessCommandRunner,
            },
        };
        let registry = AgentAdapterRegistry::discover(
            &EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/tools/codex")),
            Arc::new(ProcessCommandRunner),
        );
        Ok(registry.get("codex").expect("codex").classify_exit_failure(code, "error: unexpected argument '--no-daemon' found"))
    }
    async fn kill_session(&self, _: &str, _: &TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

// An agent's exit differs from a live persistent shell. Preserve the checkout
// and record the actual exit so unfinished crew work can become Interrupted.
#[tokio::test]
async fn agent_exit_is_observed_even_while_its_terminal_shell_is_running() {
    for (code, phase) in [
        (0, TerminalSessionPhase::Running),
        (2, TerminalSessionPhase::Running),
        (2, TerminalSessionPhase::Lost),
        (42, TerminalSessionPhase::Running),
        (130, TerminalSessionPhase::Running),
        (137, TerminalSessionPhase::Running),
    ] {
        let backend = ResourceBackend::InMemory(Default::default());
        create_ready_environment(&backend, "env-a").await;
        create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
        let sessions = backend.clone().using::<TerminalSession>("flotilla");
        let created = sessions
            .create(&meta("exited-agent"), &TerminalSessionSpec {
                env_ref: "env-a".into(),
                role: "coder".into(),
                cwd: "/workspace".into(),
                pool: "cleat".into(),
                source: flotilla_resources::TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".into(),
                        content: "original brief".into(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "demo".into(),
                        vessel_ref: "demo-work".into(),
                    }),
                    message: None,
                },
            })
            .await
            .expect("session");
        let running = sessions
            .update_status(&created.metadata.name, &created.metadata.resource_version, &TerminalSessionStatus {
                phase,
                session_id: Some("live-shell".into()),
                crew: Some(
                    flotilla_resources::CrewSessionStatus::builder()
                        .id("launch-id".into())
                        .adapter("codex".into())
                        .stance("trusted".into())
                        .build(),
                ),
                ..Default::default()
            })
            .await
            .expect("running shell");
        let reconciler = TerminalSessionReconciler::new(Arc::new(ExitedAgentRuntime(code)), backend, "flotilla");
        let prepared = reconciler.prepare(&running).await.expect("observe process");
        let outcome = reconciler.reconcile(&running, &prepared, Utc::now());
        let mut status = running.status.clone().expect("status");
        outcome.patch.expect("exit patch").apply(&mut status);
        if code == 2 {
            // Issue #2694: an immediate usage error is terminal failure,
            // never interruption followed by automatic relaunch.
            assert_eq!(status.phase, TerminalSessionPhase::Failed);
            assert!(status.message.as_ref().expect("usage error").contains("unexpected argument '--no-daemon'"));
            let mut failed = running.clone();
            failed.status = Some(status.clone());
            let next = reconciler.prepare(&failed).await.expect("failed launch stays terminal");
            assert!(reconciler.reconcile(&failed, &next, Utc::now()).patch.is_none());
        } else {
            assert_eq!(status.phase, TerminalSessionPhase::Stopped);
            assert_eq!(status.inner_exit_code, Some(code));
            assert!(status.message.expect("recovery guidance").contains("resume"));
        }
    }
}

// An unavailable provider is a typed observation, never proof that an elapsed
// Lost grace permits another launch. A successful probe can still revive it.
#[tokio::test]
async fn typed_unavailability_preserves_lost_session_until_provider_recovers() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let spec = TerminalSessionSpec {
        env_ref: "env-a".into(),
        role: "shell".into(),
        source: flotilla_resources::TerminalSessionSource::Tool { command: "sh".into() },
        cwd: "/workspace".into(),
        pool: "cleat".into(),
    };
    let created = sessions.create(&meta("lost-unavailable"), &spec).await.expect("terminal");
    let lost = sessions
        .update_status("lost-unavailable", &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Lost,
            session_id: Some("existing-session".into()),
            stopped_at: Some(Utc::now() - chrono::Duration::days(100)),
            ..Default::default()
        })
        .await
        .expect("lost terminal");
    let runtime = Arc::new(UnavailableRunningRuntime::default());
    assert!(matches!(runtime.session_liveness("existing-session", &spec).await.expect("typed outcome"), TerminalLiveness::Unavailable(_)));
    let reconciler = TerminalSessionReconciler::new(runtime.clone(), backend, "flotilla");
    assert!(reconciler.prepare(&lost).await.is_err(), "outage cannot authorize recovery based only on elapsed time");
    runtime.available.store(true, Ordering::SeqCst);
    let prepared = reconciler.prepare(&lost).await.expect("provider recovers");
    assert!(matches!(reconciler.reconcile(&lost, &prepared, Utc::now()).patch, Some(TerminalSessionStatusPatch::MarkRevived)));
}

// The fake is the terminal launch/kill boundary. Receipt writes, reads and
// removals use the real shell through the injected environment runner.
#[derive(Default)]
struct ReceiptLifecycleRuntime {
    fail_launch: AtomicBool,
    fail_kill: AtomicBool,
    fail_cleanup: AtomicBool,
    killed: AtomicBool,
}

#[async_trait]
impl TerminalRuntime for ReceiptLifecycleRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        spec: &TerminalSessionSpec,
        _: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        if self.fail_launch.load(Ordering::SeqCst) {
            return Err("launch failed".into());
        }
        use flotilla_core::providers::{ChannelLabel, CommandRunner, ProcessCommandRunner};
        ProcessCommandRunner
            .run(
                "sh",
                &["-c", &flotilla_core::agent_process::monitored_command("exit 0", "replacement")],
                std::path::Path::new(&spec.cwd),
                &ChannelLabel::Default,
            )
            .await?;
        Ok(TerminalRuntimeState::builder()
            .session_id(name.to_string())
            .maybe_pid(None)
            .started_at(Utc::now())
            .crew(
                flotilla_resources::CrewSessionStatus::builder()
                    .id("replacement".into())
                    .adapter("codex".into())
                    .stance("trusted".into())
                    .build(),
            )
            .launch_command("exit 0".into())
            .maybe_delivered_message_id(None)
            .build())
    }
    async fn agent_exit_code(
        &self,
        spec: &TerminalSessionSpec,
        crew: &flotilla_resources::CrewSessionStatus,
    ) -> Result<Option<i32>, String> {
        use flotilla_core::{
            agent_process::{exit_receipt, ExitReceiptObserver},
            providers::ProcessCommandRunner,
        };
        ExitReceiptObserver::default()
            .observe(&spec.env_ref, &ProcessCommandRunner, std::path::Path::new(&spec.cwd).join(exit_receipt(&crew.id)), || async {
                Ok(Vec::new())
            })
            .await
    }
    async fn remove_exit_receipt(&self, spec: &TerminalSessionSpec, launch_id: &str) -> Result<(), String> {
        if self.fail_cleanup.swap(false, Ordering::SeqCst) {
            return Err("cleanup transport unavailable".into());
        }
        flotilla_core::agent_process::remove_exit_receipt(
            &flotilla_core::providers::ProcessCommandRunner,
            std::path::Path::new(&spec.cwd),
            launch_id,
        )
        .await
    }
    async fn kill_session(&self, _: &str, _: &TerminalSessionSpec) -> Result<(), String> {
        if self.fail_kill.load(Ordering::SeqCst) {
            return Err("terminal kill unavailable".into());
        }
        self.killed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

// Resume retains the previous receipt through failed launches. A persisted
// replacement permits retirement, including after restart or cleanup failure;
// teardown removes its final receipt without touching another role's launch.
#[tokio::test]
async fn receipt_lifecycle_survives_failed_relaunch_cleanup_outage_and_restart() {
    use flotilla_core::{
        agent_process::exit_receipt,
        providers::{ChannelLabel, CommandRunner, ProcessCommandRunner},
    };
    let cwd = tempfile::tempdir().expect("shared checkout");
    for launch in ["old", "other-role"] {
        ProcessCommandRunner
            .run("sh", &["-c", &flotilla_core::agent_process::monitored_command("exit 42", launch)], cwd.path(), &ChannelLabel::Default)
            .await
            .expect("exit receipt");
    }
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("receipt-session"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            cwd: cwd.path().display().to_string(),
            pool: "cleat".into(),
            source: flotilla_resources::TerminalSessionSource::Agent {
                selector: flotilla_resources::Selector::for_capability("coding"),
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".into(),
                    content: "work".into(),
                    artifact_digest: None,
                    copies: Vec::new(),
                },
                context: Box::new(flotilla_resources::TerminalCrewContext {
                    namespace: "flotilla".into(),
                    convoy: "demo".into(),
                    vessel_ref: "demo-work".into(),
                }),
                message: None,
            },
        })
        .await
        .expect("session");
    let mut status = TerminalSessionStatus {
        phase: TerminalSessionPhase::Stopped,
        crew: Some(
            flotilla_resources::CrewSessionStatus::builder().id("old".into()).adapter("codex".into()).stance("trusted".into()).build(),
        ),
        ..Default::default()
    };
    TerminalSessionStatusPatch::MarkStarting.apply(&mut status);
    assert!(status.retired_launches.contains("old"));
    // Exercise the stored shape rather than keeping pending cleanup in memory.
    let status = serde_json::from_str(&serde_json::to_string(&status).expect("serialize")).expect("restart decode");
    let starting = sessions.update_status("receipt-session", &created.metadata.resource_version, &status).await.expect("resume");
    let runtime = Arc::new(ReceiptLifecycleRuntime::default());
    runtime.fail_launch.store(true, Ordering::SeqCst);
    let reconciler = TerminalSessionReconciler::new(runtime.clone(), backend.clone(), "flotilla");
    let prepared = reconciler.prepare(&starting).await.expect("failed launch preparation");
    let failed = reconciler.reconcile(&starting, &prepared, Utc::now());
    let mut failed_status = status.clone();
    failed.patch.expect("launch failure").apply(&mut failed_status);
    assert_eq!(failed_status.phase, TerminalSessionPhase::Failed);
    assert!(cwd.path().join(exit_receipt("old")).exists());
    reconciler.run_finalizer(&ResourceObject { status: Some(failed_status), ..starting.clone() }).await.expect("failed launch teardown");
    assert!(!cwd.path().join(exit_receipt("old")).exists());
    assert!(cwd.path().join(exit_receipt("other-role")).exists());
    // Re-create the old receipt to model retry of the unchanged Starting record.
    std::fs::write(cwd.path().join(exit_receipt("old")), "42\n").expect("old receipt");
    runtime.fail_launch.store(false, Ordering::SeqCst);
    let prepared = reconciler.prepare(&starting).await.expect("replacement launched");
    let mut running_status = status;
    reconciler.reconcile(&starting, &prepared, Utc::now()).patch.expect("running patch").apply(&mut running_status);
    assert!(cwd.path().join(exit_receipt("old")).exists(), "retain until replacement success is durable");
    let running = sessions
        .update_status("receipt-session", &starting.metadata.resource_version, &running_status)
        .await
        .expect("persist successful replacement");
    let restarted_runtime = Arc::new(ReceiptLifecycleRuntime::default());
    restarted_runtime.fail_cleanup.store(true, Ordering::SeqCst);
    let restarted = TerminalSessionReconciler::new(restarted_runtime.clone(), backend, "flotilla");
    let prepared = restarted.prepare(&running).await.expect("cleanup outage does not block positive exit observation");
    let outcome = restarted.reconcile(&running, &prepared, Utc::now());
    assert_eq!(outcome.requeue_after, Some(Duration::from_secs(1)), "retain a cleanup retry after process exit");
    outcome.patch.expect("replacement exit observed").apply(&mut running_status);
    assert_eq!(running_status.phase, TerminalSessionPhase::Stopped);
    assert_eq!(running_status.inner_exit_code, Some(0));
    assert!(running_status.retired_launches.contains("old"));
    assert!(cwd.path().join(exit_receipt("old")).exists());
    let stopped = ResourceObject { status: Some(running_status.clone()), ..running.clone() };
    let prepared = restarted.prepare(&stopped).await.expect("cleanup retries even after replacement exits");
    restarted.reconcile(&stopped, &prepared, Utc::now()).patch.expect("cleanup confirmed").apply(&mut running_status);
    assert_eq!(running_status.phase, TerminalSessionPhase::Stopped);
    assert!(running_status.retired_launches.is_empty());
    assert!(!cwd.path().join(exit_receipt("old")).exists());
    assert!(cwd.path().join(exit_receipt("replacement")).exists());
    assert!(cwd.path().join(exit_receipt("other-role")).exists());
    let finalizing = ResourceObject { status: Some(running_status), ..running };
    restarted_runtime.fail_kill.store(true, Ordering::SeqCst);
    assert!(restarted.run_finalizer(&finalizing).await.is_err());
    assert!(cwd.path().join(exit_receipt("replacement")).exists(), "do not remove receipts before kill succeeds");
    restarted_runtime.fail_kill.store(false, Ordering::SeqCst);
    restarted_runtime.fail_cleanup.store(true, Ordering::SeqCst);
    assert!(restarted.run_finalizer(&finalizing).await.is_err());
    assert!(cwd.path().join(exit_receipt("replacement")).exists(), "failed cleanup keeps the finalization obligation");
    restarted.run_finalizer(&finalizing).await.expect("teardown retry");
    assert!(restarted_runtime.killed.load(Ordering::SeqCst));
    assert!(!cwd.path().join(exit_receipt("replacement")).exists());
    assert!(cwd.path().join(exit_receipt("other-role")).exists());
}

#[async_trait]
impl ConvoyTeardownRuntime for CleanupRecordingTerminalRuntime {
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>, _checkouts: &[ResourceObject<Checkout>]) -> Result<(), String> {
        TerminalRuntime::verify_reclaim(self, convoy).await
    }
}

#[derive(Clone, Default)]
struct ReclaimLogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for ReclaimLogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Retained terminal convoy orphans must be reclaimed at startup and resync
// without resource writes; refused and unmanaged sessions must be preserved.
// Exhaustive matrix covers local/replicated owners, all terminal phases plus
// Landing, immediate/recovered gates, and all lifecycle authorities.
#[rstest::rstest]
#[tokio::test(start_paused = true)]
async fn retained_terminal_convoy_orphans_follow_reclaim_matrix(
    #[values(ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Cancelled, ConvoyPhase::Abandoned, ConvoyPhase::Landing)]
    phase: ConvoyPhase,
    #[values(false, true)] replicated: bool,
    #[values(false, true)] initially_refused: bool,
    #[values(LifecycleAuthority::Managed, LifecycleAuthority::Adopted, LifecycleAuthority::Observed)] authority_kind: LifecycleAuthority,
) {
    let backend = ResourceBackend::InMemory(Default::default());
    let authority = if replicated { ResourceBackend::InMemory(Default::default()) } else { backend.clone() };
    create_ready_environment(&backend, "env-a").await;
    create_convoy_with_single_task(&authority, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let convoys = authority.using::<Convoy>("flotilla");
    let convoy = convoys.get("demo").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    status.phase = phase;
    convoys.update_status("demo", &convoy.metadata.resource_version, &status).await.expect("phase");
    if replicated {
        backend
            .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("coordinator"), "flotilla")
            .replace(&convoys.list().await.expect("convoys"), Utc::now())
            .await
            .expect("replicate convoy");
    }
    let sessions = backend.using::<TerminalSession>("flotilla");
    let created = sessions
        .create(
            &InputMeta::builder()
                .name("old-orphan".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string())]))
                .annotations(if replicated {
                    BTreeMap::from([(flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "coordinator".to_string())])
                } else {
                    BTreeMap::new()
                })
                .finalizers(vec!["flotilla.work/terminal-teardown".to_string()])
                .build()
                .with_lifecycle_authority(authority_kind),
            &TerminalSessionSpec {
                env_ref: "env-a".into(),
                role: "coder".into(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".into() },
                cwd: "/workspace".into(),
                pool: "cleat".into(),
            },
        )
        .await
        .expect("session");
    sessions
        .update_status("old-orphan", &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("old-process".into()),
            ..Default::default()
        })
        .await
        .expect("running");
    let runtime = Arc::new(CleanupRecordingTerminalRuntime::default());
    runtime.reclaim_refused.store(initially_refused, Ordering::SeqCst);
    let logs = ReclaimLogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
    let task = tokio::spawn(
        ControllerLoop {
            primary: sessions.clone(),
            secondaries: Vec::new(),
            reconciler: TerminalSessionReconciler::new(runtime.clone(), backend.clone(), "flotilla")
                .with_federated_convoys(&backend, "flotilla"),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run()
        .with_subscriber(subscriber),
    );
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let managed = authority_kind == LifecycleAuthority::Managed;
    let startup_reclaimed = managed && phase.is_terminal() && (!initially_refused || phase == ConvoyPhase::Abandoned);
    assert_eq!(
        matches!(sessions.get("old-orphan").await, Err(ResourceError::NotFound { .. })),
        startup_reclaimed,
        "startup must reap only eligible managed terminal sessions without a change event"
    );
    if managed && initially_refused && phase.is_terminal() && phase != ConvoyPhase::Abandoned {
        let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
        assert!(
            output.contains("convoy=\"demo\"")
                && output.contains("session=old-orphan")
                && output.contains("gate_outcome=\"refused\"")
                && output.contains("session_disposition=\"retain\"")
                && output.contains("reason=\"checkout evidence is stale\""),
            "every refusal must name the convoy, session, gate outcome and reason: {output}"
        );
    }
    // The verification boundary recovers without a resource write. Only the
    // periodic resync can discover that the retained convoy is now reclaimable.
    runtime.reclaim_refused.store(false, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    task.abort();
    let reclaimed = managed && phase.is_terminal();
    assert_eq!(
        matches!(sessions.get("old-orphan").await, Err(ResourceError::NotFound { .. })),
        reclaimed,
        "periodic resync must retry reclaim without a change event"
    );
    if reclaimed {
        let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
        assert!(
            output.contains("convoy=\"demo\"")
                && output.contains("session=old-orphan")
                && output.contains(if phase == ConvoyPhase::Abandoned {
                    "gate_outcome=\"not_required\""
                } else {
                    "gate_outcome=\"allowed\""
                })
                && output.contains("session_disposition=\"request_deletion\"")
                && output.contains("reason="),
            "successful reclaim must be logged: {output}"
        );
    }
    assert_eq!(*runtime.killed.lock().expect("killed"), if reclaimed { vec!["old-process"] } else { vec![] });
    assert_eq!(convoys.get("demo").await.expect("retained convoy").status.expect("status").phase, phase);
}

// Gate approval is independent of session eligibility. Logs must distinguish
// refused gates, unmanaged sessions, and deletion already in flight.
#[rstest::rstest]
#[tokio::test]
async fn convoy_reclaim_logs_distinguish_gate_from_session_disposition(
    #[values(LifecycleAuthority::Managed, LifecycleAuthority::Adopted, LifecycleAuthority::Observed)] authority: LifecycleAuthority,
    #[values(false, true)] deleting: bool,
    #[values(false, true)] refused: bool,
) {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, "flotilla", "demo", "work", "https://github.com/flotilla-org/flotilla", "main").await;
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("demo").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    status.phase = ConvoyPhase::Landed;
    let convoy = convoys.update_status("demo", &convoy.metadata.resource_version, &status).await.expect("landed");
    let sessions = backend.using::<TerminalSession>("flotilla");
    sessions
        .create(
            &InputMeta::builder()
                .name("old-orphan".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "demo".to_string())]))
                .finalizers(vec!["test-teardown".to_string()])
                .build()
                .with_lifecycle_authority(authority),
            &TerminalSessionSpec {
                env_ref: "env-a".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("session");
    if deleting {
        sessions.delete("old-orphan").await.expect("request deletion");
    }
    let runtime = Arc::new(CleanupRecordingTerminalRuntime::default());
    runtime.reclaim_refused.store(refused, Ordering::SeqCst);
    let reconciler = ConvoyReconciler::new(backend.definitions::<flotilla_resources::WorkflowTemplate>("flotilla"))
        .with_terminal_sessions(sessions)
        .with_teardown_runtime(runtime);
    let logs = ReclaimLogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
    let prepared = reconciler.prepare(&convoy).with_subscriber(subscriber).await.expect("prepare");
    let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
    let disposition = if deleting {
        "already_deleting"
    } else if authority != LifecycleAuthority::Managed {
        "unmanaged"
    } else if refused {
        "retain"
    } else {
        "request_deletion"
    };
    assert!(output.contains(&format!("session_disposition=\"{disposition}\"")), "{output}");
    let gate = if refused { "refused" } else { "allowed" };
    assert!(output.contains(&format!("gate_outcome=\"{gate}\"")), "{output}");
    assert_eq!(
        reconciler
            .reconcile(&convoy, &prepared, Utc::now())
            .actuations
            .iter()
            .any(|actuation| matches!(actuation, Actuation::DeleteTerminalSession { name } if name == "old-orphan")),
        disposition == "request_deletion"
    );
}

// Owner absence sanctions deletion without running the convoy gate. An
// independent session must not acquire a fabricated convoy name in the log.
#[tokio::test]
async fn independent_owner_absence_log_does_not_claim_gate_approval() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.using::<TerminalSession>("flotilla");
    let session = sessions
        .create(&meta("independent"), &TerminalSessionSpec {
            env_ref: "missing-env".to_string(),
            role: "shell".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "bash".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
        .await
        .expect("session");
    let reconciler = TerminalSessionReconciler::new(Arc::new(CleanupRecordingTerminalRuntime::default()), backend, "flotilla");
    let logs = ReclaimLogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_writer(move || writer.clone()).finish();
    let prepared = reconciler.prepare(&session).with_subscriber(subscriber).await.expect("prepare");
    let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
    assert!(output.contains("independent=true") && !output.contains("convoy=") && !output.contains("<independent>"), "{output}");
    assert!(output.contains("gate_outcome=\"not_required\"") && output.contains("reason=\"owning environment is absent\""), "{output}");
    assert!(reconciler
        .reconcile(&session, &prepared, Utc::now())
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteTerminalSession { name } if name == "independent")));
}

// #2654: unchanged attention decisions retain structured fields but emit once;
// changing the incoming state emits a new decision even with the same resource key.
#[tokio::test]
async fn unchanged_attention_decisions_log_once() {
    let backend = ResourceBackend::InMemory(Default::default());
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let created = sessions
        .create(&meta("decision-crew"), &TerminalSessionSpec {
            env_ref: "env-a".into(),
            role: "coder".into(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "test".into() },
            cwd: "/workspace".into(),
            pool: "cleat".into(),
        })
        .await
        .expect("attention tracing scenario");
    let now = Utc::now();
    let session = sessions
        .update_status("decision-crew", &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            attention: Some(TerminalAttention { state: TerminalAttentionState::Idle, source: TerminalAttentionSource::Hook, as_of: now }),
            ..Default::default()
        })
        .await
        .expect("attention tracing scenario");
    let reconciler = TerminalSessionReconciler::new(Arc::new(HooklessTerminalRuntime), backend, "flotilla");
    let logs = ReclaimLogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    async {
        for step in 1..=10 {
            let prepared = flotilla_controllers::reconcilers::terminal_session::TerminalPrepared::Attention(TerminalObservation {
                output_digest: None,
                occupancy: TerminalOccupancy::Vacant,
                attention: Some(TerminalAttention {
                    state: if step == 10 { TerminalAttentionState::Working } else { TerminalAttentionState::Idle },
                    source: TerminalAttentionSource::Screen,
                    as_of: now + chrono::Duration::seconds(step),
                }),
            });
            reconciler.reconcile(&session, &prepared, now + chrono::Duration::seconds(step));
        }
    }
    .with_subscriber(subscriber)
    .await;
    let output = String::from_utf8(logs.0.lock().expect("attention tracing scenario").clone()).expect("attention tracing scenario");
    assert_eq!(output.matches("terminal attention decision").count(), 2, "{output}");
    assert!(output.contains("skip_precedence_or_debounce"));
    assert!(output.contains("accept_observation"));
    assert!(output.contains("attention_source=Some(Screen)"));
}
