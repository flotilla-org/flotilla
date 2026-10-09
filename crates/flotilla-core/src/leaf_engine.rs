#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::Leaf;
use flotilla_resources::{
    controller::SecondaryWatch, Convoy, HoldAct, LeafMaker, ResourceBackend, TerminalAttentionSource, TerminalAttentionState,
    TurnDeliveryRule, TurnDeliveryRung,
};
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
};

use crate::{
    change_request_observer::{ChangeRequestRef, ChangeRequestRefresher},
    event_sink::EventSink,
    issue_observer::{IssueObservationSource, IssueRef, IssueRefreshCadence, IssueRefresher},
    providers::change_request::ObservationError,
};
use turn_delivery::UnavailableTurnDeliveryActuator;
use wake::ReconcilerWake;

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

/// In-process workflow activation context. Only the resulting Message resource
/// crosses hosts; transport requests are not a wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct CrewTurnIntent {
    pub receiver: Option<String>,
    pub namespace: String,
    pub convoy: String,
    pub source: String,
    pub vessel: String,
    pub role: String,
    pub brief: String,
    pub subject_revision: String,
    pub subject: Option<flotilla_protocol::Subject>,
    pub sender: String,
    #[builder(default = flotilla_resources::MessageRelation::System)]
    pub relation: flotilla_resources::MessageRelation,
    #[builder(default)]
    pub references: Vec<flotilla_resources::MessageReference>,
    pub message_subject: Option<flotilla_resources::MessageReference>,
    pub delivery_condition: Option<Leaf>,
    #[builder(default)]
    pub expectation: flotilla_resources::MessageExpectation,
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
    async fn patch_status(
        self: Arc<Self>,
        _namespace: &str,
        _kind: &str,
        _name: &str,
        _status: serde_json::Value,
        _expected_resource_version: &str,
    ) -> Result<(), String> {
        Err("resource status mutation router unavailable".into())
    }
}

#[async_trait]
pub trait TurnDeliveryActuator: Send + Sync {
    async fn deliver(&self, request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String>;
    async fn hold(&self, request: &CrewTurnIntent, act: &HoldAct, reason: &str) -> Result<(), String>;
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
    message_inboxes: Arc<Mutex<HashMap<String, flotilla_resources::MessageInbox>>>,
    charter_brief_renderer: Arc<dyn flotilla_resources::CharterBriefRenderer>,
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

impl LeafSubscriptionTable {
    pub fn with_charter_brief_renderer(mut self, renderer: Arc<dyn flotilla_resources::CharterBriefRenderer>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure rendering before sharing subscriptions").charter_brief_renderer = renderer;
        self
    }

    pub fn with_message_inboxes(mut self, inboxes: Arc<Mutex<HashMap<String, flotilla_resources::MessageInbox>>>) -> Self {
        Arc::get_mut(&mut self.inner).expect("configure inboxes before sharing subscriptions").message_inboxes = inboxes;
        self
    }

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
                message_inboxes: Default::default(),
                charter_brief_renderer: Arc::new(flotilla_resources::CharterProseRenderer),
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
        let wake = ReconcilerWake::new(self.clone());
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
}

mod sources;
mod stalls;
mod subscriptions;
mod turn_delivery;
mod wake;

pub(crate) use stalls::{crew_role_address, supervision_message_source};
pub(crate) use turn_delivery::{durable_message_evidence, queued_turn_evidence, queued_turn_session, turn_message_producer_key};

#[cfg(test)]
mod tests;
