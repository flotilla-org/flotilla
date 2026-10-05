#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{arg::shell_quote, DaemonEvent, Leaf, LeafAddress, LeafFire, LeafOperator, NodeId, WaitSubscriptionRequest};
use flotilla_resources::{
    actor_obligation, admit_leaf,
    controller::{SecondaryWatch, WorkQueueSender},
    evaluate_leaf, expected_change_request_leaves, external_patches, instantiate_exit, instantiate_turn_delivery, select_convoy_children,
    subject_relationship_conflicts, Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, Checkout, CheckoutSpec,
    ControllerRetry, Convoy, ConvoyAttention, ConvoyEnsure, ConvoyLeafSubject, ConvoyPhase, ConvoyStatus, CrewCompletionRefusal,
    CrewCompletionRefusalCause, Forge, HoldAct, InstantiatedExit, Issue, IssueLeafSubject, LeafMaker, NudgeObligation, ObservedChecks,
    Project, ReadResourceObject, ReadWatchEvent, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, RetryCeiling,
    StallEvidenceSource, StallNudge, StallRung, StallSupervisor, StalledCondition, StatusPatch, SupervisionTarget, TerminalAttention,
    TerminalAttentionSource, TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionSource, ThreeValue,
    TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRule, TurnDeliveryRung, Usage, UsageLeafSubject, Vessel, VesselLeafSubject,
    WatchEvent, WatchStart, WorkLeafSubject, WorkPhase, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};

use crate::{
    change_request_observer::{ChangeRequestRef, ChangeRequestRefresher},
    event_sink::EventSink,
    in_process::convoy_message_address,
    issue_observer::{IssueObservationSource, IssueRef, IssueRefreshCadence, IssueRefresher},
    providers::change_request::ObservationError,
};

struct UnavailableIssues;

#[async_trait]
impl IssueObservationSource for UnavailableIssues {
    async fn observe(&self, _subject: &IssueRef) -> Result<flotilla_resources::IssueStatus, String> {
        Err("issue observation source unavailable".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafWatcher {
    WaitCaller { connection_id: uuid::Uuid },
    ReconcilerWake { convoy: String },
    TurnDelivery { convoy: String, source: String, rule: Box<TurnDeliveryRule> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeKeyFields {
    pub source: Option<String>,
    pub convoy: Option<String>,
    pub vessel: Option<String>,
    pub role: Option<String>,
    pub subject_revision: Option<String>,
}

pub use flotilla_protocol::TurnDeliveryRequest;

#[async_trait]
pub trait RemoteTurnDelivery: Send + Sync {
    async fn deliver(self: Arc<Self>, request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String>;
}

#[async_trait]
pub trait TurnDeliveryActuator: Send + Sync {
    async fn deliver(&self, request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String>;
    async fn hold(&self, request: &TurnDeliveryRequest, act: &HoldAct, reason: &str) -> Result<(), String>;
}

// Bound silent turns, not their total runtime: long tool-using turns stay able.
const TURN_INACTIVITY_BOUND: chrono::Duration = chrono::Duration::minutes(15);

fn turn_inactivity_reason(
    status: &flotilla_resources::TerminalSessionStatus,
    obligation: &NudgeObligation,
    now: DateTime<Utc>,
) -> Option<String> {
    let since = obligation.working_since?;
    let hook = status
        .attention
        .as_ref()
        .filter(|attention| attention.source == TerminalAttentionSource::Hook && attention.state != TerminalAttentionState::Unobservable)
        .map(|attention| attention.as_of);
    let last_activity =
        [status.last_tool_activity_at, status.last_output_activity_at, obligation.last_hook_at, hook, obligation.reply_after]
            .into_iter()
            .flatten()
            .fold(since, std::cmp::max);
    let quiet = now.signed_duration_since(last_activity);
    (quiet >= TURN_INACTIVITY_BOUND).then(|| format!(
        "turn inactivity bound exceeded: turn began {since}, last tool {:?}, last hook {:?}, last output {:?}, latest attention {:?}; no tool, hook, or output activity for {} seconds; automatic interrupt deferred to supervisor",
        status.last_tool_activity_at, obligation.last_hook_at.or(hook), status.last_output_activity_at, status.attention.as_ref(), quiet.num_seconds()
    ))
}

// A long first turn may be legitimate; missing hooks are advisory, never a stall.
const FIRST_HOOK_GRACE: chrono::Duration = chrono::Duration::minutes(15);

fn missing_turn_hook(session: &ResourceObject<TerminalSession>, obligations: &[NudgeObligation], now: DateTime<Utc>) -> Option<String> {
    if !matches!(session.spec.source, TerminalSessionSource::Agent { .. }) {
        return None;
    }
    let status = session.status.as_ref()?;
    let crew = status.crew.as_ref()?;
    if status.phase != TerminalSessionPhase::Running || crate::agents::parser_for_harness(&crew.adapter).is_err() {
        return None;
    }
    let started = status.started_at?;
    let vessel = session.metadata.labels.get(VESSEL_LABEL)?;
    let hook_seen = status.last_tool_activity_at.is_some_and(|at| at >= started)
        || status.attention.as_ref().is_some_and(|attention| {
            attention.source == TerminalAttentionSource::Hook
                && attention.state != TerminalAttentionState::Unobservable
                && attention.as_of >= started
        }) || obligations.iter().any(|obligation| {
        matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role } if actor_vessel == vessel && role == &session.spec.role)
            && obligation.last_hook_at.is_some_and(|at| at >= started)
    });
    (!hook_seen && now.signed_duration_since(started) >= FIRST_HOOK_GRACE).then(|| {
        format!("{} crew {}@{} has delivered no hook since session {} started at {}; check turn hook wiring (Codex notify / Claude managed settings)",
            crew.adapter, session.spec.role, vessel, session.metadata.name, started)
    })
}

fn actor_is_working(
    status: &flotilla_resources::TerminalSessionStatus,
    obligations: &[NudgeObligation],
    vessel: &str,
    role: &str,
    now: DateTime<Utc>,
) -> bool {
    let working = status
        .attention
        .as_ref()
        .is_some_and(|attention| attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now));
    let silent = obligations.iter().any(|obligation| {
        matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
            && turn_inactivity_reason(status, obligation, now).is_some()
    });
    working && !silent
}

#[derive(Debug)]
struct DeliveryError {
    reason: String,
    kind: flotilla_resources::TurnDeliveryFailureKind,
}
impl From<String> for DeliveryError {
    fn from(reason: String) -> Self {
        Self { reason, kind: flotilla_resources::TurnDeliveryFailureKind::Transient }
    }
}
impl DeliveryError {
    fn permanent(reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), kind: flotilla_resources::TurnDeliveryFailureKind::Permanent }
    }
}

struct UnavailableTurnDeliveryActuator;

#[async_trait]
impl TurnDeliveryActuator for UnavailableTurnDeliveryActuator {
    async fn deliver(&self, _request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
        Err("turn-delivery actuator unavailable".to_string())
    }

    async fn hold(&self, _request: &TurnDeliveryRequest, _act: &HoldAct, _reason: &str) -> Result<(), String> {
        Err("turn-delivery hold actuator unavailable".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafSubscriptionRow {
    pub id: uuid::Uuid,
    pub namespace: String,
    pub leaves: Vec<Leaf>,
    pub watcher: LeafWatcher,
    pub maker: LeafMaker,
    pub freshness_demand: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub episode_key: EpisodeKeyFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafFiringRecord {
    pub leaf: Leaf,
    pub value: String,
    pub fired_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct LeafSubscriptionTable {
    inner: Arc<LeafSubscriptionTableInner>,
}

struct LeafSubscriptionTableInner {
    backend: ResourceBackend,
    event_sink: Arc<dyn EventSink>,
    rows: Mutex<HashMap<uuid::Uuid, LeafSubscriptionRow>>,
    last_firings: Mutex<HashMap<(uuid::Uuid, Leaf), LeafFiringRecord>>,
    unable_since: Mutex<HashMap<uuid::Uuid, (UnableEvidenceKey, DateTime<Utc>)>>,
    stale_attention_reported: Mutex<HashSet<uuid::Uuid>>,
    tasks: Mutex<HashMap<uuid::Uuid, JoinHandle<()>>>,
    change_requests: ChangeRequestRefresher,
    issues: IssueRefresher,
    reconciler_tx: broadcast::Sender<String>,
    turn_delivery: Mutex<Arc<dyn TurnDeliveryActuator>>,
    episode_limit: u32,
    decisions: crate::decision_log::DecisionLog,
    supervisor_context: Mutex<HashMap<(String, String), String>>,
    #[cfg(test)]
    snapshot_loads: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnableEvidenceKey {
    Attention { state: TerminalAttentionState, source: TerminalAttentionSource },
    Absent,
}

fn stalled_source_actor(condition: &StalledCondition) -> Option<(&str, &str)> {
    let leaf = condition.leaves.first()?;
    let LeafAddress::Work { work, .. } = &leaf.address else { return None };
    let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
    Some((work, role))
}

/// Shared by supervisor delivery and the operator backstop, so both identify
/// the same crew and provide commands targeting the exact convoy record and crew.
fn stall_supervision_brief(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    let address = convoy_message_address(convoy);
    let actor =
        stalled_source_actor(condition).map(|(vessel, role)| format!("{role}@{vessel}")).unwrap_or_else(|| "unidentified crew".to_string());
    let reason = condition.reason.map_or_else(|| "inferred stall".to_string(), |reason| reason.to_string());
    let proposal = condition.proposed_disposition.map_or(String::new(), |disposition| format!(" Proposed disposition: {disposition}."));
    let mut brief = format!(
        "Supervise stalled crew {actor} in convoy {address} (resource ref: {}). Reason: {reason}. Evidence: {}.{proposal}",
        convoy.metadata.name, condition.evidence,
    );
    if let Some((vessel, role)) = stalled_source_actor(condition) {
        brief.push_str(" Resume it with guidance, convert it to failed, or escalate it.");
        for action in ["resume", "convert-to-failed", "escalate"] {
            write!(
                brief,
                "\n`flotilla crew supervise --convoy {} --vessel {} --role {} {action} --message 'guidance'`",
                shell_quote(&convoy.metadata.name),
                shell_quote(vessel),
                shell_quote(role),
            )
            .expect("writing to a String cannot fail");
        }
    }
    brief
}

// Keep each warning one event line while retaining copyable command text.
fn stall_supervision_log_brief(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    stall_supervision_brief(convoy, condition).replace('\n', " ")
}

const DEFAULT_REFUSAL_LIMIT: u32 = 2;
const DEFAULT_IDLE_GRACE_SECONDS: u32 = 180;

fn nudge_policy<'a>(status: &'a ConvoyStatus, vessel: &str, role: &str) -> Option<&'a flotilla_resources::StallNudgePolicy> {
    status.workflow_snapshot.as_ref()?.stall_nudges.get(&format!("{vessel}/{role}"))
}

fn idle_grace_seconds(status: &ConvoyStatus, vessel: &str, role: &str) -> u32 {
    nudge_policy(status, vessel, role).and_then(|policy| policy.idle_grace_seconds).unwrap_or(DEFAULT_IDLE_GRACE_SECONDS)
}

fn is_conflict_probe(leaf: &Leaf) -> bool {
    leaf.field_path == ".mergeable" && leaf.operator == LeafOperator::Equal && leaf.literal == "conflicting"
}

/// Workflow-declared checks and review rules run during active crew work too,
/// including custom rules; conflict probes retain their active delivery behavior.
fn is_active_change_request_probe(status: &ConvoyStatus, rule: &TurnDeliveryRule, leaf: &Leaf) -> bool {
    let active_field = is_conflict_probe(leaf)
        || (matches!(leaf.address, LeafAddress::ChangeRequest { .. })
            && matches!(leaf.field_path.as_str(), ".checks" | ".review.actionable-at-head"));
    status.phase == ConvoyPhase::Active
        && active_field
        && status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role)).is_some_and(|work| {
            work.finished_at.is_none()
                && (is_conflict_probe(leaf)
                    || matches!(work.phase, flotilla_resources::CrewWorkPhase::Working | flotilla_resources::CrewWorkPhase::Interrupted))
        })
}

fn refusal_nudge_brief(refusal: &CrewCompletionRefusal) -> String {
    let conflict = refusal.causes.iter().find_map(|cause| match cause {
        CrewCompletionRefusalCause::ConflictingChangeRequest { number, .. } => Some(*number),
        _ => None,
    });
    let missing = refusal.causes.iter().find_map(|cause| match cause {
        CrewCompletionRefusalCause::MissingChangeRequestObservation { number, .. } => Some(*number),
        _ => None,
    });
    // Prefer actionable conflicts over missing observations when several causes coexist.
    let remedy = if let Some(number) = conflict {
        format!("PR #{number} is conflicting: rebase onto the current base branch, rerun the gates, push, then `flotilla crew complete`.")
    } else if let Some(number) = missing {
        format!("PR #{number} has no observation yet: check the PR URL and forge access, then run `flotilla crew complete` again.")
    } else {
        "Resolve the unmet expectation, then run `flotilla crew complete` again.".to_string()
    };
    format!("Your settlement claim was refused: {}. {remedy}", refusal.expectation)
}

fn refusal_limit(status: &ConvoyStatus, vessel: &str, role: &str) -> u32 {
    nudge_policy(status, vessel, role).and_then(|policy| policy.max_refusals).unwrap_or(DEFAULT_REFUSAL_LIMIT).max(1)
}

const LEAF_WATCH_RECOVERY_INITIAL_DELAY: Duration = Duration::from_millis(25);
const LEAF_WATCH_RECOVERY_MAX_DELAY: Duration = Duration::from_secs(1);
const LEAF_WATCH_RECOVERY_RESET_AFTER: Duration = Duration::from_secs(5);

// One immediate burst recovery, then repeated expiries back off from 25 ms
// to one second. Five healthy seconds restore immediate burst recovery.
#[derive(Default)]
struct LeafWatchRecovery {
    delay: Duration,
}

impl LeafWatchRecovery {
    fn expired(&mut self, healthy_for: Duration) -> Duration {
        // Require sustained healthy consumption so intermittent lag cannot
        // repeatedly restore immediate retries and trigger snapshot bursts.
        if healthy_for >= LEAF_WATCH_RECOVERY_RESET_AFTER {
            self.delay = Duration::ZERO;
        }
        let delay = self.delay;
        self.delay = if delay.is_zero() { LEAF_WATCH_RECOVERY_INITIAL_DELAY } else { (delay * 2).min(LEAF_WATCH_RECOVERY_MAX_DELAY) };
        delay
    }
}

impl LeafSubscriptionTable {
    pub fn new(backend: ResourceBackend, event_sink: Arc<dyn EventSink>, change_requests: ChangeRequestRefresher) -> Self {
        Self::with_episode_limit(backend, event_sink, change_requests, 3)
    }

    pub fn with_episode_limit(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        episode_limit: u32,
    ) -> Self {
        let issues =
            IssueRefresher::new(backend.clone(), "unavailable".into(), Arc::new(UnavailableIssues), IssueRefreshCadence::default());
        Self::with_issues_and_episode_limit(backend, event_sink, change_requests, issues, episode_limit)
    }

    pub fn with_issues(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
    ) -> Self {
        Self::with_issues_and_episode_limit(backend, event_sink, change_requests, issues, 3)
    }

    fn with_issues_and_episode_limit(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
        episode_limit: u32,
    ) -> Self {
        let (reconciler_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(LeafSubscriptionTableInner {
                backend,
                event_sink,
                rows: Mutex::new(HashMap::new()),
                last_firings: Mutex::new(HashMap::new()),
                unable_since: Mutex::new(HashMap::new()),
                stale_attention_reported: Mutex::new(HashSet::new()),
                tasks: Mutex::new(HashMap::new()),
                change_requests,
                issues,
                reconciler_tx,
                turn_delivery: Mutex::new(Arc::new(UnavailableTurnDeliveryActuator)),
                episode_limit,
                decisions: Default::default(),
                supervisor_context: Default::default(),
                #[cfg(test)]
                snapshot_loads: AtomicUsize::new(0),
            }),
        }
    }

    /// Step locally authored convoys once. Supervisor and child context is read
    /// from the replica view during judgement; replica convoys are never actuated.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn reconcile_stalls_once(&self, namespace: &str) -> Result<(), String> {
        let convoys = self
            .inner
            .backend
            .using::<Convoy>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|convoy| (convoy.metadata.name.clone(), convoy))
            .collect();
        let wake = ReconcilerWake { subscriptions: self.clone(), _marker: PhantomData };
        wake.sync_rows(namespace, &convoys).await?;
        wake.judge_stalls(namespace, &convoys).await
    }

    pub async fn set_turn_delivery_actuator(&self, actuator: Arc<dyn TurnDeliveryActuator>) {
        *self.inner.turn_delivery.lock().await = actuator;
    }

    pub async fn subscribe_wait(&self, connection_id: uuid::Uuid, request: WaitSubscriptionRequest) -> Result<uuid::Uuid, String> {
        if request.leaves.is_empty() {
            return Err("wait requires at least one --for leaf".to_string());
        }
        let mut leaves = Vec::with_capacity(request.leaves.len());
        for leaf in request.leaves {
            admit_leaf(&leaf)?;
            if !leaves.contains(&leaf) {
                leaves.push(leaf);
            }
        }
        let id = uuid::Uuid::new_v4();
        let row = LeafSubscriptionRow {
            id,
            namespace: request.namespace,
            leaves,
            watcher: LeafWatcher::WaitCaller { connection_id },
            maker: LeafMaker::Observed { refresher: "caller".into(), external_party: "unknown".into() },
            freshness_demand: request.freshness_demand,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        };
        self.inner.rows.lock().await.insert(id, row.clone());
        for subject in row.leaves.iter().filter_map(|leaf| ChangeRequestRef::from_address(&row.namespace, &leaf.address)) {
            if let Err(error) = self.inner.change_requests.demand(id, subject, row.freshness_demand).await {
                self.inner.rows.lock().await.remove(&id);
                self.forget_firings(id).await;
                self.inner.change_requests.release(id).await;
                return Err(error);
            }
        }
        for subject in row.leaves.iter().filter_map(|leaf| IssueRef::from_address(&row.namespace, &leaf.address)) {
            if let Err(error) = self.inner.issues.demand(id, subject, row.freshness_demand).await {
                self.inner.rows.lock().await.remove(&id);
                self.forget_firings(id).await;
                self.inner.change_requests.release(id).await;
                self.inner.issues.release(id).await;
                return Err(error);
            }
        }
        let table = self.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = table.watch_row(row).await {
                tracing::warn!(subscription_id = %id, %error, "leaf subscription watch ended");
                table.finish(id).await;
            }
        });
        self.inner.tasks.lock().await.insert(id, task);
        Ok(id)
    }

    pub async fn unsubscribe_connection(&self, connection_id: uuid::Uuid) {
        let ids = self
            .inner
            .rows
            .lock()
            .await
            .values()
            .filter_map(|row| match row.watcher {
                LeafWatcher::WaitCaller { connection_id: owner } if owner == connection_id => Some(row.id),
                _ => None,
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.inner.rows.lock().await.remove(&id);
            self.forget_firings(id).await;
            if let Some(task) = self.inner.tasks.lock().await.remove(&id) {
                task.abort();
            }
            self.inner.change_requests.release(id).await;
            self.inner.issues.release(id).await;
        }
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
        Box::new(ReconcilerWake { subscriptions: self.clone(), _marker: PhantomData })
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.inner.change_requests.stale_after()
    }

    pub fn issue_stale_after(&self) -> Duration {
        self.inner.issues.stale_after()
    }

    pub async fn change_request_observation_error(&self, subject: &ChangeRequestRef) -> Option<ObservationError> {
        self.inner.change_requests.observation_error(subject).await
    }

    pub async fn refresh_change_request_once(&self, subject: &ChangeRequestRef) -> Result<(), ObservationError> {
        self.inner.change_requests.refresh_once(subject).await
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_relay_protocol::Subject) -> Result<(), String> {
        self.inner.change_requests.refresh_hint(hint).await?;
        self.inner.issues.refresh_hint(hint).await
    }

    pub async fn refresh_demanded_owned_change_requests(&self) -> Result<(), String> {
        self.inner.change_requests.refresh_demanded_owned().await?;
        self.inner.issues.refresh_demanded_owned().await
    }

    pub fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.inner.change_requests.set_relay_healthy(healthy);
        self.inner.issues.set_relay_healthy(healthy);
    }

    pub async fn rows(&self) -> Vec<LeafSubscriptionRow> {
        self.inner.rows.lock().await.values().cloned().collect()
    }

    pub async fn diagnostics(&self) -> Vec<(LeafSubscriptionRow, Vec<LeafFiringRecord>)> {
        let mut rows = self.rows().await;
        rows.sort_by_key(|row| row.created_at);
        let firings = self.inner.last_firings.lock().await;
        rows.into_iter()
            .map(|row| {
                let mut row_firings =
                    firings.iter().filter(|((id, _), _)| *id == row.id).map(|(_, firing)| firing.clone()).collect::<Vec<_>>();
                row_firings.sort_by_key(|firing| firing.fired_at);
                (row, row_firings)
            })
            .collect()
    }

    async fn finish(&self, id: uuid::Uuid) {
        self.inner.rows.lock().await.remove(&id);
        self.forget_firings(id).await;
        self.inner.tasks.lock().await.remove(&id);
        self.inner.change_requests.release(id).await;
        self.inner.issues.release(id).await;
    }

    async fn forget_firings(&self, id: uuid::Uuid) {
        self.inner.last_firings.lock().await.retain(|(subscription_id, _), _| *subscription_id != id);
        self.inner.unable_since.lock().await.remove(&id);
        self.inner.stale_attention_reported.lock().await.remove(&id);
    }

    async fn watch_row(&self, row: LeafSubscriptionRow) -> Result<(), String> {
        let mut recovery = LeafWatchRecovery::default();
        loop {
            if !self.inner.rows.lock().await.contains_key(&row.id) {
                return Ok(());
            }
            // Attempt time includes opening watches and loading snapshots.
            // A slow relist already bounds snapshot work, so it may reset backoff.
            let started = tokio::time::Instant::now();
            match self.watch_row_once(row.clone()).await {
                Err(ResourceError::WatchExpired { .. }) => {
                    if !self.inner.rows.lock().await.contains_key(&row.id) {
                        return Ok(());
                    }
                    // Level-triggered leaves recover from the current snapshot;
                    // keep the same subscription and firing/episode accounting.
                    let delay = recovery.expired(started.elapsed());
                    if delay.is_zero() {
                        tokio::task::yield_now().await;
                    } else {
                        // unsubscribe_connection and row replacement abort this
                        // task, including its sleep, releasing demands immediately.
                        tokio::time::sleep(delay).await;
                    }
                }
                result => return result.map_err(|error| error.to_string()),
            }
        }
    }

    async fn watch_row_once(&self, row: LeafSubscriptionRow) -> Result<(), ResourceError> {
        let convoys = self.inner.backend.including_replicas::<Convoy>(&row.namespace);
        let vessels = self.inner.backend.including_replicas::<Vessel>(&row.namespace);
        let change_requests = self.inner.backend.including_replicas::<ChangeRequest>(&row.namespace);
        let usages = self.inner.backend.including_replicas::<Usage>(&row.namespace);
        let issues = self.inner.backend.including_replicas::<Issue>(&row.namespace);
        let artifacts = self.inner.backend.including_replicas::<Artifact>(&row.namespace);
        // Open watches before taking the level-triggered snapshots. Writes
        // racing the lists are then buffered by the streams and replayed by
        // the loop instead of falling through a list-then-watch gap.
        let mut convoy_watch = convoys.watch().await?;
        let mut vessel_watch = vessels.watch().await?;
        let mut change_request_watch = change_requests.watch().await?;
        let mut usage_watch = usages.watch().await?;
        let mut issue_watch = issues.watch().await?;
        let mut artifact_watch = artifacts.watch().await?;
        let convoy_list = convoys.list().await?;
        let vessel_list = vessels.list().await?;
        let change_request_list = change_requests.list().await?;
        let usage_list = usages.list().await?;
        let issue_list = issues.list().await?;
        let artifact_list = artifacts.list().await?;
        #[cfg(test)]
        self.inner.snapshot_loads.fetch_add(1, Ordering::SeqCst);
        let mut convoy_objects = convoy_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut vessel_objects = vessel_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut change_request_sources = change_request_sources(change_request_list);
        let mut change_request_objects = freshest_change_requests(&change_request_sources);
        let mut usage_objects = usage_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });

        let mut issue_sources = issue_sources(issue_list);
        let mut issue_objects = freshest_issues(&issue_sources);
        let mut artifact_objects = artifact_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let staleness = LeafObservationStaleness { change_request: self.change_request_stale_after(), issue: self.issue_stale_after() };

        let current_row = self.inner.rows.lock().await.get(&row.id).cloned().unwrap_or_else(|| row.clone());
        if let Some(fire) = evaluate_row(
            &current_row,
            &LeafSubjects {
                convoys: &convoy_objects,
                vessels: &vessel_objects,
                change_requests: &change_request_objects,
                usages: &usage_objects,
                issues: &issue_objects,
                artifacts: &artifact_objects,
            },
            staleness,
        )
        .map_err(ResourceError::other)?
        {
            self.fire(row.id, fire).await;
            if !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }) {
                return Ok(());
            }
        }

        let mut last_retry_deadline = None;
        loop {
            // A level-triggered delivery must retry even if no resource changes
            // after the failed attempt. The durable deadline survives restart.
            let retry_at = match &row.watcher {
                LeafWatcher::TurnDelivery { convoy, source, .. } => convoy_objects
                    .get(convoy)
                    .and_then(|convoy| convoy.status.as_ref())
                    .and_then(|status| status.turn_deliveries.get(source))
                    .and_then(|delivery| delivery.failure.as_ref())
                    .filter(|failure| failure.kind == flotilla_resources::TurnDeliveryFailureKind::Transient)
                    .map(|failure| failure.retry_at)
                    .filter(|deadline| Some(*deadline) != last_retry_deadline),
                _ => None,
            };
            let retry_delay = retry_at.map_or(Duration::from_secs(86400), |at| (at - Utc::now()).to_std().unwrap_or_default());
            tokio::select! {
                _ = tokio::time::sleep(retry_delay), if retry_at.is_some() => {
                    // If evidence no longer fires, await its next change rather
                    // than spinning on the already expired durable deadline.
                    last_retry_deadline = retry_at;
                }
                event = convoy_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("convoy resource watch closed"))??;
                    apply_read_event(event, &mut convoy_objects);
                }
                event = vessel_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("vessel resource watch closed"))??;
                    apply_read_event(event, &mut vessel_objects);
                }
                event = change_request_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("change request resource watch closed"))??;
                    let name = match event {
                        ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => item.object.metadata.name,
                        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                    };
                    // A buffered event may precede the initial list snapshot, and a
                    // deletion may expose a suppressed self-origin copy. Refresh
                    // only this name from the current store on either transition.
                    let copies = change_requests.get_all(&name).await?;
                    let mut by_source = BTreeMap::new();
                    for item in copies.items {
                        by_source.insert(resource_source(&item.provenance), item.object);
                    }
                    if by_source.is_empty() {
                        change_request_sources.remove(&name);
                    } else {
                        change_request_sources.insert(name.clone(), by_source);
                    }
                    update_freshest_change_request(&name, &change_request_sources, &mut change_request_objects);
                }
                event = issue_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("issue resource watch closed"))??;
                    let name = match event {
                        ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => item.object.metadata.name,
                        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                    };
                    let copies = issues.get_all(&name).await?;
                    let mut by_source = BTreeMap::new();
                    for item in copies.items {
                        by_source.insert(resource_source(&item.provenance), item.object);
                    }
                    if by_source.is_empty() {
                        issue_sources.remove(&name);
                    } else {
                        issue_sources.insert(name.clone(), by_source);
                    }
                    update_freshest_issue(&name, &issue_sources, &mut issue_objects);
                }
                event = usage_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("usage resource watch closed"))??;
                    apply_read_event(event, &mut usage_objects);
                }
                event = artifact_watch.next() => {
                    let event = event.ok_or_else(|| ResourceError::other("artifact resource watch closed"))??;
                    apply_read_event(event, &mut artifact_objects);
                }
            }
            let current_row = self.inner.rows.lock().await.get(&row.id).cloned().unwrap_or_else(|| row.clone());
            if let Some(fire) = evaluate_row(
                &current_row,
                &LeafSubjects {
                    convoys: &convoy_objects,
                    vessels: &vessel_objects,
                    change_requests: &change_request_objects,
                    usages: &usage_objects,
                    issues: &issue_objects,
                    artifacts: &artifact_objects,
                },
                staleness,
            )
            .map_err(ResourceError::other)?
            {
                self.fire(row.id, fire).await;
                if !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }) {
                    return Ok(());
                }
            }
        }
    }

    async fn fire(&self, subscription_id: uuid::Uuid, mut fire: LeafFire) {
        fire.subscription_id = subscription_id;
        self.inner.last_firings.lock().await.insert((subscription_id, fire.leaf.clone()), LeafFiringRecord {
            leaf: fire.leaf.clone(),
            value: fire.value.clone(),
            fired_at: Utc::now(),
        });
        let watcher = self.inner.rows.lock().await.get(&subscription_id).map(|row| row.watcher.clone());
        match watcher {
            Some(LeafWatcher::WaitCaller { connection_id }) => {
                fire.watcher_id = connection_id;
                self.inner.event_sink.emit(DaemonEvent::LeafFired(fire));
                self.finish(subscription_id).await;
            }
            Some(LeafWatcher::ReconcilerWake { convoy }) => {
                // Internal rows remain until convoy phase re-derivation removes
                // them. This keeps their refresher demands alive and makes one
                // row per expected CR a naturally idempotent fired latch.
                let _ = self.inner.reconciler_tx.send(convoy);
            }
            Some(LeafWatcher::TurnDelivery { convoy, source, rule }) => {
                if let Err(error) = self.deliver_turn(subscription_id, &convoy, &source, &rule, &fire.leaf).await {
                    let namespace = self.inner.rows.lock().await.get(&subscription_id).map(|row| row.namespace.clone());
                    if let Some(namespace) = namespace {
                        if let Err(record_error) = self.record_delivery_failure(&namespace, &convoy, &source, &error).await {
                            tracing::warn!(%convoy, %source, %record_error, "could not record turn delivery failure");
                        }
                    }
                }
                let _ = self.inner.reconciler_tx.send(convoy);
            }
            None => {}
        }
    }

    async fn record_delivery_failure(&self, namespace: &str, convoy: &str, source: &str, error: &DeliveryError) -> Result<(), String> {
        let convoys = self.inner.backend.clone().using::<Convoy>(namespace);
        let current = convoys.get(convoy).await.map_err(|error| error.to_string())?;
        let prior =
            current.status.as_ref().and_then(|status| status.turn_deliveries.get(source)).and_then(|delivery| delivery.failure.as_ref());
        let changed = prior.is_none_or(|prior| prior.reason != error.reason || prior.kind != error.kind);
        let attempts = prior.map_or(1, |prior| prior.attempts.saturating_add(1));
        let delay = 5_i64.saturating_mul(1_i64 << attempts.saturating_sub(1).min(6)).min(300);
        let now = Utc::now();
        flotilla_resources::apply_status_patch(&convoys, convoy, &flotilla_resources::ConvoyStatusPatch::FailTurnDelivery {
            source: source.to_string(),
            failure: flotilla_resources::TurnDeliveryFailure::builder()
                .reason(error.reason.clone())
                .failed_at(now)
                .kind(error.kind)
                .attempts(attempts)
                .retry_at(now + chrono::Duration::seconds(delay))
                .build(),
        })
        .await
        .map_err(|error| error.to_string())?;
        if changed {
            tracing::warn!(%convoy, %source, error = %error.reason, failure_kind = ?error.kind, attempts, retry_seconds = delay, "turn delivery failed");
        }
        Ok(())
    }

    async fn deliver_turn(
        &self,
        subscription_id: uuid::Uuid,
        convoy_name: &str,
        source: &str,
        rule: &TurnDeliveryRule,
        leaf: &Leaf,
    ) -> Result<(), DeliveryError> {
        let namespace = self
            .inner
            .rows
            .lock()
            .await
            .get(&subscription_id)
            .map(|row| row.namespace.clone())
            .ok_or_else(|| "turn-delivery subscription disappeared".to_string())?;
        let convoys = self.inner.backend.clone().using::<Convoy>(&namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{convoy_name}` has no status"))?;
        if status
            .turn_deliveries
            .get(source)
            .and_then(|delivery| delivery.failure.as_ref())
            .is_some_and(|failure| failure.kind == flotilla_resources::TurnDeliveryFailureKind::Permanent || Utc::now() < failure.retry_at)
        {
            return Ok(());
        }
        // A stale firing after settlement has no work left to deliver.
        if status.phase.is_terminal() {
            return Ok(());
        }
        if status.phase == ConvoyPhase::Pending {
            return Err(DeliveryError::from("wrong convoy kind or phase for turn delivery".to_string()));
        }
        if !status.crew_work.get(&rule.to.vessel).is_some_and(|crew| crew.contains_key(&rule.to.role)) {
            return Err(DeliveryError::from("turn-delivery target crew is absent".to_string()));
        }
        let claim = status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role));
        let claim_at = claim.and_then(|claim| claim.finished_at);
        let active_probe = is_active_change_request_probe(status, rule, leaf);
        // A cached row can fire after its target stalls or settles, before the
        // reconciler removes it. Judge eligibility against the current status.
        if status.phase == ConvoyPhase::Active && !active_probe {
            return Ok(());
        }
        let active_conflict = active_probe && is_conflict_probe(leaf);
        let (subject_revision, evidence_at, brief) = match &leaf.address {
            LeafAddress::ChangeRequest { service, scope, number } => {
                let record_name = flotilla_resources::change_request_record_name(service, scope, *number);
                let record = self
                    .inner
                    .backend
                    .including_replicas::<ChangeRequest>(&namespace)
                    .get(&record_name)
                    .await
                    .map_err(|error| error.to_string())?;
                let cr =
                    record.object.status.as_ref().ok_or_else(|| format!("change-request observation `{record_name}` has no status"))?;
                let head_sha = cr.head_sha.value.clone().ok_or_else(|| "change-request head SHA is unknown".to_string())?;
                if claim_at.is_some_and(|claim_at| cr.head_sha.observed_at <= claim_at) {
                    return Ok(());
                }
                let evidence_at = match leaf.field_path.as_str() {
                    ".checks" => cr.checks.observed_at,
                    ".review.actionable-at-head" => cr.review.actionable_at_head.observed_at,
                    ".mergeable" => cr.mergeable.observed_at,
                    _ => {
                        return Err(DeliveryError::permanent(format!(
                            "turn-delivery leaf path `{}` has no firing evidence timestamp",
                            leaf.field_path
                        )))
                    }
                };
                let brief = if active_conflict {
                    format!("{}\n\nPR #{number} is conflicting. Rebase onto the current base branch, rerun the gates, push, then file a settlement claim.", rule.brief.trim())
                } else if active_probe {
                    let checks = match cr.checks.value {
                        Some(ObservedChecks::Pass) => "pass",
                        Some(ObservedChecks::Fail) => "fail",
                        Some(ObservedChecks::Pending) => "pending",
                        None => "unknown",
                    };
                    let actionable_review = match cr.review.actionable_at_head.value {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "unknown",
                    };
                    format!(
                        "{}\n\n## Turn firing context\n\n- Condition source: `{source}`\n- Head SHA: `{head_sha}`\n- Checks: {checks}\n- Review actionable at head: {actionable_review}\n- Durable convoy record: `{namespace}/{convoy_name}`\n- Target crew: `{}/{}`\n",
                        rule.brief.trim(), rule.to.vessel, rule.to.role,
                    )
                } else {
                    claim_at
                        .map(|claim_at| {
                            compose_change_request_turn_brief(
                                &convoy,
                                source,
                                rule,
                                leaf,
                                cr,
                                claim_at,
                                claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                            )
                        })
                        .unwrap_or_default()
                };
                (head_sha, evidence_at, brief)
            }
            LeafAddress::Issue { service, scope, number } => {
                let record_name = flotilla_resources::issue_record_name(service, scope, *number);
                let record = self
                    .inner
                    .backend
                    .including_replicas::<Issue>(&namespace)
                    .get(&record_name)
                    .await
                    .map_err(|error| error.to_string())?;
                let issue = record.object.status.as_ref().ok_or_else(|| format!("issue observation `{record_name}` has no status"))?;
                let updated_at = issue.updated_at.value.ok_or_else(|| "issue updated-at is unknown".to_string())?;
                if claim_at.is_some_and(|claim_at| updated_at <= claim_at) {
                    return Ok(());
                }
                let evidence_at = match leaf.field_path.as_str() {
                    ".state" => issue.state.observed_at,
                    ".updated-at" => issue.updated_at.observed_at,
                    path if path.starts_with(".labels.") => issue.labels.observed_at,
                    _ => {
                        return Err(DeliveryError::permanent(format!(
                            "turn-delivery leaf path `{}` has no firing evidence timestamp",
                            leaf.field_path
                        )))
                    }
                };
                let observation = format!(
                    "- Issue updated at: `{updated_at}`\n- Issue state: {:?}\n- Issue labels: {:?}\n",
                    issue.state.value, issue.labels.value
                );
                let brief = claim_at
                    .map(|claim_at| {
                        compose_subject_turn_brief(
                            &convoy,
                            source,
                            rule,
                            leaf,
                            &observation,
                            claim_at,
                            claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                        )
                    })
                    .unwrap_or_default();
                (format!("{}@{}", leaf.address, updated_at.to_rfc3339()), evidence_at, brief)
            }
            LeafAddress::Artifact { convoy: artifact_convoy, producer, kind, subject } => {
                if matches!(&rule.on.subject, flotilla_resources::SubjectVariable::Artifact {
                    about: flotilla_resources::ArtifactSubjectBinding::ChangeRequestHead,
                    ..
                }) {
                    let checkout_sources =
                        self.inner.backend.including_replicas::<Checkout>(&namespace).list().await.map_err(|error| error.to_string())?;
                    let checkouts = select_convoy_children(&convoy, &checkout_sources.items);
                    let leaves = expected_change_request_leaves(&convoy, &checkouts)?;
                    let mut bound_to_current_head = false;
                    for candidate in leaves {
                        let LeafAddress::ChangeRequest { service, scope, number } = candidate.address else { continue };
                        let name = flotilla_resources::change_request_record_name(&service, &scope, number);
                        let record = self.inner.backend.including_replicas::<ChangeRequest>(&namespace).get(&name).await;
                        if record.ok().and_then(|record| record.object.status.and_then(|status| status.head_sha.value)).as_deref()
                            == Some(subject.as_str())
                        {
                            bound_to_current_head = true;
                            break;
                        }
                    }
                    if !bound_to_current_head {
                        return Ok(());
                    }
                }
                let name = flotilla_resources::artifact_record_name(artifact_convoy, producer, kind, subject);
                let artifact = self
                    .inner
                    .backend
                    .including_replicas::<Artifact>(&namespace)
                    .get(&name)
                    .await
                    .map_err(|error| error.to_string())?
                    .object;
                let bound_leaf = Leaf {
                    address: LeafAddress::Artifact {
                        convoy: artifact_convoy.clone(),
                        producer: producer.clone(),
                        kind: kind.clone(),
                        subject: subject.clone(),
                    },
                    ..leaf.clone()
                };
                if evaluate_leaf(&bound_leaf, Some(&ArtifactLeafSubject(&artifact)), None)?.result != ThreeValue::True {
                    return Ok(());
                }
                let evidence_at = artifact.spec.recorded_at.unwrap_or(artifact.metadata.creation_timestamp);
                let observation = format!(
                    "- Artifact: `{name}`\n- Subject: `{subject}`\n- Digest: `{}`\n- Summary: `{}`\n",
                    artifact.spec.digest,
                    serde_json::to_string(&artifact.spec.summary).map_err(|error| error.to_string())?
                );
                let brief = claim_at
                    .map(|claim_at| {
                        compose_subject_turn_brief(
                            &convoy,
                            source,
                            rule,
                            leaf,
                            &observation,
                            claim_at,
                            claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                        )
                    })
                    .unwrap_or_default();
                (format!("{subject}@{}", artifact.spec.digest), evidence_at, brief)
            }
            _ => return Err(DeliveryError::permanent("turn-delivery leaf is not externally observed")),
        };
        if status
            .turn_deliveries
            .get(source)
            .is_some_and(|delivery| delivery.episodes.iter().any(|episode| episode.subject_revision == subject_revision))
        {
            return Ok(());
        }
        // Active checks/review turns require evidence newer than this work's start;
        // an already-green adopted PR waits for a fresh observation.
        let judged_at = claim_at
            .or_else(|| active_probe.then(|| status.started_at.unwrap_or(convoy.metadata.creation_timestamp)))
            .ok_or_else(|| format!("turn-delivery target {}/{} has no settlement claim", rule.to.vessel, rule.to.role))?;
        if !active_conflict && evidence_at <= judged_at {
            return Ok(());
        }

        let request = TurnDeliveryRequest::builder()
            .namespace(namespace.clone())
            .convoy(convoy_name.to_string())
            .source(source.to_string())
            .vessel(rule.to.vessel.clone())
            .role(rule.to.role.clone())
            .brief(brief)
            .subject_revision(subject_revision.clone())
            .maybe_subject(match &leaf.address {
                LeafAddress::ChangeRequest { service, scope, number } => Some(flotilla_protocol::Subject {
                    kind: flotilla_protocol::SubjectKind::ChangeRequest,
                    source: flotilla_protocol::IssueSource { service: service.clone(), scope: scope.clone() },
                    id: number.to_string(),
                }),
                _ => None,
            })
            .sender(flotilla_resources::CrewMessageSender::FlotillaTurn { source: source.to_string() })
            .build();
        let prior_episodes = status.turn_deliveries.get(source).map_or(0, |delivery| delivery.episodes.len()) as u32;
        let now = Utc::now();
        let patch = if prior_episodes >= self.inner.episode_limit {
            let reason =
                format!("turn delivery refused after {} consecutive episodes for condition source `{source}`", self.inner.episode_limit);
            let actuator = self.inner.turn_delivery.lock().await.clone();
            if request.subject.is_none() {
                match flotilla_resources::active_change_request_subjects(&convoy)?.len() {
                    0 => return Err(DeliveryError::permanent("turn-delivery convoy has no bound change request for hold")),
                    1 => {}
                    _ => return Err(DeliveryError::permanent("turn-delivery hold has ambiguous change request subjects")),
                }
            }
            actuator.hold(&request, &rule.hold, &reason).await?;
            external_patches::refuse_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: judged_at,
                    outcome: TurnDeliveryOutcome::Refused { reason: reason.clone(), refused_at: now, hold_executed: true },
                    sender: request.sender.clone(),
                },
                ConvoyAttention { source: source.to_string(), reason, raised_at: now },
            )
        } else {
            let actuator = self.inner.turn_delivery.lock().await.clone();
            let sessions = self
                .inner
                .backend
                .including_replicas::<TerminalSession>(&namespace)
                .list_matching_labels(&BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy_name.to_string()),
                    (VESSEL_LABEL.to_string(), rule.to.vessel.clone()),
                    (ROLE_LABEL.to_string(), rule.to.role.clone()),
                ]))
                .await
                .map_err(|error| error.to_string())?;
            if sessions.items.iter().any(|session| !matches!(session.object.spec.source, TerminalSessionSource::Agent { .. })) {
                return Err(DeliveryError::permanent("turn-delivery target is not an agent"));
            }
            let rung = actuator.deliver(&request).await?;
            external_patches::record_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: judged_at,
                    outcome: TurnDeliveryOutcome::Queued {
                        rung,
                        queued_at: Utc::now(),
                        vessel: rule.to.vessel.clone(),
                        role: rule.to.role.clone(),
                        message_id: format!("turn-delivery:{source}:{subject_revision}"),
                        blocking_reason: "waiting for terminal readiness or submission evidence".into(),
                    },
                    sender: request.sender.clone(),
                },
                rule.to.vessel.clone(),
                rule.to.role.clone(),
                request.brief.clone(),
            )
        };
        // The actuator may reopen the work before publishing the session message.
        // Apply the delivery record to that newer status rather than restoring
        // the pre-delivery snapshot (which would revoke credentials again).
        let current = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let mut next = current.status.clone().ok_or_else(|| format!("convoy {convoy_name} has no status"))?;
        patch.apply(&mut next);
        convoys.update_status(convoy_name, &current.metadata.resource_version, &next).await.map_err(|error| error.to_string())?;
        if let Some(row) = self.inner.rows.lock().await.get_mut(&subscription_id) {
            row.episode_key.subject_revision = Some(subject_revision);
        }
        Ok(())
    }
}

fn compose_change_request_turn_brief(
    convoy: &ResourceObject<Convoy>,
    source: &str,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    cr: &flotilla_resources::ChangeRequestStatus,
    claim_at: DateTime<Utc>,
    decision_ledger_ref: Option<&str>,
) -> String {
    let review_link = match (&leaf.address, leaf.field_path.as_str()) {
        (LeafAddress::ChangeRequest { service, scope, number }, ".review.actionable-at-head") if service == "github.com" => {
            format!("- Review feedback: https://github.com/{scope}/pull/{number}\n")
        }
        _ => String::new(),
    };
    let observation = format!(
        "- Head SHA: `{}`\n- Review actionable at head: {:?}\n{}- Checks: {:?}\n- Mergeability: {:?}\n",
        cr.head_sha.value.as_deref().unwrap_or("unknown"),
        cr.review.actionable_at_head.value,
        review_link,
        cr.checks.value,
        cr.mergeable.value,
    );
    compose_subject_turn_brief(convoy, source, rule, leaf, &observation, claim_at, decision_ledger_ref)
}

fn compose_subject_turn_brief(
    convoy: &ResourceObject<Convoy>,
    source: &str,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    observation: &str,
    claim_at: DateTime<Utc>,
    decision_ledger_ref: Option<&str>,
) -> String {
    let repositories = convoy
        .spec
        .repositories
        .iter()
        .map(|repo| format!("- {}: branch `{}` → `{}`", repo.url, repo.source_ref, repo.target_ref))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\n## Turn firing context\n\n- Condition source: `{source}`\n- Fired leaf: `{leaf:?}`\n{}- Claim durability fence: `{}`\n- Decision ledger: {}\n- Durable convoy record: `{}/{}`\n- Target crew: `{}/{}`\n\n## Repositories and branches\n\n{}\n",
        rule.brief.trim(),
        observation,
        claim_at.to_rfc3339(),
        decision_ledger_ref.unwrap_or("MISSING (crew completed without a decision ledger)"),
        convoy.metadata.namespace,
        convoy.metadata.name,
        rule.to.vessel,
        rule.to.role,
        repositories,
    )
}

pub(crate) fn queued_turn_evidence(session: Option<&ResourceObject<TerminalSession>>, message_id: &str) -> (bool, String) {
    let Some(session) = session else { return (false, "terminal session unavailable".into()) };
    let status = session.status.as_ref();
    if status.and_then(|status| status.delivered_message_id.as_deref()) == Some(message_id) {
        return (true, String::new());
    }
    if let TerminalSessionSource::Agent { message: Some(message), .. } = &session.spec.source {
        if message.delivered_through(status.and_then(|status| status.delivered_message_id.as_deref()), message_id) {
            return (true, String::new());
        }
        if message.next_after(status.and_then(|status| status.delivered_message_id.as_deref())).is_some_and(|next| next.id != message_id) {
            return (false, "waiting behind an earlier terminal message".into());
        }
    }
    if let Some(condition) = status.and_then(|status| status.degraded.as_ref()) {
        return (false, format!("{}: {}", condition.reason, condition.message));
    }
    match status {
        Some(status) if status.phase == TerminalSessionPhase::Running => match &status.attention {
            Some(attention) => (false, format!("attention {:?}; waiting for turn readiness or submission evidence", attention.state)),
            None => (false, "waiting for startup readiness or submission evidence".into()),
        },
        Some(status) => (false, format!("terminal {:?}; waiting for startup readiness", status.phase)),
        None => (false, "waiting for terminal startup".into()),
    }
}

#[derive(Clone)]
struct ReconcilerWake {
    subscriptions: LeafSubscriptionTable,
    _marker: PhantomData<Convoy>,
}

impl SecondaryWatch for ReconcilerWake {
    type Primary = Convoy;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move { self.run(namespace, sender).await.map_err(ResourceError::other) })
    }
}

impl ReconcilerWake {
    async fn report_stale_attention(&self, row: &LeafSubscriptionRow, source: TerminalAttentionSource) {
        if self.subscriptions.inner.stale_attention_reported.lock().await.insert(row.id) {
            tracing::warn!(subscription_id = %row.id, ?source, maker = ?row.maker, "terminal attention evidence stale");
        }
    }

    async fn maker_debouncing(
        &self,
        row_id: uuid::Uuid,
        key: UnableEvidenceKey,
        first_seen: DateTime<Utc>,
        delay: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        let mut episodes = self.subscriptions.inner.unable_since.lock().await;
        let episode = episodes.entry(row_id).or_insert((key, first_seen));
        if episode.0 != key {
            *episode = (key, first_seen);
        }
        now.signed_duration_since(episode.1) < delay
    }

    async fn observe_queued_turns(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(status) = &convoy.status else { return Ok(()) };
        if status.phase.is_terminal() {
            return Ok(());
        }
        for (source, delivery) in &status.turn_deliveries {
            for episode in &delivery.episodes {
                let TurnDeliveryOutcome::Queued { vessel, role, message_id, .. } = &episode.outcome else { continue };
                let session = sessions.values().find(|session| {
                    session.metadata.labels.get(VESSEL_LABEL) == Some(vessel) && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                });
                let (confirmed, blocking_reason) = queued_turn_evidence(session, message_id);
                let patch = flotilla_resources::ConvoyStatusPatch::ObserveQueuedTurnDelivery {
                    source: source.clone(),
                    subject_revision: episode.subject_revision.clone(),
                    confirmed,
                    blocking_reason,
                    observed_at: now,
                };
                let mut next = status.clone();
                patch.apply(&mut next);
                if next != *status {
                    flotilla_resources::apply_status_patch(
                        &self.subscriptions.inner.backend.clone().using::<Convoy>(namespace),
                        &convoy.metadata.name,
                        &patch,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    async fn judge_stalls(&self, namespace: &str, convoys: &HashMap<String, ResourceObject<Convoy>>) -> Result<(), String> {
        self.judge_stalls_at(namespace, convoys, Utc::now()).await
    }

    async fn judge_stalls_at(
        &self,
        namespace: &str,
        convoys: &HashMap<String, ResourceObject<Convoy>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let backend = &self.subscriptions.inner.backend;
        let projects = backend.including_replicas::<Project>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let available_convoys = backend.including_replicas::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let available_ensures =
            backend.including_replicas::<ConvoyEnsure>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let sessions = backend.including_replicas::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let observation_list = backend.including_replicas::<ChangeRequest>(namespace).list().await.map_err(|error| error.to_string())?;
        let progress_observations = freshest_change_requests(&change_request_sources(observation_list.clone()));
        let observations = observation_list.items;
        let checkouts = backend.including_replicas::<Checkout>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let vessels = backend.including_replicas::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let rows = self.subscriptions.rows().await;
        self.subscriptions.inner.supervisor_context.lock().await.retain(|(ns, name), _| ns != namespace || convoys.contains_key(name));
        for convoy in convoys.values() {
            let Some(status) = &convoy.status else { continue };
            let selected_sessions = select_convoy_children(convoy, &sessions);
            self.observe_queued_turns(namespace, convoy, &selected_sessions, now).await?;
            let selected_vessels = select_convoy_children(convoy, &vessels);
            let selected_checkouts = select_convoy_children(convoy, &checkouts);
            let mut obligations = status.nudge_obligations.clone();
            let holding = matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing | ConvoyPhase::Anchored);
            if holding {
                let mut missing_hooks =
                    selected_sessions.values().filter_map(|session| missing_turn_hook(session, &obligations, now)).collect::<Vec<_>>();
                missing_hooks.sort();
                let reason = (!missing_hooks.is_empty()).then(|| missing_hooks.join("\n"));
                let prior = status.attention.as_ref().filter(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE);
                // Skip no-op writes here; the patch repeats the source guard after
                // an optimistic retry so concurrent settlement attention stays intact.
                if status.attention.as_ref().is_none_or(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE)
                    && prior.map(|attention| &attention.reason) != reason.as_ref()
                {
                    flotilla_resources::apply_status_patch(
                        &backend.clone().using::<Convoy>(namespace),
                        &convoy.metadata.name,
                        &flotilla_resources::ConvoyStatusPatch::ObserveTurnHookHealth { reason, observed_at: now },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                }
            }
            if let Some(stalled) =
                status.stalled.as_ref().filter(|stalled| stalled.supervisor.is_some() && stalled.source != StallEvidenceSource::Crew)
            {
                if let Some((vessel, role)) = stalled_source_actor(stalled) {
                    let source_working = selected_sessions.values().any(|session| {
                        session.metadata.labels.get(VESSEL_LABEL).is_some_and(|name| name == vessel)
                            && session.metadata.labels.get(ROLE_LABEL).is_some_and(|name| name == role)
                            && session.status.as_ref().is_some_and(|status| actor_is_working(status, &obligations, vessel, role, now))
                    });
                    if source_working {
                        for obligation in &mut obligations {
                            if matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
                            {
                                obligation.quiet_since = None;
                            }
                        }
                        if status.nudge_obligations != obligations {
                            flotilla_resources::apply_status_patch(
                                &backend.clone().using::<Convoy>(namespace),
                                &convoy.metadata.name,
                                &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations },
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        }
                        flotilla_resources::apply_status_patch(
                            &backend.clone().using::<Convoy>(namespace),
                            &convoy.metadata.name,
                            &flotilla_resources::ConvoyStatusPatch::SetStalled { condition: None },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        continue;
                    }
                }
            }
            if !holding && status.stalled.is_none() {
                continue;
            }
            let convoy_rows = rows.iter().filter(|row| {
                row.namespace == namespace
                    && (status.phase != ConvoyPhase::Active || !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }))
                    && matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy: name } | LeafWatcher::TurnDelivery { convoy: name, .. } if name == &convoy.metadata.name)
            }).collect::<Vec<_>>();
            obligations.retain(|obligation| match &obligation.maker {
                LeafMaker::Actor { vessel, role } => {
                    !status.phase.is_terminal()
                        && !status
                            .crew_work
                            .get(vessel)
                            .and_then(|crew| crew.get(role))
                            .is_some_and(|work| work.phase == flotilla_resources::CrewWorkPhase::Done)
                }
                _ => false,
            });
            let mut progress_changed = false;
            let mut unable = None;
            let mut able = false;
            let mut unknown = false;
            let mut actionable_rows = 0;
            'rows: for row in convoy_rows {
                if row.leaves.iter().any(|leaf| {
                    let LeafAddress::Work { work, .. } = &leaf.address else { return false };
                    let Some(role) = leaf.field_path.strip_prefix(".crew.").and_then(|field| field.strip_suffix(".phase")) else {
                        return false;
                    };
                    status.crew_work.get(work).and_then(|crew| crew.get(role)).is_some_and(|state| {
                        state.phase == flotilla_resources::CrewWorkPhase::Done
                            && leaf.operator == LeafOperator::Equal
                            && leaf.literal == "Done"
                    })
                }) {
                    continue;
                }
                actionable_rows += 1;
                let judgement = match &row.maker {
                    LeafMaker::Actor { vessel, role } => {
                        let session = selected_sessions.values().find(|session| {
                            session.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name)
                                && session.metadata.labels.get(VESSEL_LABEL) == Some(vessel)
                                && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                        });
                        let index =
                            obligations.iter().position(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves);
                        let obligation = if let Some(index) = index {
                            &mut obligations[index]
                        } else {
                            let history = status
                                .stalled
                                .as_ref()
                                .filter(|stall| stall.maker.as_ref() == Some(&row.maker) && stall.leaves == row.leaves)
                                .map_or_else(Vec::new, |stall| stall.nudge_history.clone());
                            obligations.push(
                                NudgeObligation::builder().maker(row.maker.clone()).leaves(row.leaves.clone()).history(history).build(),
                            );
                            obligations.last_mut().expect("inserted obligation")
                        };
                        // Only observed revisions reset a budget. Missing/unknown
                        // records and routine refresh timestamps are not progress.
                        let checkout_refs = selected_vessels
                            .values()
                            .filter(|object| object.spec.vessel_name == *vessel)
                            .flat_map(|object| object.status.iter().flat_map(|status| status.checkout_refs.values()))
                            .collect::<HashSet<_>>();
                        let mut progress = BTreeMap::new();
                        for checkout in selected_checkouts.values() {
                            if checkout_refs.contains(&checkout.metadata.name) {
                                if let Some(commit) = checkout.status.as_ref().and_then(|status| status.integration.head_revision.as_ref())
                                {
                                    progress.insert(format!("checkout/{}", checkout.metadata.name), commit.clone());
                                }
                                if let Some(status) = &checkout.status {
                                    for (reference, observation) in &status.integration.remote_refs {
                                        progress.insert(format!("push/{}/{reference}", checkout.metadata.name), observation.digest.clone());
                                    }
                                }
                            }
                        }
                        for subject in flotilla_resources::active_change_request_subjects(convoy)? {
                            for object in progress_observations.values() {
                                if object.spec.service == subject.source.service
                                    && object.spec.scope == subject.source.scope
                                    && object.spec.number.to_string() == subject.id
                                {
                                    if let Some(head) = object.status.as_ref().and_then(|status| status.head_sha.value.as_ref()) {
                                        progress.insert(format!("cr/{}", object.metadata.name), head.clone());
                                    }
                                }
                            }
                        }
                        let changed = progress.iter().any(|(key, value)| obligation.progress.get(key).is_some_and(|prior| prior != value));
                        if changed {
                            obligation.history.clear();
                            obligation.quiet_since = None;
                            obligation.reply_after = None;
                            progress_changed = true;
                        }
                        obligation.progress.extend(progress);
                        let session_status = session.and_then(|session| session.status.as_ref());
                        let message_id = session.and_then(|session| match &session.spec.source {
                            TerminalSessionSource::Agent { message, .. } => message.as_ref().map(|message| message.id.clone()),
                            _ => None,
                        });
                        let delivered_id = session_status.and_then(|status| status.delivered_message_id.clone());
                        if message_id != obligation.message_id || delivered_id != obligation.delivered_message_id {
                            if message_id.is_some() || delivered_id.is_some() {
                                obligation.reply_after = Some(now);
                            }
                            obligation.message_id = message_id.clone();
                            obligation.delivered_message_id = delivered_id;
                        }
                        let tool_activity = session_status.and_then(|status| status.last_tool_activity_at);
                        if tool_activity.is_some_and(|at| obligation.last_tool_activity_at.is_none_or(|previous| at > previous)) {
                            obligation.quiet_since = None;
                            obligation.reply_after = None;
                            obligation.last_tool_activity_at = tool_activity;
                        }
                        let pending_message = session.is_some_and(|session| match &session.spec.source {
                            TerminalSessionSource::Agent { message: Some(message), .. } => !message
                                .delivered_through(session_status.and_then(|status| status.delivered_message_id.as_deref()), &message.id),
                            _ => false,
                        });
                        if let Some(attention) = session_status.and_then(|status| status.attention.as_ref()) {
                            let echo = obligation.reply_after.is_some();
                            if !echo {
                                if obligation
                                    .last_attention_at
                                    .is_some_and(|previous| attention.as_of.signed_duration_since(previous) >= TerminalAttention::FRESH_FOR)
                                {
                                    obligation.quiet_since = None;
                                }
                                if attention.source == TerminalAttentionSource::Hook && obligation.last_hook_at != Some(attention.as_of) {
                                    obligation.quiet_since = None;
                                }
                                if attention.state != TerminalAttentionState::Idle || attention.is_stale_at(now) {
                                    obligation.quiet_since = None;
                                }
                            }
                            obligation.last_attention_at = Some(attention.as_of);
                            if attention.source == TerminalAttentionSource::Hook && attention.state != TerminalAttentionState::Unobservable
                            {
                                obligation.last_hook_at = Some(attention.as_of);
                                if attention.state == TerminalAttentionState::Idle
                                    && obligation.reply_after.is_some_and(|at| attention.as_of > at)
                                    && !pending_message
                                {
                                    // Consume exactly this delivered message's response end.
                                    // It neither starts an episode nor restarts its idle clock.
                                    obligation.reply_after = None;
                                }
                            }
                            if attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now) && !pending_message {
                                obligation.quiet_since.get_or_insert(now);
                            }
                        } else if obligation.reply_after.is_none() {
                            obligation.quiet_since = None;
                        }
                        match session_status.and_then(|status| status.attention.as_ref()).map(|attention| attention.state) {
                            Some(TerminalAttentionState::Working) => {
                                obligation.working_since.get_or_insert(now);
                            }
                            Some(TerminalAttentionState::Idle | TerminalAttentionState::NeedsInput) => {
                                obligation.working_since = None;
                            }
                            _ => {}
                        }
                        let turn_inactivity = session_status.and_then(|status| turn_inactivity_reason(status, obligation, now));
                        let awaiting_reply_observation = obligation.reply_after.is_some_and(|after| {
                            session_status.and_then(|status| status.attention.as_ref()).is_none_or(|attention| attention.as_of <= after)
                        });
                        let idle_grace = chrono::Duration::seconds(i64::from(idle_grace_seconds(status, vessel, role)));
                        let lost_reason = session
                            .and_then(|session| session.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Lost)
                            .and_then(|status| status.message.as_deref());
                        let attention = session
                            .and_then(|session| session.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Running)
                            .and_then(|status| status.attention.as_ref());
                        if status
                            .crew_work
                            .get(vessel)
                            .and_then(|crew| crew.get(role))
                            .is_some_and(|work| work.phase == flotilla_resources::CrewWorkPhase::Stalled)
                        {
                            Err((
                                status.crew_work[vessel][role].message.clone().unwrap_or_else(|| "crew stalled".into()),
                                StallEvidenceSource::Crew,
                            ))
                        } else if let Some(reason) = lost_reason {
                            self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                            Err((format!("session dead: {reason}"), StallEvidenceSource::Session))
                        } else if let Some(reason) = turn_inactivity {
                            Err((reason, StallEvidenceSource::Session))
                        } else {
                            match attention {
                                Some(attention) if attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now) => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                    Ok(())
                                }
                                Some(attention) if attention.is_stale_at(now) => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    self.report_stale_attention(row, attention.source).await;
                                    unknown = true;
                                    continue;
                                }
                                Some(attention) if attention.state == TerminalAttentionState::Unobservable => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    unknown = true;
                                    continue;
                                }
                                Some(attention) => {
                                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                    let source = match attention.source {
                                        TerminalAttentionSource::Screen => StallEvidenceSource::Screen,
                                        TerminalAttentionSource::Hook => StallEvidenceSource::Hook,
                                    };
                                    if attention.state == TerminalAttentionState::Idle {
                                        if pending_message
                                            || awaiting_reply_observation
                                            || obligation.quiet_since.is_none_or(|since| now.signed_duration_since(since) < idle_grace)
                                        {
                                            unknown = true;
                                            continue;
                                        }
                                    } else if self
                                        .maker_debouncing(
                                            row.id,
                                            UnableEvidenceKey::Attention { state: attention.state, source: attention.source },
                                            attention.as_of,
                                            if attention.source == TerminalAttentionSource::Hook {
                                                chrono::Duration::zero()
                                            } else {
                                                TerminalAttention::DEBOUNCE_FOR
                                            },
                                            now,
                                        )
                                        .await
                                    {
                                        unknown = true;
                                        continue;
                                    }
                                    let evidence = match attention.state {
                                        TerminalAttentionState::Idle => "idle".into(),
                                        TerminalAttentionState::NeedsInput => "NeedsInput".into(),
                                        TerminalAttentionState::Unobservable => unreachable!("handled as unknown"),
                                        TerminalAttentionState::Working => "working".into(),
                                    };
                                    Err((evidence, source))
                                }
                                None => {
                                    if session.is_some_and(|session| {
                                        session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
                                    }) {
                                        self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                        unknown = true;
                                        continue;
                                    }
                                    if self
                                        .maker_debouncing(row.id, UnableEvidenceKey::Absent, now, TerminalAttention::DEBOUNCE_FOR, now)
                                        .await
                                    {
                                        unknown = true;
                                        continue;
                                    }
                                    Err(("session dead or absent".into(), StallEvidenceSource::Session))
                                }
                            }
                        }
                    }
                    LeafMaker::Supervisor { convoy: supervisor_convoy, vessel, role } => {
                        let session = sessions.iter().find(|source| {
                            let session = &source.object;
                            session.metadata.labels.get(CONVOY_LABEL) == Some(supervisor_convoy)
                                && session.metadata.labels.get(VESSEL_LABEL) == Some(vessel)
                                && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                        });
                        let attention = session
                            .and_then(|source| source.object.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Running)
                            .and_then(|status| status.attention.as_ref());
                        match attention {
                            Some(attention) if attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                Ok(())
                            }
                            Some(attention) if attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                if self
                                    .maker_debouncing(
                                        row.id,
                                        UnableEvidenceKey::Attention { state: attention.state, source: attention.source },
                                        row.created_at,
                                        TerminalAttention::DEBOUNCE_FOR,
                                        now,
                                    )
                                    .await
                                {
                                    unknown = true;
                                    continue;
                                }
                                Err(("supervisor idle".into(), StallEvidenceSource::Session))
                            }
                            Some(attention) if attention.state == TerminalAttentionState::NeedsInput && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                Err(("supervisor needs input".into(), StallEvidenceSource::Hook))
                            }
                            Some(attention) if attention.is_stale_at(now) => {
                                self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                self.report_stale_attention(row, attention.source).await;
                                unknown = true;
                                continue;
                            }
                            None if session.is_some_and(|source| {
                                source.object.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
                            }) =>
                            {
                                unknown = true;
                                continue;
                            }
                            Some(_) => {
                                unknown = true;
                                continue;
                            }
                            None => {
                                if self
                                    .maker_debouncing(
                                        row.id,
                                        UnableEvidenceKey::Absent,
                                        row.created_at,
                                        TerminalAttention::DEBOUNCE_FOR,
                                        now,
                                    )
                                    .await
                                {
                                    unknown = true;
                                    continue;
                                }
                                Err(("supervisor session dead or absent".into(), StallEvidenceSource::Session))
                            }
                        }
                    }
                    LeafMaker::Observed { .. } => {
                        let mut reason = None;
                        for leaf in &row.leaves {
                            if let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address {
                                let name = flotilla_resources::change_request_record_name(service, scope, *number);
                                let observation = observations
                                    .iter()
                                    .filter(|source| source.object.metadata.name == name)
                                    .filter_map(|source| source.object.status.as_ref())
                                    .max_by_key(|status| status.state.observed_at);
                                let has_value = observation.is_some_and(|status| match leaf.field_path.as_str() {
                                    ".state" => status.state.value.is_some(),
                                    ".head-sha" => status.head_sha.value.is_some(),
                                    ".checks" => status.checks.value.is_some(),
                                    ".review.actionable-at-head" => status.review.actionable_at_head.value.is_some(),
                                    ".mergeable" => status.mergeable.value.is_some(),
                                    _ => false,
                                });
                                let fresh = has_value
                                    && observation.is_some_and(|status| {
                                        let at = match leaf.field_path.as_str() {
                                            ".head-sha" => status.head_sha.observed_at,
                                            ".checks" => status.checks.observed_at,
                                            ".review.actionable-at-head" => status.review.actionable_at_head.observed_at,
                                            ".mergeable" => status.mergeable.observed_at,
                                            _ => status.state.observed_at,
                                        };
                                        row.freshness_demand.is_none_or(|demand| at >= demand)
                                            && now.signed_duration_since(at)
                                                < chrono::Duration::from_std(self.subscriptions.change_request_stale_after())
                                                    .expect("duration fits")
                                    });
                                if !fresh {
                                    let subject = ChangeRequestRef {
                                        namespace: namespace.into(),
                                        service: service.clone(),
                                        scope: scope.clone(),
                                        number: *number,
                                    };
                                    let refresh_error = self.subscriptions.change_request_observation_error(&subject).await;
                                    if refresh_error.as_ref().and_then(ObservationError::retry_at).is_some_and(|retry_at| retry_at > now) {
                                        // A known forge retry deadline keeps the observed
                                        // maker pending; missing evidence is not a crew stall.
                                        // Deliberately defer every leaf in this row until
                                        // its observed maker can be judged with fresh evidence.
                                        unknown = true;
                                        continue 'rows;
                                    }
                                    reason = Some(match (observation.is_some(), has_value, refresh_error) {
                                        (true, true, Some(error)) => format!("stale; refresh failed: {error}"),
                                        (true, true, None) => "stale".into(),
                                        (_, _, Some(error)) => error.to_string(),
                                        _ => "not refreshed".into(),
                                    });
                                    break;
                                }
                            }
                        }
                        reason.map_or(Ok(()), |reason| Err((reason, StallEvidenceSource::Observation)))
                    }
                    LeafMaker::Controller { retry, ceiling, .. } => {
                        retry.stall_reason(now, *ceiling).map_or(Ok(()), |reason| Err((reason, StallEvidenceSource::LeafEngine)))
                    }
                };
                match judgement {
                    Ok(()) => {
                        able = true;
                        continue;
                    }
                    Err((evidence, source)) => {
                        if matches!(row.maker, LeafMaker::Controller { .. }) {
                            unable = Some((row, evidence, source));
                            able = false;
                            break;
                        }
                        unable.get_or_insert((row, evidence, source))
                    }
                };
            }
            let project_policy = convoy.spec.project_ref.as_ref().and_then(|project| {
                projects
                    .iter()
                    .find(|source| source.object.metadata.name == *project)
                    .and_then(|source| source.object.spec.supervision.clone())
            });
            let policy = status
                .workflow_snapshot
                .as_ref()
                .and_then(|workflow| workflow.supervision.clone())
                .or(project_policy)
                .unwrap_or_else(|| {
                    vec![SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() }, SupervisionTarget::ProjectCrew {
                        convoy_role: "governor".into(),
                        vessel: String::new(),
                        role: "governor".into(),
                    }]
                });
            // An exhausted cursor on a crew target is a legacy failed lookup,
            // not a consumed operator policy. Retry that target after one roll.
            let retry_exhausted = status.stalled.as_ref().is_some_and(|stalled| {
                stalled.supervision_exhausted
                    && stalled.supervisor.is_none()
                    && stalled
                        .supervision_index
                        .is_some_and(|index| policy.get(index).is_some_and(|target| !matches!(target, SupervisionTarget::Operator)))
            });
            // Previous-generation fallback records were marked exhausted with no
            // consumed rung. Retry those too; only a consumed ladder stays exhausted.
            if status.stalled.as_ref().is_some_and(|stalled| stalled.supervision_exhausted && stalled.supervision_index.is_some())
                && !retry_exhausted
                && !able
                && !progress_changed
                && status.phase == ConvoyPhase::Active
                && actionable_rows > 0
            {
                continue;
            }
            let next = if holding && !able && !unknown && (status.phase != ConvoyPhase::Active || actionable_rows > 0) {
                let (leaves, maker, evidence, source) = if let Some((row, evidence, source)) = unable.as_ref() {
                    (row.leaves.clone(), Some(row.maker.clone()), evidence.clone(), source.clone())
                } else {
                    (Vec::new(), None, "no armed row with an able maker".into(), StallEvidenceSource::LeafEngine)
                };
                let prior = status.stalled.as_ref().filter(|prior| prior.leaves == leaves && prior.maker == maker);
                let nudge_history = obligations
                    .iter()
                    .find(|obligation| Some(&obligation.maker) == maker.as_ref() && obligation.leaves == leaves)
                    .map_or_else(Vec::new, |obligation| obligation.history.clone());
                let mut condition = StalledCondition {
                    leaves,
                    maker,
                    evidence,
                    source,
                    cause: None,
                    began_at: prior.map_or(now, |stalled| stalled.began_at),
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history,
                };
                let declared = status
                    .stalled
                    .as_ref()
                    .filter(|stalled| stalled.source == StallEvidenceSource::Crew && stalled.leaves == condition.leaves);
                if let Some(declared) = declared {
                    // Keep the crew's evidence, rather than accumulating prior delivery diagnostics.
                    condition.evidence = stalled_source_actor(declared)
                        .and_then(|(vessel, role)| status.crew_work.get(vessel)?.get(role)?.message.clone())
                        .unwrap_or_else(|| declared.evidence.clone());
                    condition.source = StallEvidenceSource::Crew;
                    condition.reason = declared.reason;
                    condition.proposed_disposition = declared.proposed_disposition;
                    condition.began_at = declared.began_at;
                }
                if matches!(condition.maker, Some(LeafMaker::Supervisor { .. })) {
                    if let Some(prior) = prior {
                        condition.evidence = prior.evidence.clone();
                        condition.source = prior.source.clone();
                        condition.reason = prior.reason;
                        condition.proposed_disposition = prior.proposed_disposition;
                        condition.began_at = prior.began_at;
                    }
                }
                let mut awaiting_resumed_turn = false;
                if let (Some(LeafMaker::Actor { vessel, role }), Some(row)) = (&condition.maker, unable.as_ref().map(|(row, _, _)| *row)) {
                    let refusal =
                        status.crew_work.get(vessel).and_then(|crew| crew.get(role)).and_then(|work| work.completion_refusal.as_ref());
                    let refusal_escalated = refusal.is_some_and(|refusal| refusal.consecutive_count >= refusal_limit(status, vessel, role));
                    let session = selected_sessions.values().find(|session| {
                        session.metadata.labels.get(VESSEL_LABEL) == Some(vessel) && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                    });
                    let idle_at = session
                        .filter(|session| matches!(session.spec.source, TerminalSessionSource::Agent { .. }))
                        .and_then(|session| session.status.as_ref())
                        .and_then(|status| status.attention.as_ref())
                        .filter(|attention| attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now))
                        .map(|attention| attention.as_of);
                    let resume_grace = status
                        .crew_work
                        .get(vessel)
                        .and_then(|crew| crew.get(role))
                        .filter(|work| work.phase == flotilla_resources::CrewWorkPhase::Working)
                        .and_then(|work| work.resumed_at.zip(work.resume_brief_id.as_deref()))
                        .is_some_and(|(resumed_at, brief_id)| {
                            // A missing or recreated session must not suppress
                            // supervision indefinitely while its brief is unconfirmed.
                            if now.signed_duration_since(resumed_at) >= chrono::Duration::minutes(2) {
                                return false;
                            }
                            let operator_brief_delivered = session.is_some_and(|session| {
                                let delivered_id = session.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
                                matches!(&session.spec.source, TerminalSessionSource::Agent { message: Some(message), .. }
                                    if message.delivered_through(delivered_id, brief_id))
                            });
                            !operator_brief_delivered || idle_at.is_none_or(|idle_at| idle_at <= resumed_at)
                        });
                    awaiting_resumed_turn = resume_grace;
                    if refusal_escalated {
                        condition.rung = StallRung::Operator;
                        condition.evidence = format!(
                            "settlement claim refused {} times: {}",
                            refusal.map_or(0, |refusal| refusal.consecutive_count),
                            refusal.map_or("", |refusal| refusal.expectation.as_str())
                        );
                    } else if idle_at.is_some() && declared.is_none() && !resume_grace {
                        let limit = nudge_policy(status, vessel, role).map_or(2, |policy| policy.max_per_episode) as usize;
                        let grace_seconds = idle_grace_seconds(status, vessel, role);
                        // Initial grace is 1x; intervals after delivered nudges are 1x, 2x, 4x, ...
                        let backoff = chrono::Duration::seconds(
                            i64::from(grace_seconds.max(1))
                                .saturating_mul(1_i64 << condition.nudge_history.len().saturating_sub(1).min(20)),
                        );
                        let due = condition.nudge_history.last().is_none_or(|nudge| now.signed_duration_since(nudge.at) >= backoff);
                        if prior.is_some_and(|stalled| {
                            stalled.rung == StallRung::Operator && stalled.evidence.starts_with("nudge delivery failed:")
                        }) {
                            condition.rung = StallRung::Operator;
                            condition.evidence = prior.expect("checked above").evidence.clone();
                        } else if condition.nudge_history.len() < limit {
                            condition.rung = StallRung::Nudge;
                            if due {
                                let leaf = row.leaves.first().ok_or_else(|| "actor row has no leaf".to_string())?;
                                let obligation =
                                    if let Some(refusal) = refusal { refusal_nudge_brief(refusal) } else { actor_obligation(leaf)? };
                                let brief = format!(
                                    "For {role}@{vessel} in {} (resource ref: {}):\n{obligation}",
                                    convoy_message_address(convoy),
                                    convoy.metadata.name
                                );
                                let request = TurnDeliveryRequest::builder()
                                    .namespace(namespace.to_string())
                                    .convoy(convoy.metadata.name.clone())
                                    .source(format!("stall-nudge-{}", condition.nudge_history.len() + 1))
                                    .vessel(vessel.clone())
                                    .role(role.clone())
                                    .brief(brief)
                                    .subject_revision(now.timestamp_micros().to_string())
                                    .sender(flotilla_resources::CrewMessageSender::FlotillaNudge)
                                    .build();
                                match self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&request).await {
                                    Ok(_) => {
                                        condition.nudge_history.push(StallNudge { at: now, row: leaf.clone() });
                                        if let Some(obligation) = obligations
                                            .iter_mut()
                                            .find(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves)
                                        {
                                            obligation.history = condition.nudge_history.clone();
                                            obligation.reply_after = Some(now);
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            convoy = %convoy.metadata.name,
                                            target = %role,
                                            %vessel,
                                            reason = %error,
                                            brief = %stall_supervision_log_brief(convoy, &condition),
                                            "stall nudge fell back to operator"
                                        );
                                        condition.rung = StallRung::Operator;
                                        condition.evidence = format!("nudge delivery failed: {error}");
                                    }
                                }
                            }
                        } else if !due {
                            condition.rung = StallRung::Nudge;
                        }
                    }
                }
                let needs_supervisor = matches!(condition.maker, Some(LeafMaker::Supervisor { .. }))
                    || condition.source == StallEvidenceSource::Crew
                    || condition.source == StallEvidenceSource::Session
                    || matches!(&condition.maker, Some(LeafMaker::Actor { vessel, role }) if status.crew_work.get(vessel)
                        .and_then(|crew| crew.get(role)).and_then(|work| work.completion_refusal.as_ref())
                        .is_some_and(|refusal| refusal.consecutive_count >= refusal_limit(status, vessel, role)))
                    || condition.evidence == "NeedsInput"
                    || (condition.rung == StallRung::Operator && !condition.nudge_history.is_empty())
                    || (condition.rung == StallRung::Operator
                        && matches!(condition.maker, Some(LeafMaker::Actor { .. }))
                        && condition.evidence == "idle"
                        && condition.nudge_history.is_empty());
                if needs_supervisor && !awaiting_resumed_turn && !condition.evidence.starts_with("nudge delivery failed:") {
                    let start = prior
                        .and_then(|stalled| stalled.supervision_index.map(|index| if retry_exhausted { index } else { index + 1 }))
                        .unwrap_or(0);
                    let keep_current = prior.is_some_and(|stalled| stalled.supervisor.is_some())
                        && !matches!(condition.maker, Some(LeafMaker::Supervisor { .. }));
                    let context = format!(
                        "{:?}",
                        (
                            &policy,
                            status.crew_work.iter().map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>())).collect::<Vec<_>>(),
                            available_convoys
                                .iter()
                                .filter(|candidate| candidate.object.metadata.name != convoy.metadata.name
                                    && candidate.object.spec.project_ref == convoy.spec.project_ref)
                                .map(|candidate| {
                                    let candidate = &candidate.object;
                                    (
                                        &candidate.metadata.name,
                                        (
                                            &candidate.spec.role,
                                            candidate.spec.generation,
                                            candidate.status.as_ref().map(|status| {
                                                (
                                                    &status.phase,
                                                    status
                                                        .crew_work
                                                        .iter()
                                                        .map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>()))
                                                        .collect::<Vec<_>>(),
                                                )
                                            }),
                                        ),
                                    )
                                })
                                .collect::<BTreeMap<_, _>>(),
                            available_ensures
                                .iter()
                                .map(|ensure| (&ensure.object.metadata.name, &ensure.object.metadata.resource_version))
                                .collect::<BTreeMap<_, _>>(),
                            projects
                                .iter()
                                .map(|project| (&project.object.metadata.name, &project.object.metadata.resource_version))
                                .collect::<BTreeMap<_, _>>()
                        )
                    );
                    let context_key = (namespace.to_string(), convoy.metadata.name.clone());
                    let unchanged_context = self.subscriptions.inner.supervisor_context.lock().await.get(&context_key) == Some(&context);
                    let cached_lookup = unchanged_context
                        && prior.is_some_and(|prior| {
                            prior.rung == StallRung::Operator && prior.supervisor.is_none() && !prior.supervision_exhausted
                        });
                    if keep_current || cached_lookup {
                        condition = prior.expect("checked above").clone();
                    } else {
                        self.subscriptions.inner.supervisor_context.lock().await.remove(&context_key);
                        condition.rung = StallRung::Operator;
                        condition.supervisor = None;
                        // The cursor records consumed rungs, not failed delivery attempts.
                        // Retain it so retrying a higher rung cannot route back to a lower one.
                        // Persist the last consumed rung, not the next attempt.
                        // Retrying legacy rung i stores i - 1 (None at zero), so
                        // failed lookup/delivery retries i without revisiting lower rungs.
                        condition.supervision_index = prior.and_then(|stalled| stalled.supervision_index).and_then(|index| {
                            if retry_exhausted {
                                index.checked_sub(1)
                            } else {
                                Some(index)
                            }
                        });
                        condition.maker = condition.leaves.first().and_then(|leaf| {
                            let LeafAddress::Work { work, .. } = &leaf.address else { return None };
                            let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
                            Some(LeafMaker::Actor { vessel: work.clone(), role: role.to_string() })
                        });
                        condition.supervision_exhausted = true;
                        let mut unavailable_target = None;
                        let mut delivery_failed = false;
                        for (index, target) in policy.iter().enumerate().skip(start) {
                            let candidate = match target {
                                SupervisionTarget::ConvoyCrew { vessel, role } => {
                                    let found = status
                                        .crew_work
                                        .iter()
                                        .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role));
                                    found.map(|(name, _)| (convoy.metadata.name.clone(), name.clone(), role.clone(), StallRung::Bosun))
                                }
                                SupervisionTarget::ProjectCrew { convoy_role, vessel, role } => {
                                    let owned = available_ensures
                                        .iter()
                                        .map(|source| &source.object)
                                        .find(|ensure| {
                                            Some(ensure.spec.project_ref.as_str()) == convoy.spec.project_ref.as_deref()
                                                && ensure.spec.role == *convoy_role
                                        })
                                        .and_then(|ensure| ensure.status.as_ref())
                                        .and_then(|status| status.convoy_ref.as_deref());
                                    let candidate = available_convoys
                                        .iter()
                                        .map(|source| &source.object)
                                        .filter(|candidate| {
                                            convoy.spec.project_ref.is_some()
                                                && candidate.spec.project_ref == convoy.spec.project_ref
                                                && candidate.spec.role == *convoy_role
                                                && candidate.metadata.name != convoy.metadata.name
                                                && candidate.status.as_ref().is_some_and(|status| !status.phase.is_terminal())
                                        })
                                        .max_by_key(|candidate| {
                                            (
                                                Some(candidate.metadata.name.as_str()) == owned,
                                                candidate.spec.generation,
                                                &candidate.metadata.name,
                                            )
                                        });
                                    let supervisor = candidate.and_then(|candidate| {
                                        candidate.status.as_ref().and_then(|status| {
                                            status
                                                .crew_work
                                                .iter()
                                                .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role))
                                                .map(|(name, _)| {
                                                    (candidate.metadata.name.clone(), name.clone(), role.clone(), StallRung::Governor)
                                                })
                                        })
                                    });
                                    if supervisor.is_none() {
                                        if let Some(project) = convoy.spec.project_ref.as_deref() {
                                            let missing = if candidate.is_some() { " crew" } else { "" };
                                            let detail = if candidate.is_some() {
                                                format!("required crew {vessel}/{role} is absent")
                                            } else {
                                                let matching = available_convoys
                                                    .iter()
                                                    .filter(|source| {
                                                        source.object.spec.project_ref.as_deref() == Some(project)
                                                            && source.object.spec.role == *convoy_role
                                                    })
                                                    .count();
                                                if matching == 0 {
                                                    "no matching convoy in replica view".to_string()
                                                } else {
                                                    "matching convoys are terminal or the stalled convoy itself".to_string()
                                                }
                                            };
                                            condition
                                                .evidence
                                                .push_str(&format!("; no live {convoy_role}{missing} for project {project} ({detail})"));
                                        } else {
                                            condition.evidence.push_str(&format!("; cannot find {convoy_role}: convoy has no project_ref"));
                                        }
                                    }
                                    supervisor
                                }
                                SupervisionTarget::Operator => None,
                            };
                            if let Some((target_convoy, target_vessel, target_role, rung)) = candidate {
                                if target_convoy == convoy.metadata.name
                                    && stalled_source_actor(&condition)
                                        .is_some_and(|(vessel, role)| vessel == target_vessel && role == target_role)
                                {
                                    unavailable_target = Some(target);
                                    continue;
                                }
                                let brief = stall_supervision_brief(convoy, &condition);
                                let delivery = TurnDeliveryRequest::builder()
                                    .namespace(namespace.to_string())
                                    .convoy(target_convoy.clone())
                                    .source(format!("supervision-{}-{index}", convoy.metadata.name))
                                    .vessel(target_vessel.clone())
                                    .role(target_role.clone())
                                    .brief(brief)
                                    .subject_revision(condition.began_at.timestamp_micros().to_string())
                                    .sender(flotilla_resources::CrewMessageSender::FlotillaEscalation {
                                        from: stalled_source_actor(&condition)
                                            .map(|(vessel, role)| format!("{role}@{vessel} in {}", convoy_message_address(convoy)))
                                            .unwrap_or_else(|| convoy_message_address(convoy)),
                                    })
                                    .build();
                                if let Err(error) = self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&delivery).await {
                                    if prior.is_none_or(|prior| {
                                        prior.rung != StallRung::Operator
                                            || !prior.evidence.ends_with(&format!("supervisor delivery failed: {error}"))
                                    }) {
                                        tracing::warn!(
                                            convoy = %convoy.metadata.name,
                                            target = %target_convoy,
                                            %target_vessel,
                                            %target_role,
                                            reason = %error,
                                            brief = %stall_supervision_log_brief(convoy, &condition),
                                            "stall escalation fell back to operator"
                                        );
                                    }
                                    condition.evidence.push_str(&format!("; supervisor delivery failed: {error}"));
                                    condition.supervision_exhausted = false;
                                    delivery_failed = true;
                                    break;
                                }
                                condition.rung = rung;
                                condition.supervision_exhausted = false;
                                condition.supervision_index = Some(index);
                                condition.supervisor = Some(StallSupervisor {
                                    convoy: target_convoy.clone(),
                                    vessel: target_vessel.clone(),
                                    role: target_role.clone(),
                                });
                                condition.maker =
                                    Some(LeafMaker::Supervisor { convoy: target_convoy, vessel: target_vessel, role: target_role });
                                break;
                            }
                            if matches!(target, SupervisionTarget::Operator) {
                                if unavailable_target.is_none() {
                                    condition.supervision_index = Some(index);
                                }
                                break;
                            }
                            unavailable_target = Some(target);
                        }
                        if condition.supervisor.is_none() && !delivery_failed {
                            let target = unavailable_target.unwrap_or(&SupervisionTarget::Operator);
                            if prior.is_none_or(|prior| {
                                prior.rung != condition.rung
                                    || prior.supervisor != condition.supervisor
                                    || prior.evidence != condition.evidence
                            }) {
                                tracing::warn!(
                                    convoy = %convoy.metadata.name,
                                    ?target,
                                    reason = if unavailable_target.is_some() { "supervisor_lookup_failed" }
                                        else if start >= policy.len() { "supervision_policy_exhausted" }
                                        else { "operator_rung_selected" },
                                    supervision_start = start,
                                    supervision_policy_len = policy.len(),
                                    evidence = %condition.evidence,
                                    brief = %stall_supervision_log_brief(convoy, &condition),
                                    "stall escalation fell back to operator"
                                );
                            }
                            if unavailable_target.is_some() {
                                self.subscriptions.inner.supervisor_context.lock().await.insert(context_key, context);
                                // Operator attention is a backstop while supervisors reconnect,
                                // not a consumed ladder. Try the same unconsumed rungs next pass.
                                condition.supervision_exhausted = false;
                            } else {
                                // An empty or fully consumed policy ends at the operator.
                                condition.supervision_index = Some(policy.len());
                            }
                        }
                    }
                }
                Some(condition)
            } else {
                status
                    .stalled
                    .as_ref()
                    .filter(|stalled| {
                        status.phase == ConvoyPhase::Active
                            && actionable_rows > 0
                            && (stalled.supervisor.is_some()
                                || (stalled.rung == StallRung::Operator && stalled.source == StallEvidenceSource::Crew))
                    })
                    .cloned()
            };
            if status.nudge_obligations != obligations {
                flotilla_resources::apply_status_patch(
                    &backend.clone().using::<Convoy>(namespace),
                    &convoy.metadata.name,
                    &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            if status.stalled != next {
                if let Err(error) = flotilla_resources::apply_status_patch(
                    &backend.clone().using::<Convoy>(namespace),
                    &convoy.metadata.name,
                    &flotilla_resources::ConvoyStatusPatch::SetStalled { condition: next },
                )
                .await
                {
                    tracing::warn!(namespace, convoy = %convoy.metadata.name, %error, "write stalled condition failed");
                }
            }
        }
        Ok(())
    }

    async fn run(&self, namespace: String, sender: WorkQueueSender) -> Result<(), String> {
        let convoys = self.subscriptions.inner.backend.clone().using::<Convoy>(&namespace);
        let checkouts = self.subscriptions.inner.backend.including_replicas::<Checkout>(&namespace);
        let listed_convoys = convoys.list().await.map_err(|error| error.to_string())?;
        let mut convoy_watch = convoys.watch(WatchStart::resuming_from(&listed_convoys)).await.map_err(|error| error.to_string())?;
        let mut checkout_watch = checkouts.watch().await.map_err(|error| error.to_string())?;
        let mut change_request_watch = self
            .subscriptions
            .inner
            .backend
            .including_replicas::<ChangeRequest>(&namespace)
            .watch()
            .await
            .map_err(|error| error.to_string())?;
        let mut vessel_watch =
            self.subscriptions.inner.backend.including_replicas::<Vessel>(&namespace).watch().await.map_err(|error| error.to_string())?;
        let mut forge_watch =
            self.subscriptions.inner.backend.including_replicas::<Forge>(&namespace).watch().await.map_err(|error| error.to_string())?;
        let mut convoy_objects =
            listed_convoys.items.into_iter().map(|convoy| (convoy.metadata.name.clone(), convoy)).collect::<HashMap<_, _>>();
        let mut wake_rx = self.subscriptions.inner.reconciler_tx.subscribe();
        self.sync_rows(&namespace, &convoy_objects).await?;
        let mut judge_tick = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = judge_tick.tick() => {
                    self.judge_stalls(&namespace, &convoy_objects).await?;
                }
                event = convoy_watch.next() => {
                    let event = event.ok_or_else(|| "reconciler wake convoy watch closed".to_string())?.map_err(|error| error.to_string())?;
                    match event {
                        WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                            convoy_objects.insert(convoy.metadata.name.clone(), convoy);
                        }
                        WatchEvent::Deleted(convoy) => {
                            convoy_objects.remove(&convoy.metadata.name);
                        }
                        WatchEvent::DeletedByName(tombstone) => {
                            convoy_objects.remove(&tombstone.name);
                        }
                    }
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                event = checkout_watch.next() => {
                    event.ok_or_else(|| "reconciler wake checkout watch closed".to_string())?.map_err(|error| error.to_string())?;
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                event = change_request_watch.next() => {
                    event.ok_or_else(|| "reconciler wake change request watch closed".to_string())?.map_err(|error| error.to_string())?;
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                event = vessel_watch.next() => {
                    event.ok_or_else(|| "reconciler wake vessel watch closed".to_string())?.map_err(|error| error.to_string())?;
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                event = forge_watch.next() => {
                    event.ok_or_else(|| "reconciler wake forge watch closed".to_string())?.map_err(|error| error.to_string())?;
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                wake = wake_rx.recv() => match wake {
                    Ok(convoy) => sender.send(convoy).await.map_err(|_| "convoy controller queue closed".to_string())?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for convoy in convoy_objects.values().filter(|convoy| {
                            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing | ConvoyPhase::Anchored))
                        }) {
                            sender.send(convoy.metadata.name.clone()).await.map_err(|_| "convoy controller queue closed".to_string())?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err("reconciler wake channel closed".to_string()),
                }
            }
        }
    }

    async fn sync_rows(&self, namespace: &str, convoys: &HashMap<String, ResourceObject<Convoy>>) -> Result<(), String> {
        let forges = self
            .subscriptions
            .inner
            .backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let checkout_sources = self
            .subscriptions
            .inner
            .backend
            .including_replicas::<Checkout>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items;
        let change_request_list = self
            .subscriptions
            .inner
            .backend
            .including_replicas::<ChangeRequest>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?;
        let observed_change_requests =
            freshest_change_requests(&change_request_sources(change_request_list)).into_iter().collect::<BTreeMap<_, _>>();
        let vessel_sources =
            self.subscriptions.inner.backend.including_replicas::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let mut desired = Vec::<LeafSubscriptionRow>::new();
        for convoy in convoys.values().filter(|convoy| {
            convoy
                .status
                .as_ref()
                .is_some_and(|status| matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing | ConvoyPhase::Anchored))
        }) {
            let status = convoy.status.as_ref().expect("holding convoy has status");
            let mut controller_rows = HashSet::<(String, String)>::new();
            let conflicts = subject_relationship_conflicts(convoy);
            if !conflicts.is_empty() {
                let details = conflicts
                    .iter()
                    .map(|subject| subject.internal().unwrap_or_else(|_| subject.id.clone()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let created_at = convoy.metadata.creation_timestamp;
                desired.push(LeafSubscriptionRow {
                    id: uuid::Uuid::nil(),
                    namespace: namespace.to_string(),
                    leaves: vec![Leaf {
                        address: LeafAddress::Convoy { name: convoy.metadata.name.clone() },
                        field_path: ".status.phase".into(),
                        operator: LeafOperator::Equal,
                        literal: "Landed".into(),
                    }],
                    watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                    maker: LeafMaker::Controller {
                        resource_kind: "ConvoySubjects".into(),
                        name: Some(convoy.metadata.name.clone()),
                        retry: ControllerRetry::terminal(None, created_at, format!("conflicting relationships for {details}")),
                        ceiling: RetryCeiling::default(),
                    },
                    freshness_demand: None,
                    created_at,
                    episode_key: EpisodeKeyFields::default(),
                });
            }
            let checkouts = select_convoy_children(convoy, &checkout_sources);
            for checkout in checkouts.values() {
                let CheckoutSpec::Worktree(spec) = &checkout.spec else { continue };
                let Some(retry) = checkout.status.as_ref().and_then(|status| status.clone_retry.clone()) else { continue };
                if !controller_rows.insert(("Clone".into(), spec.clone_ref.clone())) {
                    continue;
                }
                desired.push(LeafSubscriptionRow {
                    id: uuid::Uuid::nil(),
                    namespace: namespace.to_string(),
                    leaves: vec![Leaf {
                        address: LeafAddress::Convoy { name: convoy.metadata.name.clone() },
                        field_path: ".status.phase".into(),
                        operator: LeafOperator::Equal,
                        literal: "Landed".into(),
                    }],
                    watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                    maker: LeafMaker::Controller {
                        resource_kind: "Clone".into(),
                        name: Some(spec.clone_ref.clone()),
                        retry,
                        ceiling: RetryCeiling::default(),
                    },
                    freshness_demand: None,
                    created_at: Utc::now(),
                    episode_key: EpisodeKeyFields::default(),
                });
            }
            for vessel in select_convoy_children(convoy, &vessel_sources).values() {
                let Some(vessel_status) = vessel.status.as_ref() else { continue };
                let Some(environment_ref) = vessel_status.environment_ref.as_ref() else { continue };
                for (kind, retry) in [
                    ("CredentialDelivery", vessel_status.credential_delivery_retry.as_ref()),
                    ("CredentialRefresh", vessel_status.credential_refresh_retry.as_ref()),
                ] {
                    let Some(retry) = retry else { continue };
                    if !controller_rows.insert((kind.into(), environment_ref.clone())) {
                        continue;
                    }
                    desired.push(LeafSubscriptionRow {
                        id: uuid::Uuid::nil(),
                        namespace: namespace.to_string(),
                        leaves: vec![Leaf {
                            address: LeafAddress::Convoy { name: convoy.metadata.name.clone() },
                            field_path: ".status.phase".into(),
                            operator: LeafOperator::Equal,
                            literal: "Landed".into(),
                        }],
                        watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                        maker: LeafMaker::Controller {
                            resource_kind: kind.into(),
                            name: Some(environment_ref.clone()),
                            retry: retry.clone(),
                            ceiling: RetryCeiling::default(),
                        },
                        freshness_demand: None,
                        created_at: Utc::now(),
                        episode_key: EpisodeKeyFields::default(),
                    });
                }
            }
            if status.phase == ConvoyPhase::Active {
                if let Some(stalled) = status.stalled.as_ref().filter(|stalled| stalled.supervisor.is_some()) {
                    if let Some(maker @ LeafMaker::Supervisor { .. }) = &stalled.maker {
                        desired.push(LeafSubscriptionRow {
                            id: uuid::Uuid::nil(),
                            namespace: namespace.to_string(),
                            leaves: stalled.leaves.clone(),
                            watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                            maker: maker.clone(),
                            freshness_demand: None,
                            created_at: Utc::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
                }
                for (vessel, crew) in &status.crew_work {
                    for (role, work) in crew {
                        let owes_claim = status.workflow_snapshot.as_ref().is_some_and(|snapshot| {
                            snapshot.vessels.iter().any(|requirement| {
                                requirement.name == *vessel
                                    && requirement
                                        .crew
                                        .iter()
                                        .any(|member| member.role == *role && !member.completion_conditions.is_empty())
                            })
                        });
                        if !owes_claim {
                            continue;
                        }
                        if status.stalled.as_ref().is_some_and(|stalled| {
                            stalled.supervisor.is_some()
                                && stalled.leaves.iter().any(|leaf| {
                                    leaf.address == LeafAddress::Work { convoy: convoy.metadata.name.clone(), work: vessel.clone() }
                                        && leaf.field_path == format!(".crew.{role}.phase")
                                })
                        }) {
                            continue;
                        }
                        if !status
                            .work
                            .get(vessel)
                            .is_some_and(|work| matches!(work.phase, WorkPhase::Launching | WorkPhase::Running | WorkPhase::Stalled))
                            || matches!(
                                work.phase,
                                flotilla_resources::CrewWorkPhase::Done
                                    | flotilla_resources::CrewWorkPhase::Failed
                                    | flotilla_resources::CrewWorkPhase::HandedBack
                            )
                        {
                            continue;
                        }
                        let leaf = Leaf {
                            address: LeafAddress::Work { convoy: convoy.metadata.name.clone(), work: vessel.clone() },
                            field_path: format!(".crew.{role}.phase"),
                            operator: LeafOperator::Equal,
                            literal: "Done".into(),
                        };
                        actor_obligation(&leaf)?;
                        desired.push(LeafSubscriptionRow {
                            id: uuid::Uuid::nil(),
                            namespace: namespace.to_string(),
                            leaves: vec![leaf],
                            watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                            maker: LeafMaker::Actor { vessel: vessel.clone(), role: role.clone() },
                            freshness_demand: None,
                            created_at: Utc::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
                }
                for delivery in instantiate_turn_delivery(convoy, &checkouts, &observed_change_requests, &forges)? {
                    let eligible = is_active_change_request_probe(status, &delivery.rule, &delivery.leaf);
                    let changed = self
                        .subscriptions
                        .inner
                        .decisions
                        .changed(format!("{namespace}/{}/{}", convoy.metadata.name, delivery.source), eligible);
                    if !eligible {
                        if changed {
                            tracing::debug!(convoy = %convoy.metadata.name, source = %delivery.source,
                            reason = "skip_ineligible_active_crew_or_subject", "turn delivery subscription decision");
                        }
                        continue;
                    }
                    if changed {
                        tracing::debug!(convoy = %convoy.metadata.name, source = %delivery.source,
                            reason = "arm_eligible_active_crew_and_subject", "turn delivery subscription decision");
                    }
                    desired.push(LeafSubscriptionRow {
                        id: uuid::Uuid::nil(),
                        namespace: namespace.to_string(),
                        leaves: vec![delivery.leaf],
                        watcher: LeafWatcher::TurnDelivery {
                            convoy: convoy.metadata.name.clone(),
                            source: delivery.source,
                            rule: Box::new(delivery.rule),
                        },
                        maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
                        freshness_demand: None,
                        created_at: Utc::now(),
                        episode_key: EpisodeKeyFields::default(),
                    });
                }
                continue;
            }
            let exit = match instantiate_exit(convoy, &checkouts) {
                Ok(exit) => exit,
                Err(error) => {
                    tracing::warn!(convoy = %convoy.metadata.name, %error, "derive reconciler leaf subscriptions failed");
                    continue;
                }
            };
            if let InstantiatedExit::Table(entries) = exit {
                for entry in entries {
                    if !entry.leaves.is_empty() {
                        desired.push(LeafSubscriptionRow {
                            id: uuid::Uuid::nil(),
                            namespace: namespace.to_string(),
                            leaves: entry.leaves,
                            watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
                            freshness_demand: Some(Utc::now()),
                            created_at: Utc::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
                }
            }
            let status = convoy.status.as_ref().expect("parked convoy has status");
            for delivery in instantiate_turn_delivery(convoy, &checkouts, &observed_change_requests, &forges)? {
                let Some(claim_at) = status
                    .crew_work
                    .get(&delivery.rule.to.vessel)
                    .and_then(|crew| crew.get(&delivery.rule.to.role))
                    .and_then(|work| work.finished_at)
                else {
                    continue;
                };
                let maker = if delivery.leaf.address.kind() == flotilla_protocol::LeafKind::Artifact {
                    LeafMaker::Observed { refresher: "artifact".into(), external_party: "crew".into() }
                } else {
                    LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() }
                };
                desired.push(LeafSubscriptionRow {
                    id: uuid::Uuid::nil(),
                    namespace: namespace.to_string(),
                    leaves: vec![delivery.leaf],
                    watcher: LeafWatcher::TurnDelivery {
                        convoy: convoy.metadata.name.clone(),
                        source: delivery.source.clone(),
                        rule: Box::new(delivery.rule.clone()),
                    },
                    maker,
                    freshness_demand: Some(claim_at),
                    created_at: Utc::now(),
                    episode_key: EpisodeKeyFields {
                        source: Some(delivery.source.clone()),
                        convoy: Some(convoy.metadata.name.clone()),
                        vessel: Some(delivery.rule.to.vessel),
                        role: Some(delivery.rule.to.role),
                        subject_revision: status
                            .turn_deliveries
                            .get(&delivery.source)
                            .and_then(|state| state.episodes.last())
                            .map(|episode| episode.subject_revision.clone()),
                    },
                });
            }
        }

        let existing = self
            .subscriptions
            .inner
            .rows
            .lock()
            .await
            .values()
            .filter(|row| {
                row.namespace == namespace && matches!(row.watcher, LeafWatcher::ReconcilerWake { .. } | LeafWatcher::TurnDelivery { .. })
            })
            .cloned()
            .collect::<Vec<_>>();
        for row in &existing {
            if desired.iter().any(|candidate| same_standing_row(candidate, row)) {
                continue;
            }
            self.subscriptions.inner.rows.lock().await.remove(&row.id);
            self.subscriptions.forget_firings(row.id).await;
            if let Some(task) = self.subscriptions.inner.tasks.lock().await.remove(&row.id) {
                task.abort();
            }
            self.subscriptions.inner.change_requests.release(row.id).await;
            self.subscriptions.inner.issues.release(row.id).await;
        }

        'desired_rows: for mut row in desired {
            if let Some(existing) = existing.iter().find(|existing| same_standing_row(&row, existing)) {
                if existing.maker != row.maker {
                    let mut rows = self.subscriptions.inner.rows.lock().await;
                    if let Some(stored) = rows.get_mut(&existing.id) {
                        stored.maker = row.maker;
                    }
                }
                continue;
            }
            let id = uuid::Uuid::new_v4();
            row.id = id;
            self.subscriptions.inner.rows.lock().await.insert(id, row.clone());
            for subject in row.leaves.iter().filter_map(|leaf| ChangeRequestRef::from_address(namespace, &leaf.address)) {
                if let Err(error) = self.subscriptions.inner.change_requests.demand(id, subject, row.freshness_demand).await {
                    self.subscriptions.inner.rows.lock().await.remove(&id);
                    self.subscriptions.forget_firings(id).await;
                    self.subscriptions.inner.change_requests.release(id).await;
                    tracing::warn!(watcher = ?row.watcher, %error, "arm standing leaf subscription failed");
                    continue 'desired_rows;
                }
            }
            for subject in row.leaves.iter().filter_map(|leaf| IssueRef::from_address(namespace, &leaf.address)) {
                if let Err(error) = self.subscriptions.inner.issues.demand(id, subject, row.freshness_demand).await {
                    self.subscriptions.inner.rows.lock().await.remove(&id);
                    self.subscriptions.forget_firings(id).await;
                    self.subscriptions.inner.change_requests.release(id).await;
                    self.subscriptions.inner.issues.release(id).await;
                    tracing::warn!(watcher = ?row.watcher, %error, "arm standing issue leaf subscription failed");
                    continue 'desired_rows;
                }
            }
            let subscriptions = self.subscriptions.clone();
            let task = tokio::spawn(async move {
                if let Err(error) = subscriptions.watch_row(row).await {
                    tracing::warn!(subscription_id = %id, %error, "reconciler leaf subscription watch ended");
                    subscriptions.finish(id).await;
                }
            });
            self.subscriptions.inner.tasks.lock().await.insert(id, task);
        }
        Ok(())
    }
}

fn same_standing_row(left: &LeafSubscriptionRow, right: &LeafSubscriptionRow) -> bool {
    let same_freshness = left.freshness_demand == right.freshness_demand
        || (matches!(left.watcher, LeafWatcher::ReconcilerWake { .. })
            && matches!(right.watcher, LeafWatcher::ReconcilerWake { .. })
            && left.freshness_demand.is_some()
            && right.freshness_demand.is_some());
    left.namespace == right.namespace
        && left.leaves == right.leaves
        && left.watcher == right.watcher
        && same_standing_maker(&left.maker, &right.maker)
        && same_freshness
        && left.episode_key == right.episode_key
}

fn same_standing_maker(left: &LeafMaker, right: &LeafMaker) -> bool {
    match (left, right) {
        (
            LeafMaker::Controller { resource_kind: left_kind, name: left_name, ceiling: left_ceiling, .. },
            LeafMaker::Controller { resource_kind: right_kind, name: right_name, ceiling: right_ceiling, .. },
        ) => left_kind == right_kind && left_name == right_name && left_ceiling == right_ceiling,
        _ => left == right,
    }
}

fn apply_read_event<T: flotilla_resources::Resource>(
    event: flotilla_resources::ReadWatchEvent<T>,
    objects: &mut HashMap<String, ResourceObject<T>>,
) {
    match event {
        flotilla_resources::ReadWatchEvent::Added(item) | flotilla_resources::ReadWatchEvent::Modified(item) => {
            objects.insert(item.object.metadata.name.clone(), item.object);
        }
        flotilla_resources::ReadWatchEvent::Deleted(item) => {
            objects.remove(&item.object.metadata.name);
        }
        flotilla_resources::ReadWatchEvent::DeletedByName { tombstone, .. } => {
            objects.remove(&tombstone.name);
        }
    }
}

type ChangeRequestSources = HashMap<String, BTreeMap<Option<NodeId>, ResourceObject<ChangeRequest>>>;

fn resource_source(provenance: &ResourceProvenance) -> Option<NodeId> {
    match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    }
}

fn change_request_sources(list: flotilla_resources::ReadResourceList<ChangeRequest>) -> ChangeRequestSources {
    let mut sources = ChangeRequestSources::new();
    for ReadResourceObject { object, provenance } in list.items {
        sources.entry(object.metadata.name.clone()).or_default().insert(resource_source(&provenance), object);
    }
    sources
}

fn update_freshest_change_request(
    name: &str,
    sources: &ChangeRequestSources,
    selected: &mut HashMap<String, ResourceObject<ChangeRequest>>,
) {
    let freshest = sources.get(name).and_then(|copies| {
        copies.iter().max_by_key(|(source, object)| {
            (
                object.status.as_ref().map_or(object.metadata.creation_timestamp, |status| status.state.observed_at),
                object.spec.observing_authority.as_str(),
                // Local wins an otherwise exact tie, matching the initial list order.
                source.is_none(),
            )
        })
    });
    if let Some((_, object)) = freshest {
        selected.insert(name.to_string(), object.clone());
    } else {
        selected.remove(name);
    }
}

fn freshest_change_requests(sources: &ChangeRequestSources) -> HashMap<String, ResourceObject<ChangeRequest>> {
    let mut selected = HashMap::new();
    for name in sources.keys() {
        update_freshest_change_request(name, sources, &mut selected);
    }
    selected
}

type IssueSources = HashMap<String, BTreeMap<Option<NodeId>, ResourceObject<Issue>>>;

fn issue_sources(list: flotilla_resources::ReadResourceList<Issue>) -> IssueSources {
    let mut sources = IssueSources::new();
    for ReadResourceObject { object, provenance } in list.items {
        sources.entry(object.metadata.name.clone()).or_default().insert(resource_source(&provenance), object);
    }
    sources
}

fn update_freshest_issue(name: &str, sources: &IssueSources, selected: &mut HashMap<String, ResourceObject<Issue>>) {
    let freshest = sources.get(name).and_then(|copies| {
        copies.iter().max_by_key(|(source, object)| {
            (
                object.status.as_ref().map_or(object.metadata.creation_timestamp, |status| status.state.observed_at),
                object.spec.observing_authority.as_str(),
                source.is_none(),
            )
        })
    });
    if let Some((_, object)) = freshest {
        selected.insert(name.to_string(), object.clone());
    } else {
        selected.remove(name);
    }
}

fn freshest_issues(sources: &IssueSources) -> HashMap<String, ResourceObject<Issue>> {
    let mut selected = HashMap::new();
    for name in sources.keys() {
        update_freshest_issue(name, sources, &mut selected);
    }
    selected
}

#[derive(Clone, Copy)]
struct LeafObservationStaleness {
    change_request: Duration,
    issue: Duration,
}

struct LeafSubjects<'a> {
    convoys: &'a HashMap<String, ResourceObject<Convoy>>,
    vessels: &'a HashMap<String, ResourceObject<Vessel>>,
    change_requests: &'a HashMap<String, ResourceObject<ChangeRequest>>,
    usages: &'a HashMap<String, ResourceObject<Usage>>,
    issues: &'a HashMap<String, ResourceObject<Issue>>,
    artifacts: &'a HashMap<String, ResourceObject<Artifact>>,
}

fn evaluate_row(
    row: &LeafSubscriptionRow,
    subjects: &LeafSubjects<'_>,
    staleness: LeafObservationStaleness,
) -> Result<Option<LeafFire>, String> {
    let LeafSubjects { convoys, vessels, change_requests, usages, issues, artifacts } = subjects;
    let require_all = matches!(row.watcher, LeafWatcher::ReconcilerWake { .. });
    let mut matched = None;
    for leaf in &row.leaves {
        let evaluation = match &leaf.address {
            LeafAddress::Convoy { name } => {
                let subject = convoys.get(name).map(ConvoyLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Vessel { name } => {
                let subject = vessels.get(name).map(VesselLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Work { convoy, work } => {
                let subject = convoys.get(convoy).and_then(|convoy| convoy.status.as_ref()).and_then(|status| {
                    status.work.get(work).map(|work_state| WorkLeafSubject { work: work_state, crew: status.crew_work.get(work) })
                });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::ChangeRequest { service, scope, number } => {
                let name = flotilla_resources::change_request_record_name(service, scope, *number);
                let subject = change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject {
                    change_request,
                    now: Utc::now(),
                    stale_after: staleness.change_request,
                });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Issue { service, scope, number } => {
                let name = flotilla_resources::issue_record_name(service, scope, *number);
                let subject = issues.get(&name).map(|issue| IssueLeafSubject { issue, now: Utc::now(), stale_after: staleness.issue });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Usage { provider, account } => {
                let name = flotilla_resources::usage_record_name(provider, account);
                let subject = usages.get(&name).map(UsageLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Artifact { convoy, producer, kind, subject } => {
                let name = flotilla_resources::artifact_record_name(convoy, producer, kind, subject);
                let subject = artifacts.get(&name).map(ArtifactLeafSubject);
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
        };
        if evaluation.result == ThreeValue::True {
            matched = Some(LeafFire {
                subscription_id: row.id,
                watcher_id: uuid::Uuid::nil(),
                leaf: leaf.clone(),
                value: evaluation.value.expect("true leaf has a value").to_string(),
            });
            if !require_all {
                return Ok(matched);
            }
        } else if require_all {
            return Ok(None);
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
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
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use flotilla_protocol::{LeafAddress, LeafOperator};
    use flotilla_resources::{
        controller::ControllerLoop, BoundChangeRequest, ChangeRequestObservation, ChangeRequestState, CheckoutIntegrationStatus,
        CheckoutPhase, CheckoutSpec, CheckoutStatus, ConditionValue, ControllerRetry, ConvoyPhase, ConvoyReconciler, ConvoyRepositorySpec,
        ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, ExitDeclaration, InMemoryBackend, InputMeta, IntegrationCondition,
        LifecycleAuthority, ObservedCheckoutSpec, PlacementStatus, RepositoryKey, SqliteBackend, WorkPhase, WorkState, WorkflowSnapshot,
        WorkflowTemplate, CONVOY_LABEL,
    };

    use super::*;
    use crate::{
        event_sink::broadcast_test_sink,
        providers::github_api::{GithubRateLimit, GithubRateLimitKind, GithubRetrySource},
    };

    fn captured_subscriber(logs: Arc<std::sync::Mutex<Vec<u8>>>, level: tracing::Level) -> impl tracing::Subscriber + Send + Sync {
        let writer = Writer(logs);
        tracing_subscriber::fmt().without_time().with_ansi(false).with_max_level(level).with_writer(move || writer.clone()).finish()
    }

    // #2684: queue acceptance remains queued for every attention state. Only an
    // exact terminal receipt confirms submission; age raises advisory attention.
    #[hegel::test]
    fn queued_turn_waits_for_receipt_and_exposes_bound(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // All attention states, FIFO/receipt positions, missing sessions and ages
        // across the bound; pin the boundary cases too so shrinking cannot hide them.
        let rung = if tc.draw(gs::booleans()) { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
        let state = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let receipt_kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(601));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            for age in [-1, 0, 299, 300, 301, age] {
                let (backend, wake, _) = idle_nudge_scenario().await;
                let start = Utc::now();
                let now = start + chrono::Duration::seconds(age);
                let convoys = backend.using::<Convoy>("flotilla");
                let convoy = convoys.get("stalled-work").await.unwrap();
                let mut status = convoy.status.unwrap();
                status.attention = None;
                // The original firing need not remain eligible: health still follows receipts.
                status.phase = ConvoyPhase::Interrupted;
                status.stalled = None;
                status.turn_deliveries.insert("review".into(), flotilla_resources::TurnDeliveryStatus {
                    episodes: vec![TurnDeliveryEpisode {
                        subject_revision: "head".into(),
                        evidence_at: start,
                        judged_claim_at: start,
                        outcome: TurnDeliveryOutcome::Queued {
                            rung,
                            queued_at: start,
                            vessel: "work".into(),
                            role: "coder".into(),
                            message_id: "turn".into(),
                            blocking_reason: "initial".into(),
                        },
                        sender: Default::default(),
                    }],
                    ..Default::default()
                });
                // Stored episodes, including their original age, survive daemon restoration.
                let status: ConvoyStatus = serde_json::from_value(serde_json::to_value(status).unwrap()).unwrap();
                convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.unwrap();
                let sessions = backend.using::<TerminalSession>("flotilla");
                let session = sessions.get("resumed-coder").await.unwrap();
                let mut spec = session.spec;
                let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
                let make_message = |id: &str| flotilla_resources::TerminalCrewMessage {
                    id: id.into(),
                    text: "review wake".into(),
                    sender: Default::default(),
                    delivery: flotilla_resources::CrewMessageDelivery::Queued,
                    following: Vec::new(),
                    acknowledged: Default::default(),
                };
                let mut head = make_message("turn");
                head.append(make_message("later"));
                *message = Some(head);
                let session =
                    sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
                sessions
                    .update_status("resumed-coder", &session.metadata.resource_version, &flotilla_resources::TerminalSessionStatus {
                        phase: TerminalSessionPhase::Running,
                        attention: Some(TerminalAttention {
                            state: [
                                TerminalAttentionState::Unobservable,
                                TerminalAttentionState::Working,
                                TerminalAttentionState::Idle,
                                TerminalAttentionState::NeedsInput,
                            ][state],
                            source: TerminalAttentionSource::Screen,
                            as_of: now,
                        }),
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                for repeat in 0..2 {
                    let convoy = convoys.get("stalled-work").await.unwrap();
                    let prior_version = convoy.metadata.resource_version.clone();
                    wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.unwrap();
                    let observed = convoys.get("stalled-work").await.unwrap();
                    if repeat == 1 {
                        assert_eq!(observed.metadata.resource_version, prior_version, "unchanged queue evidence must not write every tick");
                    }
                    let status = observed.status.unwrap();
                    assert!(
                        matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Queued { .. }),
                        "repeat {repeat}"
                    );
                    assert_eq!(
                        status.attention.as_ref().map(|attention| attention.source.as_str()),
                        (age > 300).then_some(ConvoyAttention::QUEUED_TURN_SOURCE)
                    );
                    let TurnDeliveryOutcome::Queued { queued_at, blocking_reason, .. } =
                        &status.turn_deliveries["review"].episodes[0].outcome
                    else {
                        panic!("queued")
                    };
                    assert_eq!(*queued_at, start);
                    assert!(blocking_reason.contains("attention"));
                }
                let session = sessions.get("resumed-coder").await.unwrap();
                if receipt_kind == 2 {
                    let mut spec = session.spec.clone();
                    let TerminalSessionSource::Agent { message: Some(message), .. } = &mut spec.source else { panic!("message") };
                    message.prune_acknowledged(Some("later"));
                    sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
                } else if receipt_kind == 3 {
                    let mut spec = session.spec.clone();
                    let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
                    *message = None;
                    let session =
                        sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.unwrap();
                    let mut status = session.status.unwrap();
                    status.delivered_message_id = Some("turn".into());
                    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.unwrap();
                } else {
                    let mut status = session.status.unwrap();
                    status.delivered_message_id = Some(if receipt_kind == 0 { "turn" } else { "later" }.into());
                    sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.unwrap();
                }
                let convoy = convoys.get("stalled-work").await.unwrap();
                wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), now).await.unwrap();
                let status = convoys.get("stalled-work").await.unwrap().status.unwrap();
                assert!(matches!(status.turn_deliveries["review"].episodes[0].outcome, TurnDeliveryOutcome::Delivered {
                    rung: delivered_rung, ..
                } if delivered_rung == rung));
                assert!(status.attention.is_none());
                assert_eq!(status.turn_deliveries["review"].episodes.len(), 1);
                assert_eq!(queued_turn_evidence(None, "turn"), (false, "terminal session unavailable".into()));
            }
        });
    }

    // #2560: the inactivity bound follows the newest tool/hook evidence, not
    // total turn age or refreshed screen timestamps. Generated ages straddle
    // the exact boundary, including future evidence and absent tool/hooks.
    #[hegel::test]
    fn turn_inactivity_tracks_meaningful_evidence(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
        let evidence_age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
        let tool = tc.draw(gs::booleans());
        let hook = tc.draw(gs::booleans());
        let output = tc.draw(gs::booleans());
        let now = Utc::now();
        let start = now - chrono::Duration::seconds(age);
        let evidence = now - chrono::Duration::seconds(evidence_age);
        let obligation = NudgeObligation::builder()
            .maker(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() })
            .leaves(Vec::new())
            .working_since(start)
            .maybe_last_hook_at(hook.then_some(evidence))
            .build();
        let status = flotilla_resources::TerminalSessionStatus {
            last_tool_activity_at: tool.then_some(evidence),
            last_output_activity_at: output.then_some(evidence),
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                source: TerminalAttentionSource::Screen,
                as_of: now,
            }),
            ..Default::default()
        };
        let quiet_age = if tool || hook || output { age.min(evidence_age) } else { age };
        assert_eq!(turn_inactivity_reason(&status, &obligation, now).is_some(), quiet_age >= 900);
    }

    // #2634: a hook-capable crew with no hook after the first-turn grace gets
    // advisory attention. Screen activity does not hide it; a real hook clears it.
    #[hegel::test]
    fn missing_turn_hook_is_visible_and_recovers(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Ages cross the exact grace boundary; both harnesses, existing unrelated
        // attention, and repeated observations exercise preservation and idempotence.
        let age = tc.draw(gs::integers::<i64>().min_value(-1).max_value(1801));
        let claude = tc.draw(gs::booleans());
        let other_attention = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            // Pin the boundary with unoccupied attention so generated preservation
            // cases cannot hide an off-by-one regression.
            for (age, other_attention) in [(899, false), (900, false), (901, false), (900, true), (age, other_attention)] {
                let (backend, wake, _) = idle_nudge_scenario().await;
                let start = Utc::now();
                let now = start + chrono::Duration::seconds(age);
                let sessions = backend.using::<TerminalSession>("flotilla");
                let session = sessions.get("resumed-coder").await.expect("session");
                sessions
                    .update_status("resumed-coder", &session.metadata.resource_version, &flotilla_resources::TerminalSessionStatus {
                        phase: TerminalSessionPhase::Running,
                        started_at: Some(start),
                        crew: Some(
                            flotilla_resources::CrewSessionStatus::builder()
                                .id("crew".into())
                                .adapter(if claude { "claude-code" } else { "codex" }.into())
                                .stance("trusted-implicit".into())
                                .build(),
                        ),
                        ..Default::default()
                    })
                    .await
                    .expect("running session");
                let convoys = backend.using::<Convoy>("flotilla");
                let other = ConvoyAttention { source: "settlement".into(), reason: "keep this".into(), raised_at: start };
                if other_attention {
                    flotilla_resources::apply_status_patch(
                        &convoys,
                        "stalled-work",
                        &flotilla_resources::ConvoyStatusPatch::SetSettlementAttention { attention: Some(other.clone()) },
                    )
                    .await
                    .expect("other attention");
                }
                for _ in 0..2 {
                    observe_actor(&backend, &wake, TerminalAttentionState::Working, now).await;
                    let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
                    if other_attention {
                        assert_eq!(status.attention, Some(other.clone()));
                    } else {
                        assert_eq!(status.attention.as_ref().map(|a| a.source.as_str()), (age >= 900).then_some("missing-turn-hook"));
                        if let Some(attention) = status.attention {
                            assert_eq!(attention.raised_at, now);
                            assert!(attention.reason.contains("no hook"));
                        }
                    }
                    assert!(status.stalled.is_none(), "missing hooks alone never stall a working crew");
                }
                let hook_at = now + chrono::Duration::seconds(1);
                observe_actor_source(&backend, &wake, TerminalAttentionState::Idle, TerminalAttentionSource::Hook, hook_at).await;
                let status = convoys.get("stalled-work").await.expect("convoy").status.expect("status");
                assert_eq!(status.attention, other_attention.then_some(other));
                assert_eq!(status.nudge_obligations[0].last_hook_at, Some(hook_at));
                observe_actor(&backend, &wake, TerminalAttentionState::Working, hook_at + chrono::Duration::seconds(120)).await;
                assert!(convoys
                    .get("stalled-work")
                    .await
                    .expect("convoy")
                    .status
                    .expect("status")
                    .attention
                    .is_none_or(|a| a.source != "missing-turn-hook"));
            }
        });
    }

    // #2560: fresh screen redraws cannot keep a silent turn able forever.
    // The injected clock crosses the inactivity boundary without sleeps or a live crew.
    #[tokio::test]
    async fn silent_working_turn_reaches_supervision() {
        let (backend, wake, delivery) = actor_nudge_scenario(&[("governor", 1, ConvoyPhase::Active)]).await;
        let start = Utc::now();
        let sessions = backend.using::<TerminalSession>("flotilla");
        let governor = sessions
            .create(
                &InputMeta::builder()
                    .name("governor-session".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "governor".into()),
                        (VESSEL_LABEL.into(), "govern".into()),
                        (ROLE_LABEL.into(), "governor".into()),
                    ]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec::builder()
                    .env_ref("env".into())
                    .role("governor".into())
                    .source(TerminalSessionSource::Tool { command: "test".into() })
                    .cwd("/".into())
                    .pool("cleat".into())
                    .build(),
            )
            .await
            .expect("governor session");
        sessions
            .update_status("governor-session", &governor.metadata.resource_version, &flotilla_resources::TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: Some(TerminalAttention {
                    state: TerminalAttentionState::Working,
                    source: TerminalAttentionSource::Screen,
                    as_of: start + chrono::Duration::seconds(900),
                }),
                ..Default::default()
            })
            .await
            .expect("working supervisor");
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start).await;
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(899)).await;
        assert!(backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_none());
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
        let status = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
        let stall = status.stalled.expect("silent working turn must stall");
        assert!(stall.evidence.contains("turn inactivity bound"), "{}", stall.evidence);
        assert_eq!(stall.source, StallEvidenceSource::Session);
        assert_eq!(stall.rung, StallRung::Governor);
        assert_eq!(stall.supervisor.expect("supervisor").convoy, "governor");
        assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor", "a busy TUI needs supervision, not a queued nudge");
        // The real watch loop re-arms the row with its supervisor as maker.
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        wake.sync_rows("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("re-arm supervised rows");
        // Repeated Working redraws must not clear a supervised stall.
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(901)).await;
        assert!(backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_some());
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(902)).await;
        let recovered = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
        assert!(recovered.stalled.is_none(), "tool activity recovers the maker: {:?}", recovered.stalled);
    }

    // Losing observation is not a new turn boundary: stale/Unobservable
    // evidence must not reset a hung turn's bound or masquerade as a real hook.
    #[tokio::test]
    async fn silent_turn_survives_stale_and_unobservable_evidence() {
        for source in [TerminalAttentionSource::Screen, TerminalAttentionSource::Hook] {
            let (backend, wake, _) = idle_nudge_scenario().await;
            let start = Utc::now();
            observe_actor_source(&backend, &wake, TerminalAttentionState::Working, source, start).await;
            let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
            wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(120))
                .await
                .expect("stale observation");
            observe_actor_source(&backend, &wake, TerminalAttentionState::Unobservable, source, start + chrono::Duration::seconds(121))
                .await;
            observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(900)).await;
            let status = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status");
            assert!(status.stalled.expect("silent turn still stalls").evidence.contains("turn inactivity bound"));
            observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(901)).await;
            assert!(
                backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy").status.expect("status").stalled.is_none(),
                "new turn activity restores ability"
            );
        }
    }

    // #2211: typed causes and PR identity select remedies regardless of explanation wording.
    #[hegel::test]
    fn refusal_remedies_ignore_explanation_wording(tc: hegel::TestCase) {
        use hegel::generators as gs;

        // Cover both remedies, empty/legacy cause lists, duplicates, combined causes,
        // misleading old matcher phrases, and PR numbers across the u64 boundaries.
        let number = tc.draw(gs::integers::<u64>());
        let variant = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let conflict =
            CrewCompletionRefusalCause::ConflictingChangeRequest { service: "github.com".into(), scope: "owner/repo".into(), number };
        let missing = CrewCompletionRefusalCause::MissingChangeRequestObservation {
            service: "forge.example".into(),
            scope: "other/repo".into(),
            number,
        };
        let causes = match variant {
            0 => vec![],
            1 => vec![conflict.clone()],
            2 => vec![missing.clone()],
            3 => vec![missing, conflict.clone()],
            _ => vec![conflict.clone(), conflict],
        };
        let expected = match variant {
            0 => "Resolve the unmet expectation".to_string(),
            2 => format!("PR #{number} has no observation yet"),
            _ => format!("PR #{number} is conflicting"),
        };
        for text in ["", "different human explanation", "no federated observation; could not observe PR 42; cr/github.com/owner/repo/42"] {
            let refusal = CrewCompletionRefusal::builder().expectation(text.into()).causes(causes.clone()).consecutive_count(1).build();
            let brief = super::refusal_nudge_brief(&refusal);
            assert!(brief.contains(&expected), "{brief}");
            assert!(brief.contains("flotilla crew complete"));
        }
    }

    #[test]
    fn reconciler_row_identity_ignores_regenerated_freshness_instant() {
        let row = |freshness_demand| LeafSubscriptionRow {
            id: uuid::Uuid::nil(),
            namespace: "flotilla".to_string(),
            leaves: vec!["cr/github.com/flotilla-org/flotilla/1699 .state == merged".parse().expect("leaf")],
            watcher: LeafWatcher::ReconcilerWake { convoy: "landing".to_string() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(freshness_demand),
            created_at: freshness_demand,
            episode_key: EpisodeKeyFields::default(),
        };

        assert!(same_standing_row(
            &row("2026-08-23T14:00:00Z".parse().expect("first instant")),
            &row("2026-08-23T14:01:00Z".parse().expect("second instant")),
        ));
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
        requests: std::sync::Mutex<Vec<TurnDeliveryRequest>>,
        holds: AtomicUsize,
        // Boundary fake: turn delivery may fail while the agent session reconnects.
        unavailable: AtomicBool,
    }

    #[async_trait]
    impl TurnDeliveryActuator for RecordingTurnDelivery {
        async fn deliver(&self, request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err("supervisor reconnecting".into());
            }
            let mut requests = self.requests.lock().expect("record turn-delivery request");
            let rung = if requests.is_empty() { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
            requests.push(request.clone());
            Ok(rung)
        }

        async fn hold(&self, _request: &TurnDeliveryRequest, _act: &HoldAct, _reason: &str) -> Result<(), String> {
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

    // A live wait must survive queue overflow and evaluate the current state
    // after relisting, with its original subscription identity intact.
    #[tokio::test]
    async fn leaf_wait_recovers_after_watch_overflow() {
        use futures::FutureExt;
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let table = supervision_wake(&backend).subscriptions;
        create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
        let id = uuid::Uuid::new_v4();
        let row = LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf(LeafAddress::Convoy { name: "busy".into() }, ".status.phase", "Failed")],
            watcher: LeafWatcher::WaitCaller { connection_id: uuid::Uuid::new_v4() },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        };
        table.inner.rows.lock().await.insert(id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        // Poll through watch registration and snapshot loading, then deliberately
        // stop polling while writes outrun the bounded ring.
        assert!(watching.as_mut().now_or_never().is_none());
        let convoys = backend.using::<Convoy>("flotilla");
        let mut object = convoys.get("busy").await.expect("convoy");
        for index in 0..600 {
            let spec = ConvoySpec::builder().workflow_ref(format!("workflow-{index}")).build();
            object = convoys.update(&InputMeta::from(&object.metadata), &object.metadata.resource_version, &spec).await.expect("update");
        }
        convoys
            .update_status("busy", &object.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() })
            .await
            .expect("fail convoy");
        tokio::time::timeout(Duration::from_secs(2), watching).await.expect("wait recovers").expect("watch succeeds");
        assert!(!table.rows().await.iter().any(|row| row.id == id), "recovered wait fires and releases its row");
        assert!(table.inner.last_firings.lock().await.is_empty(), "one-shot wait releases firing state");
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

    // Permanent overload must bound six-kind snapshot work, retain identity
    // and accounting during sleeps, then recover from the latest state.
    #[tokio::test(start_paused = true)]
    async fn leaf_sustained_overload_bounds_snapshot_work_and_recovers() {
        use futures::FutureExt;
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let table = supervision_wake(&backend).subscriptions;
        create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
        let row = overload_row(uuid::Uuid::new_v4());
        let id = row.id;
        table.inner.last_firings.lock().await.insert((id, row.leaves[0].clone()), LeafFiringRecord {
            leaf: row.leaves[0].clone(),
            value: "Active".into(),
            fired_at: Utc::now(),
        });
        table.inner.unable_since.lock().await.insert(id, (UnableEvidenceKey::Absent, Utc::now()));
        table.inner.stale_attention_reported.lock().await.insert(id);
        overload_demand(&table, id).await;
        table.inner.rows.lock().await.insert(id, row.clone());
        let mut watching = Box::pin(table.watch_row(row));
        assert!(watching.as_mut().now_or_never().is_none());
        // 30,000 writes in two simulated seconds, with the leaf allowed
        // to run between bursts. Count completed six-kind snapshots, not RSS.
        for _ in 0..100 {
            overload_convoy(&backend).await;
            for _ in 0..2 {
                assert!(watching.as_mut().now_or_never().is_none());
            }
            tokio::time::advance(Duration::from_millis(20)).await;
        }
        let snapshots = table.inner.snapshot_loads.load(Ordering::SeqCst);
        eprintln!("overload: 30000 writes/2s, {snapshots} six-kind snapshots ({} logical lists)", snapshots * 6);
        assert!(snapshots <= 10, "repeated expiry must throttle full snapshots: {snapshots}");
        assert!(table.rows().await.iter().any(|row| row.id == id));
        assert_eq!(table.inner.change_requests.active_demands().await, 1, "backoff retains demand");
        assert_eq!(table.inner.last_firings.lock().await.len(), 1, "backoff retains firing history");
        assert!(table.inner.unable_since.lock().await.contains_key(&id), "backoff retains episode accounting");
        assert!(table.inner.stale_attention_reported.lock().await.contains(&id));
        let convoys = backend.using::<Convoy>("flotilla");
        let object = convoys.get("busy").await.expect("convoy");
        convoys
            .update_status("busy", &object.metadata.resource_version, &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() })
            .await
            .expect("fail convoy");
        tokio::time::timeout(Duration::from_secs(2), watching).await.expect("bounded recovery latency").expect("watch succeeds");
        assert!(table.rows().await.is_empty());
        assert_eq!(table.inner.change_requests.active_demands().await, 0, "completion releases demand");
        assert!(table.inner.last_firings.lock().await.is_empty());
        assert!(table.inner.unable_since.lock().await.is_empty());
        assert!(table.inner.stale_attention_reported.lock().await.is_empty());
    }

    // Connection cancellation must abort a sleeping recovery task and release
    // row/firing state immediately rather than waiting for the recovery timer.
    #[tokio::test(start_paused = true)]
    async fn leaf_overload_backoff_is_cancelled_by_disconnect() {
        use futures::FutureExt;
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let table = supervision_wake(&backend).subscriptions;
        create_convoy(&backend, "busy", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
        let connection_id = uuid::Uuid::new_v4();
        let row = overload_row(connection_id);
        let id = row.id;
        overload_demand(&table, id).await;
        table.inner.rows.lock().await.insert(id, row.clone());
        let watching_table = table.clone();
        let mut watching = Box::pin(async move { watching_table.watch_row(row).await });
        assert!(watching.as_mut().now_or_never().is_none());
        overload_convoy(&backend).await;
        assert!(watching.as_mut().now_or_never().is_none());
        assert!(watching.as_mut().now_or_never().is_none());
        assert_eq!(table.inner.snapshot_loads.load(Ordering::SeqCst), 2, "first recovery is immediate");
        overload_convoy(&backend).await;
        assert!(watching.as_mut().now_or_never().is_none());
        assert_eq!(table.inner.snapshot_loads.load(Ordering::SeqCst), 2, "second expiry sleeps");
        let task = tokio::spawn(async move {
            watching.await.expect("watch succeeds");
        });
        let abort = task.abort_handle();
        table.inner.tasks.lock().await.insert(id, task);
        table.unsubscribe_connection(connection_id).await;
        tokio::task::yield_now().await;
        assert!(abort.is_finished(), "disconnect cancels the pending sleep");
        assert!(table.rows().await.is_empty());
        assert_eq!(table.inner.change_requests.active_demands().await, 0, "completion releases demand");
        assert!(table.inner.last_firings.lock().await.is_empty());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(table.inner.snapshot_loads.load(Ordering::SeqCst), 2, "cancelled leaf does not relist");
    }

    // Generated healthy/overloaded intervals cross the reset boundary; retry
    // delay never exceeds one second and a healthy interval restores immediacy.
    #[hegel::test]
    fn leaf_recovery_budget_resets_after_healthy_watch(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(30));
        let mut recovery = LeafWatchRecovery::default();
        for _ in 0..steps {
            let healthy_ms = tc.draw(gs::integers::<u64>().min_value(0).max_value(6000));
            let delay = recovery.expired(Duration::from_millis(healthy_ms));
            assert!(delay <= Duration::from_secs(1));
            if healthy_ms >= 5000 {
                assert!(delay.is_zero());
            }
        }
        assert!(recovery.expired(Duration::from_secs(5)).is_zero());
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

    async fn project_supervision_case(
        governors: &[(&str, u64, ConvoyPhase)],
    ) -> (ResourceBackend, ReconcilerWake, Arc<RecordingTurnDelivery>) {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let wake = supervision_wake(&backend);
        let delivery = Arc::new(RecordingTurnDelivery::default());
        wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys
            .create(
                &InputMeta::builder().name("stalled-work".into()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("workflow".into())
                    .role("graphql-budget".into())
                    .project_ref("wheelhouse".into())
                    .build(),
            )
            .await
            .expect("create work convoy");
        convoys
            .update_status("stalled-work", &source.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Active,
                work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
                workflow_snapshot: Some(WorkflowSnapshot {
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
            })
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
                .update_status(name, &created.metadata.resource_version, &ConvoyStatus {
                    phase: *phase,
                    crew_work: BTreeMap::from([(
                        "govern".into(),
                        BTreeMap::from([("governor".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
                    )]),
                    ..Default::default()
                })
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
        wake.subscriptions.inner.rows.lock().await.insert(id, LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: "stalled-work".into() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        });
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
                    pool: "cleat".into(),
                },
            )
            .await
            .expect("crew session");
        (backend, wake, delivery)
    }

    // #2211: both typed remedies survive durable status restoration and reach an
    // idle crew even when the explanation omits all former matcher phrases.
    #[tokio::test]
    async fn stored_refusal_nudges_use_typed_causes_after_restore() {
        for (cause, remedy) in [
            (
                CrewCompletionRefusalCause::ConflictingChangeRequest {
                    service: "github.com".into(),
                    scope: "owner/repo".into(),
                    number: 42,
                },
                "PR #42 is conflicting",
            ),
            (
                CrewCompletionRefusalCause::MissingChangeRequestObservation {
                    service: "forge.example".into(),
                    scope: "other/repo".into(),
                    number: 73,
                },
                "PR #73 has no observation yet",
            ),
        ] {
            let (backend, wake, delivery) = idle_nudge_scenario().await;
            let convoys = backend.using::<Convoy>("flotilla");
            let convoy = convoys.get("stalled-work").await.expect("convoy");
            let mut status = convoy.status.expect("status");
            status.crew_work.get_mut("work").expect("crew").get_mut("coder").expect("coder").completion_refusal = Some(
                CrewCompletionRefusal::builder()
                    .expectation("Presentation wording changed".into())
                    .causes(vec![cause])
                    .consecutive_count(1)
                    .build(),
            );
            let stored = serde_json::to_vec(&status).expect("store status");
            let restored = serde_json::from_slice(&stored).expect("restore status");
            convoys.update_status("stalled-work", &convoy.metadata.resource_version, &restored).await.expect("persist refusal");
            let start = Utc::now();
            for second in [0, 60, 120, 180] {
                observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
            }
            let requests = delivery.requests.lock().expect("deliveries");
            assert_eq!(requests.len(), 1);
            assert!(requests[0].brief.contains(remedy), "{}", requests[0].brief);
            assert!(requests[0].brief.contains("Presentation wording changed"));
        }
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

    #[tokio::test]
    async fn claude_stop_flickers_and_nudge_reply_do_not_nudge_active_work() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let start = Utc::now();
        for second in (0..900).step_by(30) {
            observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(second)).await;
            observe_claude_hook(&backend, &wake, "post-tool-use", start + chrono::Duration::seconds(second + 1)).await;
            observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(second + 2)).await;
        }
        assert!(delivery.requests.lock().expect("deliveries").is_empty(), "per-response Stop is not sustained idle");
        // Finally rest long enough to receive a nudge, then reply to it.
        for second in [900, 960, 1020, 1080] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(1081)).await;
        observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(1082)).await;
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "reply Stop must not rearm the nudge budget");
    }

    #[tokio::test]
    async fn nudge_reply_preserves_idle_clock_and_allows_second_nudge_after_backoff() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(181)).await;
        for second in [240, 300] {
            observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
        }
        observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(359)).await;
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").nudge_obligations[0].quiet_since, Some(start), "echo does not restart the idle clock");
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(360)).await;
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle gets the second backed-off nudge");
    }

    #[tokio::test]
    async fn actual_tool_work_after_nudge_cancels_idle_without_replenishing_budget() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        observe_claude_hook(&backend, &wake, "user-prompt-submit", start + chrono::Duration::seconds(181)).await;
        observe_claude_hook(&backend, &wake, "pre-tool-use", start + chrono::Duration::seconds(182)).await;
        for second in [240, 300] {
            observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(second)).await;
        }
        observe_claude_hook(&backend, &wake, "post-tool-use", start + chrono::Duration::seconds(358)).await;
        observe_claude_hook(&backend, &wake, "stop", start + chrono::Duration::seconds(359)).await;
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(360)).await;
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "real work requires a new continuous idle grace");
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").nudge_obligations[0].quiet_since, Some(start + chrono::Duration::seconds(359)));
        for second in [420, 480, 539] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(
            convoy.status.expect("status").nudge_obligations[0].history.len(),
            2,
            "commands alone do not reset the obligation budget"
        );
    }

    #[tokio::test]
    async fn genuine_idle_has_obligation_budget_backoff_and_operator_escalation() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let start = Utc::now();
        for second in [0, 60, 120, 179] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert!(delivery.requests.lock().expect("deliveries").is_empty());
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(180)).await;
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        // #2592: even mechanical nudges identify the crew and convoy they concern.
        assert!(delivery.requests.lock().expect("deliveries")[0]
            .brief
            .contains("coder@work in graphql-budget@wheelhouse (resource ref: stalled-work)"));
        // A brief working flicker cannot replenish the unmet claim's budget.
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(181)).await;
        for second in [182, 240, 300, 359] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        for second in [362, 420, 480, 540, 600, 659] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2);
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").stalled.expect("stall").rung, StallRung::Nudge);
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(722)).await;
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").stalled.expect("stall").rung, StallRung::Operator);
        observe_actor(&backend, &wake, TerminalAttentionState::Working, start + chrono::Duration::seconds(723)).await;
        for second in [724, 780, 840, 900, 960, 1020] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "working does not reset an obligation budget");
    }

    #[tokio::test]
    async fn operator_message_echo_preserves_clock_without_replenishing_budget() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        let sessions = backend.using::<TerminalSession>("flotilla");
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut spec = session.spec.clone();
        let TerminalSessionSource::Agent { message, .. } = &mut spec.source else { panic!("agent") };
        *message = Some(flotilla_resources::TerminalCrewMessage {
            id: "owner-guidance".into(),
            text: "Keep working on the fix".into(),
            sender: flotilla_resources::CrewMessageSender::OperatorResume { principal: None },
            delivery: flotilla_resources::CrewMessageDelivery::Queued,
            acknowledged: Default::default(),
            following: Vec::new(),
        });
        sessions.update(&InputMeta::from(&session.metadata), &session.metadata.resource_version, &spec).await.expect("owner guidance");
        for second in [240, 300, 360, 420] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1, "pending owner guidance is never displaced");
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("status");
        status.delivered_message_id = Some("owner-guidance".into());
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("deliver guidance");
        // Judge delivery without replacing its receipt with the scenario helper.
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(421))
            .await
            .expect("delivery grace");
        for second in [480, 540, 600] {
            let session = sessions.get("resumed-coder").await.expect("session");
            let mut status = session.status.expect("status");
            status.attention = Some(TerminalAttention {
                state: TerminalAttentionState::Idle,
                source: TerminalAttentionSource::Hook,
                as_of: start + chrono::Duration::seconds(second),
            });
            sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("response Stop");
            let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
            wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), start + chrono::Duration::seconds(second))
                .await
                .expect("response grace");
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 2, "continued idle can receive the second nudge after owner reply");
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").nudge_obligations[0].history.len(), 2);
    }

    #[tokio::test]
    async fn observed_commit_resets_budget_but_refresh_does_not() {
        let (backend, wake, delivery) = idle_nudge_scenario().await;
        let checkouts = backend.using::<Checkout>("flotilla");
        let checkout = checkouts
            .create(
                &InputMeta::builder()
                    .name("actor-checkout".into())
                    .labels(BTreeMap::from([(CONVOY_LABEL.into(), "stalled-work".into())]))
                    .build(),
                &CheckoutSpec::Observed(
                    flotilla_resources::ObservedCheckoutSpec::builder()
                        .r#ref("work".into())
                        .path("/workspace".into())
                        .repo_ref(flotilla_protocol::RepositoryKey("repo".into()))
                        .host_ref("host".into())
                        .is_main(false)
                        .build(),
                ),
            )
            .await
            .expect("checkout");
        let mut checkout_status = flotilla_resources::CheckoutStatus::default();
        checkout_status.integration.head_revision = Some("before".into());
        checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("initial HEAD");
        let vessels = backend.using::<Vessel>("flotilla");
        let vessel = vessels
            .create(
                &InputMeta::builder()
                    .name("actor-vessel".into())
                    .labels(BTreeMap::from([(CONVOY_LABEL.into(), "stalled-work".into())]))
                    .build(),
                &flotilla_resources::VesselSpec {
                    convoy_ref: "stalled-work".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "policy".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        vessels
            .update_status("actor-vessel", &vessel.metadata.resource_version, &flotilla_resources::VesselStatus {
                checkout_refs: BTreeMap::from([(flotilla_protocol::RepositoryKey("repo".into()), "actor-checkout".into())]),
                ..Default::default()
            })
            .await
            .expect("checkout association");
        let start = Utc::now();
        for second in [0, 60, 120, 180] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        let checkout = checkouts.get("actor-checkout").await.expect("checkout");
        checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("same HEAD refresh");
        observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(240)).await;
        let convoy = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("convoy");
        assert_eq!(convoy.status.expect("status").nudge_obligations[0].history.len(), 1);
        let checkout = checkouts.get("actor-checkout").await.expect("checkout");
        checkout_status.integration.head_revision = Some("after".into());
        checkouts.update_status("actor-checkout", &checkout.metadata.resource_version, &checkout_status).await.expect("new commit");
        for second in [241, 300, 360, 420, 421] {
            observe_actor(&backend, &wake, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
        }
        let requests = delivery.requests.lock().expect("deliveries");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.source == "stall-nudge-1"), "new commit resets the obligation budget");
    }

    #[tokio::test]
    async fn resumed_stalled_crew_is_not_nudged_until_its_briefed_turn_ends() {
        let (backend, wake, delivery) = project_supervision_case(&[]).await;
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let resumed_at = Utc::now();
        flotilla_resources::apply_status_patch(
            &convoys,
            "stalled-work",
            &flotilla_resources::external_patches::resume_crew_work(
                "work".into(),
                "coder".into(),
                resumed_at,
                "Continue with the operator's guidance".into(),
                Some("resume-brief".into()),
            ),
        )
        .await
        .expect("resume declared stall");
        let sessions = backend.clone().using::<TerminalSession>("flotilla");
        let session = sessions
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
                        message: Some(flotilla_resources::TerminalCrewMessage {
                            id: "resume-brief".into(),
                            text: "Continue with the operator's guidance".into(),
                            sender: flotilla_resources::CrewMessageSender::OperatorResume { principal: None },
                            delivery: flotilla_resources::CrewMessageDelivery::Queued,
                            acknowledged: Default::default(),
                            following: Vec::new(),
                        }),
                    },
                    cwd: "/workspace".into(),
                    pool: "cleat".into(),
                },
            )
            .await
            .expect("crew session");
        let idle_before_delivery = TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: Utc::now(),
            source: flotilla_resources::TerminalAttentionSource::Hook,
        };
        sessions
            .update_status(&session.metadata.name, &session.metadata.resource_version, &flotilla_resources::TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                attention: Some(idle_before_delivery),
                ..Default::default()
            })
            .await
            .expect("idle before brief delivery");
        let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge before delivery");
        assert!(delivery.requests.lock().expect("nudge requests").is_empty());
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("status");
        status.delivered_message_id = Some("resume-brief".into());
        status.attention = None;
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("brief delivered");
        let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge during turn");
        assert!(delivery.requests.lock().expect("nudge requests").is_empty());
        let session = sessions.get("resumed-coder").await.expect("session");
        let mut status = session.status.expect("status");
        status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Idle,
            as_of: Utc::now(),
            source: flotilla_resources::TerminalAttentionSource::Hook,
        });
        sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("turn ended");
        let convoy = convoys.get("stalled-work").await.expect("resumed convoy");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), convoy)])).await.expect("judge after turn");
        assert!(delivery.requests.lock().expect("nudge requests").is_empty());
        let quiet_at = Utc::now() + chrono::Duration::minutes(3);
        // Fresh screen confirmations preserve quiet duration; another Stop
        // would instead restart it because a response just happened.
        for second in [120, 60, 0] {
            let observed_at = quiet_at - chrono::Duration::seconds(second);
            let session = sessions.get("resumed-coder").await.expect("session");
            let mut status = session.status.expect("status");
            status.attention = Some(TerminalAttention {
                state: TerminalAttentionState::Idle,
                as_of: observed_at,
                source: TerminalAttentionSource::Screen,
            });
            sessions.update_status("resumed-coder", &session.metadata.resource_version, &status).await.expect("quiet confirmation");
            let convoy = convoys.get("stalled-work").await.expect("convoy");
            wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), convoy)]), observed_at)
                .await
                .expect("judge quiet turn");
        }
        assert_eq!(delivery.requests.lock().expect("nudge requests").len(), 1);
    }

    #[tokio::test]
    async fn missing_resumed_session_does_not_suppress_supervision_forever() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        flotilla_resources::apply_status_patch(
            &convoys,
            "stalled-work",
            &flotilla_resources::external_patches::resume_crew_work(
                "work".into(),
                "coder".into(),
                Utc::now() - chrono::Duration::minutes(3),
                "Continue with the operator's guidance".into(),
                Some("lost-resume-brief".into()),
            ),
        )
        .await
        .expect("resume declared stall");
        let row_id = *wake.subscriptions.inner.rows.lock().await.keys().next().expect("actor row");
        wake.subscriptions
            .inner
            .unable_since
            .lock()
            .await
            .insert(row_id, (UnableEvidenceKey::Absent, Utc::now() - chrono::Duration::minutes(3)));
        let source = convoys.get("stalled-work").await.expect("resumed convoy");
        let governor = convoys.get("governor").await.expect("governor");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source), ("governor".into(), governor)]))
            .await
            .expect("judge missing session");
        let status = convoys.get("stalled-work").await.expect("source").status.expect("status");
        let requests = delivery.requests.lock().expect("supervisor requests");
        assert!(!requests.is_empty(), "missing session should be supervised: {:?}", status.stalled);
        assert_eq!(requests[0].convoy, "governor");
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
            .update_status("wheelhouse-governor", &created.metadata.resource_version, &flotilla_resources::ConvoyEnsureStatus {
                convoy_ref: Some(convoy_ref.into()),
                ..Default::default()
            })
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

    #[tokio::test]
    async fn unavailable_governor_recovers_in_one_pass() {
        for unavailable in GovernorUnavailable::ALL {
            unavailable_governor_scenario(unavailable, &[false, true], FallbackRecord::Current).await;
            unavailable_governor_scenario(unavailable, &[true], FallbackRecord::LegacyExhausted).await;
        }
    }

    // A governor retry after an explicit escalation must not revisit the Bosun
    // rung that the supervisor already consumed, even across a restart.
    #[tokio::test]
    async fn unavailable_governor_preserves_consumed_bosun_rung() {
        let (backend, mut wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        status
            .crew_work
            .get_mut("work")
            .expect("crew")
            .insert("bosun".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build());
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("bosun available");
        let now = Utc::now();
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("route to bosun");
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        let condition = status.stalled.as_mut().expect("stall");
        assert_eq!(condition.rung, StallRung::Bosun);
        // Same persisted transition as crew supervise ... escalate: clear ownership,
        // preserve the consumed index, and restore the original actor as maker.
        condition.supervisor = None;
        condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
        condition.rung = StallRung::Operator;
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("escalate");
        delivery.unavailable.store(true, Ordering::SeqCst);
        for _ in 0..2 {
            wake = supervision_wake(&backend);
            wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
            let objects =
                convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
            wake.sync_rows("flotilla", &objects).await.expect("rebuild rows");
            wake.judge_stalls_at("flotilla", &objects, now).await.expect("retry governor");
            let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
            assert_eq!(stalled.rung, StallRung::Operator);
            assert_eq!(stalled.supervision_index, Some(0));
            assert!(!stalled.supervision_exhausted);
        }
        delivery.unavailable.store(false, Ordering::SeqCst);
        let objects =
            convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
        wake.sync_rows("flotilla", &objects).await.expect("rows");
        wake.judge_stalls_at("flotilla", &objects, now).await.expect("recover governor");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stalled.rung, StallRung::Governor);
        assert_eq!(stalled.supervision_index, Some(1));
        let requests = delivery.requests.lock().expect("deliveries");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].role, "bosun");
        assert_eq!(requests[1].role, "governor");
    }

    // Explicit escalation by the last governor consumes the whole policy.
    // Legacy cursor recovery must not send the stall back to that governor.
    #[tokio::test]
    async fn governor_escalation_exhausts_default_policy_once() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys.get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("route governor");
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        let condition = status.stalled.as_mut().expect("stall");
        condition.supervisor = None;
        condition.maker = Some(LeafMaker::Actor { vessel: "work".into(), role: "coder".into() });
        condition.rung = StallRung::Operator;
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("governor escalates");
        for _ in 0..2 {
            let source = convoys.get("stalled-work").await.expect("source");
            wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("consume policy");
            let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
            assert_eq!(stalled.rung, StallRung::Operator);
            assert!(stalled.supervision_exhausted);
            assert_eq!(stalled.supervision_index, Some(2));
            assert!(stalled.supervisor.is_none());
            assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
        }
    }

    // Deliberate operator-only or empty policies consume the ladder. Routine
    // reconciles must not rewrite the condition or repeatedly retry that decision.
    #[tokio::test]
    async fn operator_only_supervision_is_consumed_once() {
        for policy in [Vec::new(), vec![SupervisionTarget::Operator]] {
            let (backend, wake, delivery) = project_supervision_case(&[]).await;
            let convoys = backend.using::<Convoy>("flotilla");
            let source = convoys.get("stalled-work").await.expect("source");
            let mut status = source.status.expect("status");
            status.workflow_snapshot.as_mut().expect("snapshot").supervision = Some(policy);
            convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("operator policy");
            let mut previous_version = None;
            for _ in 0..3 {
                let objects =
                    convoys.list().await.expect("convoys").items.into_iter().map(|object| (object.metadata.name.clone(), object)).collect();
                wake.sync_rows("flotilla", &objects).await.expect("rows");
                wake.judge_stalls("flotilla", &objects).await.expect("judge operator policy");
                let source = convoys.get("stalled-work").await.expect("source");
                let stalled = source.status.expect("status").stalled.expect("stall");
                assert!(stalled.supervision_exhausted);
                assert!(stalled.supervision_index.is_some());
                assert!(stalled.supervisor.is_none());
                assert_eq!(stalled.rung, StallRung::Operator);
                if let Some(version) = previous_version {
                    assert_eq!(source.metadata.resource_version, version);
                }
                previous_version = Some(source.metadata.resource_version);
                assert!(delivery.requests.lock().expect("deliveries").is_empty());
            }
        }
    }

    // WARN events must expose routing failures as structured convoy/target/reason
    // fields; skipped lower candidates must not warn when a governor accepts delivery.
    #[test]
    fn unavailable_governor_warns_with_routing_fields() {
        #[derive(Clone)]
        struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("logs").extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for unavailable in GovernorUnavailable::ALL {
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = LogWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            tracing::subscriber::with_default(subscriber, || {
                runtime.block_on(unavailable_governor_scenario(unavailable, &[false, true], FallbackRecord::Current));
            });
            let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8 logs");
            let warnings = text.lines().collect::<Vec<_>>();
            assert_eq!(warnings.len(), 1, "unchanged unavailable routing warns once: {text}");
            for warning in warnings {
                assert!(warning.contains("WARN"), "{warning}");
                assert!(warning.contains("convoy=stalled-work"), "{warning}");
                assert!(warning.contains("target="), "{warning}");
                assert!(warning.contains("reason="), "{warning}");
                // DeliveryError logs the transport failure at the attempted send,
                // before the policy-fallback warning that carries cursor/evidence.
                if !matches!(unavailable, GovernorUnavailable::DeliveryError) {
                    assert!(warning.contains("supervision_start="), "{warning}");
                    assert!(warning.contains("supervision_policy_len="), "{warning}");
                    assert!(warning.contains("evidence="), "{warning}");
                }
                // #2592: operator fallback logs contain the source address and exact actionable crew.
                assert!(warning.contains("coder@work in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{warning}");
                assert!(warning.contains("--convoy 'stalled-work' --vessel 'work' --role 'coder' resume"), "{warning}");
                assert!(
                    warning.contains(if matches!(unavailable, GovernorUnavailable::DeliveryError) {
                        "supervisor reconnecting"
                    } else {
                        "supervisor_lookup_failed"
                    }),
                    "{warning}"
                );
            }
        }
        // A live governor does not turn an explicit/consumed operator policy
        // into a lookup failure. Both operator routes must explain that choice.
        for (policy, expected_reason) in
            [(Vec::new(), "supervision_policy_exhausted"), (vec![SupervisionTarget::Operator], "operator_rung_selected")]
        {
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = LogWriter(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            tracing::subscriber::with_default(subscriber, || {
                runtime.block_on(async {
                    let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
                    let convoys = backend.using::<Convoy>("flotilla");
                    let source = convoys.get("stalled-work").await.expect("source");
                    let now = source.metadata.creation_timestamp + chrono::Duration::seconds(120);
                    let mut status = source.status.expect("status");
                    status.workflow_snapshot.as_mut().expect("snapshot").supervision = Some(policy);
                    let source =
                        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("operator policy");
                    wake.judge_stalls_at("flotilla", &HashMap::from([("stalled-work".into(), source)]), now).await.expect("judge policy");
                    assert!(delivery.requests.lock().expect("deliveries").is_empty(), "policy must not contact the live governor");
                });
            });
            let text = String::from_utf8(logs.lock().expect("logs").clone()).expect("utf8 logs");
            assert_eq!(text.lines().count(), 1, "one operator decision: {text}");
            assert!(text.contains(expected_reason), "{text}");
            assert!(text.contains("target=Operator"), "{text}");
            assert!(text.contains("supervision_start=0"), "{text}");
            assert!(!text.contains("supervisor_lookup_failed"), "{text}");
        }
    }

    // Draw failure modes and sequences of reconciles/restarts. Check fallback after
    // every step and require one-pass recovery and exactly one delivery after healing.
    #[hegel::test]
    fn generated_unavailable_governor_recovers(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let unavailable =
            GovernorUnavailable::ALL[tc.draw(gs::integers::<usize>().min_value(0).max_value(GovernorUnavailable::ALL.len() - 1))];
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let restarts = (0..steps).map(|_| tc.draw(gs::booleans())).collect::<Vec<_>>();
        let record = [FallbackRecord::Current, FallbackRecord::LegacyExhausted, FallbackRecord::LegacyExhaustedCursor]
            [tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(unavailable_governor_scenario(unavailable, &restarts, record));
    }

    #[tokio::test]
    async fn stalled_work_routes_to_live_governor_after_abandoned_generation() {
        let (backend, wake, delivery) =
            project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned), ("governor-two", 2, ConvoyPhase::Active)]).await;
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        let objects = HashMap::from([
            ("stalled-work".into(), source),
            ("governor-two".into(), backend.using::<Convoy>("flotilla").get("governor-two").await.expect("live governor")),
        ]);
        wake.judge_stalls("flotilla", &objects).await.expect("judge stalls");
        let stalled = backend
            .using::<Convoy>("flotilla")
            .get("stalled-work")
            .await
            .expect("source")
            .status
            .expect("status")
            .stalled
            .expect("stalled");
        assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-two");
        assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-two");
    }

    // Legacy stalls consumed the governor rung while no supervisor was assigned.
    // They must retry that rung after a roll, even when the cursor is present.
    #[tokio::test]
    async fn exhausted_governor_cursor_retries_in_one_pass() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        flotilla_resources::apply_status_patch(
            &convoys,
            "stalled-work",
            &external_patches::mark_crew_stalled(
                "stalled-work".into(),
                "work".into(),
                "coder".into(),
                Utc::now(),
                flotilla_resources::StallReason::Infra,
                None,
                "needs decision".into(),
            ),
        )
        .await
        .expect("declare stall");
        let source = convoys.get("stalled-work").await.expect("source");
        let mut status = source.status.expect("status");
        let stalled = status.stalled.as_mut().expect("stall");
        stalled.supervision_exhausted = true;
        stalled.supervision_index = Some(1);
        convoys.update_status("stalled-work", &source.metadata.resource_version, &status).await.expect("legacy cursor");
        let source = convoys.get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("one pass");
        let source = convoys.get("stalled-work").await.expect("source");
        assert_eq!(source.status.expect("status").stalled.expect("stall").rung, StallRung::Governor);
        assert_eq!(delivery.requests.lock().expect("deliveries").len(), 1);
    }

    // A governor homed on another root must receive the escalation in the same
    // pass that assigns its rung; replica visibility alone is not delivery.
    #[tokio::test]
    async fn replicated_governor_receives_stall_in_one_pass() {
        let (remote, _, _) =
            project_supervision_case(&[("governor", 1, ConvoyPhase::Active), ("newer-unowned-governor", 2, ConvoyPhase::Active)]).await;
        create_governor_ensure(&remote, "governor").await;
        let (backend, wake, delivery) = project_supervision_case(&[]).await;
        let mut list = remote.using::<Convoy>("flotilla").list().await.expect("remote convoys");
        list.items.retain(|object| object.metadata.name != "stalled-work");
        backend.replica_writer::<Convoy>(NodeId::new("host-b"), "flotilla").replace(&list, Utc::now()).await.expect("replicate governor");
        backend
            .replica_writer::<ConvoyEnsure>(NodeId::new("host-b"), "flotilla")
            .replace(&remote.using::<ConvoyEnsure>("flotilla").list().await.expect("remote ensure"), Utc::now())
            .await
            .expect("replicate governor ownership");
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("one pass");
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        assert_eq!(source.status.expect("status").stalled.expect("stall").supervisor.expect("supervisor").convoy, "governor");
        let requests = delivery.requests.lock().expect("deliveries");
        assert_eq!(requests.len(), 1, "remote governor must receive its escalation");
        assert_eq!(requests[0].convoy, "governor");
    }

    // ProjectCrew requires project scope; a missing ref must be diagnostic,
    // never a match with an unrelated projectless governor.
    #[tokio::test]
    async fn stalled_work_without_project_records_lookup_failure() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor", 1, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        for name in ["stalled-work", "governor"] {
            let source = convoys.get(name).await.expect("convoy");
            let mut spec = source.spec;
            spec.project_ref = None;
            convoys.update(&InputMeta::from(&source.metadata), &source.metadata.resource_version, &spec).await.expect("clear project");
        }
        let source = convoys.get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge stalls");
        let stall = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stall");
        assert_eq!(stall.rung, StallRung::Operator);
        assert!(stall.evidence.contains("convoy has no project_ref"), "{}", stall.evidence);
        assert!(!stall.supervision_exhausted);
        assert!(delivery.requests.lock().expect("deliveries").is_empty());
    }

    // Supervision commands must preserve identifiers containing shell metacharacters.
    // Formatting glue: spaces and an apostrophe exercise the shared shell quoting helper.
    #[tokio::test]
    async fn stall_supervision_commands_quote_role_and_vessel_identifiers() {
        let (backend, wake, _) = project_supervision_case(&[]).await;
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge");
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
        let leaf = condition.leaves.first_mut().expect("actor leaf");
        let LeafAddress::Work { work, .. } = &mut leaf.address else { panic!("work leaf") };
        *work = "work space".to_string();
        leaf.field_path = ".crew.coder's role.phase".to_string();
        let brief = stall_supervision_brief(&source, &condition);
        for action in ["resume", "convert-to-failed", "escalate"] {
            assert!(
                brief.contains(&format!(r"--convoy 'stalled-work' --vessel 'work space' --role 'coder'\''s role' {action}")),
                "{brief}"
            );
        }
    }

    // #2592: a non-actor stall retains convoy identity and evidence, without
    // fabricating role/vessel supervision commands for an unknown actor.
    #[tokio::test]
    async fn unattributed_stall_brief_preserves_identity_without_inventing_commands() {
        let (backend, wake, _) = project_supervision_case(&[]).await;
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge");
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        let mut condition = source.status.as_ref().expect("status").stalled.clone().expect("stall");
        condition.leaves.clear();
        let brief = stall_supervision_brief(&source, &condition);
        assert!(brief.contains("unidentified crew in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{brief}");
        assert!(brief.contains("Reason: inferred stall. Evidence: needs decision"), "{brief}");
        assert!(!brief.contains("flotilla crew supervise"), "{brief}");
    }

    // #2654: permanent failures park once, transient failures retain a retry
    // deadline; neither repeated ticks nor a reconstructed subscription retries early.
    #[test]
    fn delivery_failures_are_durable_and_log_once() {
        for kind in 0..5 {
            // Pending convoy, missing observation, permanent hold failure, and timed retry.
            let permanent = kind == 2;
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            let subscriber = captured_subscriber(logs.clone(), tracing::Level::WARN);
            tracing::subscriber::with_default(subscriber, || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(async {
                    let (backend, _, _) = project_supervision_case(&[]).await;
                    let limit = if kind == 2 || kind == 3 { 0 } else { 3 };
                    let mut wake = supervision_wake_with_limit(&backend, limit);
                    let convoys = backend.using::<Convoy>("flotilla");
                    let convoy = convoys.get("stalled-work").await.expect("tracing scenario");
                    let mut status = convoy.status.expect("tracing scenario");
                    status.phase = ConvoyPhase::Landing;
                    if kind == 0 || kind == 3 {
                        status.phase = ConvoyPhase::Pending;
                    }
                    status.crew_work.get_mut("work").expect("tracing scenario").get_mut("coder").expect("tracing scenario").finished_at =
                        Some(Utc::now() - chrono::Duration::seconds(2));
                    let target_crew = status.crew_work["work"]["coder"].clone();
                    if kind == 4 {
                        status.crew_work.get_mut("work").expect("delivery retry fixture").remove("coder");
                    }
                    convoys.update_status("stalled-work", &convoy.metadata.resource_version, &status).await.expect("tracing scenario");
                    let rule = TurnDeliveryRule::builder()
                        .on(if kind == 2 || kind == 3 { "$issue.state == closed" } else { "$cr.checks == fail" }
                            .parse()
                            .expect("tracing scenario"))
                        .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
                        .brief("continue".into())
                        .hold(HoldAct::ChangeRequestComment { body: "paused".into() })
                        .build();
                    let mut leaf = Leaf {
                        address: LeafAddress::ChangeRequest { service: "github.com".into(), scope: "team/repo".into(), number: 1 },
                        field_path: ".checks".into(),
                        operator: LeafOperator::Equal,
                        literal: "fail".into(),
                    };
                    if kind == 2 || kind == 3 {
                        let records = backend.using::<Issue>("flotilla");
                        let name = flotilla_resources::issue_record_name("github.com", "team/repo", 1);
                        let record = records
                            .create(
                                &InputMeta::builder().name(name.clone()).build(),
                                &flotilla_resources::IssueSpec::builder()
                                    .service("github.com".into())
                                    .scope("team/repo".into())
                                    .number(1)
                                    .observing_authority("test-host".into())
                                    .build(),
                            )
                            .await
                            .expect("tracing scenario");
                        let now = Utc::now();
                        records
                            .update_status(&name, &record.metadata.resource_version, &flotilla_resources::IssueStatus {
                                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, now),
                                updated_at: flotilla_resources::Observation::known(now, now),
                                title: Default::default(),
                                assignees: Default::default(),
                                labels: Default::default(),
                            })
                            .await
                            .expect("tracing scenario");
                        leaf.address = LeafAddress::Issue { service: "github.com".into(), scope: "team/repo".into(), number: 1 };
                        leaf.field_path = ".state".into();
                        leaf.literal = "closed".into();
                    }
                    for tick in 0..10 {
                        if tick == 5 {
                            wake = supervision_wake_with_limit(&backend, limit);
                        }
                        let id = uuid::Uuid::new_v4();
                        wake.subscriptions.inner.rows.lock().await.insert(id, LeafSubscriptionRow {
                            id,
                            namespace: "flotilla".into(),
                            leaves: vec![leaf.clone()],
                            watcher: LeafWatcher::TurnDelivery {
                                convoy: "stalled-work".into(),
                                source: "checks".into(),
                                rule: Box::new(rule.clone()),
                            },
                            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
                            freshness_demand: None,
                            created_at: Utc::now(),
                            episode_key: Default::default(),
                        });
                        wake.subscriptions
                            .fire(id, LeafFire {
                                subscription_id: id,
                                watcher_id: uuid::Uuid::nil(),
                                leaf: leaf.clone(),
                                value: "fail".into(),
                            })
                            .await;
                        let status = convoys.get("stalled-work").await.expect("tracing scenario").status.expect("tracing scenario");
                        let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                        assert_eq!(failure.kind == flotilla_resources::TurnDeliveryFailureKind::Permanent, permanent);
                        if kind == 2 {
                            assert!(failure.reason.contains("no bound change request"));
                        }
                        assert_eq!(failure.attempts, 1);
                        assert!(failure.retry_at > Utc::now());
                    }
                    if kind == 3 {
                        // No observation or convoy event after the watch starts:
                        // its own deadline must re-evaluate this level-triggered leaf.
                        let current = convoys.get("stalled-work").await.expect("convoy");
                        let mut status = current.status.expect("status");
                        status
                            .turn_deliveries
                            .get_mut("checks")
                            .expect("delivery retry fixture")
                            .failure
                            .as_mut()
                            .expect("delivery retry fixture")
                            .retry_at = Utc::now() + chrono::Duration::milliseconds(30);
                        convoys.update_status("stalled-work", &current.metadata.resource_version, &status).await.expect("deadline");
                        let row = wake.subscriptions.rows().await.into_iter().next().expect("row");
                        let _ = tokio::time::timeout(Duration::from_millis(150), wake.subscriptions.watch_row_once(row)).await;
                        let status =
                            convoys.get("stalled-work").await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                        assert_eq!(status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture").attempts, 2);
                        return;
                    }
                    // Advance the durable deadline without a wall-clock sleep.
                    let current = convoys.get("stalled-work").await.expect("tracing scenario");
                    let mut status = current.status.expect("tracing scenario");
                    status
                        .turn_deliveries
                        .get_mut("checks")
                        .expect("tracing scenario")
                        .failure
                        .as_mut()
                        .expect("tracing scenario")
                        .retry_at = Utc::now() - chrono::Duration::seconds(1);
                    convoys.update_status("stalled-work", &current.metadata.resource_version, &status).await.expect("tracing scenario");
                    let row = wake
                        .subscriptions
                        .rows()
                        .await
                        .into_iter()
                        .find(|row| matches!(&row.watcher, LeafWatcher::TurnDelivery { source, .. } if source == "checks"))
                        .expect("tracing scenario");
                    wake.subscriptions
                        .fire(row.id, LeafFire {
                            subscription_id: row.id,
                            watcher_id: uuid::Uuid::nil(),
                            leaf: leaf.clone(),
                            value: "fail".into(),
                        })
                        .await;
                    let status = convoys.get("stalled-work").await.expect("tracing scenario").status.expect("tracing scenario");
                    let failure = status.turn_deliveries["checks"].failure.as_ref().expect("tracing scenario");
                    assert_eq!(failure.attempts, if permanent { 1 } else { 2 });
                    if !permanent {
                        assert_eq!(failure.retry_at.signed_duration_since(failure.failed_at), chrono::Duration::seconds(10));
                    }
                    if kind == 0 || kind == 4 {
                        let current = convoys.get("stalled-work").await.expect("delivery retry fixture");
                        let mut status = current.status.expect("delivery retry fixture");
                        status.phase = ConvoyPhase::Landing;
                        status.crew_work.get_mut("work").expect("delivery retry fixture").insert("coder".into(), target_crew);
                        status
                            .turn_deliveries
                            .get_mut("checks")
                            .expect("delivery retry fixture")
                            .failure
                            .as_mut()
                            .expect("delivery retry fixture")
                            .retry_at = Utc::now();
                        convoys
                            .update_status("stalled-work", &current.metadata.resource_version, &status)
                            .await
                            .expect("delivery retry fixture");
                        wake.subscriptions
                            .fire(row.id, LeafFire { subscription_id: row.id, watcher_id: uuid::Uuid::nil(), leaf, value: "fail".into() })
                            .await;
                        let status =
                            convoys.get("stalled-work").await.expect("delivery retry fixture").status.expect("delivery retry fixture");
                        let failure = status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture");
                        assert_eq!(failure.attempts, 3);
                        assert!(
                            !failure.reason.contains("phase") && !failure.reason.contains("crew is absent"),
                            "recovery must get past the old failure"
                        );
                    }
                });
            });
            let text = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
            assert_eq!(text.matches("turn delivery failed").count(), if kind == 0 || kind == 4 { 2 } else { 1 }, "{text}");
            assert!(text.contains("source=checks"));
        }
    }

    // #2654: unchanged operator fallback emits once across ticks, including a restart.
    #[test]
    fn unchanged_governorless_stall_logs_once() {
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = captured_subscriber(logs.clone(), tracing::Level::WARN);
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(async {
                let (backend, mut wake, _) = project_supervision_case(&[]).await;
                for tick in 0..10 {
                    if tick == 5 {
                        wake = supervision_wake(&backend);
                    }
                    let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("tracing scenario");
                    let objects = HashMap::from([("stalled-work".into(), source)]);
                    wake.sync_rows("flotilla", &objects).await.expect("tracing scenario");
                    wake.judge_stalls("flotilla", &objects).await.expect("tracing scenario");
                }
            });
        });
        let text = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
        assert_eq!(text.matches("stall escalation fell back to operator").count(), 1, "{text}");
    }

    #[tokio::test]
    async fn stalled_work_without_live_governor_names_reason_at_operator_rung() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor-one", 1, ConvoyPhase::Abandoned)]).await;
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source)])).await.expect("judge stalls");
        let stalled = backend
            .using::<Convoy>("flotilla")
            .get("stalled-work")
            .await
            .expect("source")
            .status
            .expect("status")
            .stalled
            .expect("stalled");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert!(!stalled.supervision_exhausted);
        // #2592: the operator backstop uses the same actionable description as a supervisor turn.
        let source = backend.using::<Convoy>("flotilla").get("stalled-work").await.expect("source");
        let brief = stall_supervision_brief(&source, &stalled);
        assert!(brief.contains("coder@work in convoy graphql-budget@wheelhouse (resource ref: stalled-work)"), "{brief}");
        assert!(brief.contains("--convoy 'stalled-work' --vessel 'work' --role 'coder' resume"), "{brief}");
        assert!(stalled.evidence.contains("no live governor for project wheelhouse"), "{}", stalled.evidence);
        assert!(delivery.requests.lock().expect("deliveries").is_empty());
    }

    #[tokio::test]
    async fn stalled_work_prefers_governor_owned_by_ensure_over_higher_live_generation() {
        let (backend, wake, delivery) =
            project_supervision_case(&[("governor-two", 2, ConvoyPhase::Active), ("governor-three", 3, ConvoyPhase::Active)]).await;
        create_governor_ensure(&backend, "governor-two").await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys.get("stalled-work").await.expect("source");
        let second = convoys.get("governor-two").await.expect("owned governor");
        let third = convoys.get("governor-three").await.expect("higher generation");
        wake.judge_stalls(
            "flotilla",
            &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), second), ("governor-three".into(), third)]),
        )
        .await
        .expect("judge stalls");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
        assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-two");
        assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-two");
    }

    #[tokio::test]
    async fn stalled_work_names_missing_governor_crew_at_operator_rung() {
        let (backend, wake, delivery) = project_supervision_case(&[("governor-two", 2, ConvoyPhase::Active)]).await;
        let convoys = backend.using::<Convoy>("flotilla");
        let governor = convoys.get("governor-two").await.expect("governor");
        let mut status = governor.status.expect("governor status");
        status.crew_work.clear();
        convoys.update_status("governor-two", &governor.metadata.resource_version, &status).await.expect("clear governor crew");
        let source = convoys.get("stalled-work").await.expect("source");
        let governor = convoys.get("governor-two").await.expect("governor");
        wake.judge_stalls("flotilla", &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), governor)]))
            .await
            .expect("judge stalls");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
        assert_eq!(stalled.rung, StallRung::Operator);
        assert!(stalled.evidence.contains("no live governor crew for project wheelhouse"), "{}", stalled.evidence);
        assert!(delivery.requests.lock().expect("deliveries").is_empty());
    }

    #[tokio::test]
    async fn stalled_work_ignores_terminal_ensure_attempt_and_uses_highest_live_generation() {
        let (backend, wake, delivery) = project_supervision_case(&[
            ("governor-one", 1, ConvoyPhase::Abandoned),
            ("governor-two", 2, ConvoyPhase::Active),
            ("governor-three", 3, ConvoyPhase::Active),
        ])
        .await;
        create_governor_ensure(&backend, "governor-one").await;
        let convoys = backend.using::<Convoy>("flotilla");
        let source = convoys.get("stalled-work").await.expect("source");
        let second = convoys.get("governor-two").await.expect("live governor");
        let third = convoys.get("governor-three").await.expect("highest live governor");
        wake.judge_stalls(
            "flotilla",
            &HashMap::from([("stalled-work".into(), source), ("governor-two".into(), second), ("governor-three".into(), third)]),
        )
        .await
        .expect("judge stalls");
        let stalled = convoys.get("stalled-work").await.expect("source").status.expect("status").stalled.expect("stalled");
        assert_eq!(stalled.supervisor.expect("supervisor").convoy, "governor-three");
        assert_eq!(delivery.requests.lock().expect("deliveries")[0].convoy, "governor-three");
    }

    #[tokio::test]
    async fn lost_crew_session_stalls_with_dead_evidence_but_stale_busy_screen_does_not() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "test-host".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let wake = ReconcilerWake {
            subscriptions: LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher),
            _marker: PhantomData,
        };
        create_convoy(&backend, "delivery", ConvoyStatus {
            phase: ConvoyPhase::Active,
            crew_work: BTreeMap::from([(
                "work".into(),
                BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
            )]),
            ..Default::default()
        })
        .await;
        let terminal = backend.using::<TerminalSession>("flotilla");
        let created = terminal
            .create(
                &InputMeta::builder()
                    .name("terminal-delivery-work-coder".into())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.into(), "delivery".into()),
                        (VESSEL_LABEL.into(), "work".into()),
                        (ROLE_LABEL.into(), "coder".into()),
                    ]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec {
                    env_ref: "env".into(),
                    role: "coder".into(),
                    source: TerminalSessionSource::Tool { command: "codex".into() },
                    cwd: "/workspace".into(),
                    pool: "cleat".into(),
                },
            )
            .await
            .expect("create terminal");
        let mut status = flotilla_resources::TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            attention: Some(TerminalAttention {
                state: TerminalAttentionState::Working,
                source: TerminalAttentionSource::Screen,
                as_of: Utc::now() - TerminalAttention::FRESH_FOR - chrono::Duration::seconds(1),
            }),
            ..Default::default()
        };
        let running = terminal.update_status(&created.metadata.name, &created.metadata.resource_version, &status).await.expect("running");
        let leaf = Leaf {
            address: LeafAddress::Work { convoy: "delivery".into(), work: "work".into() },
            field_path: ".crew.coder.phase".into(),
            operator: LeafOperator::Equal,
            literal: "Done".into(),
        };
        let id = uuid::Uuid::new_v4();
        wake.subscriptions.inner.rows.lock().await.insert(id, LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: "delivery".into() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        });
        let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
        let objects = HashMap::from([("delivery".into(), convoy)]);
        wake.judge_stalls("flotilla", &objects).await.expect("judge stale screen");
        assert!(backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled.is_none());

        flotilla_resources::TerminalSessionStatusPatch::MarkLost {
            reason: "daemon generation is dead; session is recreatable".into(),
            lost_at: Utc::now(),
        }
        .apply(&mut status);
        terminal.update_status(&created.metadata.name, &running.metadata.resource_version, &status).await.expect("lost");
        wake.judge_stalls("flotilla", &objects).await.expect("judge dead session");
        let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
        let stalled = stalled.expect("dead actor must stall");
        assert!(stalled.evidence.contains("session dead: daemon generation is dead"));
        assert_eq!(stalled.source, StallEvidenceSource::Session);
    }

    #[tokio::test]
    async fn credential_delivery_and_clone_controller_rows_judge_transient_terminal_and_exhausted_failures() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let wake = ReconcilerWake {
            subscriptions: LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher),
            _marker: PhantomData,
        };
        create_convoy(&backend, "delivery", ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() }).await;
        let vessels = backend.using::<Vessel>("flotilla");
        let vessel = vessels
            .create(
                &InputMeta::builder()
                    .name("delivery-work".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "delivery".to_string())]))
                    .build(),
                &flotilla_resources::VesselSpec {
                    convoy_ref: "delivery".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "test".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        vessels
            .update_status("delivery-work", &vessel.metadata.resource_version, &flotilla_resources::VesselStatus {
                environment_ref: Some("delivery-env".into()),
                ..Default::default()
            })
            .await
            .expect("place vessel");
        let now = Utc::now();
        let backoff = flotilla_resources::RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) };
        let scenarios = [
            (ControllerRetry::retryable(None, now, backoff), None),
            (ControllerRetry::terminal(None, now, "credential spec does not decode"), Some("credential spec does not decode")),
            (
                ControllerRetry {
                    attempts: RetryCeiling::default().attempts,
                    first_failure_at: now,
                    disposition: flotilla_resources::ControllerRetryDisposition::Retryable { next_attempt_at: now },
                },
                Some("retrying without progress"),
            ),
        ];
        let mut standing_row_id = None;
        for (retry, expected) in scenarios {
            flotilla_resources::apply_status_patch(&vessels, "delivery-work", &flotilla_resources::VesselStatusPatch::CredentialDelivery {
                retry: Some(retry),
            })
            .await
            .expect("record delivery retry");
            let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
            let objects = HashMap::from([("delivery".to_string(), convoy)]);
            wake.sync_rows("flotilla", &objects).await.expect("arm controller row");
            let controller_row = wake
                .subscriptions
                .rows()
                .await
                .into_iter()
                .find(|row| matches!(&row.maker, LeafMaker::Controller { resource_kind, .. } if resource_kind == "CredentialDelivery"))
                .expect("credential delivery row");
            if let Some(id) = standing_row_id {
                assert_eq!(controller_row.id, id, "retry changes should update the standing row without reopening its watches");
            }
            standing_row_id = Some(controller_row.id);
            wake.judge_stalls("flotilla", &objects).await.expect("judge controller row");
            let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
            assert_eq!(stalled.as_ref().map(|stalled| stalled.evidence.as_str()), expected);
        }
        flotilla_resources::apply_status_patch(&vessels, "delivery-work", &flotilla_resources::VesselStatusPatch::CredentialDelivery {
            retry: None,
        })
        .await
        .expect("clear credential retry");
        let checkouts = backend.using::<Checkout>("flotilla");
        checkouts
            .create(
                &InputMeta::builder()
                    .name("delivery-checkout".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "delivery".to_string())]))
                    .build(),
                &CheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
                    repo_ref: RepositoryKey("repo".into()),
                    env_ref: "delivery-env".into(),
                    r#ref: "main".into(),
                    base_ref: None,
                    target_path: "/tmp/checkout".into(),
                    clone_ref: "delivery-clone".into(),
                }),
            )
            .await
            .expect("worktree checkout");
        for (retry, expected) in [
            (ControllerRetry::retryable(None, now, backoff), None),
            (ControllerRetry::terminal(None, now, "repository not found"), Some("repository not found")),
            (
                ControllerRetry {
                    attempts: RetryCeiling::default().attempts,
                    first_failure_at: now,
                    disposition: flotilla_resources::ControllerRetryDisposition::Retryable { next_attempt_at: now },
                },
                Some("retrying without progress"),
            ),
        ] {
            flotilla_resources::apply_status_patch(
                &checkouts,
                "delivery-checkout",
                &flotilla_resources::CheckoutStatusPatch::ObserveCloneRetry { retry: Some(retry) },
            )
            .await
            .expect("project clone retry");
            let convoy = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy");
            let objects = HashMap::from([("delivery".to_string(), convoy)]);
            wake.sync_rows("flotilla", &objects).await.expect("arm clone controller row");
            wake.judge_stalls("flotilla", &objects).await.expect("judge clone controller row");
            let stalled = backend.using::<Convoy>("flotilla").get("delivery").await.expect("convoy").status.expect("status").stalled;
            assert_eq!(stalled.as_ref().map(|stalled| stalled.evidence.as_str()), expected);
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

    #[tokio::test]
    async fn in_memory_leaf_subscription_contract() {
        assert_leaf_subscription_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_leaf_subscription_contract() {
        assert_leaf_subscription_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("open sqlite"))).await;
    }

    #[tokio::test]
    async fn usage_leaf_fires_from_the_named_window_in_the_replicated_resource_path() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let provider = "codex";
        let account = "user@example.com";
        let records = backend.using::<Usage>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(flotilla_resources::usage_record_name(provider, account)).build(),
                &flotilla_resources::UsageSpec { provider: provider.to_string(), account: account.to_string() },
            )
            .await
            .expect("create usage record");
        records
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &flotilla_resources::UsageStatus::builder()
                    .windows(vec![
                        flotilla_resources::UsageWindow::builder().name("session").used_percent(8.0).build(),
                        flotilla_resources::UsageWindow::builder().name("weekly").used_percent(100.0).build(),
                    ])
                    .observed_at(Utc::now())
                    .build(),
            )
            .await
            .expect("publish usage status");

        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend, broadcast_test_sink(event_tx.clone()), refresher);
        let mut events = event_tx.subscribe();
        let mut usage_leaf =
            leaf(LeafAddress::Usage { provider: provider.to_string(), account: account.to_string() }, ".windows.weekly.used-percent", "90");
        usage_leaf.operator = LeafOperator::GreaterThan;
        let subscription_id = table
            .subscribe_wait(uuid::Uuid::new_v4(), WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![usage_leaf],
                freshness_demand: None,
            })
            .await
            .expect("subscribe usage leaf");

        assert_eq!(receive_fire(&mut events, subscription_id).await.value, "100");
    }

    #[tokio::test]
    async fn reconciler_wake_rederives_at_boot_and_lands_without_resync() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(3600),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_secs(60),
        };
        let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let repo_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("workflow".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repo_ref.clone())
                .source_ref("feature/reconciler-wake".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(BoundChangeRequest::builder().id("1364".to_string()).repository_ref(repo_ref).title("wake".to_string()).build())
            .build();
        let mut meta = InputMeta::builder().name("wake".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
        meta.set_lifecycle_authority(LifecycleAuthority::Managed);
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let created = convoys.create(&meta, &spec).await.expect("create Landing convoy before watcher boot");
        let status = ConvoyStatus {
            phase: ConvoyPhase::Landing,
            workflow_snapshot: Some(WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: Some(ExitDeclaration::standard_table()),
                turn_delivery: Default::default(),
                vessels: Vec::new(),
            }),
            observed_workflow_ref: Some("workflow".to_string()),
            work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
            ..Default::default()
        };
        convoys.update_status("wake", &created.metadata.resource_version, &status).await.expect("mark Landing");

        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                secondaries: vec![table.reconciler_wake_watch()],
                reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
                    .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
                resync_interval: Duration::from_secs(3600),
                backend: backend.clone(),
            }
            .run(),
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if table.rows().await.iter().any(|row| matches!(row.watcher, LeafWatcher::ReconcilerWake { .. })) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("boot must rederive a ReconcilerWake row");
        assert!(
            table.rows().await.iter().any(|row| row.freshness_demand.is_some()),
            "Landing settlement must demand fresh targeted observations"
        );
        assert_eq!(convoys.get("wake").await.expect("open convoy").status.expect("status").phase, ConvoyPhase::Landing);
        let change_requests = backend.using::<ChangeRequest>("flotilla");
        let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1364);
        let change_request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(record) = change_requests.get(&record_name).await {
                    break record;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("home authority must publish the demanded change request record");
        assert_eq!(change_request.spec.observing_authority, "authority");

        source.merged.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if convoys.get("wake").await.expect("convoy").status.expect("status").phase == ConvoyPhase::Landed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("leaf fire must enqueue reconcile without waiting for hourly resync");
        controller.abort();
    }

    // #2499: Landing keeps its exit gates while the observed maker waits at a
    // known forge deadline. Only an expired deadline without recovery can stall.
    #[tokio::test]
    async fn landing_observation_cooldown_waits_until_deadline_without_stalling() {
        struct LimitedSource(ObservationError);
        #[async_trait]
        impl crate::change_request_observer::ChangeRequestObservationSource for LimitedSource {
            async fn observe(&self, _: &ChangeRequestRef) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
                Err(self.0.clone())
            }
        }
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let now = Utc::now();
        let retry_at = now + chrono::Duration::seconds(60);
        let error = ObservationError::RateLimited {
            budget: "GraphQL".into(),
            limit: GithubRateLimit {
                kind: GithubRateLimitKind::Secondary,
                retry_source: GithubRetrySource::RetryAfter,
                retry_at: Some(retry_at),
            },
        };
        let refresher = ChangeRequestRefresher::new(
            "fleet".into(),
            backend.clone(),
            "authority".into(),
            Arc::new(LimitedSource(error)),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "team/one".into(), number: 1 };
        refresher.refresh_once(&subject).await.expect_err("limited observation records its diagnostic");
        let (event_tx, _) = broadcast::channel(4);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let convoys = backend.using::<Convoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("cooldown".into()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".into()).repositories(Vec::new()).build(),
            )
            .await
            .expect("convoy");
        convoys
            .update_status("cooldown", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                ..Default::default()
            })
            .await
            .expect("landing");
        let id = uuid::Uuid::new_v4();
        table.inner.rows.lock().await.insert(id, LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec!["cr/github.com/team/one/1 .state == merged".parse().expect("exit leaf")],
            watcher: LeafWatcher::ReconcilerWake { convoy: "cooldown".into() },
            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
            freshness_demand: Some(now),
            created_at: now,
            episode_key: EpisodeKeyFields::default(),
        });
        let wake = ReconcilerWake { subscriptions: table, _marker: PhantomData };
        for at in [now, retry_at - chrono::Duration::nanoseconds(1), retry_at] {
            let convoy = convoys.get("cooldown").await.expect("convoy");
            wake.judge_stalls_at("flotilla", &HashMap::from([("cooldown".into(), convoy)]), at).await.expect("judge");
            let status = convoys.get("cooldown").await.expect("convoy").status.expect("status");
            assert_eq!(status.phase, ConvoyPhase::Landing, "missing merge evidence must never land");
            assert_eq!(status.stalled.is_some(), at >= retry_at, "only the expired unrecovered wait can stall");
        }
    }

    #[tokio::test]
    async fn diagnostics_retain_the_last_firing_for_an_armed_reconciler_row() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(4);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "authority".to_string(),
            Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) }),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend, broadcast_test_sink(event_tx), refresher);
        let id = uuid::Uuid::new_v4();
        let fired_leaf = leaf(LeafAddress::Convoy { name: "held".to_string() }, ".status.phase", "Landed");
        table.inner.rows.lock().await.insert(id, LeafSubscriptionRow {
            id,
            namespace: "flotilla".to_string(),
            leaves: vec![fired_leaf.clone()],
            watcher: LeafWatcher::ReconcilerWake { convoy: "held".to_string() },
            maker: LeafMaker::Observed { refresher: "test".into(), external_party: "test".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        });

        table
            .fire(id, LeafFire {
                subscription_id: uuid::Uuid::nil(),
                watcher_id: uuid::Uuid::nil(),
                leaf: fired_leaf,
                value: "Landed".into(),
            })
            .await;

        let diagnostics = table.diagnostics().await;
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].1.len(), 1);
        assert_eq!(diagnostics[0].1[0].value, "Landed");
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
                &ConvoySpec::builder()
                    .workflow_ref(if role == "reviewer" { "implement-review".into() } else { "single-agent".into() })
                    .build(),
            )
            .await
            .expect("convoy");
        let base = Utc::now() - chrono::Duration::seconds(2);
        let mut status = ConvoyStatus {
            phase: ConvoyPhase::Active,
            started_at: Some(base),
            workflow_snapshot: Some(WorkflowSnapshot {
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
        for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Stalled, CrewWorkPhase::Done, CrewWorkPhase::HandedBack, CrewWorkPhase::Failed]
        {
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
                    table.deliver_turn(row.id, "checks-wake", source, rule, &row.leaves[0]).await.expect("duplicate observation");
                }
                let status = convoys.get("checks-wake").await.expect("convoy").status.expect("status");
                let settled = checks.is_some_and(|value| value != ObservedChecks::Pending);
                assert_eq!(status.turn_deliveries.get(source).map_or(0, |state| state.episodes.len()), index + usize::from(settled));
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
        assert_eq!(actuator.holds.load(Ordering::SeqCst), outcomes.len().saturating_sub(3));
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
            let conflict =
                table.rows().await.into_iter().find(|row| is_conflict_probe(&row.leaves[0])).expect("active conflict subscription");
            let LeafWatcher::TurnDelivery { source, rule, .. } = &conflict.watcher else { unreachable!() };
            table.deliver_turn(conflict.id, "checks-wake", source, rule, &conflict.leaves[0]).await.expect("active conflict turn");
            assert!(actuator.requests.lock().expect("requests").last().expect("conflict request").brief.starts_with(rule.brief.trim()));
        }
        for row in table.rows().await {
            table.finish(row.id).await;
        }
    }

    // #2654: each subscription key logs its ineligible-to-eligible transition;
    // duplicate ticks with different crew phases but the same decision stay quiet.
    #[test]
    fn unchanged_subscription_decisions_log_once() {
        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = captured_subscriber(logs.clone(), tracing::Level::DEBUG);
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tracing scenario").block_on(active_checks_scenario(
                &[true],
                false,
                "coder",
            ));
        });
        let output = String::from_utf8(logs.lock().expect("tracing scenario").clone()).expect("tracing scenario");
        let decisions = output
            .lines()
            .filter(|line| line.contains("turn delivery subscription decision") && line.contains("source=checks-settled"))
            .collect::<Vec<_>>();
        assert_eq!(decisions.len(), 2, "{output}");
        assert!(decisions[0].contains("skip_ineligible_active_crew_or_subject"));
        assert!(decisions[1].contains("arm_eligible_active_crew_and_subject"));
    }

    #[tokio::test]
    async fn active_produced_pr_checks_settle_to_pass_or_fail_once_per_head() {
        for role in ["coder", "reviewer"] {
            for pass in [false, true] {
                active_checks_scenario(&[pass, !pass, pass, !pass], true, role).await;
            }
        }
    }

    #[hegel::test]
    fn generated_active_checks_settlement_deduplicates_heads(tc: hegel::TestCase) {
        // Generate pass/fail sequences across the delivery ceiling (3), including
        // coder/reviewer roles, new heads, unknown and pending observations, and duplicate settlement.
        let role = if tc.draw(hegel::generators::booleans()) { "reviewer" } else { "coder" };
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(5));
        let outcomes = (0..count).map(|_| tc.draw(hegel::generators::booleans())).collect::<Vec<_>>();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(active_checks_scenario(&outcomes, false, role));
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
            .hold(HoldAct::ChangeRequestComment { body: "Automatic delivery paused.".to_string() })
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
            .update_status("wake-turn", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
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
            })
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
            address: LeafAddress::ChangeRequest {
                service: "github.com".to_string(),
                scope: "flotilla-org/flotilla".to_string(),
                number: 1392,
            },
            field_path: field_path.to_string(),
            operator: LeafOperator::Equal,
            literal: literal.to_string(),
        };
        let subscription_id = uuid::Uuid::new_v4();
        table.inner.rows.lock().await.insert(subscription_id, LeafSubscriptionRow {
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
        });

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
        assert_eq!(episodes[0].sender, flotilla_resources::CrewMessageSender::FlotillaTurn { source: source.to_string() });
        // #2684: accepting a terminal FIFO entry is not confirmation of an agent turn.
        assert_eq!(serde_json::to_value(&episodes[0].outcome).unwrap()["kind"], "queued");
        assert!(matches!(episodes[1].outcome, TurnDeliveryOutcome::Queued { rung: TurnDeliveryRung::FreshAgent, .. }));
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

    #[tokio::test]
    async fn actionable_review_turn_delivery_enforces_head_identity_records_rungs_and_escalates() {
        assert_turn_delivery_enforces_head_identity_records_rungs_and_escalates(
            "review",
            "$cr.review.actionable-at-head == true",
            ".review.actionable-at-head",
            "true",
            true,
            flotilla_resources::ObservedMergeability::Mergeable,
            "Address the actionable review and push the fix.",
        )
        .await;
    }

    #[tokio::test]
    async fn conflicting_mergeability_delivers_once_per_episode_and_escalates_to_hold() {
        assert_turn_delivery_enforces_head_identity_records_rungs_and_escalates(
            "conflicting",
            "$cr.mergeable == conflicting",
            ".mergeable",
            "conflicting",
            false,
            flotilla_resources::ObservedMergeability::Conflicting,
            "Rebase onto the current base branch, resolve additively, run pinned CI, push the same branch, process review, and file a fresh settlement claim; the previous claim is superseded.",
        )
        .await;
    }

    #[tokio::test]
    async fn declared_exit_entry_name_is_recorded_when_its_instantiated_leaf_fires() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_millis(20),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_secs(60),
        };
        let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let repo_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("workflow".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repo_ref.clone())
                .source_ref("feature/custom-disposition".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .change_request(
                BoundChangeRequest::builder().id("1391".to_string()).repository_ref(repo_ref).title("custom".to_string()).build(),
            )
            .build();
        let mut meta =
            InputMeta::builder().name("custom".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
        meta.set_lifecycle_authority(LifecycleAuthority::Managed);
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let created = convoys.create(&meta, &spec).await.expect("create custom-exit convoy");
        let status = ConvoyStatus {
            phase: ConvoyPhase::Landing,
            observed_workflow_ref: Some("workflow".to_string()),
            workflow_snapshot: Some(WorkflowSnapshot {
                stall_nudges: Default::default(),
                supervision: None,
                exit: Some(ExitDeclaration::Table(indexmap::IndexMap::from([(
                    "shipped".to_string(),
                    "$cr.state == merged".parse().expect("custom leaf template"),
                )]))),
                turn_delivery: Default::default(),
                vessels: Vec::new(),
            }),
            work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
            ..Default::default()
        };
        convoys.update_status("custom", &created.metadata.resource_version, &status).await.expect("mark custom convoy Landing");

        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                secondaries: vec![table.reconciler_wake_watch()],
                reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
                    .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
                resync_interval: Duration::from_secs(3600),
                backend: backend.clone(),
            }
            .run(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !table.rows().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("custom exit should instantiate at binding");

        source.merged.store(true, Ordering::SeqCst);
        let settled = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let convoy = convoys.get("custom").await.expect("convoy");
                let status = convoy.status.expect("status");
                if status.phase == ConvoyPhase::Landed {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("custom exit leaf should settle through the engine");
        assert_eq!(settled.disposition.as_deref(), Some("shipped"));
        controller.abort();
    }

    #[tokio::test]
    async fn zero_subject_landing_settles_as_claim_exit_through_engine() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence::default();
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableChangeRequests),
            cadence,
        );
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("no-cr".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".to_string()).build(),
            )
            .await
            .expect("create zero-subject convoy");
        convoys
            .update_status("no-cr", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                ..Default::default()
            })
            .await
            .expect("mark zero-subject convoy Landing");
        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                secondaries: vec![table.reconciler_wake_watch()],
                reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla")),
                resync_interval: Duration::from_secs(3600),
                backend,
            }
            .run(),
        );

        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = convoys.get("no-cr").await.expect("convoy").status.expect("status");
                if status.phase == ConvoyPhase::Landed {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("zero-subject convoy should claim-exit");
        assert_eq!(status.disposition.as_deref(), Some("claim"));
        assert!(table.rows().await.is_empty(), "claim exit should not arm leaves");
        controller.abort();
    }

    #[tokio::test]
    async fn change_request_bound_after_claims_instantiates_and_settles() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let source = Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) });
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_millis(20),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_secs(60),
        };
        let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let repo_ref = RepositoryKey("repo".to_string());
        let mut spec = ConvoySpec::builder()
            .workflow_ref("workflow".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repo_ref.clone())
                .source_ref("feature/adopt-late".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .build();
        let meta = InputMeta::builder().name("adopt-late".to_string()).build();
        let convoys = backend.clone().using::<Convoy>("flotilla");
        let created = convoys.create(&meta, &spec).await.expect("create unbound convoy");
        let landing = convoys
            .update_status("adopt-late", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                ..Default::default()
            })
            .await
            .expect("claims enter Landing before binding");
        spec.change_request =
            Some(BoundChangeRequest::builder().id("1391".to_string()).repository_ref(repo_ref).title("adopted late".to_string()).build());
        convoys.update(&meta, &landing.metadata.resource_version, &spec).await.expect("bind CR after claims");

        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                secondaries: vec![table.reconciler_wake_watch()],
                reconciler: ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla"))
                    .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
                resync_interval: Duration::from_secs(3600),
                backend: backend.clone(),
            }
            .run(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !table.rows().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late binding should instantiate exit leaves");
        source.merged.store(true, Ordering::SeqCst);
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = convoys.get("adopt-late").await.expect("convoy").status.expect("status");
                if status.phase == ConvoyPhase::Landed {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late-bound CR should settle");
        assert_eq!(status.disposition.as_deref(), Some("merged"));
        controller.abort();
    }

    #[tokio::test]
    async fn authority_reconciler_wakes_from_replicated_checkout_and_change_request() {
        let authority = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote = ResourceBackend::InMemory(InMemoryBackend::default());
        let repo_ref = RepositoryKey("repo".to_string());
        let spec = ConvoySpec::builder()
            .workflow_ref("workflow".to_string())
            .r#ref("feature/reconciler-wake".to_string())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".to_string())
                .repo_ref(repo_ref.clone())
                .source_ref("feature/reconciler-wake".to_string())
                .target_ref("main".to_string())
                .workspace_slug("flotilla".to_string())
                .subpaths(Vec::new())
                .build()])
            .build();
        let mut meta =
            InputMeta::builder().name("cross-host".to_string()).finalizers(vec!["flotilla.work/convoy-teardown".to_string()]).build();
        meta.set_lifecycle_authority(LifecycleAuthority::Managed);
        let convoys = authority.clone().using::<Convoy>("flotilla");
        let created = convoys.create(&meta, &spec).await.expect("create authority convoy");
        let work = WorkState::builder()
            .phase(WorkPhase::Complete)
            .placement(PlacementStatus {
                fields: BTreeMap::from([(
                    "checkout_refs".to_string(),
                    serde_json::json!(BTreeMap::from([(repo_ref.clone(), "remote-checkout".to_string())])),
                )]),
            })
            .build();
        convoys
            .update_status("cross-host", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                workflow_snapshot: Some(WorkflowSnapshot {
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: Default::default(),
                    vessels: Vec::new(),
                }),
                observed_workflow_ref: Some("workflow".to_string()),
                work: BTreeMap::from([("work".to_string(), work)]),
                ..Default::default()
            })
            .await
            .expect("mark authority convoy Landing");

        let remote_checkouts = remote.clone().using::<Checkout>("flotilla");
        let checkout = remote_checkouts
            .create(
                &InputMeta::builder()
                    .name("remote-checkout".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "cross-host".to_string())]))
                    .build(),
                &CheckoutSpec::Observed(ObservedCheckoutSpec {
                    r#ref: "feature/reconciler-wake".to_string(),
                    path: "/remote/checkout".to_string(),
                    repo_ref,
                    host_ref: "remote".to_string(),
                    is_main: false,
                }),
            )
            .await
            .expect("create remote checkout");
        remote_checkouts
            .update_status("remote-checkout", &checkout.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                integration: CheckoutIntegrationStatus {
                    landed: IntegrationCondition::builder().value(ConditionValue::False).build(),
                    change_request: Some(
                        ChangeRequestObservation::builder()
                            .id("1364".to_string())
                            .state(ChangeRequestState::Open)
                            .mergeability(flotilla_resources::ChangeRequestMergeability::Mergeable)
                            .observed_at(Utc::now().to_rfc3339())
                            .build(),
                    ),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("record remote checkout CR evidence");
        authority
            .replica_writer::<Checkout>(flotilla_protocol::NodeId::new("remote-root"), "flotilla")
            .replace(&remote_checkouts.list().await.expect("list remote checkout"), Utc::now())
            .await
            .expect("replicate checkout evidence");

        let remote_records = remote.using::<ChangeRequest>("flotilla");
        let record_name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1364);
        let record = remote_records
            .create(
                &InputMeta::builder().name(record_name).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope("flotilla-org/flotilla".to_string())
                    .number(1364)
                    .observing_authority("remote-root".to_string())
                    .build(),
            )
            .await
            .expect("create remote CR record");
        let observed_at = Utc::now();
        remote_records
            .update_status(&record.metadata.name, &record.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            })
            .await
            .expect("publish remote merge");
        authority
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("remote-root"), "flotilla")
            .replace(&remote_records.list().await.expect("list remote CR"), Utc::now())
            .await
            .expect("replicate CR evidence");

        let (event_tx, _) = broadcast::channel(16);
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence::default();
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            authority.clone(),
            "authority-root".to_string(),
            Arc::new(UnavailableChangeRequests),
            cadence,
        );
        let table = LeafSubscriptionTable::new(authority.clone(), broadcast_test_sink(event_tx), refresher);
        let controller = tokio::spawn(
            ControllerLoop {
                primary: convoys.clone(),
                secondaries: vec![table.reconciler_wake_watch()],
                reconciler: ConvoyReconciler::new(authority.definitions::<WorkflowTemplate>("flotilla"))
                    .with_federated_checkouts(authority.including_replicas::<Checkout>("flotilla"))
                    .with_change_requests(authority.including_replicas::<ChangeRequest>("flotilla"), cadence.stale_after),
                resync_interval: Duration::from_secs(3600),
                backend: authority,
            }
            .run(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if convoys.get("cross-host").await.expect("convoy").status.expect("status").phase == ConvoyPhase::Landed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("authority engine must land from replicated checkout and CR evidence");
        controller.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn replayed_real_merge_observation_unblocks_wait_and_releases_demand() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let fixture = crate::providers::testing::fixture_path("change_request", "cr_observation_merge.yaml");
        let session = crate::providers::replay::Session::replaying(fixture, crate::providers::replay::Masks::new());
        let source = Arc::new(crate::change_request_observer::GhChangeRequestObservationSource::new(
            crate::providers::replay::test_runner(&session),
        ));
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(60),
            checks_pending: Duration::from_secs(5),
            freshness_demanded: Duration::from_secs(2),
            stale_after: Duration::from_secs(120),
        };
        let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx.clone()), refresher.clone());
        let connection_id = uuid::Uuid::new_v4();
        let mut events = event_tx.subscribe();
        let request = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1363 },
                ".state",
                "merged",
            )],
            freshness_demand: None,
        };
        let subscription_id = table.subscribe_wait(connection_id, request).await.expect("subscribe CR wait");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        let fire = receive_fire(&mut events, subscription_id).await;
        assert_eq!(fire.value, "merged");
        assert_eq!(refresher.active_demands().await, 0, "fired wait must stop polling");
        session.finish();
    }

    #[tokio::test(start_paused = true)]
    async fn unsubscribe_stops_change_request_observation_and_freshness_tightens_cadence() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) });
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(60),
            checks_pending: Duration::from_secs(20),
            freshness_demanded: Duration::from_secs(5),
            stale_after: Duration::from_secs(120),
        };
        let refresher = ChangeRequestRefresher::new("fleet".to_string(), backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher.clone());
        let connection_id = uuid::Uuid::new_v4();
        let request = WaitSubscriptionRequest {
            namespace: "flotilla".to_string(),
            leaves: vec![leaf(
                LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1364 },
                ".state",
                "merged",
            )],
            freshness_demand: Some(Utc::now()),
        };
        table.subscribe_wait(connection_id, request).await.expect("subscribe CR wait");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            backend.using::<ChangeRequest>("flotilla").list().await.expect("list demanded CR").items.len(),
            1,
            "subscription must materialize its individually bound subject"
        );
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must use tighter cadence");

        table.unsubscribe_connection(connection_id).await;
        assert_eq!(refresher.active_demands().await, 0);
        assert!(
            backend.using::<ChangeRequest>("flotilla").list().await.expect("list released CRs").items.is_empty(),
            "last unsubscribe must garbage collect the observed record"
        );
        let stopped_at = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), stopped_at, "no subscribers means no polling");
    }

    #[tokio::test]
    async fn replica_reading_evaluator_fires_from_authority_change_request_record() {
        let authority = ResourceBackend::InMemory(InMemoryBackend::default());
        let subject =
            LeafAddress::ChangeRequest { service: "github.com".to_string(), scope: "flotilla-org/flotilla".to_string(), number: 1365 };
        let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 1365);
        let records = authority.using::<ChangeRequest>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(name).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope("flotilla-org/flotilla".to_string())
                    .number(1365)
                    .observing_authority("feta".to_string())
                    .build(),
            )
            .await
            .expect("create authority CR");
        let observed_at = Utc::now();
        records
            .update_status(&created.metadata.name, &created.metadata.resource_version, &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            })
            .await
            .expect("publish authority CR");

        let reader = ResourceBackend::InMemory(InMemoryBackend::default());
        let authority_list = records.list().await.expect("list authority CR");
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("feta-root"), "flotilla")
            .replace(&authority_list, Utc::now())
            .await
            .expect("replicate authority CR");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            reader.clone(),
            "kiwi".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let (event_tx, _) = broadcast::channel(16);
        let table = LeafSubscriptionTable::new(reader, broadcast_test_sink(event_tx.clone()), refresher);
        let mut events = event_tx.subscribe();
        let subscription_id = table
            .subscribe_wait(uuid::Uuid::new_v4(), WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(subject, ".state", "merged")],
                freshness_demand: None,
            })
            .await
            .expect("subscribe replica CR");
        assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "replica reader must not become a second observing authority");
    }

    #[tokio::test(start_paused = true)]
    async fn foreign_change_request_authority_does_not_fetch_or_write_status() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let records = backend.using::<ChangeRequest>("flotilla");
        let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 2049);
        records
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".to_string())
                    .scope("flotilla-org/flotilla".to_string())
                    .number(2049)
                    .observing_authority("other-host".to_string())
                    .build(),
            )
            .await
            .expect("create foreign-owned record");
        let calls = Arc::new(AtomicUsize::new(0));
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_secs(1),
            checks_pending: Duration::from_secs(1),
            freshness_demanded: Duration::from_secs(1),
            stale_after: Duration::from_secs(60),
        };
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "local-host".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            cadence,
        );
        let subject = crate::change_request_observer::ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 2049,
        };
        let id = uuid::Uuid::new_v4();
        refresher.demand(id, subject, None).await.expect("demand foreign-owned record");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "non-owner must not fetch from forge");
        assert!(records.get(&name).await.expect("record").status.is_none(), "non-owner must not write status");
        refresher.release(id).await;
    }

    #[tokio::test]
    async fn two_hosts_demand_one_change_request_and_only_owner_fetches() {
        let owner = ResourceBackend::InMemory(InMemoryBackend::default());
        let reader = ResourceBackend::InMemory(InMemoryBackend::default());
        let subject = crate::change_request_observer::ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 2051,
        };
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_millis(20),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_millis(80),
        };
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let owner_refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            owner.clone(),
            "owner".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&owner_calls) }),
            cadence,
        );
        let owner_id = uuid::Uuid::new_v4();
        owner_refresher.demand(owner_id, subject.clone(), None).await.expect("owner demand");
        tokio::time::timeout(Duration::from_secs(2), async {
            while owner_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner fetches");
        let records = owner.using::<ChangeRequest>("flotilla");
        tokio::time::timeout(Duration::from_secs(2), async {
            while records.get(&subject.record_name()).await.expect("owner record").status.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner publishes");
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
            .replace(&records.list().await.expect("owner records"), Utc::now())
            .await
            .expect("replicate owner observation");
        let reader_calls = Arc::new(AtomicUsize::new(0));
        let reader_refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            reader.clone(),
            "reader".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&reader_calls) }),
            cadence,
        );
        let reader_id = uuid::Uuid::new_v4();
        reader_refresher.demand(reader_id, subject.clone(), None).await.expect("reader demand");
        let initial_observed_at =
            records.get(&subject.record_name()).await.expect("owner record").status.expect("owner status").state.observed_at;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(30)).await;
            reader
                .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
                .replace(&records.list().await.expect("owner records"), Utc::now())
                .await
                .expect("replicate owner heartbeat");
        }
        assert_eq!(reader_calls.load(Ordering::SeqCst), 0, "reader must use replicated observation");
        assert!(
            records.get(&subject.record_name()).await.expect("owner record").status.expect("renewed status").state.observed_at
                > initial_observed_at,
            "healthy owner must renew unchanged observations before they become stale"
        );
        assert!(reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.is_err());
        assert!(reader
            .including_replicas::<ChangeRequest>("flotilla")
            .get(&subject.record_name())
            .await
            .expect("replicated record")
            .object
            .status
            .is_some());
        reader_refresher.release(reader_id).await;
        owner_refresher.release(owner_id).await;
    }

    #[tokio::test]
    async fn former_owner_evaluates_fresher_takeover_replica() {
        let former_owner = ResourceBackend::InMemory(InMemoryBackend::default());
        let new_owner = ResourceBackend::InMemory(InMemoryBackend::default());
        let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", 2052);
        let spec = |authority: &str| {
            flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(2052)
                .observing_authority(authority.to_string())
                .build()
        };
        let status = |state, observed_at| flotilla_resources::ChangeRequestStatus {
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
        };
        let old_records = former_owner.using::<ChangeRequest>("flotilla");
        let old = old_records.create(&InputMeta::builder().name(name.clone()).build(), &spec("former")).await.expect("old record");
        old_records
            .update_status(
                &name,
                &old.metadata.resource_version,
                &status(flotilla_resources::ObservedChangeRequestState::Open, Utc::now() - chrono::Duration::seconds(10)),
            )
            .await
            .expect("old observation");
        let new_records = new_owner.using::<ChangeRequest>("flotilla");
        let new = new_records.create(&InputMeta::builder().name(name.clone()).build(), &spec("new")).await.expect("new record");
        new_records
            .update_status(
                &name,
                &new.metadata.resource_version,
                &status(flotilla_resources::ObservedChangeRequestState::Merged, Utc::now()),
            )
            .await
            .expect("new observation");
        former_owner
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("new-root"), "flotilla")
            .replace(&new_records.list().await.expect("new records"), Utc::now())
            .await
            .expect("replicate takeover");
        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            former_owner.clone(),
            "former".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let keeper_id = uuid::Uuid::new_v4();
        refresher
            .demand(
                keeper_id,
                crate::change_request_observer::ChangeRequestRef {
                    namespace: "flotilla".to_string(),
                    service: "github.com".to_string(),
                    scope: "flotilla-org/flotilla".to_string(),
                    number: 2052,
                },
                None,
            )
            .await
            .expect("keep local observation while waits finish");
        let table = LeafSubscriptionTable::new(former_owner.clone(), broadcast_test_sink(event_tx.clone()), refresher.clone());
        let mut events = event_tx.subscribe();
        let subscription_id = table
            .subscribe_wait(uuid::Uuid::new_v4(), WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(
                    LeafAddress::ChangeRequest {
                        service: "github.com".to_string(),
                        scope: "flotilla-org/flotilla".to_string(),
                        number: 2052,
                    },
                    ".state",
                    "merged",
                )],
                freshness_demand: None,
            })
            .await
            .expect("wait on takeover observation");
        assert_eq!(receive_fire(&mut events, subscription_id).await.value, "merged");

        let fallback_id = table
            .subscribe_wait(uuid::Uuid::new_v4(), WaitSubscriptionRequest {
                namespace: "flotilla".to_string(),
                leaves: vec![leaf(
                    LeafAddress::ChangeRequest {
                        service: "github.com".to_string(),
                        scope: "flotilla-org/flotilla".to_string(),
                        number: 2052,
                    },
                    ".state",
                    "open",
                )],
                freshness_demand: None,
            })
            .await
            .expect("wait for local fallback");
        assert!(tokio::time::timeout(Duration::from_millis(100), events.recv()).await.is_err(), "stale local state must not fire");
        former_owner
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("new-root"), "flotilla")
            .replace(
                &flotilla_resources::ResourceList {
                    items: vec![],
                    resource_version: "0".to_string(),
                    generation: Some("next".to_string()),
                },
                Utc::now(),
            )
            .await
            .expect("remove takeover replica");
        assert_eq!(receive_fire(&mut events, fallback_id).await.value, "open");
        refresher.release(keeper_id).await;
    }

    #[tokio::test]
    async fn former_owner_can_reclaim_after_takeover_owner_goes_stale() {
        let former = ResourceBackend::InMemory(InMemoryBackend::default());
        let takeover = ResourceBackend::InMemory(InMemoryBackend::default());
        let subject = crate::change_request_observer::ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 2053,
        };
        let status = |observed_at| flotilla_resources::ChangeRequestStatus {
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
        };
        for (backend, authority, age) in [(&former, "former", 10), (&takeover, "takeover", 6)] {
            let records = backend.using::<ChangeRequest>("flotilla");
            let created = records
                .create(
                    &InputMeta::builder().name(subject.record_name()).build(),
                    &flotilla_resources::ChangeRequestSpec::builder()
                        .service(subject.service.clone())
                        .scope(subject.scope.clone())
                        .number(subject.number)
                        .observing_authority(authority.to_string())
                        .build(),
                )
                .await
                .expect("create authority record");
            records
                .update_status(
                    &subject.record_name(),
                    &created.metadata.resource_version,
                    &status(Utc::now() - chrono::Duration::seconds(age)),
                )
                .await
                .expect("publish old observation");
        }
        former
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("takeover-root"), "flotilla")
            .replace(&takeover.using::<ChangeRequest>("flotilla").list().await.expect("takeover records"), Utc::now())
            .await
            .expect("replicate stale takeover");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            former.clone(),
            "former".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            crate::change_request_observer::ChangeRequestRefreshCadence {
                state: Duration::from_millis(20),
                checks_pending: Duration::from_millis(20),
                freshness_demanded: Duration::from_millis(20),
                stale_after: Duration::from_secs(2),
            },
        );
        let id = uuid::Uuid::new_v4();
        refresher.demand(id, subject.clone(), None).await.expect("former owner keeps demand");
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("former owner reclaims stale takeover");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let reclaimed =
                    former.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.expect("reclaimed local record");
                assert_eq!(reclaimed.spec.observing_authority, "former");
                if reclaimed.status.is_some_and(|status| status.state.observed_at > Utc::now() - chrono::Duration::seconds(2)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("former owner publishes a fresh status after reclaim");
        refresher.release(id).await;
    }

    #[tokio::test]
    async fn stale_replicated_change_request_is_claimed_by_demanding_host() {
        let owner = ResourceBackend::InMemory(InMemoryBackend::default());
        let reader = ResourceBackend::InMemory(InMemoryBackend::default());
        let subject = crate::change_request_observer::ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 2050,
        };
        let records = owner.using::<ChangeRequest>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name(subject.record_name()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service(subject.service.clone())
                    .scope(subject.scope.clone())
                    .number(subject.number)
                    .observing_authority("owner".to_string())
                    .build(),
            )
            .await
            .expect("create owner record");
        let old = Utc::now() - chrono::Duration::seconds(3);
        let old_status = flotilla_resources::ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Open, old),
            head_sha: flotilla_resources::Observation::known("old".to_string(), old),
            checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pending, old),
            review: flotilla_resources::ChangeRequestReviewObservation {
                actionable_at_head: flotilla_resources::Observation::known(false, old),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, old),
        };
        records.update_status(&created.metadata.name, &created.metadata.resource_version, &old_status).await.expect("publish stale status");
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
            .replace(&records.list().await.expect("owner records"), Utc::now())
            .await
            .expect("replicate owner record");
        let calls = Arc::new(AtomicUsize::new(0));
        let cadence = crate::change_request_observer::ChangeRequestRefreshCadence {
            state: Duration::from_millis(20),
            checks_pending: Duration::from_millis(20),
            freshness_demanded: Duration::from_millis(20),
            stale_after: Duration::from_secs(2),
        };
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            reader.clone(),
            "reader".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            cadence,
        );
        let id = uuid::Uuid::new_v4();
        refresher.demand(id, subject.clone(), None).await.expect("demand stale replica");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "one stale threshold must not move a healthy owner's record");
        assert!(reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.is_err());
        let current = records.get(&created.metadata.name).await.expect("owner record");
        let mut very_old_status = old_status;
        let very_old = Utc::now() - chrono::Duration::seconds(10);
        very_old_status.state.observed_at = very_old;
        records
            .update_status(&created.metadata.name, &current.metadata.resource_version, &very_old_status)
            .await
            .expect("publish very stale status");
        reader
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("owner-root"), "flotilla")
            .replace(&records.list().await.expect("owner records"), Utc::now())
            .await
            .expect("replicate very stale record");
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reader should take over and refresh");
        let claimed = reader.using::<ChangeRequest>("flotilla").get(&subject.record_name()).await.expect("local claim");
        assert_eq!(claimed.spec.observing_authority, "reader");
        assert!(claimed.status.is_some());
        owner
            .replica_writer::<ChangeRequest>(flotilla_protocol::NodeId::new("reader-root"), "flotilla")
            .replace(&reader.using::<ChangeRequest>("flotilla").list().await.expect("reader records"), Utc::now())
            .await
            .expect("replicate claim to old owner");
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let old_owner = ChangeRequestRefresher::new(
            "fleet".to_string(),
            owner.clone(),
            "owner".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&owner_calls) }),
            cadence,
        );
        let owner_id = uuid::Uuid::new_v4();
        old_owner.demand(owner_id, subject, None).await.expect("old owner still demands record");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(owner_calls.load(Ordering::SeqCst), 0, "old owner must yield to the fresh claim");
        old_owner.release(owner_id).await;
        refresher.release(id).await;
    }
    #[tokio::test]
    async fn observed_issue_change_fires_wait_leaf() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, mut events) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "authority".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let connection = uuid::Uuid::new_v4();
        let leaf: Leaf = "issue/github.com/flotilla-org/flotilla/2052 .state == closed".parse().expect("issue leaf");
        table
            .subscribe_wait(connection, WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec![leaf.clone()],
                freshness_demand: None,
            })
            .await
            .expect("subscribe issue");
        let name = flotilla_resources::issue_record_name("github.com", "flotilla-org/flotilla", 2052);
        let records = backend.using::<Issue>("flotilla");
        let created = records.get(&name).await.expect("demand creates record");
        let now = Utc::now();
        let status = flotilla_resources::IssueStatus {
            title: Default::default(),
            assignees: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Open, now),
            labels: flotilla_resources::Observation::known(vec!["ready".into()], now),
            updated_at: flotilla_resources::Observation::known(now, now),
        };
        let opened = records.update_status(&name, &created.metadata.resource_version, &status).await.expect("open issue");
        let closed = flotilla_resources::IssueStatus {
            title: Default::default(),
            assignees: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, Utc::now()),
            ..status
        };
        records.update_status(&name, &opened.metadata.resource_version, &closed).await.expect("close issue");
        let fired = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("issue leaf fired").expect("event");
        assert!(matches!(fired, DaemonEvent::LeafFired(fire) if fire.leaf == leaf && fire.value == "closed"));
        table.unsubscribe_connection(connection).await;
    }

    #[tokio::test]
    async fn issue_leaf_uses_issue_refresher_staleness() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let records = backend.using::<Issue>("flotilla");
        let name = flotilla_resources::issue_record_name("github.com", "flotilla-org/flotilla", 2052);
        let created = records
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &flotilla_resources::IssueSpec::builder()
                    .service("github.com".into())
                    .scope("flotilla-org/flotilla".into())
                    .number(2052)
                    .observing_authority("issue-owner".into())
                    .build(),
            )
            .await
            .expect("issue");
        let old = Utc::now() - chrono::Duration::seconds(5);
        let stale = flotilla_resources::IssueStatus {
            title: Default::default(),
            assignees: Default::default(),
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, old),
            labels: flotilla_resources::Observation::known(vec![], old),
            updated_at: flotilla_resources::Observation::known(old, old),
        };
        let created = records.update_status(&name, &created.metadata.resource_version, &stale).await.expect("stale issue");
        let (event_tx, mut events) = broadcast::channel(16);
        let change_requests = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "cr-owner".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let issues = IssueRefresher::new(backend.clone(), "issue-owner".into(), Arc::new(UnavailableIssues), IssueRefreshCadence {
            state: Duration::from_secs(90),
            freshness_demanded: Duration::from_secs(10),
            stale_after: Duration::from_secs(1),
        });
        let table = LeafSubscriptionTable::with_issues(backend.clone(), broadcast_test_sink(event_tx), change_requests, issues);
        let connection = uuid::Uuid::new_v4();
        table
            .subscribe_wait(connection, WaitSubscriptionRequest {
                namespace: "flotilla".into(),
                leaves: vec!["issue/github.com/flotilla-org/flotilla/2052 .state == closed".parse().expect("leaf")],
                freshness_demand: None,
            })
            .await
            .expect("subscribe");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv()).await.is_err(),
            "stale issue must not fire under issue cadence"
        );
        let now = Utc::now();
        records
            .update_status(&name, &created.metadata.resource_version, &flotilla_resources::IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, now),
                labels: flotilla_resources::Observation::known(vec![], now),
                updated_at: flotilla_resources::Observation::known(now, now),
            })
            .await
            .expect("fresh issue");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv()).await.expect("fire").expect("event"),
            DaemonEvent::LeafFired(_)
        ));
        table.unsubscribe_connection(connection).await;
    }

    #[tokio::test]
    async fn issue_turn_delivery_fires_once_for_changed_issue() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(4);
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "authority".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), broadcast_test_sink(event_tx), refresher);
        let actuator = Arc::new(RecordingTurnDelivery::default());
        table.set_turn_delivery_actuator(actuator.clone()).await;
        let rule = TurnDeliveryRule::builder()
            .on("$issue.state == closed".parse().expect("issue rule"))
            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".into()).role("coder".into()).build())
            .brief("Respond to the issue change.".into())
            .hold(HoldAct::ChangeRequestComment { body: "Paused".into() })
            .build();
        let reference = flotilla_protocol::IssueRef {
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "2052".into(),
        };
        let spec = ConvoySpec::builder()
            .workflow_ref("workflow".into())
            .repositories(vec![ConvoyRepositorySpec::builder()
                .url("https://github.com/flotilla-org/flotilla".into())
                .repo_ref(RepositoryKey("repo".into()))
                .source_ref("feature/issue".into())
                .target_ref("main".into())
                .workspace_slug("flotilla".into())
                .subpaths(Vec::new())
                .build()])
            .issues(vec![flotilla_resources::ConvoyIssue {
                reference: reference.clone(),
                repository_ref: None,
                snapshot: flotilla_resources::IssueSnapshot {
                    title: "Issue".into(),
                    body: None,
                    state: flotilla_protocol::IssueState::Open,
                    labels: vec![],
                    as_of: Utc::now(),
                },
            }])
            .build();
        let convoys = backend.using::<Convoy>("flotilla");
        let created = convoys.create(&InputMeta::builder().name("issue-turn".into()).build(), &spec).await.expect("convoy");
        let claim_at = Utc::now() - chrono::Duration::seconds(2);
        convoys
            .update_status("issue-turn", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landing,
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([(
                        "coder".into(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .finished_at(claim_at)
                            .decision_ledger_ref("https://example.com/ledger".into())
                            .build(),
                    )]),
                )]),
                ..Default::default()
            })
            .await
            .expect("claim");
        let leaf = flotilla_resources::issue_address(&reference)
            .map(|address| Leaf { address, field_path: ".state".into(), operator: LeafOperator::Equal, literal: "closed".into() })
            .expect("issue address");
        let subscription_id = uuid::Uuid::new_v4();
        table.inner.rows.lock().await.insert(subscription_id, LeafSubscriptionRow {
            id: subscription_id,
            namespace: "flotilla".into(),
            leaves: vec![leaf.clone()],
            watcher: LeafWatcher::TurnDelivery { convoy: "issue-turn".into(), source: "issue".into(), rule: Box::new(rule.clone()) },
            maker: LeafMaker::Observed { refresher: "issue".into(), external_party: "forge".into() },
            freshness_demand: Some(claim_at),
            created_at: claim_at,
            episode_key: EpisodeKeyFields::default(),
        });
        let records = backend.using::<Issue>("flotilla");
        let name = flotilla_resources::issue_record_name("github.com", "flotilla-org/flotilla", 2052);
        let issue = records
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &flotilla_resources::IssueSpec::builder()
                    .service("github.com".into())
                    .scope("flotilla-org/flotilla".into())
                    .number(2052)
                    .observing_authority("authority".into())
                    .build(),
            )
            .await
            .expect("issue");
        let watched_row = table.inner.rows.lock().await[&subscription_id].clone();
        let watching_table = table.clone();
        let watch = tokio::spawn(async move { watching_table.watch_row(watched_row).await });
        let changed_at = Utc::now();
        records
            .update_status(&name, &issue.metadata.resource_version, &flotilla_resources::IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, changed_at),
                labels: flotilla_resources::Observation::known(vec!["done".into()], changed_at),
                updated_at: flotilla_resources::Observation::known(changed_at, changed_at),
            })
            .await
            .expect("observation");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if convoys
                    .get("issue-turn")
                    .await
                    .expect("convoy")
                    .status
                    .as_ref()
                    .is_some_and(|status| status.turn_deliveries.get("issue").is_some_and(|delivery| !delivery.episodes.is_empty()))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("observed issue change wakes turn delivery");
        table.deliver_turn(subscription_id, "issue-turn", "issue", &rule, &leaf).await.expect("duplicate issue event");
        assert_eq!(actuator.requests.lock().expect("requests").len(), 1);
        let brief = actuator.requests.lock().expect("requests")[0].brief.clone();
        assert!(brief.contains("Target crew: `work/coder`"));
        assert!(brief.contains("Decision ledger: https://example.com/ledger"));
        assert!(brief.contains("feature/issue"));
        let status = convoys.get("issue-turn").await.expect("convoy").status.expect("status");
        assert_eq!(status.turn_deliveries["issue"].episodes.len(), 1);
        watch.abort();
    }
}
