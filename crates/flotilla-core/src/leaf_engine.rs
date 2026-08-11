use std::{collections::HashMap, future::Future, marker::PhantomData, pin::Pin, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{DaemonEvent, Leaf, LeafAddress, LeafFire, WaitSubscriptionRequest};
use flotilla_resources::{
    admit_leaf, controller::SecondaryWatch, evaluate_leaf, external_patches, instantiate_exit, instantiate_turn_delivery,
    select_convoy_children, ChangeRequest, ChangeRequestLeafSubject, Checkout, Convoy, ConvoyAttention, ConvoyLeafSubject, ConvoyPhase,
    HoldAct, InstantiatedExit, ResourceBackend, ResourceError, ResourceObject, StatusPatch, ThreeValue, TurnDeliveryEpisode,
    TurnDeliveryOutcome, TurnDeliveryRule, TurnDeliveryRung, Usage, UsageLeafSubject, Vessel, VesselLeafSubject, WatchEvent, WatchStart,
    WorkLeafSubject,
};
use futures::StreamExt;
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    task::JoinHandle,
};

use crate::change_request_observer::{ChangeRequestRef, ChangeRequestRefresher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafWatcher {
    WaitCaller { connection_id: uuid::Uuid },
    ReconcilerWake { convoy: String },
    TurnDelivery { convoy: String, source: String, rule: TurnDeliveryRule },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeKeyFields {
    pub source: Option<String>,
    pub convoy: Option<String>,
    pub vessel: Option<String>,
    pub role: Option<String>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TurnDeliveryRequest {
    pub namespace: String,
    pub convoy: String,
    pub source: String,
    pub vessel: String,
    pub role: String,
    pub brief: String,
    pub head_sha: String,
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
    tasks: Mutex<HashMap<uuid::Uuid, JoinHandle<()>>>,
    change_requests: ChangeRequestRefresher,
    reconciler_tx: broadcast::Sender<String>,
    turn_delivery: Mutex<Arc<dyn TurnDeliveryActuator>>,
    episode_limit: u32,
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
        let (reconciler_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(LeafSubscriptionTableInner {
                backend,
                event_tx,
                rows: Mutex::new(HashMap::new()),
                last_firings: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                change_requests,
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
        }
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn SecondaryWatch<Primary = Convoy>> {
        Box::new(ReconcilerWake { subscriptions: self.clone(), _marker: PhantomData })
    }

    pub fn change_request_stale_after(&self) -> std::time::Duration {
        self.inner.change_requests.stale_after()
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
    }

    async fn forget_firings(&self, id: uuid::Uuid) {
        self.inner.last_firings.lock().await.retain(|(subscription_id, _), _| *subscription_id != id);
    }

    async fn watch_row(&self, row: LeafSubscriptionRow) -> Result<(), String> {
        let convoys = self.inner.backend.including_replicas::<Convoy>(&row.namespace);
        let vessels = self.inner.backend.including_replicas::<Vessel>(&row.namespace);
        let change_requests = self.inner.backend.including_replicas::<ChangeRequest>(&row.namespace);
        let usages = self.inner.backend.including_replicas::<Usage>(&row.namespace);
        // Open watches before taking the level-triggered snapshots. Writes
        // racing the lists are then buffered by the streams and replayed by
        // the loop instead of falling through a list-then-watch gap.
        let mut convoy_watch = convoys.watch().await.map_err(|error| error.to_string())?;
        let mut vessel_watch = vessels.watch().await.map_err(|error| error.to_string())?;
        let mut change_request_watch = change_requests.watch().await.map_err(|error| error.to_string())?;
        let mut usage_watch = usages.watch().await.map_err(|error| error.to_string())?;
        let convoy_list = convoys.list().await.map_err(|error| error.to_string())?;
        let vessel_list = vessels.list().await.map_err(|error| error.to_string())?;
        let change_request_list = change_requests.list().await.map_err(|error| error.to_string())?;
        let usage_list = usages.list().await.map_err(|error| error.to_string())?;
        let mut convoy_objects = convoy_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut vessel_objects = vessel_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut change_request_objects = change_request_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });
        let mut usage_objects = usage_list.items.into_iter().fold(HashMap::new(), |mut objects, item| {
            objects.entry(item.object.metadata.name.clone()).or_insert(item.object);
            objects
        });

        if let Some(fire) = evaluate_row(
            &row,
            &convoy_objects,
            &vessel_objects,
            &change_request_objects,
            &usage_objects,
            self.inner.change_requests.stale_after(),
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
                    apply_read_event(event, &mut change_request_objects);
                }
                event = usage_watch.next() => {
                    let event = event.ok_or_else(|| "usage resource watch closed".to_string())?.map_err(|error| error.to_string())?;
                    apply_read_event(event, &mut usage_objects);
                }
            }
            if let Some(fire) = evaluate_row(
                &row,
                &convoy_objects,
                &vessel_objects,
                &change_request_objects,
                &usage_objects,
                self.inner.change_requests.stale_after(),
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
        let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address else {
            return Err("turn-delivery leaf is not change-request addressed".to_string());
        };
        let record_name = flotilla_resources::change_request_record_name(service, scope, *number);
        let record = self
            .inner
            .backend
            .including_replicas::<ChangeRequest>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .find(|item| item.object.metadata.name == record_name)
            .map(|item| item.object)
            .ok_or_else(|| format!("change-request observation `{record_name}` is absent"))?;
        let cr = record.status.as_ref().ok_or_else(|| format!("change-request observation `{record_name}` has no status"))?;
        let head_sha = cr.head_sha.value.clone().ok_or_else(|| "change-request head SHA is unknown".to_string())?;
        let evidence_at = match leaf.field_path.as_str() {
            ".checks" => cr.checks.observed_at,
            ".review.actionable-at-head" => cr.review.actionable_at_head.observed_at,
            ".mergeable" => cr.mergeable.observed_at,
            _ => return Err(format!("turn-delivery leaf path `{}` has no firing evidence timestamp", leaf.field_path)),
        };
        if status.turn_deliveries.get(source).is_some_and(|delivery| delivery.episodes.iter().any(|episode| episode.head_sha == head_sha)) {
            return Ok(());
        }
        let claim_at = status
            .crew_work
            .get(&rule.to.vessel)
            .and_then(|crew| crew.get(&rule.to.role))
            .and_then(|work| work.finished_at)
            .ok_or_else(|| format!("turn-delivery target {}/{} has no settlement claim", rule.to.vessel, rule.to.role))?;
        if evidence_at <= claim_at || cr.head_sha.observed_at <= claim_at {
            return Ok(());
        }

        let brief = compose_turn_brief(&convoy, source, rule, leaf, cr, claim_at);
        let request = TurnDeliveryRequest::builder()
            .namespace(namespace.clone())
            .convoy(convoy_name.to_string())
            .source(source.to_string())
            .vessel(rule.to.vessel.clone())
            .role(rule.to.role.clone())
            .brief(brief)
            .head_sha(head_sha.clone())
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
                    head_sha: head_sha.clone(),
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
                    head_sha: head_sha.clone(),
                    evidence_at,
                    judged_claim_at: claim_at,
                    outcome: TurnDeliveryOutcome::Delivered { rung, delivered_at: now },
                },
                rule.to.vessel.clone(),
                rule.to.role.clone(),
                request.brief.clone(),
            )
        };
        let mut next = status.clone();
        patch.apply(&mut next);
        convoys.update_status(convoy_name, &convoy.metadata.resource_version, &next).await.map_err(|error| error.to_string())?;
        if let Some(row) = self.inner.rows.lock().await.get_mut(&subscription_id) {
            row.episode_key.head_sha = Some(head_sha);
        }
        Ok(())
    }
}

fn compose_turn_brief(
    convoy: &ResourceObject<Convoy>,
    source: &str,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    cr: &flotilla_resources::ChangeRequestStatus,
    claim_at: DateTime<Utc>,
) -> String {
    let repositories = convoy
        .spec
        .repositories
        .iter()
        .map(|repo| format!("- {}: branch `{}` → `{}`", repo.url, repo.source_ref, repo.target_ref))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\n## Turn firing context\n\n- Condition source: `{source}`\n- Fired leaf: `{leaf:?}`\n- Head SHA: `{}`\n- Review actionable at head: {:?}\n- Checks: {:?}\n- Mergeability: {:?}\n- Claim durability fence: `{}`\n- Durable convoy record: `{}/{}`\n- Target crew: `{}/{}`\n\n## Change request and branches\n\n{}\n",
        rule.brief.trim(),
        cr.head_sha.value.as_deref().unwrap_or("unknown"),
        cr.review.actionable_at_head.value,
        cr.checks.value,
        cr.mergeable.value,
        claim_at.to_rfc3339(),
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
    async fn run(&self, namespace: String, sender: mpsc::Sender<String>) -> Result<(), String> {
        let convoys = self.subscriptions.inner.backend.clone().using::<Convoy>(&namespace);
        let checkouts = self.subscriptions.inner.backend.including_replicas::<Checkout>(&namespace);
        let listed_convoys = convoys.list().await.map_err(|error| error.to_string())?;
        let mut convoy_watch = convoys.watch(WatchStart::resuming_from(&listed_convoys)).await.map_err(|error| error.to_string())?;
        let mut checkout_watch = checkouts.watch().await.map_err(|error| error.to_string())?;
        let mut convoy_objects =
            listed_convoys.items.into_iter().map(|convoy| (convoy.metadata.name.clone(), convoy)).collect::<HashMap<_, _>>();
        let mut wake_rx = self.subscriptions.inner.reconciler_tx.subscribe();
        self.sync_rows(&namespace, &convoy_objects).await?;

        loop {
            tokio::select! {
                event = convoy_watch.next() => {
                    let event = event.ok_or_else(|| "reconciler wake convoy watch closed".to_string())?.map_err(|error| error.to_string())?;
                    match event {
                        WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                            convoy_objects.insert(convoy.metadata.name.clone(), convoy);
                        }
                        WatchEvent::Deleted(convoy) => {
                            convoy_objects.remove(&convoy.metadata.name);
                        }
                    }
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                event = checkout_watch.next() => {
                    event.ok_or_else(|| "reconciler wake checkout watch closed".to_string())?.map_err(|error| error.to_string())?;
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
        let checkout_sources = self
            .subscriptions
            .inner
            .backend
            .including_replicas::<Checkout>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items;
        let mut desired = Vec::<LeafSubscriptionRow>::new();
        for convoy in convoys.values().filter(|convoy| {
            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing | ConvoyPhase::Anchored))
        }) {
            let checkouts = select_convoy_children(convoy, &checkout_sources);
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
                            freshness_demand: None,
                            created_at: Utc::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
                }
            }
            let status = convoy.status.as_ref().expect("parked convoy has status");
            for delivery in instantiate_turn_delivery(convoy, &checkouts)? {
                let Some(claim_at) = status
                    .crew_work
                    .get(&delivery.rule.to.vessel)
                    .and_then(|crew| crew.get(&delivery.rule.to.role))
                    .and_then(|work| work.finished_at)
                else {
                    continue;
                };
                desired.push(LeafSubscriptionRow {
                    id: uuid::Uuid::nil(),
                    namespace: namespace.to_string(),
                    leaves: vec![delivery.leaf],
                    watcher: LeafWatcher::TurnDelivery {
                        convoy: convoy.metadata.name.clone(),
                        source: delivery.source.clone(),
                        rule: delivery.rule.clone(),
                    },
                    freshness_demand: Some(claim_at),
                    created_at: Utc::now(),
                    episode_key: EpisodeKeyFields {
                        source: Some(delivery.source.clone()),
                        convoy: Some(convoy.metadata.name.clone()),
                        vessel: Some(delivery.rule.to.vessel),
                        role: Some(delivery.rule.to.role),
                        head_sha: status
                            .turn_deliveries
                            .get(&delivery.source)
                            .and_then(|state| state.episodes.last())
                            .map(|episode| episode.head_sha.clone()),
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
        }

        'desired_rows: for mut row in desired {
            if existing.iter().any(|existing| same_standing_row(&row, existing)) {
                continue;
            }
            let id = uuid::Uuid::new_v4();
            row.id = id;
            self.subscriptions.inner.rows.lock().await.insert(id, row.clone());
            for subject in row.leaves.iter().filter_map(|leaf| ChangeRequestRef::from_address(namespace, &leaf.address)) {
                if let Err(error) = self.subscriptions.inner.change_requests.demand(id, subject, None).await {
                    self.subscriptions.inner.rows.lock().await.remove(&id);
                    self.subscriptions.forget_firings(id).await;
                    self.subscriptions.inner.change_requests.release(id).await;
                    tracing::warn!(watcher = ?row.watcher, %error, "arm standing leaf subscription failed");
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
    left.namespace == right.namespace
        && left.leaves == right.leaves
        && left.watcher == right.watcher
        && left.freshness_demand == right.freshness_demand
        && left.episode_key == right.episode_key
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
    }
}

fn evaluate_row(
    row: &LeafSubscriptionRow,
    convoys: &HashMap<String, ResourceObject<Convoy>>,
    vessels: &HashMap<String, ResourceObject<Vessel>>,
    change_requests: &HashMap<String, ResourceObject<ChangeRequest>>,
    usages: &HashMap<String, ResourceObject<Usage>>,
    change_request_stale_after: std::time::Duration,
) -> Result<Option<LeafFire>, String> {
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
                    stale_after: change_request_stale_after,
                });
                evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn flotilla_resources::LeafSubject), row.freshness_demand)?
            }
            LeafAddress::Usage { provider, account } => {
                let name = flotilla_resources::usage_record_name(provider, account);
                let subject = usages.get(&name).map(UsageLeafSubject);
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
        CheckoutPhase, CheckoutSpec, CheckoutStatus, ConditionValue, ConvoyPhase, ConvoyReconciler, ConvoyRepositorySpec, ConvoySpec,
        ConvoyStatus, CrewWorkPhase, CrewWorkState, ExitDeclaration, InMemoryBackend, InputMeta, IntegrationCondition, LifecycleAuthority,
        ObservedCheckoutSpec, PlacementStatus, RepositoryKey, SqliteBackend, WorkPhase, WorkState, WorkflowSnapshot, WorkflowTemplate,
        CONVOY_LABEL,
    };

    use super::*;

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
                reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla"))
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
        assert_eq!(convoys.get("wake").await.expect("open convoy").status.expect("status").phase, ConvoyPhase::Landing);

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

    #[tokio::test]
    async fn turn_delivery_enforces_head_identity_records_rungs_and_escalates() {
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
            .on("$cr.review.actionable-at-head == true".parse().expect("wake leaf"))
            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("work".to_string()).role("coder".to_string()).build())
            .brief("Address the actionable review and push the fix.".to_string())
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
                    exit: Some(ExitDeclaration::standard_table()),
                    turn_delivery: indexmap::IndexMap::from([("review".to_string(), rule.clone())]),
                    vessels: Vec::new(),
                }),
                work: BTreeMap::from([("work".to_string(), WorkState::builder().phase(WorkPhase::Complete).build())]),
                crew_work: BTreeMap::from([(
                    "work".to_string(),
                    BTreeMap::from([("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(base).build())]),
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
            field_path: ".review.actionable-at-head".to_string(),
            operator: LeafOperator::Equal,
            literal: "true".to_string(),
        };
        let subscription_id = uuid::Uuid::new_v4();
        table.inner.rows.lock().await.insert(subscription_id, LeafSubscriptionRow {
            id: subscription_id,
            namespace: "flotilla".to_string(),
            leaves: vec![leaf.clone()],
            watcher: LeafWatcher::TurnDelivery { convoy: "wake-turn".to_string(), source: "review".to_string(), rule: rule.clone() },
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
                actionable_at_head: flotilla_resources::Observation::known(true, base),
            },
            mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, base),
        };
        let updated = records.update_status(&record.metadata.name, &record_version, &stale_status).await.expect("observe stale head");
        record_version = updated.metadata.resource_version;
        table.deliver_turn(subscription_id, "wake-turn", "review", &rule, &leaf).await.expect("ignore stale firing");
        assert_eq!(table.inner.rows.lock().await[&subscription_id].episode_key.head_sha, None);
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
                    actionable_at_head: flotilla_resources::Observation::known(true, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            };
            let updated = records.update_status(&record.metadata.name, &record_version, &cr_status).await.expect("observe head");
            record_version = updated.metadata.resource_version;
            table.deliver_turn(subscription_id, "wake-turn", "review", &rule, &leaf).await.expect("process firing");
            if index == 0 {
                table.deliver_turn(subscription_id, "wake-turn", "review", &rule, &leaf).await.expect("same head is a no-op");
            }
        }

        let status = convoys.get("wake-turn").await.expect("convoy").status.expect("status");
        let episodes = &status.turn_deliveries["review"].episodes;
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
                reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla"))
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
                reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla")),
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
                reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla"))
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
                reconciler: ConvoyReconciler::new(authority.clone().using::<WorkflowTemplate>("flotilla"))
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
}
