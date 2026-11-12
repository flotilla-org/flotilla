use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use std::{collections::HashMap, sync::Arc};

use crate::convoy::ConvoyReconciler;
use crate::engine_home::EngineHome;
use crate::{agent_reads, agents};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_change_request::observation;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_forge_github::github::GithubRateLimit;
use flotilla_forge_github::github::GithubRateLimitKind;
use flotilla_forge_github::github::GithubRetrySource;
use flotilla_protocol::{DaemonEvent, Leaf, LeafFire, NodeId, WaitSubscriptionRequest};
use flotilla_protocol::{LeafAddress, LeafOperator};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::checkout_records::CheckoutRecords as _;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::terminal_records::TerminalRecords as _;
use flotilla_resources::vessel_records::VesselStep;
use flotilla_resources::StatusPatch;
use flotilla_resources::{
    external_patches, Artifact, ChangeRequest, Convoy, ConvoyAttention, ConvoyEnsure, HoldAct, Issue, LeafMaker, NudgeObligation,
    ObservedChecks, Project, ResourceError, RetryCeiling, StallEvidenceSource, StallRung, SupervisionTarget, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalSession, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRule,
    TurnDeliveryRung, WatchEvent, WatchStart,
};
use flotilla_resources::{
    BoundChangeRequest, CheckoutSpec, ControllerRetry, ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewWorkPhase,
    ExitDeclaration, InputMeta, ObservedCheckoutSpec, RepositoryKey, WorkPhase, WorkflowSnapshot,
};
use flotilla_resources::{ResourceObject, SubjectDiscoverySource};
use flotilla_store::controller::ControllerLoop;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;
use tokio::sync::broadcast;

use super::routing::RowDependencies;
use super::sources::*;
use super::stalls::*;
use super::subscriptions::*;
use super::wake::*;
use super::*;
use flotilla_observation::change_request_observer::ChangeRequestRefresher;
use flotilla_observation::issue_observer::IssueRefreshCadence;
use flotilla_observation::issue_observer::IssueRefresher;
use flotilla_protocol_testkit::event_sink::broadcast_test_sink;
use flotilla_replay_testkit::test_runner;
use flotilla_replay_testkit::Masks;
use flotilla_replay_testkit::Session;
use flotilla_store_testkit::cr_name;

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
impl observation::ChangeRequestObservationSource for UnavailableChangeRequests {
    async fn observe(&self, _subject: &observation::ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
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
impl observation::ChangeRequestObservationSource for ControlledChangeRequests {
    async fn observe(&self, _subject: &observation::ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
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
impl observation::ChangeRequestObservationSource for CountingChangeRequests {
    async fn observe(&self, _subject: &observation::ChangeRequestKey) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
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

/// A caller's wait, on connection `connection_id`, for launch `launch` to fail.
fn overload_row(connection_id: uuid::Uuid, launch: &str) -> LeafSubscriptionRow {
    LeafSubscriptionRow {
        id: uuid::Uuid::new_v4(),
        namespace: "flotilla".into(),
        leaves: vec![leaf(LeafAddress::Convoy { name: launch.into() }, ".status.phase", "Failed")],
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
            ChangeRequestKey::new(
                "flotilla",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 1, &[]).expect("address"),
            )
            .expect("key"),
            None,
        )
        .await
        .expect("refresher demand");
    assert_eq!(table.inner.change_requests.active_demands().await, 1);
}

/// 300 writes to one change-request record, outrunning the store's bounded watch ring. The
/// engine's launches reach the router through their own feed, which conflates and never
/// overflows, so the router's watch is lost through the store's kinds it also watches.
async fn overload_store(backend: &ResourceBackend) {
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "org/load", 2);
    let spec = |authority: String| {
        flotilla_resources::ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/load".into())
            .number(2)
            .observing_authority(authority)
            .build()
    };
    let mut object = match records.get(&name).await {
        Ok(object) => object,
        Err(_) => records.create(&InputMeta::builder().name(name.clone()).build(), &spec("load".into())).await.expect("load record"),
    };
    for index in 0..300 {
        object = records
            .update(&InputMeta::from(&object.metadata), &object.metadata.resource_version, &spec(format!("load-{index}")))
            .await
            .expect("overload write");
    }
}

/// Changes launch `launch` as its view reads, through a note no leaf reads: an event on it.
async fn touch(backend: &ResourceBackend, home: &EngineHome, launch: &str) {
    home.note(backend, launch, |status| status.branch_subject_scan_at = Some(Utc::now())).await;
}

fn convoy_spec() -> ConvoySpec {
    ConvoySpec::builder().workflow_ref("workflow".to_string()).build()
}

fn leaf(address: LeafAddress, field_path: &str, literal: &str) -> Leaf {
    Leaf { address, field_path: field_path.to_string(), operator: LeafOperator::Equal, literal: literal.to_string() }
}

/// A leaf test's backend, serving the launches, vessels and sessions of one engine at its home
/// (#3108), and that engine.
fn engine_backend() -> (ResourceBackend, Arc<EngineHome>) {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let home = Arc::new(EngineHome::install(&backend, "flotilla", "test-host"));
    (backend, home)
}

/// A status whose frozen workflow has one vessel, `work`, crewed by an agent `role`: the least a
/// convoy owes to be admitted.
fn single_agent(role: &str) -> ConvoyStatus {
    flotilla_engine_convoys_testkit::single_agent(role)
}

/// Admits convoy `name` with [`convoy_spec`] and `status`, the way admission does; returns its
/// launch.
async fn create_convoy(home: &EngineHome, name: &str, status: ConvoyStatus) -> String {
    home.admit(name, convoy_spec(), status).await
}

fn supervision_wake(backend: &ResourceBackend, home: &EngineHome) -> ReconcilerWake {
    supervision_wake_with_limit(backend, home, 3)
}

fn supervision_wake_with_limit(backend: &ResourceBackend, home: &EngineHome, limit: u32) -> ReconcilerWake {
    supervision_wake_over(backend, home, home.reads(), limit)
}

/// A wake whose table takes `reads`, as a daemon that holds them does.
fn supervision_wake_over(backend: &ResourceBackend, home: &EngineHome, reads: Arc<dyn LifecycleReads>, limit: u32) -> ReconcilerWake {
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".into(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    ReconcilerWake::new(
        LeafSubscriptionTable::with_episode_limit(backend.clone(), broadcast_test_sink(event_tx), refresher, limit)
            .with_lifecycle_reads(reads)
            .with_convoy_engine(home.path()),
    )
}

/// The supervision scenario: a coder's work launched on the engine, and the leaf subscriptions
/// that judge it.
struct Case {
    backend: ResourceBackend,
    home: Arc<EngineHome>,
    wake: ReconcilerWake,
    delivery: Arc<RecordingTurnDelivery>,
    /// The work's launch.
    work: String,
    /// The launch of the work's vessel.
    vessel: String,
    /// The governors' launches, by the names the case was given them.
    governors: BTreeMap<String, String>,
}

impl Case {
    /// The work's launch as its view reads.
    async fn convoy(&self) -> ResourceObject<Convoy> {
        self.home.view(&self.backend, &self.work).await
    }

    /// The work's launch as its view reads, keyed for a judge.
    async fn objects(&self) -> HashMap<String, ResourceObject<Convoy>> {
        HashMap::from([(self.work.clone(), self.convoy().await)])
    }

    /// The work's status as its view reads.
    async fn status(&self) -> ConvoyStatus {
        self.convoy().await.status.expect("status")
    }

    /// The coder's address, as a Message names its receiver.
    fn coder(&self) -> String {
        flotilla_resources::crew_role_address("wheelhouse", &self.work, "work", "coder")
    }

    /// A wake as a restarted daemon builds it, with this case's delivery.
    async fn restarted_wake(&self) -> ReconcilerWake {
        let wake = supervision_wake(&self.backend, &self.home);
        wake.subscriptions.set_turn_delivery_actuator(self.delivery.clone()).await;
        wake
    }

    /// Changes the home's notes on the work as `change` changes its status.
    async fn note(&self, change: impl FnOnce(&mut ConvoyStatus)) -> ResourceObject<Convoy> {
        self.home.note(&self.backend, &self.work, change).await
    }

    /// The launch session `session` serves now.
    fn launch(&self, session: &str) -> String {
        self.home.engine.terminals.record(session).name
    }

    /// Waits until `wake`'s views have heard the work's launch as it now reads: a judge skips a
    /// launch whose view lags, until the watch catches up.
    async fn caught_up(&self, wake: &ReconcilerWake) {
        caught_up(wake, &self.convoy().await).await;
    }

    /// `wake` judges the work's launch at `now`, once its views have heard the launch as it reads.
    async fn judge(&self, wake: &ReconcilerWake, now: DateTime<Utc>) {
        let convoy = self.convoy().await;
        caught_up(wake, &convoy).await;
        wake.judge_stalls_at("flotilla", &HashMap::from([(self.work.clone(), convoy)]), now).await.expect("judge scenario");
    }

    /// The launch of the governor the case was given as `name`.
    fn governor(&self, name: &str) -> String {
        self.governors[name].clone()
    }
}

/// The work's frozen workflow: one vessel, `work`, whose agent coder owes a decision ledger.
fn supervised_workflow() -> WorkflowSnapshot {
    WorkflowSnapshot {
        cascade: None,
        stall_nudges: Default::default(),
        supervision: None,
        exit: Some(ExitDeclaration::standard_table()),
        turn_delivery: Default::default(),
        vessels: vec![flotilla_resources::VesselRequirement::builder()
            .name("work".into())
            .crew(vec![flotilla_resources::CrewSpec::builder()
                .role("coder".into())
                .source(flotilla_resources::CrewSource::Agent {
                    selector: flotilla_resources::Selector::for_capability("coding"),
                    prompt: None,
                    brief_template: None,
                })
                .completion_conditions(vec![flotilla_resources::CrewCompletionExpectation::artifact_exists(
                    "coder",
                    "decision-ledger",
                    flotilla_resources::ArtifactSubjectBinding::Convoy,
                )])
                .build()])
            .build()],
    }
}

/// A coder's work in wheelhouse, admitted with `workflow` and running in its vessel, whose coder
/// declared it is stalled ("needs decision") before any session of it started. Each governor is
/// a launch of the project's governor role at its generation, its governor at work, or ended as
/// its phase names. The coder's actor row is armed.
async fn supervised_case(workflow: WorkflowSnapshot, governors: &[(&str, u64, ConvoyPhase)]) -> Case {
    let (backend, home) = engine_backend();
    let wake = supervision_wake(&backend, &home);
    let delivery = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).role("graphql-budget".into()).project_ref("wheelhouse".into()).build();
    let work = home.admit("stalled-work", spec, ConvoyStatus { workflow_snapshot: Some(workflow), ..Default::default() }).await;
    let vessel = home.vessel(&work, "work", "env");
    let mut case = Case { backend, home, wake, delivery, work, vessel, governors: BTreeMap::new() };
    for (name, generation, phase) in governors {
        let launch = admit_governor(&case, name, *generation, "governor").await;
        if phase.is_terminal() {
            case.home.end(&case.backend, &launch, *phase).await;
        }
        case.governors.insert((*name).to_owned(), launch);
    }
    let now = Utc::now();
    case.note(|status| {
        external_patches::mark_crew_stalled(
            case.work.clone(),
            "work".into(),
            "coder".into(),
            now,
            flotilla_resources::StallReason::Other,
            None,
            "needs decision".into(),
        )
        .apply(status)
    })
    .await;
    let leaf = Leaf {
        address: LeafAddress::Work { convoy: case.work.clone(), work: "work".into() },
        field_path: ".crew.coder.phase".into(),
        operator: LeafOperator::Equal,
        literal: "Done".into(),
    };
    let id = uuid::Uuid::new_v4();
    case.wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: case.work.clone() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );
    case
}

async fn project_supervision_case(governors: &[(&str, u64, ConvoyPhase)]) -> Case {
    supervised_case(supervised_workflow(), governors).await
}

// Exercise the actor-row scenario through resource observations and the
// recording delivery collaborator, with explicit time rather than sleeps.
/// An operator's Message to the coder, accepted and waiting for delivery.
async fn message_to_coder(case: &Case, name: &str, body: &str) {
    let spec = flotilla_resources::MessageSpec::builder()
        .sender("principal:implicit".into())
        .receiver(case.coder())
        .body(body.into())
        .relation(flotilla_resources::MessageRelation::Supervisor)
        .build();
    flotilla_messaging::MessageInbox::new(case.backend.clone(), "flotilla")
        .accept(&InputMeta::builder().name(name.into()).build(), &spec, Utc::now())
        .await
        .expect("message accepted");
}

/// Records Message `name` delivered to the coder's session at `at`, as its delivery does.
async fn deliver_to_coder(case: &Case, name: &str, at: DateTime<Utc>) {
    let messages = case.backend.using::<flotilla_resources::Message>("flotilla");
    let message = messages.get(name).await.expect("message");
    let mut status = message.status.clone().expect("accepted status");
    status.phase = flotilla_resources::MessagePhase::Delivered;
    status.since = at;
    status.resolved_receiver = Some(
        flotilla_resources::ResolvedMessageReceiver::builder()
            .crew_id(case.launch("resumed-coder"))
            .session("resumed-coder".into())
            .delivered_at(at)
            .evidence("turn".into())
            .build(),
    );
    messages.update_status(name, &message.metadata.resource_version, &status).await.expect("message delivered");
}

async fn idle_nudge_scenario() -> Case {
    actor_nudge_scenario(&[]).await
}

/// [`project_supervision_case`] once the coder is back at work: its stall settled and its
/// session, `resumed-coder`, running in the work's vessel.
async fn actor_nudge_scenario(governors: &[(&str, u64, ConvoyPhase)]) -> Case {
    let case = project_supervision_case(governors).await;
    case.note(|status| status.stalled = None).await;
    resume_coder(&case);
    case
}

/// Starts the coder's session, `resumed-coder`, in the work's vessel; returns its launch.
fn resume_coder(case: &Case) -> String {
    case.home.session(&case.vessel, "coder", "resumed-coder", "env")
}

async fn observe_actor(case: &Case, wake: &ReconcilerWake, state: TerminalAttentionState, now: DateTime<Utc>) {
    observe_actor_source(case, wake, state, TerminalAttentionSource::Screen, now).await;
}

async fn observe_actor_source(
    case: &Case,
    wake: &ReconcilerWake,
    state: TerminalAttentionState,
    source: TerminalAttentionSource,
    now: DateTime<Utc>,
) {
    observe_session(&case.home, "resumed-coder", Some(TerminalAttention { state, as_of: now, source }));
    observe_agent(&case.home, "resumed-coder", state, source, now);
    case.judge(wake, now).await;
}

/// This home observes `attention` of session `session`'s running launch.
fn observe_session(home: &EngineHome, session: &str, attention: Option<TerminalAttention>) {
    let launch = home.engine.terminals.record(session).name;
    let terminals = home.engine.terminals.terminals.clone();
    terminals.observations().update(&launch, |observation| observation.attention = attention);
}

/// Records an observation of the agent in session `session`'s running launch on its AgentSession,
/// as the daemon does beside the terminal's attention (#3108). An unobservable screen records
/// nothing.
fn observe_agent(home: &EngineHome, session: &str, state: TerminalAttentionState, source: TerminalAttentionSource, at: DateTime<Utc>) {
    let launch = home.engine.terminals.record(session).name;
    let activity = match state {
        TerminalAttentionState::Working => flotilla_protocol::AgentActivity::Working,
        TerminalAttentionState::Idle => flotilla_protocol::AgentActivity::Idle,
        TerminalAttentionState::NeedsInput => flotilla_protocol::AgentActivity::NeedsInput,
        TerminalAttentionState::Unobservable => return,
    };
    let cause = match source {
        TerminalAttentionSource::Screen => flotilla_resources::agent_activity::SCREEN_CAUSE,
        TerminalAttentionSource::Hook => "hook",
    };
    agents().observe(&launch, activity, cause, at);
    if source == TerminalAttentionSource::Hook {
        agents().progress.hook(session, at);
    }
}

async fn observe_claude_hook(case: &Case, wake: &ReconcilerWake, event: &str, now: DateTime<Utc>) {
    use flotilla_agent::hooks::ClaudeCodeParser;
    use flotilla_agent::hooks::HarnessHookParser;
    let parsed = ClaudeCodeParser.parse_event(event, br#"{"session_id":"claude-scenario"}"#).expect("Claude hook");
    let state = match parsed.event_type {
        flotilla_protocol::AgentEventType::Active | flotilla_protocol::AgentEventType::ToolActive => TerminalAttentionState::Working,
        flotilla_protocol::AgentEventType::Idle => TerminalAttentionState::Idle,
        other => panic!("unexpected hook: {other:?}"),
    };
    if parsed.event_type == flotilla_protocol::AgentEventType::ToolActive {
        agents().progress.tool("resumed-coder", now);
    }
    observe_actor_source(case, wake, state, TerminalAttentionSource::Hook, now).await;
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
    let crewed = !matches!(unavailable, GovernorUnavailable::CrewMissing);
    let case = project_supervision_case(&[]).await;
    // A governor whose workflow crews no governor seat is a governor without its crew.
    let first = admit_governor(&case, "governor", 1, if crewed { "governor" } else { "deputy" }).await;
    let delivery = case.delivery.clone();
    let convoys = case.backend.using::<Convoy>("flotilla");
    let mut wake = case.wake.clone();
    match unavailable {
        GovernorUnavailable::DeliveryError => delivery.unavailable.store(true, Ordering::SeqCst),
        GovernorUnavailable::NoLiveGeneration => {
            let governor = case.home.view(&case.backend, &first).await;
            case.home.engine().abandon(&governor, "operator", "abandoned").await.expect("abandon the governor");
        }
        GovernorUnavailable::CrewMissing => {}
    }
    let now = Utc::now();
    for restart in restarts {
        if *restart {
            wake = case.restarted_wake().await;
        }
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("judge unavailable governor");
        let stalled = case.status().await.stalled.expect("stall");
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
    let began_at = case.status().await.stalled.as_ref().expect("stall").began_at;
    if matches!(record, FallbackRecord::LegacyExhausted | FallbackRecord::LegacyExhaustedCursor) {
        // Previous generation persisted temporary unavailability as exhausted.
        case.note(|status| {
            status.stalled.as_mut().expect("stall").supervision_exhausted = true;
            if matches!(record, FallbackRecord::LegacyExhaustedCursor) {
                status.stalled.as_mut().expect("stall").supervision_index = Some(1);
            }
        })
        .await;
        wake = case.restarted_wake().await;
    }
    let reachable = if crewed && !matches!(unavailable, GovernorUnavailable::NoLiveGeneration) {
        first
    } else {
        // An abandoned generation stays terminal, and a crewless one stays crewless. Startup
        // eventually reveals a new live generation rather than resurrecting the old one.
        admit_governor(&case, "governor", 2, "governor").await
    };
    delivery.unavailable.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("judge reachable governor");
        let stalled = case.status().await.stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Governor, "unavailability {unavailable:?}");
        assert_eq!(stalled.began_at, began_at);
        assert_eq!(stalled.supervisor.expect("supervisor").convoy, reachable);
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    }
}

/// Launches generation `generation` of wheelhouse's governor role, as `name`, its `govern`
/// vessel crewed by an agent `seat` at work; returns the launch.
async fn admit_governor(case: &Case, name: &str, generation: u64, seat: &str) -> String {
    let spec = ConvoySpec::builder()
        .workflow_ref("workflow".into())
        .project_ref("wheelhouse".into())
        .role("governor".into())
        .generation(generation)
        .build();
    let mut status = single_agent(seat);
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].name = "govern".into();
    let launch = case.home.admit(name, spec, status).await;
    let vessel = case.home.vessel(&launch, "govern", "env");
    case.home.session(&vessel, seat, &format!("{name}-{generation}-{seat}"), "env");
    launch
}

/// The supervisor's running session, `governor-session` in the work's `supervise` vessel, with
/// this attention observed.
fn governor_session(case: &Case, supervise: &str, governor: Option<TerminalAttentionState>, now: DateTime<Utc>) {
    case.home.session(supervise, "governor", "governor-session", "env");
    let attention = governor.map(|state| TerminalAttention { state, as_of: now, source: TerminalAttentionSource::Hook });
    observe_session(&case.home, "governor-session", attention);
    if let Some(state) = governor {
        observe_agent(&case.home, "governor-session", state, TerminalAttentionSource::Hook, now);
    }
}

/// A coder beside its convoy's crew supervisor, whose session runs with this
/// attention, under a supervision policy naming that supervisor first. The coder
/// has either declared its stall or is working with its own session observed.
async fn coder_beside_governor(declared: bool, governor: Option<TerminalAttentionState>, now: DateTime<Utc>) -> Case {
    let mut workflow = supervised_workflow();
    workflow.vessels.push(
        flotilla_resources::VesselRequirement::builder()
            .name("supervise".into())
            .crew(vec![flotilla_engine_convoys_testkit::agent("governor")])
            .build(),
    );
    workflow.supervision = Some(vec![
        flotilla_resources::SupervisionTarget::ConvoyCrew { vessel: "supervise".into(), role: "governor".into() },
        flotilla_resources::SupervisionTarget::Operator,
    ]);
    let case = supervised_case(workflow, &[]).await;
    if !declared {
        case.note(|status| status.stalled = None).await;
        resume_coder(&case);
    }
    let supervise = case.home.vessel(&case.work, "supervise", "env");
    governor_session(&case, &supervise, governor, now);
    let id = uuid::Uuid::new_v4();
    case.wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf(LeafAddress::Work { convoy: case.work.clone(), work: "supervise".into() }, ".crew.governor.phase", "Done")],
            watcher: LeafWatcher::ReconcilerWake { convoy: case.work.clone() },
            maker: LeafMaker::Actor { vessel: "supervise".into(), role: "governor".into() },
            freshness_demand: None,
            created_at: now,
            episode_key: EpisodeKeyFields::default(),
        },
    );
    case
}

// #3893: a crew's declared stall escalates to its convoy's crew supervisor on
// its own evidence. Neither the supervisor's unjudged row (running, no attention
// yet) nor its unable row (needs input), in either row order, holds the stall
// at the operator rung.
#[tokio::test]
async fn a_declared_crew_stall_reaches_its_supervisor_beside_other_rows() {
    for governor in [None, Some(TerminalAttentionState::NeedsInput)] {
        // Rows are judged in the subscription table's hash order, which a test
        // cannot fix; 16 runs miss an order with probability about 3e-5.
        for _ in 0..16 {
            let now = Utc::now();
            let case = coder_beside_governor(true, governor, now).await;
            case.wake.judge_stalls_at("flotilla", &case.objects().await, now).await.expect("judge");
            let stalled = case.status().await.stalled.expect("stalled");
            assert_eq!(stalled.source, StallEvidenceSource::Crew, "governor {governor:?}");
            let supervisor = stalled.supervisor.expect("the crew rung");
            assert_eq!((supervisor.vessel.as_str(), supervisor.role.as_str()), ("supervise", "governor"), "governor {governor:?}");
            assert_eq!(case.delivery.requests.lock().expect("deliveries").iter().filter(|request| request.role == "governor").count(), 1);
        }
    }
}

// #3893: only the crew's own declaration overrides its neighbours. A coder
// unable on its session's evidence (needs input), beside a working supervisor,
// is still held: no stall is raised and nothing is escalated.
#[tokio::test]
async fn an_undeclared_unable_row_beside_an_able_row_still_holds() {
    for _ in 0..4 {
        let now = Utc::now();
        let case = coder_beside_governor(false, Some(TerminalAttentionState::Working), now).await;
        observe_actor_source(&case, &case.wake, TerminalAttentionState::NeedsInput, TerminalAttentionSource::Hook, now).await;
        let status = case.status().await;
        assert!(status.stalled.is_none(), "an able neighbour holds an undeclared stall: {:?}", status.stalled);
        assert!(case.delivery.requests.lock().expect("deliveries").is_empty(), "nothing escalated");
    }
}

// A crew's own stall remains actionable across judge ticks even when its
// workflow declares no claims. Settling the crew's seat removes that obligation.
#[tokio::test]
async fn promise_less_crew_stall_survives_judge_ticks() {
    for resolved in [CrewWorkPhase::Failed, CrewWorkPhase::HandedBack] {
        let mut workflow = supervised_workflow();
        let member = &mut workflow.vessels[0].crew[0];
        member.promises.clear();
        member.completion_conditions.clear();
        let case = supervised_case(workflow, &[]).await;
        case.wake.subscriptions.inner.rows.lock().await.clear();
        let now = Utc::now();
        for tick in 0..4 {
            let objects = case.objects().await;
            case.wake.sync_rows("flotilla", &objects).await.expect("sync actor rows");
            case.wake.judge_stalls_at("flotilla", &objects, now + chrono::Duration::seconds(tick)).await.expect("judge tick");
            let status = case.status().await;
            assert!(status.stalled.is_some(), "crew stall erased at tick {tick}");
            assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
        }
        case.home.seat(&case.backend, &case.work, (&case.vessel, "work"), ("coder", "resumed-coder"), resolved).await;
        let objects = case.objects().await;
        case.wake.sync_rows("flotilla", &objects).await.expect("sync resolved rows");
        case.wake.judge_stalls_at("flotilla", &objects, now + chrono::Duration::seconds(5)).await.expect("judge resolution");
        let status = case.status().await;
        assert!(status.stalled.is_none(), "resolved {resolved:?} at {:?}: {:?}", status.phase, status.stalled);
    }
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
    let home = EngineHome::install(&backend, "flotilla", "test-host");
    let (event_tx, _) = broadcast::channel(16);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "test-host".to_string(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx.clone()), refresher);
    let connection_id = uuid::Uuid::new_v4();
    let mut events = event_tx.subscribe();
    // A wait can name a launch before it is admitted: admission reserves it first.
    let demo = home.reserve("demo").await;

    let unknown = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(LeafAddress::Convoy { name: demo.clone() }, ".status.typo", "Landed")],
        freshness_demand: None,
    };
    let error = table.subscribe_wait(connection_id, unknown).await.expect_err("unknown path admission");
    assert!(error.contains("admitted vocabulary"));
    assert!(error.contains("work.latest-claim.disposition"));

    let absent = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![leaf(LeafAddress::Convoy { name: demo.clone() }, ".status.phase", "Landed")],
        freshness_demand: None,
    };
    let absent_id = table.subscribe_wait(connection_id, absent).await.expect("subscribe absent leaf");
    assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "absent record must remain unknown");

    // The coder owes a decision ledger, so its launch lands once that is kept.
    let mut status = single_agent("coder");
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0].promises =
        vec![flotilla_resources::TemplatePromise::decision_ledger("coder")];
    home.admit_reserved(&demo, convoy_spec(), status).await;
    home.vessel(&demo, "work", "env");
    assert_eq!(home.view(&backend, &demo).await.status.expect("status").phase, ConvoyPhase::Active);
    assert!(tokio::time::timeout(Duration::from_millis(40), events.recv()).await.is_err(), "false leaf must not fire");
    home.keep(&backend, &demo, "work", "coder").await;
    home.end(&backend, &demo, ConvoyPhase::Landed).await;
    let fire = receive_fire(&mut events, absent_id).await;
    assert_eq!(fire.leaf.address, LeafAddress::Convoy { name: demo.clone() });
    assert_eq!(fire.value, "Landed");

    let immediate = WaitSubscriptionRequest {
        namespace: "flotilla".to_string(),
        leaves: vec![
            leaf(LeafAddress::Vessel { name: "missing".to_string() }, ".status.phase", "Ready"),
            leaf(LeafAddress::Convoy { name: demo.clone() }, ".status.phase", "Landed"),
        ],
        freshness_demand: None,
    };
    let immediate_id = table.subscribe_wait(connection_id, immediate).await.expect("subscribe immediate OR-set");
    assert_eq!(receive_fire(&mut events, immediate_id).await.leaf.address, LeafAddress::Convoy { name: demo.clone() });

    let recorded_at = Utc::now();
    let artifact_address = LeafAddress::Artifact {
        convoy: demo.clone(),
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
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address(&demo, "reviewer", "review-round", "head-X").expect("artifact address"),
    )
    .expect("record name");
    backend
        .using::<Artifact>("flotilla")
        .create(
            &InputMeta::builder().name(name).build(),
            &flotilla_resources::ArtifactSpec::builder()
                .convoy(demo.clone())
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
async fn merged_unclaimed_scenario(repeats: usize, unknown_head: bool, stalled: bool, open_ledger: bool) {
    use flotilla_resources::convoy::engine::Actor;
    use flotilla_resources::{Observation, ObservedChangeRequestState};
    let (backend, home) = engine_backend();
    let wake = supervision_wake(&backend, &home);
    let table = &wake.subscriptions;
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let convoys = backend.using::<Convoy>("flotilla");
    let base = Utc::now() - chrono::Duration::seconds(2);
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "flotilla-org/flotilla", 2596);
    // The home's Host is the forge's only observer, so the record is its own: a foreign
    // authority would be taken over, and its observation dropped, once a row demands it.
    records
        .create(
            &InputMeta::builder().name(name.clone()).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("flotilla-org/flotilla".into())
                .number(2596)
                .observing_authority("test-host".into())
                .build(),
        )
        .await
        .expect("PR record");
    // Pending and terminal crews do not owe an active settlement turn.
    for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Done, CrewWorkPhase::Failed, CrewWorkPhase::HandedBack] {
        let launch = produced_pr_launch(&backend, &home, "single-agent", "coder", phase).await;
        let updated = convoys.get(&launch).await.expect("inactive crew");
        wake.sync_rows("flotilla", &HashMap::from([(launch, updated)])).await.expect("inactive subscriptions");
        assert!(
            !table
                .rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed")),
            "{phase:?}"
        );
    }
    // An unclaimed crew still at work. (A crew reads Interrupted only once its vessel is lost,
    // which interrupts the launch too, and an Interrupted launch arms no rows.)
    let launch = produced_pr_launch(&backend, &home, "single-agent", "coder", CrewWorkPhase::Working).await;
    let launch = launch.as_str();
    if stalled {
        use flotilla_resources::promises::{PromiseKind, PromiseOperation, PromiseSource, Submission};
        let engine = home.engine();
        // The launch owes the workflow's promises: the crew submits its PR to the PR promise, and
        // without an open ledger an operator waives the ledger promise.
        let mut operations = vec![(
            Actor::Crew,
            PromiseOperation::Submit {
                id: "template/pr".into(),
                kind: PromiseKind::Pr,
                source: PromiseSource::Crew,
                submission: Submission {
                    reference: "cr/github.com/flotilla-org/flotilla/2596".into(),
                    metadata: BTreeMap::new(),
                    submitted_at: base,
                    verdict: None,
                },
            },
        )];
        if !open_ledger {
            operations.push((
                Actor::Operator { principal: "operator".into() },
                PromiseOperation::Retract { id: "template/decision-ledger".into(), reason: "not required in this scenario".into() },
            ));
        }
        for (actor, operation) in operations {
            let convoy = convoys.get(launch).await.expect("convoy");
            engine.promise(&convoy, "work", "coder", &operation, actor).await.expect("the promise edit");
        }
        let status = home
            .note(&backend, launch, |status| {
                external_patches::mark_crew_stalled(
                    launch.into(),
                    "work".into(),
                    "coder".into(),
                    base,
                    flotilla_resources::StallReason::Infra,
                    None,
                    "waiting on PR".into(),
                )
                .apply(status)
            })
            .await
            .status
            .expect("status");
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
        assert_eq!(status.stalled.as_ref().expect("crew stall").rung, StallRung::Operator);
    }
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
    // Arm this launch's rows before the PR is observed: the sync retires the earlier launches'
    // rows, and the record they alone demanded is collected and recreated unobserved. Disarm
    // their watches so the merged observation fires only through the deterministic evaluation
    // below.
    let objects = HashMap::from([(launch.to_owned(), convoys.get(launch).await.expect("convoy"))]);
    wake.sync_rows("flotilla", &objects).await.expect("active subscriptions");
    for task in table.inner.tasks.lock().await.drain().map(|(_, task)| task) {
        task.abort();
    }
    let record = records.get(&name).await.expect("record");
    let record = records.update_status(&name, &record.metadata.resource_version, &observed).await.expect("merged observation");
    if stalled {
        let observed = observe_promise(&home, &backend, launch, &BTreeMap::from([(name.clone(), record.clone())]), &BTreeMap::new()).await;
        assert!(observed, "PR merge keeps submitted promise");
        let status = convoys.get(launch).await.expect("convoy").status.expect("status");
        let submitted = flotilla_resources::promises::owned(&status, "work", "coder")
            .iter()
            .find(|promise| promise.id == "template/pr")
            .expect("submitted promise")
            .state;
        assert_eq!(submitted, flotilla_resources::promises::PromiseState::Kept);
        if !open_ledger {
            // With the ledger waived, the merge keeps the last promise the crew owed: that is its
            // claim, so the launch lands and no merged-unclaimed turn is armed.
            assert_eq!(status.phase, ConvoyPhase::Landing);
            let objects = HashMap::from([(launch.to_owned(), convoys.get(launch).await.expect("convoy"))]);
            wake.sync_rows("flotilla", &objects).await.expect("subscriptions");
            assert!(!table
                .rows()
                .await
                .iter()
                .any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed")));
            return;
        }
    }
    for _ in 0..repeats {
        let objects = HashMap::from([(launch.to_owned(), convoys.get(launch).await.expect("convoy"))]);
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
            issues: &HashMap::new(),
            artifacts: &HashMap::new(),
        };
        let fire = evaluate_row(
            row,
            &subjects,
            &RowDependencies::derive(row, &[]).names,
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
                &RowDependencies::derive(competing, &[]).names,
                LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
            )
            .expect("competing evaluation")
            {
                table.fire(competing.id, fire).await;
            }
        }
        let LeafWatcher::TurnDelivery { source, rule, .. } = &row.watcher else { unreachable!() };
        table.deliver_turn(row.id, launch, source, rule, &row.leaves[0]).await.expect("duplicate delivery");
        assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
        let current = convoys.get(launch).await.expect("convoy");
        assert_eq!(current.status.expect("status").turn_deliveries["merged-unclaimed"].episodes.len(), 1);
    }
    let brief = actuator.requests.lock().expect("requests")[0].brief.clone();
    // With no pending promises, merged settlement preserves the existing brief
    // without appending an empty promise-resolution section.
    if !stalled {
        assert!(!brief.contains("Resolve these non-terminal promises"));
    }
    if stalled {
        assert_eq!(brief.contains("- `template/decision-ledger` (decision-ledger)"), open_ledger);
        assert!(!brief.contains("- `template/pr`"));
        let current = convoys.get(launch).await.expect("resumed");
        assert_eq!(current.status.as_ref().expect("status").crew_work["work"]["coder"].phase, CrewWorkPhase::Working);
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
    if stalled {
        if open_ledger {
            let artifacts = backend.using::<Artifact>("flotilla");
            let ledger_name = flotilla_resources::address::record_name(
                &flotilla_resources::address::artifact_address(launch, "coder", "decision-ledger", launch).expect("artifact address"),
            )
            .expect("record name");
            let ledger = artifacts
                .create(
                    &InputMeta::builder().name(ledger_name.clone()).build(),
                    &flotilla_resources::ArtifactSpec::builder()
                        .convoy(launch.into())
                        .producer("coder".into())
                        .kind("decision-ledger".into())
                        .subject(launch.into())
                        .digest("ledger-digest".into())
                        .size(1)
                        .media_type("text/markdown".into())
                        .expires_at(Utc::now() + chrono::Duration::days(1))
                        .build(),
                )
                .await
                .expect("ledger artifact");
            let artifacts = BTreeMap::from([(ledger_name, ledger)]);
            let change_requests = BTreeMap::from([(name.clone(), record.clone())]);
            for _ in 0..8 {
                if !observe_promise(&home, &backend, launch, &change_requests, &artifacts).await {
                    break;
                }
            }
        }
        let status = convoys.get(launch).await.expect("convoy").status.expect("status");
        assert!(flotilla_resources::promises::pending(&status, "work", "coder").is_empty());
        // Every promise kept is the crew's settlement: the launch lands once delivered.
        assert_eq!(status.phase, ConvoyPhase::Landing);
        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                // Primary status watches drive completion rollup; use the
                // explicit merged observation rather than a network refresher.
                secondaries: Vec::new(),
                reconciler: ConvoyReconciler::new()
                    .with_lifecycle_reads(home.reads())
                    .with_engine(home.path())
                    .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), Duration::from_secs(60)),
                backend: backend.clone(),
            }
            .run(),
        );
        let landed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if convoys.get(launch).await.expect("convoy").status.expect("status").phase == ConvoyPhase::Landed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            landed.is_ok(),
            "merged stalled crew completes and convoy lands: {:?}",
            convoys.get(launch).await.expect("held convoy").status
        );
        controller.abort();
        // The stalled scenario ends at Landed; cached-row and claimed-crew
        // assertions below exercise the non-stalled path before landing.
        return;
    }
    // The crew settles: an operator keeps what it owes.
    home.keep(&backend, launch, "work", "coder").await;
    assert_eq!(convoys.get(launch).await.expect("convoy").status.expect("status").phase, ConvoyPhase::Landing);
    let stale_row = table
        .rows()
        .await
        .into_iter()
        .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed"))
        .expect("cached merge row");
    let LeafWatcher::TurnDelivery { source, rule, .. } = &stale_row.watcher else { unreachable!() };
    // A cached firing must recheck the parked settlement claim. Clear the episode to prove
    // eligibility, rather than deduplication, prevents reopening a claimed crew.
    home.note(&backend, launch, |status| status.turn_deliveries.clear()).await;
    table.deliver_turn(stale_row.id, launch, source, rule, &stale_row.leaves[0]).await.expect("cached firing after claim");
    assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
    let updated = convoys.get(launch).await.expect("claimed convoy");
    wake.sync_rows("flotilla", &HashMap::from([(launch.to_owned(), updated)])).await.expect("settled subscriptions");
    assert!(!table
        .rows()
        .await
        .iter()
        .any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "merged-unclaimed")));
}

// #2596: an active crew with a produced PR yields while checks are pending;
// settled checks deliver once per head, with the existing episode ceiling.
/// The PR the settlement scenarios' crews produce.
fn produced_pr() -> flotilla_protocol::Subject {
    flotilla_protocol::Subject {
        kind: flotilla_protocol::SubjectKind::ChangeRequest,
        source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
        id: "2596".into(),
    }
}

/// Records the next promise observation `change_requests` and `artifacts` show of launch `launch`,
/// as the convoy reconciler records it; whether there was one.
async fn observe_promise(
    home: &EngineHome,
    backend: &ResourceBackend,
    launch: &str,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    artifacts: &BTreeMap<String, ResourceObject<Artifact>>,
) -> bool {
    let convoy = home.view(backend, launch).await;
    let status = convoy.status.as_ref().expect("status");
    let observation = flotilla_resources::promises::next_observation(
        status,
        launch,
        change_requests,
        artifacts,
        &[],
        Utc::now(),
        Duration::from_secs(60),
    );
    let Some(flotilla_resources::ConvoyStatusPatch::ObservePromise { vessel, role, operation }) = observation else {
        return false;
    };
    let observer = flotilla_resources::convoy::engine::Actor::Observer;
    home.engine().promise(&convoy, &vessel, &role, &operation, observer).await.expect("the observed promise edge");
    true
}

/// The stock workflow `workflow_ref` names, frozen as admission freezes it.
fn stock_snapshot(workflow_ref: &str) -> WorkflowSnapshot {
    let workflow = match workflow_ref {
        "implement-review" => flotilla_resources::implement_review_workflow_spec(),
        "single-agent" => flotilla_resources::single_agent_workflow_spec(),
        other => panic!("no stock workflow `{other}`"),
    };
    WorkflowSnapshot {
        cascade: None,
        stall_nudges: workflow.stall_nudges,
        supervision: workflow.supervision,
        exit: workflow.exit,
        turn_delivery: workflow.turn_delivery,
        vessels: workflow.vessels,
    }
}

/// A launch of `checks-wake` on stock workflow `workflow_ref`, running in its vessel, whose crew
/// claimed [`produced_pr`] as what it produces, and whose `role` seat reads `phase`.
async fn produced_pr_launch(backend: &ResourceBackend, home: &EngineHome, workflow_ref: &str, role: &str, phase: CrewWorkPhase) -> String {
    let status = ConvoyStatus { workflow_snapshot: Some(stock_snapshot(workflow_ref)), ..Default::default() };
    let launch = home.admit("checks-wake", ConvoySpec::builder().workflow_ref(workflow_ref.into()).build(), status).await;
    let vessel = home.vessel(&launch, "work", "env");
    let convoy = home.view(backend, &launch).await;
    home.engine().link(&convoy, &produced_pr(), SubjectDiscoverySource::Claim, "crew").await.expect("the crew claims its PR");
    let session = format!("{}-{role}", launch.replace('#', "-"));
    home.seat(backend, &launch, (&vessel, "work"), (role, &session), phase).await;
    launch
}

/// The single-agent status [`launch_at`] admits: its coder owes a decision ledger.
fn ledger_owed() -> ConvoyStatus {
    let mut status = single_agent("coder");
    status.workflow_snapshot.as_mut().expect("workflow").vessels[0].crew[0].promises =
        vec![flotilla_resources::TemplatePromise::decision_ledger("coder")];
    status
}

/// Admits `name` with `status` (a [`ledger_owed`] one, or with more noted) and brings the launch to
/// `phase` by the facts that make it so: Active runs its vessel; Interrupted loses it; Landing
/// keeps what the coder owes; Landed then delivers it; Failed and Abandoned end it so. (No launch
/// reads Pending: one with no work to start is refused.) Returns the launch.
async fn launch_at(backend: &ResourceBackend, home: &EngineHome, name: &str, status: ConvoyStatus, phase: ConvoyPhase) -> String {
    let launch = home.admit(name, convoy_spec(), status).await;
    let vessel = home.vessel(&launch, "work", "env");
    match phase {
        ConvoyPhase::Interrupted => {
            home.engine.vessels.step(&vessel, VesselStep::Lost { reason: "environment lost".into() });
        }
        ConvoyPhase::Landing | ConvoyPhase::Landed => home.keep(backend, &launch, "work", "coder").await,
        _ => {}
    }
    if phase.is_terminal() {
        home.end(backend, &launch, phase).await;
    }
    assert_eq!(home.view(backend, &launch).await.status.expect("status").phase, phase, "the launch's facts read as its phase");
    launch
}

/// Admits `name` with `spec` and exit `exit`, its coder owing a decision ledger, starts the
/// coder, and keeps its ledger: the launch is Landing. Returns the launch.
async fn landing_launch(backend: &ResourceBackend, home: &EngineHome, name: &str, spec: ConvoySpec, exit: ExitDeclaration) -> String {
    let mut status = ledger_owed();
    status.workflow_snapshot.as_mut().expect("workflow").exit = Some(exit);
    let launch = home.admit(name, spec, status).await;
    let vessel = home.vessel(&launch, "work", "env");
    let session = format!("{}-coder", launch.replace('#', "-"));
    home.seat(backend, &launch, (&vessel, "work"), ("coder", &session), CrewWorkPhase::Working).await;
    home.keep(backend, &launch, "work", "coder").await;
    assert_eq!(home.view(backend, &launch).await.status.expect("status").phase, ConvoyPhase::Landing);
    launch
}

/// The convoy reconciler over `home`'s engine, woken by `table`, observing change requests
/// no older than `stale_after` when given it.
fn convoy_controller(
    backend: &ResourceBackend,
    home: &EngineHome,
    table: &LeafSubscriptionTable,
    stale_after: Option<Duration>,
) -> tokio::task::JoinHandle<Result<(), ResourceError>> {
    let reconciler = ConvoyReconciler::new().with_lifecycle_reads(home.reads()).with_engine(home.path());
    let reconciler = match stale_after {
        Some(stale_after) => reconciler.with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), stale_after),
        None => reconciler,
    };
    // The daemon's wakes: an evidence change (a change request observed merged) wakes the convoy
    // whose promise it settles, as well as the leaf rows' wakes.
    let mut secondaries = ConvoyReconciler::secondary_watches(backend, "flotilla", home.path());
    secondaries.push(table.reconciler_wake_watch());
    secondaries.push(Box::new(crate::convoy::EngineConvoyWake { engine: home.path() }));
    tokio::spawn(ControllerLoop { primary: backend.using::<Convoy>("flotilla"), secondaries, reconciler, backend: backend.clone() }.run())
}

/// The coder of launch `launch` submits the decision ledger it owes, at `reference`, at `at`.
async fn submit_ledger(home: &EngineHome, backend: &ResourceBackend, launch: &str, reference: &str, at: DateTime<Utc>) {
    use flotilla_resources::promises::{owned, PromiseKind, PromiseOperation, Submission};
    let convoy = home.view(backend, launch).await;
    let ledger = owned(convoy.status.as_ref().expect("status"), "work", "coder")
        .iter()
        .find(|promise| promise.kind == PromiseKind::DecisionLedger)
        .expect("the ledger the coder owes")
        .clone();
    let operation = PromiseOperation::Submit {
        id: ledger.id,
        kind: PromiseKind::DecisionLedger,
        source: ledger.source,
        submission: Submission { reference: reference.into(), metadata: BTreeMap::new(), submitted_at: at, verdict: None },
    };
    let crew = flotilla_resources::convoy::engine::Actor::Crew;
    home.engine().promise(&convoy, "work", "coder", &operation, crew).await.expect("the coder submits its ledger");
}

/// Whether this home's notes changed since `notes` last looked: a write the old store counted
/// as a status update.
/// Waits until `wake`'s views have heard `convoy` at its version, on their change events: a judge
/// skips a launch whose view lags, until the watch catches up.
async fn caught_up(wake: &ReconcilerWake, convoy: &ResourceObject<Convoy>) {
    let (name, current) = (&convoy.metadata.name, &convoy.metadata.resource_version);
    let cell = wake.views("flotilla").await.expect("views");
    let views = cell.get().expect("initialized views");
    let mut changes = views.convoys.subscribe_local_keys();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let heard = views.convoys.shared_named(name).await.expect("view");
            if heard.iter().any(|read| read.object.metadata.resource_version == *current) {
                return;
            }
            match changes.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("the convoy view stopped"),
            }
        }
    })
    .await
    .expect("the views hear the launch");
}

fn notes_changed(notes: &mut broadcast::Receiver<String>) -> bool {
    let mut changed = false;
    loop {
        match notes.try_recv() {
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => changed = true,
            Err(_) => return changed,
        }
    }
}

async fn active_checks_scenario(outcomes: &[bool], watch_events: bool, role: &str) {
    use flotilla_resources::{Observation, ObservedChecks};

    let (backend, home) = engine_backend();
    let wake = supervision_wake(&backend, &home);
    let table = &wake.subscriptions;
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let workflow = if role == "reviewer" { "implement-review" } else { "single-agent" };
    let convoys = backend.using::<Convoy>("flotilla");
    let records = backend.using::<ChangeRequest>("flotilla");
    let name = cr_name("github.com", "flotilla-org/flotilla", 2596);
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
    // Pending roles await handoff, and handed-back or failed roles must not be reopened by CI/review
    // probes. A Done seat has claimed by keeping its promises, and its landing launch still gets
    // check failures and actionable reviews after the claim (#3108 Q5). Each phase is a launch
    // whose seat's facts read so.
    let mut inactive = Vec::new();
    for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Done, CrewWorkPhase::HandedBack, CrewWorkPhase::Failed] {
        let launch = produced_pr_launch(&backend, &home, workflow, role, phase).await;
        let updated = convoys.get(&launch).await.expect("inactive crew");
        wake.sync_rows("flotilla", &HashMap::from([(launch.clone(), updated)])).await.expect("inactive subscriptions");
        let armed = table.rows().await.iter().any(|row| {
            matches!(&row.watcher, LeafWatcher::TurnDelivery { rule, .. }
            if matches!(flotilla_resources::legacy_field_path(&rule.on.path).as_str(), ".checks" | ".review.actionable-at-head"))
        });
        // A Done reviewer beside a coder still pending leaves its launch Active, where a claimed
        // seat is not probed.
        let landing = convoys.get(&launch).await.expect("inactive crew").status.expect("status").phase == ConvoyPhase::Landing;
        assert_eq!(armed, phase == CrewWorkPhase::Done && landing, "{phase:?}");
        inactive.push((phase, launch, landing));
    }
    // A working crew. (An Interrupted crew's launch is Interrupted too, and arms no rows.)
    let launch = produced_pr_launch(&backend, &home, workflow, role, CrewWorkPhase::Working).await;
    let updated = convoys.get(&launch).await.expect("active crew");
    wake.sync_rows("flotilla", &HashMap::from([(launch.clone(), updated)])).await.expect("active subscriptions");
    assert!(table.rows().await.iter().any(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && flotilla_resources::legacy_field_path(&rule.on.path) == ".checks")));
    let launch = launch.as_str();
    let objects = HashMap::from([(launch.to_owned(), convoys.get(launch).await.expect("convoy"))]);
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
                LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && flotilla_resources::legacy_field_path(&rule.on.path) == ".checks"
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
                    issues: &HashMap::new(),
                    artifacts: &HashMap::new(),
                },
                &RowDependencies::derive(&row, &[]).names,
                LeafObservationStaleness { change_request: Duration::from_secs(60), issue: Duration::from_secs(60) },
            )
            .expect("evaluate checks");
            assert_eq!(fire.is_some(), checks.is_some_and(|value| value != ObservedChecks::Pending));
            if let Some(fire) = fire {
                if watch_events {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let current = convoys.get(launch).await.expect("convoy").status.expect("status");
                            if current.turn_deliveries.get(source).is_some_and(|state| state.episodes.len() == index + 1) {
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
                table.deliver_turn(row.id, launch, source, rule, &row.leaves[0]).await.expect("duplicate observation");
            }
            let status = convoys.get(launch).await.expect("convoy").status.expect("status");
            let settled = checks.is_some_and(|value| value != ObservedChecks::Pending);
            // Each new head is progress: the episode limit never holds a
            // crew that keeps pushing review rounds (#3565).
            assert_eq!(status.turn_deliveries.get(source).map_or(0, |state| state.episodes.len()), index + usize::from(settled));
            assert_eq!(actuator.requests.lock().expect("requests").len(), index + usize::from(settled));
            if settled {
                // Agent-facing observations use the same plain status words
                // as the leaf vocabulary, including unknown review evidence.
                let requests = actuator.requests.lock().expect("requests");
                let brief = &requests.last().expect("settled turn").brief;
                assert!(brief.contains(&format!("- Checks: {}", if *pass { "pass" } else { "fail" })));
                assert!(brief.contains("- Review actionable at head: unknown"));
            }
        }
    }
    assert_eq!(actuator.holds.load(Ordering::SeqCst), 0);
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
    // settled head that has no delivery episode yet. At a Done seat of a landing
    // launch the head settled after its claim, so the cached row delivers.
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
    let mut requests_before = actuator.requests.lock().expect("requests").len();
    let holds_before = actuator.holds.load(Ordering::SeqCst);
    for (phase, inactive, landing) in &inactive {
        // Keep the old row armed to model delivery racing its removal: it fires for a launch
        // whose seat no longer works.
        table.deliver_turn(row.id, inactive, source, rule, &row.leaves[0]).await.expect("ignore cached inactive row");
        requests_before += usize::from(*phase == CrewWorkPhase::Done && *landing);
        assert_eq!(actuator.requests.lock().expect("requests").len(), requests_before, "{phase:?}");
        assert_eq!(actuator.holds.load(Ordering::SeqCst), holds_before, "{phase:?}");
        let current = convoys.get(inactive).await.expect("convoy").status.expect("status");
        assert_eq!(current.crew_work["work"][role].phase, *phase);
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
                LeafWatcher::TurnDelivery { rule, .. } if rule.to.role == role && flotilla_resources::legacy_field_path(&rule.on.path) == ".review.actionable-at-head"
            )
        })
        .expect("active review subscription");
    let LeafWatcher::TurnDelivery { source, rule, .. } = &review.watcher else { unreachable!() };
    let before = actuator.requests.lock().expect("requests").len();
    table.deliver_turn(review.id, launch, source, rule, &review.leaves[0]).await.expect("active review turn");
    table.deliver_turn(review.id, launch, source, rule, &review.leaves[0]).await.expect("duplicate review");
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
        table.deliver_turn(conflict.id, launch, source, rule, &conflict.leaves[0]).await.expect("active conflict turn");
        let brief = actuator.requests.lock().expect("requests").last().expect("conflict request").brief.clone();
        assert!(brief.starts_with(rule.brief.trim()));
        // #3083: turn briefs carry no settlement-claim pressure.
        assert!(!brief.contains("settlement claim"), "{brief}");
    }
    for row in table.rows().await {
        table.finish(row.id).await;
    }
}

async fn assert_turn_delivery_enforces_head_identity_and_records_rungs(
    source: &str,
    condition: &str,
    field_path: &str,
    literal: &str,
    actionable_review: bool,
    mergeability: flotilla_resources::ObservedMergeability,
    brief: &str,
) {
    let (backend, home) = engine_backend();
    let (event_tx, _) = broadcast::channel(4);
    let refresher = ChangeRequestRefresher::new(
        "fleet".to_string(),
        backend.clone(),
        "authority".to_string(),
        Arc::new(UnavailableChangeRequests),
        flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
    );
    let table = engine_table(backend.clone(), &home, broadcast_test_sink(event_tx), refresher);
    let actuator = Arc::new(RecordingTurnDelivery::default());
    table.set_turn_delivery_actuator(actuator.clone()).await;
    let rule = TurnDeliveryRule::builder()
        .on(flotilla_resources::parse_leaf_template(condition).expect("wake leaf"))
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
    let base = Utc::now();
    // The coder's work is done: it kept the decision ledger it owed, and the launch is landing.
    let mut status = single_agent("coder");
    let snapshot = status.workflow_snapshot.as_mut().expect("workflow");
    snapshot.exit = Some(ExitDeclaration::standard_table());
    snapshot.turn_delivery = indexmap::IndexMap::from([(source.to_string(), rule.clone())]);
    snapshot.vessels[0].crew[0].promises = vec![flotilla_resources::TemplatePromise::decision_ledger("coder")];
    let launch = home.admit("wake-turn", convoy_spec, status).await;
    let launch = launch.as_str();
    let vessel = home.vessel(launch, "work", "env");
    home.seat(&backend, launch, (&vessel, "work"), ("coder", "wake-turn-coder"), CrewWorkPhase::Working).await;
    submit_ledger(&home, &backend, launch, "https://github.com/flotilla-org/flotilla/pull/1392#issuecomment-1", base).await;
    home.keep(&backend, launch, "work", "coder").await;
    assert_eq!(convoys.get(launch).await.expect("convoy").status.expect("status").phase, ConvoyPhase::Landing);
    let cr_name = cr_name("github.com", "flotilla-org/flotilla", 1392);
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
            watcher: LeafWatcher::TurnDelivery { convoy: launch.to_owned(), source: source.to_string(), rule: Box::new(rule.clone()) },
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
    table.deliver_turn(subscription_id, launch, source, &rule, &leaf).await.expect("ignore stale firing");
    assert_eq!(table.inner.rows.lock().await[&subscription_id].episode_key.subject_revision, None);
    assert!(convoys.get(launch).await.expect("convoy").status.expect("status").turn_deliveries.is_empty());

    for (index, head) in ["aaa", "bbb", "ccc", "ddd"].into_iter().enumerate() {
        let claim_at = base + chrono::Duration::seconds((index * 2) as i64);
        let observed_at = claim_at + chrono::Duration::seconds(1);
        let cr_status = flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, observed_at),
            head_sha: flotilla_resources::Observation::known(head.to_string(), observed_at),
            // The fourth review round turns the checks green.
            checks: flotilla_resources::Observation::known(
                if index == 3 { flotilla_resources::ObservedChecks::Pass } else { flotilla_resources::ObservedChecks::Fail },
                observed_at,
            ),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(actionable_review, observed_at),
            },
            mergeable: flotilla_resources::Observation::known(mergeability, observed_at),
        };
        let updated = records.update_status(&record.metadata.name, &record_version, &cr_status).await.expect("observe head");
        record_version = updated.metadata.resource_version;
        table.deliver_turn(subscription_id, launch, source, &rule, &leaf).await.expect("process firing");
        if index == 0 {
            table.deliver_turn(subscription_id, launch, source, &rule, &leaf).await.expect("same head is a no-op");
        }
    }

    let status = convoys.get(launch).await.expect("convoy").status.expect("status");
    let episodes = &status.turn_deliveries[source].episodes;
    assert_eq!(episodes.len(), 4, "same-head redelivery must not create an episode");
    assert_eq!(episodes[0].sender, flotilla_resources::CrewMessageSender::Unknown);
    assert!(serde_json::to_value(&episodes[0]).expect("episode").get("sender").is_none());
    // Message admission is a workflow latch, not confirmation of an agent turn.
    assert_eq!(serde_json::to_value(&episodes[0].outcome).unwrap()["kind"], "message-accepted");
    assert!(
        matches!(&episodes[1].outcome, TurnDeliveryOutcome::MessageAccepted { rung: TurnDeliveryRung::FreshAgent, message, .. } if message.kind == "Message")
    );
    // Each round is a new head, so no run reaches the episode limit (#3565).
    assert!(episodes.iter().all(|episode| matches!(episode.outcome, TurnDeliveryOutcome::MessageAccepted { .. })));
    assert_eq!(actuator.requests.lock().expect("requests").len(), 4);
    assert_eq!(actuator.holds.load(Ordering::SeqCst), 0);
    assert!(status.attention.is_none());
    assert!(status.turn_delivery_holds().is_empty());
    let last_brief = actuator.requests.lock().expect("requests")[3].brief.clone();
    assert!(last_brief.contains("Head SHA: `ddd`") && last_brief.contains("- Checks: Some(Pass)"), "{last_brief}");
    let first_brief = &actuator.requests.lock().expect("requests")[0].brief;
    assert!(first_brief.contains("Head SHA: `aaa`"));
    assert_eq!(
        first_brief.contains("- Review feedback: https://github.com/flotilla-org/flotilla/pull/1392"),
        source == "review",
        "review link appears only in review turns"
    );
    assert!(first_brief.contains("feature/wake"));
    assert!(first_brief.contains(&format!("Durable convoy record: `flotilla/{launch}`")));
    // The ledger the brief names is the kept ledger promise's artifact record, not the reference
    // the coder submitted.
    let ledger = flotilla_resources::address::artifact_slot_record_name(launch, "coder", "decision-ledger", launch).expect("ledger slot");
    assert!(first_brief.contains(&format!("Decision ledger: {ledger}")), "{first_brief}");
}

/// A subscription table whose readings serve no held checkouts; a test that holds checkouts gives
/// the table its own readings.
fn served_table(
    backend: ResourceBackend,
    event_sink: Arc<dyn super::EventSink>,
    change_requests: ChangeRequestRefresher,
) -> LeafSubscriptionTable {
    LeafSubscriptionTable::new(backend, event_sink, change_requests).with_lifecycle_reads(agent_reads())
}

/// A subscription table over `home`'s engine, as a daemon serving it builds one.
fn engine_table(
    backend: ResourceBackend,
    home: &EngineHome,
    event_sink: Arc<dyn super::EventSink>,
    change_requests: ChangeRequestRefresher,
) -> LeafSubscriptionTable {
    LeafSubscriptionTable::new(backend, event_sink, change_requests).with_lifecycle_reads(home.reads()).with_convoy_engine(home.path())
}

mod change_request_authority;
mod interrupted_stalls;
mod operator_stalls;
mod settlement;
mod stalls_supervision;
mod subscriptions;
mod turn_delivery;

mod landing_stalls;

mod performance;
