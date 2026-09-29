use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{DaemonEvent, Leaf, LeafAddress, LeafFire, LeafOperator, NodeId, WaitSubscriptionRequest};
use flotilla_resources::{
    actor_obligation, admit_leaf, controller::SecondaryWatch, evaluate_leaf, expected_change_request_leaves, external_patches,
    instantiate_exit, instantiate_turn_delivery, produced_subject_conflicts, select_convoy_children, Artifact, ArtifactLeafSubject,
    ChangeRequest, ChangeRequestLeafSubject, Checkout, CheckoutSpec, ControllerRetry, Convoy, ConvoyAttention, ConvoyLeafSubject,
    ConvoyPhase, Forge, HoldAct, InstantiatedExit, Issue, IssueLeafSubject, LeafMaker, Project, ReadResourceObject, ReadWatchEvent,
    ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, RetryCeiling, StallEvidenceSource, StallNudge, StallRung,
    StallSupervisor, StalledCondition, StatusPatch, SupervisionTarget, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
    TerminalSession, TerminalSessionPhase, TerminalSessionSource, ThreeValue, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRule,
    TurnDeliveryRung, Usage, UsageLeafSubject, Vessel, VesselLeafSubject, WatchEvent, WatchStart, WorkLeafSubject, WorkPhase, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    task::JoinHandle,
};

use crate::{
    change_request_observer::{ChangeRequestRef, ChangeRequestRefresher},
    issue_observer::{IssueObservationSource, IssueRef, IssueRefreshCadence, IssueRefresher},
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

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TurnDeliveryRequest {
    pub namespace: String,
    pub convoy: String,
    pub source: String,
    pub vessel: String,
    pub role: String,
    pub brief: String,
    pub subject_revision: String,
}

#[async_trait]
pub trait TurnDeliveryActuator: Send + Sync {
    async fn deliver(&self, request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String>;
    async fn hold(&self, request: &TurnDeliveryRequest, act: &HoldAct, reason: &str) -> Result<(), String>;
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
    event_tx: broadcast::Sender<DaemonEvent>,
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

impl LeafSubscriptionTable {
    pub fn new(backend: ResourceBackend, event_tx: broadcast::Sender<DaemonEvent>, change_requests: ChangeRequestRefresher) -> Self {
        Self::with_episode_limit(backend, event_tx, change_requests, 3)
    }

    pub fn with_episode_limit(
        backend: ResourceBackend,
        event_tx: broadcast::Sender<DaemonEvent>,
        change_requests: ChangeRequestRefresher,
        episode_limit: u32,
    ) -> Self {
        let issues =
            IssueRefresher::new(backend.clone(), "unavailable".into(), Arc::new(UnavailableIssues), IssueRefreshCadence::default());
        Self::with_issues_and_episode_limit(backend, event_tx, change_requests, issues, episode_limit)
    }

    pub fn with_issues(
        backend: ResourceBackend,
        event_tx: broadcast::Sender<DaemonEvent>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
    ) -> Self {
        Self::with_issues_and_episode_limit(backend, event_tx, change_requests, issues, 3)
    }

    fn with_issues_and_episode_limit(
        backend: ResourceBackend,
        event_tx: broadcast::Sender<DaemonEvent>,
        change_requests: ChangeRequestRefresher,
        issues: IssueRefresher,
        episode_limit: u32,
    ) -> Self {
        let (reconciler_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(LeafSubscriptionTableInner {
                backend,
                event_tx,
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
            }),
        }
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

    pub fn change_request_stale_after(&self) -> std::time::Duration {
        self.inner.change_requests.stale_after()
    }

    pub fn issue_stale_after(&self) -> std::time::Duration {
        self.inner.issues.stale_after()
    }

    pub async fn change_request_observation_error(&self, subject: &ChangeRequestRef) -> Option<String> {
        self.inner.change_requests.observation_error(subject).await
    }

    pub async fn refresh_change_request_once(&self, subject: &ChangeRequestRef) -> Result<(), String> {
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
        let convoys = self.inner.backend.including_replicas::<Convoy>(&row.namespace);
        let vessels = self.inner.backend.including_replicas::<Vessel>(&row.namespace);
        let change_requests = self.inner.backend.including_replicas::<ChangeRequest>(&row.namespace);
        let usages = self.inner.backend.including_replicas::<Usage>(&row.namespace);
        let issues = self.inner.backend.including_replicas::<Issue>(&row.namespace);
        let artifacts = self.inner.backend.including_replicas::<Artifact>(&row.namespace);
        // Open watches before taking the level-triggered snapshots. Writes
        // racing the lists are then buffered by the streams and replayed by
        // the loop instead of falling through a list-then-watch gap.
        let mut convoy_watch = convoys.watch().await.map_err(|error| error.to_string())?;
        let mut vessel_watch = vessels.watch().await.map_err(|error| error.to_string())?;
        let mut change_request_watch = change_requests.watch().await.map_err(|error| error.to_string())?;
        let mut usage_watch = usages.watch().await.map_err(|error| error.to_string())?;
        let mut issue_watch = issues.watch().await.map_err(|error| error.to_string())?;
        let mut artifact_watch = artifacts.watch().await.map_err(|error| error.to_string())?;
        let convoy_list = convoys.list().await.map_err(|error| error.to_string())?;
        let vessel_list = vessels.list().await.map_err(|error| error.to_string())?;
        let change_request_list = change_requests.list().await.map_err(|error| error.to_string())?;
        let usage_list = usages.list().await.map_err(|error| error.to_string())?;
        let issue_list = issues.list().await.map_err(|error| error.to_string())?;
        let artifact_list = artifacts.list().await.map_err(|error| error.to_string())?;
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
        )? {
            self.fire(row.id, fire).await;
            if !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }) {
                return Ok(());
            }
        }

        loop {
            tokio::select! {
                event = convoy_watch.next() => {
                    let event = event.ok_or_else(|| "convoy resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut convoy_objects);
                }
                event = vessel_watch.next() => {
                    let event = event.ok_or_else(|| "vessel resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut vessel_objects);
                }
                event = change_request_watch.next() => {
                    let event = event.ok_or_else(|| "change request resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    let name = match event {
                        ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => item.object.metadata.name,
                        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                    };
                    // A buffered event may precede the initial list snapshot, and a
                    // deletion may expose a suppressed self-origin copy. Refresh
                    // only this name from the current store on either transition.
                    let copies = change_requests.get_all(&name).await.map_err(|error| error.to_string())?;
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
                    let event = event.ok_or_else(|| "issue resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    let name = match event {
                        ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => item.object.metadata.name,
                        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                    };
                    let copies = issues.get_all(&name).await.map_err(|error| error.to_string())?;
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
                    let event = event.ok_or_else(|| "usage resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut usage_objects);
                }
                event = artifact_watch.next() => {
                    let event = event.ok_or_else(|| "artifact resource watch closed".to_string())?.map_err(|error| error.to_string())?;
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
            )? {
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
                let _ = self.inner.event_tx.send(DaemonEvent::LeafFired(fire));
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
                    tracing::warn!(%convoy, %source, %error, "turn delivery failed");
                }
                let _ = self.inner.reconciler_tx.send(convoy);
            }
            None => {}
        }
    }

    async fn deliver_turn(
        &self,
        subscription_id: uuid::Uuid,
        convoy_name: &str,
        source: &str,
        rule: &TurnDeliveryRule,
        leaf: &Leaf,
    ) -> Result<(), String> {
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
        let claim = status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role));
        let claim_at = claim.and_then(|claim| claim.finished_at);
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
                    _ => return Err(format!("turn-delivery leaf path `{}` has no firing evidence timestamp", leaf.field_path)),
                };
                let brief = claim_at
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
                    .unwrap_or_default();
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
                    _ => return Err(format!("turn-delivery leaf path `{}` has no firing evidence timestamp", leaf.field_path)),
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
            _ => return Err("turn-delivery leaf is not externally observed".into()),
        };
        if status
            .turn_deliveries
            .get(source)
            .is_some_and(|delivery| delivery.episodes.iter().any(|episode| episode.subject_revision == subject_revision))
        {
            return Ok(());
        }
        let claim_at =
            claim_at.ok_or_else(|| format!("turn-delivery target {}/{} has no settlement claim", rule.to.vessel, rule.to.role))?;
        if evidence_at <= claim_at {
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
            .build();
        let prior_episodes = status.turn_deliveries.get(source).map_or(0, |delivery| delivery.episodes.len()) as u32;
        let now = Utc::now();
        let patch = if prior_episodes >= self.inner.episode_limit {
            let reason =
                format!("turn delivery refused after {} consecutive episodes for condition source `{source}`", self.inner.episode_limit);
            let actuator = self.inner.turn_delivery.lock().await.clone();
            actuator.hold(&request, &rule.hold, &reason).await?;
            external_patches::refuse_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: claim_at,
                    outcome: TurnDeliveryOutcome::Refused { reason: reason.clone(), refused_at: now, hold_executed: true },
                },
                ConvoyAttention { source: source.to_string(), reason, raised_at: now },
            )
        } else {
            let actuator = self.inner.turn_delivery.lock().await.clone();
            let rung = actuator.deliver(&request).await?;
            external_patches::record_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: claim_at,
                    outcome: TurnDeliveryOutcome::Delivered { rung, delivered_at: now },
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
    let observation = format!(
        "- Head SHA: `{}`\n- Review actionable at head: {:?}\n- Checks: {:?}\n- Mergeability: {:?}\n",
        cr.head_sha.value.as_deref().unwrap_or("unknown"),
        cr.review.actionable_at_head.value,
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
        sender: mpsc::Sender<String>,
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

    async fn judge_stalls(&self, namespace: &str, convoys: &HashMap<String, ResourceObject<Convoy>>) -> Result<(), String> {
        let backend = &self.subscriptions.inner.backend;
        let projects = backend.including_replicas::<Project>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let available_convoys = backend.including_replicas::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let sessions = backend.including_replicas::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let observations = backend.including_replicas::<ChangeRequest>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let rows = self.subscriptions.rows().await;
        let now = Utc::now();
        for convoy in convoys.values() {
            let Some(status) = &convoy.status else { continue };
            let selected_sessions = select_convoy_children(convoy, &sessions);
            let holding = matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing | ConvoyPhase::Anchored);
            if let Some(stalled) =
                status.stalled.as_ref().filter(|stalled| stalled.supervisor.is_some() && stalled.source != StallEvidenceSource::Crew)
            {
                if let Some((vessel, role)) = stalled_source_actor(stalled) {
                    let source_working =
                        selected_sessions.values().any(|session| {
                            session.metadata.labels.get(VESSEL_LABEL).is_some_and(|name| name == vessel)
                                && session.metadata.labels.get(ROLE_LABEL).is_some_and(|name| name == role)
                                && session.status.as_ref().and_then(|status| status.attention.as_ref()).is_some_and(|attention| {
                                    attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now)
                                })
                        });
                    if source_working {
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
                    && matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy: name } | LeafWatcher::TurnDelivery { convoy: name, .. } if name == &convoy.metadata.name)
            }).collect::<Vec<_>>();
            let mut unable = None;
            let mut able = false;
            let mut unknown = false;
            let mut actionable_rows = 0;
            for row in convoy_rows {
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
                                    let debounce = match attention.source {
                                        TerminalAttentionSource::Screen => TerminalAttention::DEBOUNCE_FOR,
                                        TerminalAttentionSource::Hook => chrono::Duration::zero(),
                                    };
                                    if self
                                        .maker_debouncing(
                                            row.id,
                                            UnableEvidenceKey::Attention { state: attention.state, source: attention.source },
                                            attention.as_of,
                                            debounce,
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
                                    reason = Some(match (observation.is_some(), has_value, refresh_error) {
                                        (true, true, Some(error)) => format!("stale; refresh failed: {error}"),
                                        (true, true, None) => "stale".into(),
                                        (_, _, Some(error)) => error,
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
            if status.stalled.as_ref().is_some_and(|stalled| stalled.supervision_exhausted)
                && !able
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
                    nudge_history: prior.map_or_else(Vec::new, |stalled| stalled.nudge_history.clone()),
                };
                let declared = status
                    .stalled
                    .as_ref()
                    .filter(|stalled| stalled.source == StallEvidenceSource::Crew && stalled.leaves == condition.leaves);
                if let Some(declared) = declared {
                    condition.evidence = declared.evidence.clone();
                    condition.source = StallEvidenceSource::Crew;
                    condition.reason = declared.reason;
                    condition.began_at = declared.began_at;
                }
                if matches!(condition.maker, Some(LeafMaker::Supervisor { .. })) {
                    if let Some(prior) = prior {
                        condition.evidence = prior.evidence.clone();
                        condition.source = prior.source.clone();
                        condition.reason = prior.reason;
                        condition.began_at = prior.began_at;
                    }
                }
                if let (Some(LeafMaker::Actor { vessel, role }), Some(row)) = (&condition.maker, unable.as_ref().map(|(row, _, _)| *row)) {
                    let session = selected_sessions.values().find(|session| {
                        session.metadata.labels.get(VESSEL_LABEL) == Some(vessel) && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                    });
                    let idle_at = session
                        .filter(|session| matches!(session.spec.source, TerminalSessionSource::Agent { .. }))
                        .and_then(|session| session.status.as_ref())
                        .and_then(|status| status.attention.as_ref())
                        .filter(|attention| attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now))
                        .map(|attention| attention.as_of);
                    if let Some(idle_at) = idle_at.filter(|_| declared.is_none()) {
                        let limit = status
                            .workflow_snapshot
                            .as_ref()
                            .and_then(|workflow| workflow.stall_nudges.get(&format!("{vessel}/{role}")))
                            .map_or(2, |policy| policy.max_per_episode) as usize;
                        let new_idle = condition.nudge_history.last().is_none_or(|nudge| idle_at > nudge.at);
                        if prior.is_some_and(|stalled| {
                            stalled.rung == StallRung::Operator && stalled.evidence.starts_with("nudge delivery failed:")
                        }) {
                            condition.rung = StallRung::Operator;
                            condition.evidence = prior.expect("checked above").evidence.clone();
                        } else if condition.nudge_history.len() < limit {
                            condition.rung = StallRung::Nudge;
                            if new_idle {
                                let leaf = row.leaves.first().ok_or_else(|| "actor row has no leaf".to_string())?;
                                let brief = actor_obligation(leaf)?;
                                let request = TurnDeliveryRequest::builder()
                                    .namespace(namespace.to_string())
                                    .convoy(convoy.metadata.name.clone())
                                    .source(format!("stall-nudge-{}", condition.nudge_history.len() + 1))
                                    .vessel(vessel.clone())
                                    .role(role.clone())
                                    .brief(brief)
                                    .subject_revision(now.timestamp_micros().to_string())
                                    .build();
                                match self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&request).await {
                                    Ok(_) => condition.nudge_history.push(StallNudge { at: now, row: leaf.clone() }),
                                    Err(error) => {
                                        condition.rung = StallRung::Operator;
                                        condition.evidence = format!("nudge delivery failed: {error}");
                                    }
                                }
                            }
                        } else if !new_idle {
                            condition.rung = StallRung::Nudge;
                        }
                    }
                }
                let needs_supervisor = matches!(condition.maker, Some(LeafMaker::Supervisor { .. }))
                    || condition.source == StallEvidenceSource::Crew
                    || condition.evidence == "NeedsInput"
                    || (condition.rung == StallRung::Operator && !condition.nudge_history.is_empty())
                    || (condition.rung == StallRung::Operator
                        && matches!(condition.maker, Some(LeafMaker::Actor { .. }))
                        && condition.evidence == "idle"
                        && condition.nudge_history.is_empty());
                if needs_supervisor && !condition.evidence.starts_with("nudge delivery failed:") {
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
                            vec![
                                SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
                                SupervisionTarget::ProjectCrew {
                                    convoy_role: "governor".into(),
                                    vessel: String::new(),
                                    role: "governor".into(),
                                },
                            ]
                        });
                    let start = prior.and_then(|stalled| stalled.supervision_index.map(|index| index + 1)).unwrap_or(0);
                    let keep_current = prior.is_some_and(|stalled| stalled.supervisor.is_some())
                        && !matches!(condition.maker, Some(LeafMaker::Supervisor { .. }));
                    if keep_current {
                        condition = prior.expect("checked above").clone();
                    } else {
                        condition.rung = StallRung::Operator;
                        condition.supervisor = None;
                        condition.supervision_index = None;
                        condition.maker = condition.leaves.first().and_then(|leaf| {
                            let LeafAddress::Work { work, .. } = &leaf.address else { return None };
                            let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
                            Some(LeafMaker::Actor { vessel: work.clone(), role: role.to_string() })
                        });
                        condition.supervision_exhausted = true;
                        for (index, target) in policy.iter().enumerate().skip(start) {
                            let candidate = match target {
                                SupervisionTarget::ConvoyCrew { vessel, role } => {
                                    let found = status
                                        .crew_work
                                        .iter()
                                        .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role));
                                    found.map(|(name, _)| (convoy.metadata.name.clone(), name.clone(), role.clone(), StallRung::Bosun))
                                }
                                SupervisionTarget::ProjectCrew { convoy_role, vessel, role } => available_convoys
                                    .iter()
                                    .map(|source| &source.object)
                                    .find(|candidate| {
                                        candidate.spec.project_ref == convoy.spec.project_ref
                                            && candidate.spec.role == *convoy_role
                                            && candidate.metadata.name != convoy.metadata.name
                                    })
                                    .and_then(|candidate| {
                                        candidate.status.as_ref().and_then(|status| {
                                            status
                                                .crew_work
                                                .iter()
                                                .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role))
                                                .map(|(name, _)| {
                                                    (candidate.metadata.name.clone(), name.clone(), role.clone(), StallRung::Governor)
                                                })
                                        })
                                    }),
                                SupervisionTarget::Operator => None,
                            };
                            if let Some((target_convoy, target_vessel, target_role, rung)) = candidate {
                                if target_convoy == convoy.metadata.name
                                    && stalled_source_actor(&condition)
                                        .is_some_and(|(vessel, role)| vessel == target_vessel && role == target_role)
                                {
                                    continue;
                                }
                                let brief = format!(
                                    "Supervise stalled crew {} in convoy {}. Reason: {}. Resume it with guidance, fail it, or escalate it.",
                                    condition.leaves.first().map(|leaf| leaf.field_path.as_str()).unwrap_or_default(),
                                    convoy.metadata.name,
                                    condition.evidence,
                                );
                                let delivery = TurnDeliveryRequest::builder()
                                    .namespace(namespace.to_string())
                                    .convoy(target_convoy.clone())
                                    .source(format!("supervision-{}-{index}", convoy.metadata.name))
                                    .vessel(target_vessel.clone())
                                    .role(target_role.clone())
                                    .brief(brief)
                                    .subject_revision(condition.began_at.timestamp_micros().to_string())
                                    .build();
                                if convoys.contains_key(&target_convoy) {
                                    if let Err(error) = self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&delivery).await
                                    {
                                        condition.evidence = format!("supervisor delivery failed: {error}");
                                        break;
                                    }
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
                                break;
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

    async fn run(&self, namespace: String, sender: mpsc::Sender<String>) -> Result<(), String> {
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
        let mut judge_tick = tokio::time::interval(std::time::Duration::from_secs(1));

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
            let conflicts = produced_subject_conflicts(convoy);
            if !conflicts.is_empty() {
                let details = conflicts
                    .into_iter()
                    .map(|(left, right)| {
                        let left_ref = left.internal().unwrap_or_else(|_| left.id.clone());
                        let right_ref = right.internal().unwrap_or_else(|_| right.id.clone());
                        format!("{left_ref} and {right_ref}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let created_at = convoy.metadata.creation_timestamp;
                // One standing row carries all subject conflicts until the convoy lands.
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
                        retry: ControllerRetry::terminal(
                            None,
                            created_at,
                            format!("conflicting produced change requests {details}; link one as supersedes or unlink one"),
                        ),
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
    change_request: std::time::Duration,
    issue: std::time::Duration,
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
        ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
            Err("unavailable in non-CR leaf contract".to_string())
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
    }

    #[async_trait]
    impl TurnDeliveryActuator for RecordingTurnDelivery {
        async fn deliver(&self, request: &TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
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
        ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
            let observed_at = Utc::now();
            let state = if self.merged.load(Ordering::SeqCst) {
                flotilla_resources::ObservedChangeRequestState::Merged
            } else {
                flotilla_resources::ObservedChangeRequestState::Open
            };
            Ok(flotilla_resources::ChangeRequestStatus {
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
        ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let observed_at = Utc::now();
            Ok(flotilla_resources::ChangeRequestStatus {
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

    #[tokio::test]
    async fn credential_delivery_and_clone_controller_rows_judge_transient_terminal_and_exhausted_failures() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(16);
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let wake = ReconcilerWake { subscriptions: LeafSubscriptionTable::new(backend.clone(), event_tx, refresher), _marker: PhantomData };
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
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx.clone(), refresher);
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
            backend.clone(),
            "test-host".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend, event_tx.clone(), refresher);
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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

    #[tokio::test]
    async fn diagnostics_retain_the_last_firing_for_an_armed_reconciler_row() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let (event_tx, _) = broadcast::channel(4);
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(ControlledChangeRequests { merged: AtomicBool::new(false) }),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend, event_tx, refresher);
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
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
        assert!(matches!(episodes[0].outcome, TurnDeliveryOutcome::Delivered { rung: TurnDeliveryRung::WarmSession, .. }));
        assert!(matches!(episodes[1].outcome, TurnDeliveryOutcome::Delivered { rung: TurnDeliveryRung::FreshAgent, .. }));
        assert!(matches!(episodes[3].outcome, TurnDeliveryOutcome::Refused { hold_executed: true, .. }));
        assert_eq!(actuator.requests.lock().expect("requests").len(), 3);
        assert_eq!(actuator.holds.load(Ordering::SeqCst), 1);
        assert!(status.attention.is_some());
        let first_brief = &actuator.requests.lock().expect("requests")[0].brief;
        assert!(first_brief.contains("Head SHA: `aaa`"));
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), Arc::new(UnavailableChangeRequests), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source.clone(), cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
        let refresher =
            ChangeRequestRefresher::new(authority.clone(), "authority-root".to_string(), Arc::new(UnavailableChangeRequests), cadence);
        let table = LeafSubscriptionTable::new(authority.clone(), event_tx, refresher);
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx.clone(), refresher.clone());
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
        let refresher = ChangeRequestRefresher::new(backend.clone(), "authority".to_string(), source, cadence);
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher.clone());
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
            reader.clone(),
            "kiwi".to_string(),
            Arc::new(CountingChangeRequests { calls: Arc::clone(&calls) }),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let (event_tx, _) = broadcast::channel(16);
        let table = LeafSubscriptionTable::new(reader, event_tx.clone(), refresher);
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
        let table = LeafSubscriptionTable::new(former_owner.clone(), event_tx.clone(), refresher.clone());
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
            backend.clone(),
            "authority".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Open, now),
            labels: flotilla_resources::Observation::known(vec!["ready".into()], now),
            updated_at: flotilla_resources::Observation::known(now, now),
        };
        let opened = records.update_status(&name, &created.metadata.resource_version, &status).await.expect("open issue");
        let closed = flotilla_resources::IssueStatus {
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
            state: flotilla_resources::Observation::known(flotilla_resources::ObservedIssueState::Closed, old),
            labels: flotilla_resources::Observation::known(vec![], old),
            updated_at: flotilla_resources::Observation::known(old, old),
        };
        let created = records.update_status(&name, &created.metadata.resource_version, &stale).await.expect("stale issue");
        let (event_tx, mut events) = broadcast::channel(16);
        let change_requests = ChangeRequestRefresher::new(
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
        let table = LeafSubscriptionTable::with_issues(backend.clone(), event_tx, change_requests, issues);
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
            backend.clone(),
            "authority".into(),
            Arc::new(UnavailableChangeRequests),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        let table = LeafSubscriptionTable::new(backend.clone(), event_tx, refresher);
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
