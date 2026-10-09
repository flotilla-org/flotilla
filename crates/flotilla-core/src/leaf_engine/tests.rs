use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{DaemonEvent, Leaf, LeafFire, NodeId, WaitSubscriptionRequest};
use flotilla_protocol::{LeafAddress, LeafOperator};
use flotilla_resources::StatusPatch;
use flotilla_resources::{
    controller::ControllerLoop, BoundChangeRequest, ChangeRequestObservation, ChangeRequestState, CheckoutIntegrationStatus, CheckoutPhase,
    CheckoutSpec, CheckoutStatus, ConditionValue, ControllerRetry, ConvoyPhase, ConvoyReconciler, ConvoyRepositorySpec, ConvoySpec,
    ConvoyStatus, CrewWorkPhase, CrewWorkState, ExitDeclaration, InMemoryBackend, InputMeta, IntegrationCondition, LifecycleAuthority,
    ObservedCheckoutSpec, PlacementStatus, RepositoryKey, SqliteBackend, WorkPhase, WorkState, WorkflowSnapshot, WorkflowTemplate,
    CONVOY_LABEL,
};
use flotilla_resources::{
    external_patches, Artifact, ChangeRequest, Checkout, Convoy, ConvoyAttention, ConvoyEnsure, CrewCompletionRefusal,
    CrewCompletionRefusalCause, HoldAct, Issue, LeafMaker, NudgeObligation, ObservedChecks, Project, ResourceBackend, ResourceError,
    RetryCeiling, StallEvidenceSource, StallRung, SupervisionTarget, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    TerminalSession, TerminalSessionPhase, TerminalSessionSource, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRule,
    TurnDeliveryRung, Usage, Vessel, WatchEvent, WatchStart, ROLE_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::sync::broadcast;

use super::sources::*;
use super::stalls::*;
use super::subscriptions::*;
use super::wake::*;
use super::*;
use crate::providers::{
    replay::{test_runner, Masks, Session},
    testing::fixture_path,
};
use crate::{
    change_request_observer::{ChangeRequestRef, ChangeRequestRefresher},
    issue_observer::{IssueRefreshCadence, IssueRefresher},
    providers::change_request::ObservationError,
};
use crate::{
    event_sink::broadcast_test_sink,
    providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource},
};

#[derive(Clone)]
struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("tracing scenario").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn captured_subscriber(logs: Arc<std::sync::Mutex<Vec<u8>>>, level: tracing::Level) -> impl tracing::Subscriber + Send + Sync {
    let writer = Writer(logs);
    tracing_subscriber::fmt().without_time().with_ansi(false).with_max_level(level).with_writer(move || writer.clone()).finish()
}

struct UnavailableChangeRequests;

#[async_trait]
impl crate::change_request_observer::ChangeRequestObservationSource for UnavailableChangeRequests {
    async fn observe(
        &self,
        _subject: &crate::change_request_observer::ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        Err("unavailable in non-CR leaf contract".into())
    }
}

struct CountingChangeRequests {
    calls: Arc<AtomicUsize>,
}

struct ControlledChangeRequests {
    merged: AtomicBool,
}

#[derive(Default)]
struct RecordingTurnDelivery {
    requests: std::sync::Mutex<Vec<CrewTurnIntent>>,
    holds: AtomicUsize,
    // Boundary fake: turn delivery may fail while the agent session reconnects.
    unavailable: AtomicBool,
}

#[async_trait]
impl TurnDeliveryActuator for RecordingTurnDelivery {
    async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err("supervisor reconnecting".into());
        }
        let mut requests = self.requests.lock().expect("record turn-delivery request");
        let rung = if requests.is_empty() { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
        requests.push(request.clone());
        Ok(CrewTurnAdmission {
            new_turn: true,
            rung,
            message: flotilla_protocol::ResourceRef::new(
                "flotilla.work/v1",
                "Message",
                &request.namespace,
                format!("fake-{}-{}", request.source, request.subject_revision),
            ),
        })
    }

    async fn hold(&self, _request: &CrewTurnIntent, _act: &HoldAct, _reason: &str) -> Result<(), String> {
        self.holds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl crate::change_request_observer::ChangeRequestObservationSource for ControlledChangeRequests {
    async fn observe(
        &self,
        _subject: &crate::change_request_observer::ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        let observed_at = Utc::now();
        let state = if self.merged.load(Ordering::SeqCst) {
            flotilla_resources::ObservedChangeRequestState::Merged
        } else {
            flotilla_resources::ObservedChangeRequestState::Open
        };
        Ok(flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(state, observed_at),
            head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
        })
    }
}

#[async_trait]
impl crate::change_request_observer::ChangeRequestObservationSource for CountingChangeRequests {
    async fn observe(
        &self,
        _subject: &crate::change_request_observer::ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let observed_at = Utc::now();
        Ok(flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, observed_at),
            head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
        })
    }
}

fn overload_row(connection_id: uuid::Uuid) -> LeafSubscriptionRow {
    LeafSubscriptionRow {
        id: uuid::Uuid::new_v4(),
        namespace: "flotilla".into(),
        leaves: vec![leaf(LeafAddress::Convoy { name: "busy".into() }, ".status.phase", "Failed")],
        watcher: LeafWatcher::WaitCaller { connection_id },
        maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
        freshness_demand: None,
        created_at: Utc::now(),
        episode_key: EpisodeKeyFields::default(),
    }
}

async fn overload_demand(table: &LeafSubscriptionTable, id: uuid::Uuid) {
    table
        .inner
        .change_requests
        .demand(
            id,
            ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "org/repo".into(), number: 1 },
            None,
        )
        .await
        .expect("refresher demand");
    assert_eq!(table.inner.change_requests.active_demands().await, 1);
}

async fn overload_convoy(backend: &ResourceBackend) {
    let convoys = backend.using::<Convoy>("flotilla");
    let mut object = convoys.get("busy").await.expect("convoy");
    for index in 0..300 {
        object = convoys
            .update(
                &InputMeta::from(&object.metadata),
                &object.metadata.resource_version,
                &ConvoySpec::builder().workflow_ref(format!("load-{index}")).build(),
            )
            .await
            .expect("overload write");
    }
}

fn convoy_spec() -> ConvoySpec {
    ConvoySpec::builder().workflow_ref("workflow".to_string()).build()
}

fn leaf(address: LeafAddress, field_path: &str, literal: &str) -> Leaf {
    Leaf { address, field_path: field_path.to_string(), operator: LeafOperator::Equal, literal: literal.to_string() }
}

async fn create_convoy(backend: &ResourceBackend, name: &str, status: ConvoyStatus) {
    let convoys = backend.using::<Convoy>("flotilla");
    let created = convoys.create(&InputMeta::builder().name(name.to_string()).build(), &convoy_spec()).await.expect("create convoy");
    convoys.update_status(name, &created.metadata.resource_version, &status).await.expect("write convoy status");
}

fn supervision_wake(backend: &ResourceBackend) -> ReconcilerWake {
    supervision_wake_with_limit(backend, 3)
}

fn supervision_wake_with_limit(backend: &ResourceBackend, limit: u32) -> ReconcilerWake {
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".into(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    ReconcilerWake {
        subscriptions: LeafSubscriptionTable::with_episode_limit(backend.clone(), broadcast_test_sink(event_tx), refresher, limit),
        _marker: PhantomData,
    }
}

async fn project_supervision_case(governors: &[(&str, u64, ConvoyPhase)]) -> (ResourceBackend, ReconcilerWake, Arc<RecordingTurnDelivery>) {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    let delivery = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let source = convoys
        .create(
            &InputMeta::builder().name("stalled-work".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).role("graphql-budget".into()).project_ref("wheelhouse".into()).build(),
        )
        .await
        .expect("create work convoy");
    convoys
        .update_status(
            "stalled-work",
            &source.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Active,
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![flotilla_resources::VesselRequirement::builder()
                        .name("work".into())
                        .crew(vec![flotilla_resources::CrewSpec::builder()
                            .role("coder".into())
                            .source(flotilla_resources::CrewSource::Tool { command: "test".into() })
                            .completion_conditions(vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
                                "coder",
                                "decision-ledger",
                                flotilla_resources::ArtifactSubjectBinding::Convoy,
                            )])
                            .build()])
                        .build()],
                }),
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([(
                        "coder".into(),
                        CrewWorkState::builder().phase(CrewWorkPhase::Stalled).message("needs decision".into()).build(),
                    )]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("stall work convoy");
    for (name, generation, phase) in governors {
        let created = convoys
            .create(
                &InputMeta::builder().name((*name).to_string()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("workflow".into())
                    .project_ref("wheelhouse".into())
                    .role("governor".into())
                    .generation(*generation)
                    .build(),
            )
            .await
            .expect("create governor convoy");
        convoys
            .update_status(
                name,
                &created.metadata.resource_version,
                &ConvoyStatus {
                    phase: *phase,
                    crew_work: BTreeMap::from([(
                        "govern".into(),
                        BTreeMap::from([("governor".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("set governor phase");
    }
    let leaf = Leaf {
        address: LeafAddress::Work { convoy: "stalled-work".into(), work: "work".into() },
        field_path: ".crew.coder.phase".into(),
        operator: LeafOperator::Equal,
        literal: "Done".into(),
    };
    let id = uuid::Uuid::new_v4();
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: "stalled-work".into() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );
    (backend, wake, delivery)
}

// Exercise the actor-row scenario through resource observations and the
// recording delivery collaborator, with explicit time rather than sleeps.
async fn idle_nudge_scenario() -> (ResourceBackend, ReconcilerWake, Arc<RecordingTurnDelivery>) {
    actor_nudge_scenario(&[]).await
}

async fn actor_nudge_scenario(governors: &[(&str, u64, ConvoyPhase)]) -> (ResourceBackend, ReconcilerWake, Arc<RecordingTurnDelivery>) {
    let (backend, wake, delivery) = project_supervision_case(governors).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("stalled-work").await.expect("convoy");
    let mut status = convoy.status.expect("status");
    status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.expect("working crew");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    sessions
        .create(
            &InputMeta::builder()
                .name("resumed-coder".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "stalled-work".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                    (ROLE_LABEL.into(), "coder".into()),
                ]))
                .build(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "env".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    brief: flotilla_resources::TerminalBrief {
                        path: "brief.md".into(),
                        content: "Initial".into(),
                        artifact_digest: None,
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: "flotilla".into(),
                        convoy: "stalled-work".into(),
                        vessel_ref: "work".into(),
                    }),
                    message: None,
                },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "cleat".into(),
            },
        )
        .await
        .expect("crew session");
    (backend, wake, delivery)
}

async fn observe_actor(backend: &ResourceBackend, wake: &ReconcilerWake, state: TerminalAttentionState, now: DateTime<Utc>) {
    observe_actor_source(backend, wake, state, TerminalAttentionSource::Screen, now).await;
}

async fn observe_actor_source(
    backend: &ResourceBackend,
    wake: &ReconcilerWake,
    state: TerminalAttentionState,
    source: TerminalAttentionSource,
    now: DateTime<Utc>,
) {
    let sessions = backend.using::<TerminalSession>("flotilla");
    let session = sessions.get("resumed-coder").await.expect("session");
    let mut status = session.status.unwrap_or_default();
    status.phase = TerminalSessionPhase::Running;
    status.attention = Some(TerminalAttention { state, as_of: now, source });
    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("observe hook");
    let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
    wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.expect("judge scenario");
}

async fn observe_claude_hook(backend: &ResourceBackend, wake: &ReconcilerWake, event: &str, now: DateTime<Utc>) {
    use crate::agents::hooks::{ClaudeCodeParser, HarnessHookParser};
    let parsed = ClaudeCodeParser.parse_event(event, br#"{"session_id":"claude-scenario"}"#).expect("Claude hook");
    let state = match parsed.event_type {
        flotilla_protocol::AgentEventType::Active | flotilla_protocol::AgentEventType::ToolActive => TerminalAttentionState::Working,
        flotilla_protocol::AgentEventType::Idle => TerminalAttentionState::Idle,
        other => panic!("unexpected hook: {other:?}"),
    };
    if parsed.event_type == flotilla_protocol::AgentEventType::ToolActive {
        let sessions = backend.using::<TerminalSession>("flotilla");
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.unwrap_or_default();
        status.last_tool_activity_at = Some(now);
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("actual tool activity");
    }
    observe_actor_source(backend, wake, state, TerminalAttentionSource::Hook, now).await;
}

async fn create_governor_ensure(backend: &ResourceBackend, convoy_ref: &str) {
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let created = ensures
        .create(
            &InputMeta::builder().name("wheelhouse-governor".into()).build(),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("wheelhouse".into())
                .role("governor".into())
                .workflow_ref("workflow".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("create ensure");
    let created = ensures.get(&created.metadata.name).await.expect("read ensure");
    ensures
        .update_status(
            "wheelhouse-governor",
            &created.metadata.resource_version,
            &flotilla_resources::ConvoyEnsureStatus { convoy_ref: Some(convoy_ref.into()), ..Default::default() },
        )
        .await
        .expect("set owned attempt");
}

#[derive(Clone, Copy, Debug)]
enum GovernorUnavailable {
    DeliveryError,
    NoLiveGeneration,
    CrewMissing,
}

impl GovernorUnavailable {
    const ALL: [Self; 3] = [Self::DeliveryError, Self::NoLiveGeneration, Self::CrewMissing];
}

#[derive(Clone, Copy, Debug)]
enum FallbackRecord {
    Current,
    LegacyExhausted,
    LegacyExhaustedCursor,
}

// #2488: unavailable delivery, missing live convoy, and missing crew must recover
// in one pass once reachable. Repeated reconciliation must neither freeze nor duplicate delivery.
async fn unavailable_governor_scenario(unavailable: GovernorUnavailable, restarts: &[bool], record: FallbackRecord) {
    let (backend, mut wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let governor = convoys.get("governor").await.expect("governor");
    let mut unavailable_status = governor.status.clone().expect("status");
    match unavailable {
        GovernorUnavailable::DeliveryError => delivery.unavailable.store(true, Ordering::SeqCst),
        GovernorUnavailable::NoLiveGeneration => unavailable_status.phase = ConvoyPhase::Abandoned,
        GovernorUnavailable::CrewMissing => unavailable_status.crew_work.clear(),
    }
    convoys.update_status("governor", &governor.metadata.resource_version, &unavailable_status).await.expect("unavailable");
    let now = Utc::now();
    for restart in restarts {
        if *restart {
            wake = supervision_wake(&backend);
            wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
        }
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("judge unavailable governor");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert!(stalled.supervisor.is_none());
        assert!(stalled.evidence.starts_with("needs decision"));
        assert_eq!(
            stalled
                .evidence
                .matches(if matches!(unavailable, GovernorUnavailable::DeliveryError) {
                    "supervisor delivery failed"
                } else {
                    "no live governor"
                })
                .count(),
            1
        );
        assert!(delivery.requests.lock().expect("deliveries").is_empty());
    }
    let fallback = convoys.get("stalled-work").await.expect("source");
    let began_at = fallback.status.as_ref().expect("status").stalled.as_ref().expect("stall").began_at;
    if matches!(record, FallbackRecord::LegacyExhausted | FallbackRecord::LegacyExhaustedCursor) {
        // Previous generation persisted temporary unavailability as exhausted.
        let mut status = fallback.status.expect("status");
        status.stalled.as_mut().expect("stall").supervision_exhausted = true;
        if matches!(record, FallbackRecord::LegacyExhaustedCursor) {
            status.stalled.as_mut().expect("stall").supervision_index = Some(1);
        }
        convoys.update_status("stalled-work", &fallback.metadata.resource_version, &status).await.expect("legacy fallback");
        wake = supervision_wake(&backend);
        wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    }
    let reachable_name = if matches!(unavailable, GovernorUnavailable::NoLiveGeneration) { "governor-ready" } else { "governor" };
    let current = if matches!(unavailable, GovernorUnavailable::NoLiveGeneration) {
        // An abandoned generation stays terminal. Startup eventually reveals
        // a new live generation rather than resurrecting the old one.
        let mut spec = governor.spec.clone();
        spec.generation += 1;
        convoys.create(&InputMeta::builder().name(reachable_name.into()).build(), &spec).await.expect("live governor generation")
    } else {
        convoys.get(reachable_name).await.expect("governor")
    };
    convoys
        .update_status(reachable_name, &current.metadata.resource_version, &governor.status.expect("original status"))
        .await
        .expect("reachable");
    delivery.unavailable.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("judge reachable governor");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Governor, "unavailability {unavailable:?}");
        assert_eq!(stalled.began_at, began_at);
        assert_eq!(stalled.supervisor.expect("supervisor").convoy, reachable_name);
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    }
}

async fn update_convoy(backend: &ResourceBackend, name: &str, update: impl FnOnce(&mut ConvoyStatus)) {
    let convoys = backend.using::<Convoy>("flotilla");
    let current = convoys.get(name).await.expect("get convoy");
    let mut status = current.status.expect("convoy status");
    update(&mut status);
    convoys.update_status(name, &current.metadata.resource_version, &status).await.expect("update convoy status");
}

async fn receive_fire(events: &mut broadcast::Receiver<DaemonEvent>, subscription_id: uuid::Uuid) -> LeafFire {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let DaemonEvent::LeafFired(fire) = events.recv().await.expect("leaf event stream") {
                if fire.subscription_id == subscription_id {
                    return fire;
                }
            }
        }
    })
    .await
    .expect("leaf should fire")
}

async fn assert_leaf_subscription_contract(backend: ResourceBackend) {
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".to_string(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx.clone()), refresher);
    let connection_id = uuid::Uuid::new_v4();
    let mut events = event_tx.subscribe();

    let unknown = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.typo", "Landed")],
        freshness_demand: None,
    };
    let error = table.subscribe_wait(connection_id, unknown).await.expect_err("unknown path admission");
    assert!(error.contains("admitted vocabulary"));
    assert!(error.contains("work.latest-claim.disposition"));

    let absent = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.phase", "Landed")],
        freshness_demand: None,
    };
    let absent_id = table.subscribe_wait(connection_id, absent).await.expect("subscribe absent leaf");
    assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "absent record must remain unknown");

    create_convoy(&backend, "demo", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
    assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "false leaf must not fire");
    update_convoy(&backend, "demo", |status| status.phase = ConvoyPhase::Landed).await;
    let fire = receive_fire(&mut events, absent_id).await;
    assert_eq!(fire.leaf.address, LeafAddress::Convoy { name: "demo".to_string() });
    assert_eq!(fire.value, "Landed");

    let immediate = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![
            leaf(LeafAddress::Vessel { name: "missing".to_string() }, ".status.phase", "Ready"),
            leaf(LeafAddress::Convoy { name: "demo".to_string() }, ".status.phase", "Landed"),
        ],
        freshness_demand: None,
    };
    let immediate_id = table.subscribe_wait(connection_id, immediate).await.expect("subscribe immediate OR-set");
    assert_eq!(receive_fire(&mut events, immediate_id).await.leaf.address, LeafAddress::Convoy { name: "demo".to_string() });

    let claimed_at = "2026-08-03T20:00:00Z".parse().expect("claim timestamp");
    let work = WorkState::builder().phase(WorkPhase::Complete).build();
    let crew =
        CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(claimed_at).disposition("changes-pushed".to_string()).build();
    update_convoy(&backend, "demo", |status| {
        status.work = BTreeMap::from([("implement".to_string(), work)]);
        status.crew_work = BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew)]))]);
    })
    .await;
    let claim = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(
            LeafAddress::Work { convoy: "demo".to_string(), work: "implement".to_string() },
            ".latest-claim.disposition",
            "changes-pushed",
        )],
        freshness_demand: None,
    };
    let claim_id = table.subscribe_wait(connection_id, claim).await.expect("subscribe claim leaf");
    assert_eq!(receive_fire(&mut events, claim_id).await.value, "changes-pushed");

    let stale = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(
            LeafAddress::Work { convoy: "demo".to_string(), work: "implement".to_string() },
            ".latest-claim.disposition",
            "changes-pushed",
        )],
        freshness_demand: Some("2026-08-03T21:00:00Z".parse().expect("freshness timestamp")),
    };
    let stale_id = table.subscribe_wait(connection_id, stale).await.expect("subscribe stale claim leaf");
    assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "stale evidence must remain unknown");
    assert!(table.rows().await.iter().any(|row| row.id == stale_id));

    let recorded_at = Utc::now();
    let artifact_address = LeafAddress::Artifact {
        convoy: "demo".to_string(),
        producer: "reviewer".to_string(),
        kind: "review-round".to_string(),
        subject: "head-X".to_string(),
    };
    let artifact_wait = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(artifact_address.clone(), ".summary.disposition", "approve")],
        freshness_demand: Some(recorded_at - chrono::Duration::seconds(1)),
    };
    let artifact_id = table.subscribe_wait(connection_id, artifact_wait).await.expect("subscribe artifact leaf");
    let name = flotilla_resources::artifact_record_name("demo", "reviewer", "review-round", "head-X");
    backend
        .using::<Artifact>("flotilla")
        .create(
            &InputMeta::builder().name(name).build(),
            &flotilla_resources::ArtifactSpec::builder()
                .convoy("demo".to_string())
                .producer("reviewer".to_string())
                .kind("review-round".to_string())
                .subject("head-X".to_string())
                .summary(BTreeMap::from([("disposition".to_string(), serde_json::json!("approve"))]))
                .digest("test".to_string())
                .size(1)
                .media_type("text/plain".to_string())
                .recorded_at(recorded_at)
                .expires_at(recorded_at + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("publish artifact");
    assert_eq!(receive_fire(&mut events, artifact_id).await.leaf.address, artifact_address);

    table.unsubscribe_connection(connection_id).await;
    assert!(table.rows().await.is_empty(), "connection teardown must remove WaitCaller rows");
}

// #2701: a merged PR wakes an unclaimed crew exactly once, including after
// subscription reconstruction. A claimed or terminal crew is never reopened.
async fn merged_unclaimed_scenario(repeats: usize, unknown_head: bool) {
    use flotilla_protocol::Relationship;
    use flotilla_resources::{Observation, ObservedChangeRequestState, SubjectDiscoverySource};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    let table = &wake.subscriptions;
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let role = "coder";
    let workflow = flotilla_resources::single_agent_workflow_spec();
    let convoys = backend.using::<Convoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("checks-wake".into()).build(),
            &ConvoySpec::builder().workflow_ref("single-agent".into()).build(),
        )
        .await
        .expect("convoy");
    let base = Utc::now() - chrono::Duration::seconds(2);
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        started_at: Some(base),
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
        }),
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([(
            "work".into(),
            BTreeMap::from([(role.into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    status.discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "2596".into(),
        },
        Relationship::Produces,
        SubjectDiscoverySource::Claim,
        base,
    );
    convoys.update_status("checks-wake", &created.metadata.resource_version, &status).await.expect("active crew");
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 2596);
    records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(2596)
                .observing_authority("authority".into())
                .build(),
        )
        .await
        .expect("PR record");
    // Pending, stalled and terminal crews do not owe an active settlement turn.
    for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Stalled, CrewWorkPhase::Done, CrewWorkPhase::Failed, CrewWorkPhase::HandedBack] {
        let current = convoys.get("checks-wake").await.expect("convoy");
        let mut status = current.status.expect("status");
        status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = phase;
        let updated = convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("inactive crew");
        wake.sync_rows("flotilla", &HashMap::from([("checks-wake".into(), updated)])).await.expect("inactive subscriptions");
        assert!(
            !table
                .rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed")),
            "{phase:?}"
        );
    }
    let current = convoys.get("checks-wake").await.expect("convoy");
    let mut status = current.status.expect("status");
    status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew").phase = CrewWorkPhase::Interrupted;
    convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("yielded crew");
    let observed = flotilla_resources::ChangeRequestStatus {
        state: Observation::known(ObservedChangeRequestState::Merged, Utc::now()),
        head_sha: if unknown_head { Observation::unknown(Utc::now()) } else { Observation::known("merged-head".into(), Utc::now()) },
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        checks: Observation::known(ObservedChecks::Pass, Utc::now()),
        mergeable: Default::default(),
        review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
    };
    let record = records.get(&name).await.expect("record");
    let record = records.update_status(&name, &record.metadata.resource_version, &observed).await.expect("merged observation");
    for _ in 0..repeats {
        let objects = HashMap::from([("checks-wake".into(), convoys.get("checks-wake").await.expect("convoy"))]);
        wake.sync_rows("flotilla", &objects).await.expect("subscriptions");
        for task in table.inner.tasks.lock().await.drain().map(|(_, task)| task) {
            task.abort();
        }
        let rows = table.rows().await;
        let row = rows
            .iter()
            .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed"))
            .expect("merged subscription");
        let empty = HashMap::new();
        let change_requests = HashMap::from([(name.clone(), record.clone())]);
        let subjects = LeafSubjects {
            convoys: &objects,
            vessels: &empty,
            change_requests: &change_requests,
            usages: &HashMap::new(),
            issues: &HashMap::new(),
            artifacts: &HashMap::new(),
        };
        let fire = evaluate_row(
            row,
            &subjects,
            LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
        )
        .expect("evaluate merged")
        .expect("merged fires");
        table.fire(row.id, fire).await;
        // Exercise competing CI and review firings from the same merged record.
        for competing in
            rows.iter().filter(|candidate| candidate.id != row.id && matches!(candidate.watcher, LeafWatcher::TurnDelivery { .. }))
        {
            if let Some(fire) = evaluate_row(
                competing,
                &subjects,
                LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
            )
            .expect("competing evaluation")
            {
                table.fire(competing.id, fire).await;
            }
        }
        let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { unreachable!() };
        table.deliver_turn(row.id, "checks-wake", source, rule, &row.leaves[0]).await.expect("duplicate delivery");
        assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
        let current = convoys.get("checks-wake").await.expect("convoy");
        assert_eq!(current.status.expect("status").turn_deliveries["merged-unclaimed"].episodes.len(), 1);
    }
    let subject = actuator.requests.lock().unwrap()[0].message_subject.clone().unwrap();
    if unknown_head {
        assert!(matches!(subject, flotilla_resources::MessageReference::ControlRecord { resource, revision }
            if resource.kind == "ChangeRequest" && !revision.is_empty()));
    } else {
        assert!(matches!(subject, flotilla_resources::MessageReference::ChangeRequest { revision, .. } if revision == "merged-head"));
    }
    assert!(actuator.requests.lock().expect("requests")[0].brief.contains("PR merged"));
    assert!(actuator.requests.lock().expect("requests")[0].brief.contains("decision ledger"));
    let current = convoys.get("checks-wake").await.expect("convoy");
    let mut status = current.status.expect("status");
    let crew = status.crew_work.get_mut("work").expect("work").get_mut("coder").expect("crew");
    crew.phase = CrewWorkPhase::Done;
    crew.finished_at = Some(base);
    let stale_row = table
        .rows()
        .await
        .into_iter()
        .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed"))
        .expect("cached merge row");
    let LeafWatcher::TurnDelivery { source, rule, .. } = &stale_row.watcher else { unreachable!() };
    // A cached firing must recheck both active and parked settlement claims.
    for phase in [ConvoyPhase::Active, ConvoyPhase::Landing] {
        let current = convoys.get("checks-wake").await.expect("convoy");
        status.phase = phase;
        // Clear the episode to prove eligibility, rather than deduplication,
        // prevents reopening a claimed crew.
        status.turn_deliveries.clear();
        convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("claim");
        table.deliver_turn(stale_row.id, "checks-wake", source, rule, &stale_row.leaves[0]).await.expect("cached firing after claim");
        assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
    }
    let updated = convoys.get("checks-wake").await.expect("claimed convoy");
    wake.sync_rows("flotilla", &HashMap::from([("checks-wake".into(), updated)])).await.expect("settled subscriptions");
    assert!(!table
        .rows()
        .await
        .iter()
        .any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed")));
}

// #2596: an active crew with a produced PR yields while checks are pending;
// settled checks deliver once per head, with the existing episode ceiling.
async fn active_checks_scenario(outcomes: &[bool], watch_events: bool, role: &str) {
    use flotilla_protocol::Relationship;
    use flotilla_resources::{Observation, ObservedChecks, SubjectDiscoverySource};

    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let wake = supervision_wake(&backend);
    let table = &wake.subscriptions;
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let workflow = if role == "reviewer" {
        flotilla_resources::implement_review_workflow_spec()
    } else {
        flotilla_resources::single_agent_workflow_spec()
    };
    let convoys = backend.using::<Convoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("checks-wake".into()).build(),
            &ConvoySpec::builder().workflow_ref(if role == "reviewer" { "implement-review".into() } else { "single-agent".into() }).build(),
        )
        .await
        .expect("convoy");
    let base = Utc::now() - chrono::Duration::seconds(2);
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        started_at: Some(base),
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            stall_nudges: workflow.stall_nudges,
            supervision: workflow.supervision,
            exit: workflow.exit,
            turn_delivery: workflow.turn_delivery,
            vessels: workflow.vessels,
        }),
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([(
            "work".into(),
            BTreeMap::from([(role.into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
        )]),
        ..Default::default()
    };
    status.discover_subject(
        flotilla_protocol::Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "2596".into(),
        },
        Relationship::Produces,
        SubjectDiscoverySource::Claim,
        base,
    );
    convoys.update_status("checks-wake", &created.metadata.resource_version, &status).await.expect("active crew");
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 2596);
    records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(2596)
                .observing_authority("authority".into())
                .build(),
        )
        .await
        .expect("PR record");
    // Pending roles await handoff, real stalls await supervision, and
    // settled/failed roles must not be reopened by CI/review probes.
    for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Stalled, CrewWorkPhase::Done, CrewWorkPhase::HandedBack, CrewWorkPhase::Failed] {
        let current = convoys.get("checks-wake").await.expect("convoy");
        let mut status = current.status.expect("status");
        status.crew_work.get_mut("work").expect("work").get_mut(role).expect("crew").phase = phase;
        let updated = convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("inactive crew");
        wake.sync_rows("flotilla", &HashMap::from([("checks-wake".into(), updated)])).await.expect("inactive subscriptions");
        assert!(
            !table.rows().await.iter().any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { rule, .. }
            if matches!(rule.on.field_path.as_str(), ".checks" | ".review.actionable-at-head"))),
            "{phase:?}"
        );
    }
    for phase in [CrewWorkPhase::Interrupted, CrewWorkPhase::Working] {
        let current = convoys.get("checks-wake").await.expect("convoy");
        let mut status = current.status.expect("status");
        status.crew_work.get_mut("work").expect("work").get_mut(role).expect("crew").phase = phase;
        let updated = convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("active crew");
        wake.sync_rows("flotilla", &HashMap::from([("checks-wake".into(), updated)])).await.expect("active subscriptions");
        assert!(table.rows().await.iter().any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && rule.on.field_path == ".checks")), "{phase:?}");
    }
    let objects = HashMap::from([("checks-wake".into(), convoys.get("checks-wake").await.expect("convoy"))]);
    // Drive each evaluation deterministically, using the same evaluator and
    // firing path as the resource watch, rather than scheduling background watches.
    if !watch_events {
        for task in table.inner.tasks.lock().await.drain().map(|(_, task)| task) {
            task.abort();
        }
    }
    let row = table
        .rows()
        .await
        .into_iter()
        .find(|row| {
            matches!(&row.watcher,
                LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && rule.on.field_path == ".checks"
            )
        })
        .expect("active checks-settled subscription");
    let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { unreachable!() };
    // Subscribe before writing observations so a delivery racing the status
    // read remains buffered, with no polling or missed notification.
    let mut convoy_watch = convoys.watch(WatchStart::Now).await.expect("delivery watch");
    for (index, pass) in outcomes.iter().enumerate() {
        for checks in [None, Some(ObservedChecks::Pending), Some(if *pass { ObservedChecks::Pass } else { ObservedChecks::Fail })] {
            let observed = flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: Default::default(),
                head_sha: Observation::known(format!("head-{index}"), Utc::now()),
                checks: Observation { value: checks, observed_at: Utc::now() },
                review: flotilla_resources::ChangeRequestReviewObservation { actionable_at_head: Default::default() },
                mergeable: Default::default(),
            };
            let updated = loop {
                let current = records.get(&name).await.expect("record");
                match records.update_status(&name, &current.metadata.resource_version, &observed).await {
                    Ok(updated) => break updated,
                    Err(ResourceError::Conflict { .. }) => continue,
                    Err(error) => panic!("observe checks: {error}"),
                }
            };
            let empty = HashMap::new();
            let change_requests = HashMap::from([(name.clone(), updated)]);
            let fire = evaluate_row(
                &row,
                &LeafSubjects {
                    convoys: &objects,
                    vessels: &empty,
                    change_requests: &change_requests,
                    usages: &HashMap::new(),
                    issues: &HashMap::new(),
                    artifacts: &HashMap::new(),
                },
                LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
            )
            .expect("evaluate checks");
            assert_eq!(fire.is_some(), checks.is_some_and(|value| value != ObservedChecks::Pending));
            if let Some(fire) = fire {
                if watch_events {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let current = convoys.get("checks-wake").await.expect("convoy").status.expect("status");
                            if current.turn_deliveries.get(source).is_some_and(|state| state.episodes.len() == (index + 1).min(4)) {
                                break;
                            }
                            convoy_watch.next().await.expect("delivery watch open").expect("delivery event");
                        }
                    })
                    .await
                    .expect("resource watch delivers settled checks");
                } else {
                    table.fire(row.id, fire).await;
                }
                table.deliver_turn(row.id, "checks-wake", source, rule, &row.leaves[0]).await.expect("duplicate observation");
            }
            let status = convoys.get("checks-wake").await.expect("convoy").status.expect("status");
            let settled = checks.is_some_and(|value| value != ObservedChecks::Pending);
            assert_eq!(status.turn_deliveries.get(source).map_or(0, |state| state.episodes.len()), (index + usize::from(settled)).min(4));
            assert_eq!(actuator.requests.lock().expect("requests").len(), (index + usize::from(settled)).min(3));
            if settled && index < 3 {
                // Agent-facing observations use the same plain status words
                // as the leaf vocabulary, including unknown review evidence.
                let requests = actuator.requests.lock().expect("requests");
                let brief = &requests.last().expect("settled turn").brief;
                assert!(brief.contains(&format!("- Checks: {}", if *pass { "pass" } else { "fail" })));
                assert!(brief.contains("- Review actionable at head: unknown"));
            }
        }
    }
    assert_eq!(actuator.holds.load(Ordering::SeqCst), usize::from(outcomes.len() > 3));
    {
        let requests = actuator.requests.lock().expect("requests");
        assert!(requests[0].brief.contains("Head SHA: `head-0`"));
        assert!(requests[0].brief.contains("Inspect checks and reviews"));
    }
    // Drive review feedback through the same active delivery path for both
    // roles; the review source deduplicates independently of settled checks.
    for task in table.inner.tasks.lock().await.drain().map(|(_, task)| task) {
        task.abort();
    }
    // A cached checks row cannot reopen an inactive role, even for a new
    // settled head that has no delivery episode yet.
    loop {
        let current = records.get(&name).await.expect("record");
        let mut observed = current.status.expect("observation");
        observed.head_sha = Observation::known("inactive-head".into(), Utc::now());
        observed.checks = Observation::known(ObservedChecks::Pass, Utc::now());
        match records.update_status(&name, &current.metadata.resource_version, &observed).await {
            Ok(_) => break,
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => panic!("observe inactive head: {error}"),
        }
    }
    let requests_before = actuator.requests.lock().expect("requests").len();
    let holds_before = actuator.holds.load(Ordering::SeqCst);
    for phase in [
        CrewWorkPhase::Pending,
        CrewWorkPhase::Stalled,
        CrewWorkPhase::Done,
        CrewWorkPhase::HandedBack,
        CrewWorkPhase::Failed,
        CrewWorkPhase::Working,
    ] {
        let current = convoys.get("checks-wake").await.expect("convoy");
        let mut status = current.status.expect("status");
        status.crew_work.get_mut("work").expect("work").get_mut(role).expect("crew").phase = phase;
        convoys.update_status("checks-wake", &current.metadata.resource_version, &status).await.expect("crew transition");
        if phase == CrewWorkPhase::Working {
            break;
        }
        // Keep the old row armed to model delivery racing its removal.
        table.deliver_turn(row.id, "checks-wake", source, rule, &row.leaves[0]).await.expect("ignore cached inactive row");
        assert_eq!(actuator.requests.lock().expect("requests").len(), requests_before, "{phase:?}");
        assert_eq!(actuator.holds.load(Ordering::SeqCst), holds_before, "{phase:?}");
        let current = convoys.get("checks-wake").await.expect("convoy").status.expect("status");
        assert_eq!(current.crew_work["work"][role].phase, phase);
    }
    loop {
        let current = records.get(&name).await.expect("record");
        let mut observed = current.status.expect("observation");
        observed.review.actionable_at_head = Observation::known(true, Utc::now());
        observed.mergeable = Observation::known(flotilla_resources::ObservedMergeability::Conflicting, Utc::now());
        match records.update_status(&name, &current.metadata.resource_version, &observed).await {
            Ok(_) => break,
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => panic!("observe feedback: {error}"),
        }
    }
    let review = table
        .rows()
        .await
        .into_iter()
        .find(|row| {
            matches!(&row.watcher,
                LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && rule.on.field_path == ".review.actionable-at-head"
            )
        })
        .expect("active review subscription");
    let LeafWatcher::TurnDelivery { source, rule, .. } = &review.watcher else { unreachable!() };
    let before = actuator.requests.lock().expect("requests").len();
    table.deliver_turn(review.id, "checks-wake", source, rule, &review.leaves[0]).await.expect("active review turn");
    table.deliver_turn(review.id, "checks-wake", source, rule, &review.leaves[0]).await.expect("duplicate review");
    {
        let requests = actuator.requests.lock().expect("requests");
        assert_eq!(requests.len(), before + 1);
        assert_eq!(requests.last().expect("review request").role, role);
        assert!(requests.last().expect("review request").brief.contains("- Review actionable at head: true"));
    }
    if role == "coder" {
        // Active conflict turns honor the workflow's declared instructions.
        let conflict = table.rows().await.into_iter().find(|row| is_conflict_probe(&row.leaves[0])).expect("active conflict subscription");
        let LeafWatcher::TurnDelivery { source, rule, .. } = &conflict.watcher else { unreachable!() };
        table.deliver_turn(conflict.id, "checks-wake", source, rule, &conflict.leaves[0]).await.expect("active conflict turn");
        assert!(actuator.requests.lock().expect("requests").last().expect("conflict request").brief.starts_with(rule.brief.trim()));
    }
    for row in table.rows().await {
        table.finish(row.id).await;
    }
}

async fn assert_turn_delivery_enforces_head_identity_records_rungs_and_escalates(
    source: &str,
    condition: &str,
    field_path: &str,
    literal: &str,
    actionable_review: bool,
    mergeability: flotilla_resources::ObservedMergeability,
    brief: &str,
) {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(UnavailableChangeRequests),
        crate::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let rule = TurnDeliveryRule::builder()
        .on(condition.parse().expect("wake leaf"))
        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".to_string()).role("coder".to_string()).build())
        .brief(brief.to_string())
        .hold(HoldAct::State)
        .build();
    let repo_ref = RepositoryKey("repo".to_string());
    let convoy_spec = ConvoySpec::builder()
        .workflow_ref("workflow".to_string())
        .repositories(vec![ConvoyRepositorySpec::builder()
            .url("https://github.com/flotilla-org/flotilla".to_string())
            .repo_ref(repo_ref.clone())
            .source_ref("feature/wake".to_string())
            .target_ref("main".to_string())
            .workspace_slug("flotilla".to_string())
            .subpaths(Vec::new())
            .build()])
        .change_request(BoundChangeRequest::builder().id("1392".to_string()).repository_ref(repo_ref).title("wake".to_string()).build())
        .build();
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let created = convoys.create(&InputMeta::builder().name("wake-turn".to_string()).build(), &convoy_spec).await.expect("convoy");
    let base = Utc::now();
    convoys
        .update_status(
            "wake-turn",
            &created.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: indexmap::IndexMap::from([(source.to_string(), rule.clone())]),
                    vessels: Vec::new(),
                }),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([(
                        "coder".to_string(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .finished_at(base)
                            .decision_ledger_ref("https://github.com/flotilla-org/flotilla/pull/1392#issuecomment-1".to_string())
                            .build(),
                    )]),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect("landing status");
    let cr_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1392);
    let records = backend.clone().using::<ChangeRequest>("flotilla");
    let record = records
        .create(
            &InputMeta::builder().name(cr_name).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1392)
                .observing_authority("authority".to_string())
                .build(),
        )
        .await
        .expect("change request");
    let leaf = Leaf {
        address: LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1392 },
        field_path: field_path.to_string(),
        operator: LeafOperator::Equal,
        literal: literal.to_string(),
    };
    let subscription_id = uuid::Uuid::new_v4();
    table.inner.rows.lock().await.insert(
        subscription_id,
        LeafSubscriptionRow {
            id: subscription_id,
            namespace: "flotilla".to_string(),
            leaves: vec![leaf.clone()],
            watcher: LeafWatcher::TurnDelivery {
                convoy: "wake-turn".to_string(),
                source: source.to_string(),
                rule: Box::new(rule.clone()),
            },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: Some(base),
            created_at: base,
            episode_key: EpisodeKeyFields::default(),
        },
    );

    let mut record_version = record.metadata.resource_version;
    let stale_status = flotilla_resources::ChangeRequestStatus {
        title: Default::default(),
        author: Default::default(),
        review_decision: Default::default(),
        review_requested_from_owner: Default::default(),
        state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, base),
        head_sha: flotilla_resources::Observation::known("stale".to_string(), base),
        checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Fail, base),
        review: flotilla_resources::ChangeRequestReviewObservation {
            actionable_at_head: flotilla_resources::Observation::known(actionable_review, base),
        },
        mergeable: flotilla_resources::Observation::known(mergeability, base),
    };
    let updated = records.update_status(&record.metadata.name, &record_version, &stale_status).await.expect("observe stale head");
    record_version = updated.metadata.resource_version;
    table.deliver_turn(subscription_id, "wake-turn", source, &rule, &leaf).await.expect("ignore stale firing");
    assert_eq!(table.inner.rows.lock().await[&subscription_id].episode_key.subject_revision, None);
    assert!(convoys.get("wake-turn").await.expect("convoy").status.expect("status").turn_deliveries.is_empty());

    for (index, head) in ["aaa", "bbb", "ccc", "ddd"].into_iter().enumerate() {
        let claim_at = base + chrono::Duration::seconds((index * 2) as i64);
        if index > 0 {
            let current = convoys.get("wake-turn").await.expect("convoy");
            let mut status = current.status.expect("status");
            let coder = status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder");
            coder.phase = CrewWorkPhase::Done;
            coder.finished_at = Some(claim_at);
            status.work.get_mut("work").expect("work").phase = WorkPhase::Complete;
            status.phase = ConvoyPhase::Landing;
            convoys.update_status("wake-turn", &current.metadata.resource_version, &status).await.expect("new claim");
        }
        let observed_at = claim_at + chrono::Duration::seconds(1);
        let cr_status = flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, observed_at),
            head_sha: flotilla_resources::Observation::known(head.to_string(), observed_at),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Fail, observed_at),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(actionable_review, observed_at),
            },
            mergeable: flotilla_resources::Observation::known(mergeability, observed_at),
        };
        let updated = records.update_status(&record.metadata.name, &record_version, &cr_status).await.expect("observe head");
        record_version = updated.metadata.resource_version;
        table.deliver_turn(subscription_id, "wake-turn", source, &rule, &leaf).await.expect("process firing");
        if index == 0 {
            table.deliver_turn(subscription_id, "wake-turn", source, &rule, &leaf).await.expect("same head is a no-op");
        }
    }

    let status = convoys.get("wake-turn").await.expect("convoy").status.expect("status");
    let episodes = &status.turn_deliveries[source].episodes;
    assert_eq!(episodes.len(), 4, "same-head redelivery must not create an episode");
    assert_eq!(episodes[0].sender, flotilla_resources::CrewMessageSender::Unknown);
    assert!(serde_json::to_value(&episodes[0]).expect("episode").get("sender").is_none());
    // Message admission is a workflow latch, not confirmation of an agent turn.
    assert_eq!(serde_json::to_value(&episodes[0].outcome).unwrap()["kind"], "message-accepted");
    assert!(
        matches!(&episodes[1].outcome, TurnDeliveryOutcome::MessageAccepted { rung: TurnDeliveryRung::FreshAgent, message, .. } if message.kind == "Message")
    );
    assert!(matches!(episodes[3].outcome, TurnDeliveryOutcome::Refused { hold_executed: true, .. }));
    assert_eq!(actuator.requests.lock().expect("requests").len(), 3);
    assert_eq!(actuator.holds.load(Ordering::SeqCst), 1);
    assert!(status.attention.is_some());
    let first_brief = &actuator.requests.lock().expect("requests")[0].brief;
    assert!(first_brief.contains("Head SHA: `aaa`"));
    assert_eq!(
        first_brief.contains("- Review feedback: https://github.com/flotilla-org/flotilla/pull/1392"),
        source == "review",
        "review link appears only in review turns"
    );
    assert!(first_brief.contains("feature/wake"));
    assert!(first_brief.contains("Durable convoy record: `flotilla/wake-turn`"));
    assert!(first_brief.contains("Decision ledger: https://github.com/flotilla-org/flotilla/pull/1392#issuecomment-1"));
}

mod change_request_authority;
mod settlement;
mod stalls_supervision;
mod subscriptions;
mod turn_delivery;
