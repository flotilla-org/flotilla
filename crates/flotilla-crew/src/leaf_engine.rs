use flotilla_daemon_api::event_sink::EventSink;
use flotilla_resources::{EpisodeKeyFields, LeafFiringRecord, LeafSubscriptionRow, LeafWatcher};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_protocol::Leaf;
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{Convoy, HoldAct, TerminalAttentionSource, TerminalAttentionState, TurnDeliveryRung};
use flotilla_store::{controller::SecondaryWatch, ResourceBackend};
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};

use flotilla_observation::{
    change_request_observer::ChangeRequestRefresher,
    issue_observer::{IssueKey, IssueObservationSource, IssueRefreshCadence, IssueRefresher},
};
use turn_delivery::UnavailableTurnDeliveryActuator;
use wake::ReconcilerWake;

struct UnavailableIssues;

#[async_trait]
impl IssueObservationSource for UnavailableIssues {
    async fn observe(&self, _subject: &IssueKey) -> Result<flotilla_resources::IssueStatus, String> {
        Err("issue observation source unavailable".into())
    }
}

/// In-process workflow activation context. Only the resulting Message resource
/// crosses hosts; transport requests are not a wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct CrewTurnIntent {
    /// Who the turn goes to when it names them; otherwise the crew member `role` of `convoy`.
    pub receiver: Option<Party>,
    pub namespace: String,
    pub convoy: String,
    pub source: String,
    pub vessel: String,
    pub role: String,
    pub brief: String,
    pub subject_revision: String,
    pub subject: Option<flotilla_protocol::Subject>,
    pub sender: Party,
    #[builder(default = flotilla_resources::MessageRelation::System)]
    pub relation: flotilla_resources::MessageRelation,
    /// What the turn refers to; at most one is its subject.
    #[builder(default)]
    pub references: Vec<flotilla_messaging::IntentReference>,
    pub delivery_condition: Option<flotilla_address::Condition>,
    #[builder(default)]
    pub expectation: flotilla_messaging::IntentExpectation,
    /// The stall this supervision is about, with its evidence.
    pub stall_of: Option<flotilla_messaging::StallOf>,
}

/// One end of a crew message: whom it is from or to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Party {
    /// A person, by their principal's name: they send and receive as the role they hold in the
    /// project the message concerns.
    Person(String),
    /// A crew member: role `role` of convoy `convoy` in `project`.
    Seat { project: String, convoy: String, role: String },
    /// A fleet role, by name: read on the FleetDesignation where the message is sent.
    Fleet(String),
    /// An inbox already named.
    Inbox(flotilla_address::Address),
}

impl Party {
    /// A fleet role, by name.
    pub fn fleet(role: &str) -> Self {
        Self::Fleet(role.to_owned())
    }

    /// The party a supervision policy's address names: a person by `principal:<name>`, else the
    /// role inbox a crew role address names.
    pub fn of_policy_address(address: &str) -> Result<Self, String> {
        match address.strip_prefix("principal:") {
            Some(principal) => Ok(Self::Person(principal.to_owned())),
            None => flotilla_resources::convoy::engine::role_inbox(address).map(Self::Inbox),
        }
    }

    /// The inbox this party sends and receives at, read through `messages` in `namespace`; a
    /// person's is the role they hold in `project`, and a fleet role's is read on the
    /// FleetDesignation by its name.
    pub async fn inbox(
        &self,
        messages: &dyn flotilla_messaging::EngineMessages,
        namespace: &str,
        project: &str,
    ) -> Result<flotilla_address::Address, String> {
        match self {
            Self::Person(principal) => messages.person(namespace, principal, Some(project)).await.map_err(|error| error.to_string()),
            Self::Fleet(role) => messages.fleet_role(namespace, role).await.map_err(|error| error.to_string()),
            Self::Seat { project, convoy, role } => flotilla_address::Address::project(project)
                .map_err(|error| error.to_string())
                .and_then(|project| flotilla_resources::convoy::engine::seat_address(&project, convoy, role)),
            Self::Inbox(inbox) => Ok(inbox.clone()),
        }
    }

    /// The person who sends as this party, when one does: admission traces them.
    pub fn principal(&self) -> Option<String> {
        match self {
            Self::Person(principal) => Some(principal.clone()),
            _ => None,
        }
    }
}

/// Canonical receiver admission is a workflow latch, not a delivery receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewTurnAdmission {
    pub new_turn: bool,
    pub rung: TurnDeliveryRung,
    pub message: flotilla_protocol::ResourceRef,
}

/// Producers publish ordinary resource intent through the command router;
/// receiver homes own delivery and no transport request crosses this seam.
#[async_trait]
pub trait ResourceIntentPublisher: Send + Sync {
    async fn publish(self: Arc<Self>, namespace: &str, document: serde_json::Value) -> Result<flotilla_protocol::ResourceRef, String>;
    /// [`Self::publish`], with the home that admitted it when the receiver
    /// home reports one (the engine path does, #3587 layer 7).
    async fn publish_admitted(
        self: Arc<Self>,
        namespace: &str,
        document: serde_json::Value,
    ) -> Result<(flotilla_protocol::ResourceRef, Option<String>), String> {
        Ok((self.publish(namespace, document).await?, None))
    }
}

#[async_trait]
pub trait TurnDeliveryActuator: Send + Sync {
    async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String>;
    async fn hold(&self, request: &CrewTurnIntent, act: &HoldAct, reason: &str) -> Result<(), String>;
}

#[derive(Clone)]
pub struct LeafSubscriptionTable {
    inner: Arc<LeafSubscriptionTableInner>,
}

struct LeafSubscriptionTableInner {
    lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    clock: Arc<dyn flotilla_resources::Clock>,
    backend: ResourceBackend,
    watch_views: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<wake::LeafViews>>>>,
    /// The Message path, installed at startup.
    message_path: crate::ports::MessagePath,
    /// The `[primitives]` Convoy path: promises are read from the engine when it holds a convoy.
    convoy_engine: flotilla_resources::convoy::engine::EngineConvoyPath,
    charter_brief_renderer: Arc<dyn flotilla_messaging::CharterBriefRenderer>,
    event_sink: Arc<dyn EventSink>,
    rows: Mutex<HashMap<uuid::Uuid, LeafSubscriptionRow>>,
    last_firings: Mutex<HashMap<(uuid::Uuid, Leaf), LeafFiringRecord>>,
    observer_grace: Mutex<HashMap<(uuid::Uuid, ChangeRequestKey), DateTime<Utc>>>,
    unable_since: Mutex<HashMap<uuid::Uuid, (UnableEvidenceKey, DateTime<Utc>)>>,
    stale_attention_reported: Mutex<HashSet<uuid::Uuid>>,
    pending_launches: Mutex<stalls::PendingLaunches>,
    tasks: Mutex<HashMap<uuid::Uuid, JoinHandle<()>>>,
    routing: Mutex<routing::SubscriptionRouting>,
    change_requests: ChangeRequestRefresher,
    issues: IssueRefresher,
    reconciler_tx: broadcast::Sender<String>,
    controller_queues: Mutex<HashMap<String, flotilla_store::controller::WorkQueueSender>>,
    turn_delivery: Mutex<Arc<dyn TurnDeliveryActuator>>,
    episode_limit: u32,
    /// Turn-delivery failures awaiting their convoy's single status write (#3455).
    pending_failures: turn_delivery::PendingFailureMap,
    decisions: crate::decision_log::DecisionLog,
    supervisor_context: Mutex<HashMap<stalls::SupervisorContextKey, String>>,
    operator_told: Mutex<stalls::OperatorTold>,
    #[cfg(test)]
    snapshot_loads: AtomicUsize,
    #[cfg(test)]
    evaluations: AtomicUsize,
    #[cfg(test)]
    store_reads: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnableEvidenceKey {
    Attention { state: TerminalAttentionState, source: TerminalAttentionSource },
    Absent,
}

impl LeafSubscriptionTable {
    /// A restarted daemon's table: these subscriptions, read through the restarted daemon's
    /// `reads`. Before the table is shared.
    #[cfg(test)]
    pub(crate) fn restarted_with(mut self, reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>) -> Self {
        Arc::get_mut(&mut self.inner).expect("restart before sharing subscriptions").lifecycle_reads = reads;
        self
    }

    /// Use the daemon clock for stall freshness, idle grace and nudge backoff.
    pub fn with_clock(mut self, clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure clock before sharing subscriptions").clock = clock;
        self
    }

    pub fn with_charter_brief_renderer(mut self, renderer: Arc<dyn flotilla_messaging::CharterBriefRenderer>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure rendering before sharing subscriptions").charter_brief_renderer = renderer;
        self
    }

    pub fn with_convoy_engine(mut self, path: flotilla_resources::convoy::engine::EngineConvoyPath) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure the convoy path before sharing subscriptions").convoy_engine = path;
        self
    }

    pub fn with_message_path(mut self, path: crate::ports::MessagePath) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure the message path before sharing subscriptions").message_path = path;
        self
    }

    pub(crate) fn message_primitives(&self) -> Result<Arc<dyn flotilla_messaging::EngineMessages>, flotilla_resources::ResourceError> {
        crate::ports::installed_messages(&self.inner.message_path)
    }

    pub fn new(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    ) -> Self {
        Self::with_episode_limit(backend, event_sink, change_requests, lifecycle_reads, 3)
    }

    pub fn with_episode_limit(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
        episode_limit: u32,
    ) -> Self {
        let issues =
            IssueRefresher::new(backend.clone(), "unavailable".into(), Arc::new(UnavailableIssues), IssueRefreshCadence::default());
        Self::with_issues_and_episode_limit(backend, event_sink, change_requests, issues, lifecycle_reads, episode_limit)
    }

    pub fn with_issues(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
        lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    ) -> Self {
        Self::with_issues_and_episode_limit(backend, event_sink, change_requests, issues, lifecycle_reads, 3)
    }

    fn with_issues_and_episode_limit(
        backend: ResourceBackend,
        event_sink: Arc<dyn EventSink>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
        lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
        episode_limit: u32,
    ) -> Self {
        // The limit judges the latest run of retained episodes (at most limit+1).
        assert!(
            (episode_limit as usize) < flotilla_resources::TurnDeliveryStatus::EPISODE_HISTORY,
            "turn-delivery episode limit must stay below the retained episode history"
        );
        let (reconciler_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(LeafSubscriptionTableInner {
                lifecycle_reads,
                clock: Arc::new(flotilla_resources::SystemClock),
                backend,
                watch_views: Default::default(),
                message_path: Default::default(),
                convoy_engine: Default::default(),
                charter_brief_renderer: Arc::new(flotilla_messaging::CharterProseRenderer),
                event_sink,
                rows: Mutex::new(HashMap::new()),
                last_firings: Mutex::new(HashMap::new()),
                unable_since: Mutex::new(HashMap::new()),
                observer_grace: Mutex::new(HashMap::new()),
                stale_attention_reported: Mutex::new(HashSet::new()),
                pending_launches: Default::default(),
                tasks: Mutex::new(HashMap::new()),
                routing: Default::default(),
                change_requests,
                issues,
                reconciler_tx,
                controller_queues: Mutex::new(HashMap::new()),
                turn_delivery: Mutex::new(Arc::new(UnavailableTurnDeliveryActuator)),
                episode_limit,
                pending_failures: Default::default(),
                decisions: Default::default(),
                supervisor_context: Default::default(),
                operator_told: Default::default(),
                #[cfg(test)]
                snapshot_loads: AtomicUsize::new(0),
                #[cfg(test)]
                evaluations: AtomicUsize::new(0),
                #[cfg(test)]
                store_reads: AtomicUsize::new(0),
            }),
        }
    }

    /// Step locally authored convoys once. Supervisor and child context is read
    /// from the replica view during judgement; replica convoys are never actuated.
    pub async fn reconcile_stalls_once(&self, namespace: &str) -> Result<(), String> {
        let wake = ReconcilerWake::new(self.clone());
        let convoys = wake.local_convoys(namespace).await?;
        wake.sync_rows(namespace, &convoys).await?;
        wake.judge_stalls(namespace, &convoys).await
    }

    pub async fn set_turn_delivery_actuator(&self, actuator: Arc<dyn TurnDeliveryActuator>) {
        *self.inner.turn_delivery.lock().await = actuator;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
        Box::new(ReconcilerWake::new(self.clone()))
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.inner.change_requests.stale_after()
    }

    pub fn issue_stale_after(&self) -> Duration {
        self.inner.issues.stale_after()
    }

    pub async fn change_request_observation_error(&self, subject: &ChangeRequestKey) -> Option<ObservationError> {
        self.inner.change_requests.observation_error(subject).await
    }

    pub async fn watch_change_request_convoy_demands(&self, namespace: &str) {
        self.inner.change_requests.watch_convoy_demands(namespace).await;
    }

    pub async fn release_unlinked_change_request_claims(&self, namespace: &str) -> Result<(), String> {
        self.inner.change_requests.release_unlinked_history(namespace).await
    }

    pub async fn refresh_change_request_once(&self, subject: &ChangeRequestKey) -> Result<(), ObservationError> {
        self.inner.change_requests.refresh_once(subject).await
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_address::Address) -> Result<(), String> {
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
}

mod routing;
mod schedule;
mod sources;
mod stalls;
mod subscriptions;
mod supervision;
pub(crate) use supervision::EscalationAdmission;
mod turn_delivery;
mod wake;

pub use stalls::NO_EXIT_STALL_EVIDENCE;
pub(crate) use stalls::{crew_role_address, supervision_message_source};
pub(crate) use turn_delivery::turn_message_producer_key;

#[cfg(test)]
mod tests;
