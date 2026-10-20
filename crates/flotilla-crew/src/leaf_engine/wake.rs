use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress, LeafOperator};
use flotilla_resources::checkout_integration::remote_ref_for_branch;
use flotilla_resources::{
    actor_obligation,
    address::{leaf_address, leaf_for, record_name, subject_address},
    instantiate_exit, instantiate_turn_delivery, select_convoy_children, subject_relationship_conflicts, ChangeRequest, Checkout,
    CheckoutSpec, ControllerRetry, Convoy, ConvoyPhase, CrewWorkPhase, Forge, InstantiatedExit, LeafMaker, ResourceError, ResourceObject,
    RetryCeiling, StatusPatch, TerminalAttentionSource, TerminalSession, TurnDeliveryOutcome, Vessel, WorkPhase,
};
use flotilla_store::{
    controller::{SecondaryWatch, WorkQueueSender},
    ResourceBackend, StoreQuery, WatchView,
};
use tokio::sync::{broadcast, OnceCell};

use super::durable_message_evidence;
use super::sources::{change_request_sources, freshest_change_requests};
use super::stalls::{is_active_change_request_probe, is_merged_settlement_probe};
use super::{EpisodeKeyFields, LeafSubscriptionRow, LeafSubscriptionTable, LeafWatcher, UnableEvidenceKey};
use flotilla_resources::{queued_turn_evidence, queued_turn_session};

#[derive(Clone)]
pub(super) struct ReconcilerWake {
    pub(super) subscriptions: LeafSubscriptionTable,
    _marker: PhantomData<Convoy>,
}

// Both existing owned test inventories and shared watched inventories cross
// the judgement seam as references to convoy bodies.
pub(super) trait ConvoyRead: Send + Sync {
    fn convoy(&self) -> &ResourceObject<Convoy>;
}
impl ConvoyRead for ResourceObject<Convoy> {
    fn convoy(&self) -> &ResourceObject<Convoy> {
        self
    }
}
#[derive(Clone)]
pub(super) struct SharedConvoy(Arc<flotilla_resources::ReadResourceObject<Convoy>>);
impl std::ops::Deref for SharedConvoy {
    type Target = ResourceObject<Convoy>;
    fn deref(&self) -> &Self::Target {
        &self.0.object
    }
}
impl ConvoyRead for SharedConvoy {
    fn convoy(&self) -> &ResourceObject<Convoy> {
        self
    }
}
type SharedConvoyInventory = HashMap<String, SharedConvoy>;
struct CachedLocalConvoys {
    generation: Arc<Vec<Arc<flotilla_resources::ReadResourceObject<Convoy>>>>,
    objects: Arc<SharedConvoyInventory>,
}

// Only cache positive proofs whose time-sensitive leaves are final (Merged
// state/readiness or a claim exit). Every resource input is revision-keyed;
// mutable observations and digest freshness always use the evaluator again.
#[derive(PartialEq, Eq)]
struct FinalSettlementInputs {
    convoy: String,
    vessels: Vec<(String, String)>,
    checkouts: Vec<(String, String)>,
    observations: BTreeMap<String, String>,
    forges: Vec<flotilla_resources::ForgeSpec>,
}

fn selected_versions<R: flotilla_resources::Resource>(objects: &BTreeMap<String, ResourceObject<R>>) -> Vec<(String, String)> {
    objects.iter().map(|(name, object)| (name.clone(), object.metadata.resource_version.clone())).collect()
}

impl FinalSettlementInputs {
    fn new(
        convoy: &ResourceObject<Convoy>,
        exit: &InstantiatedExit,
        vessels: &BTreeMap<String, ResourceObject<Vessel>>,
        checkouts: &BTreeMap<String, ResourceObject<Checkout>>,
        observations: &BTreeMap<String, ResourceObject<ChangeRequest>>,
        forges: &[flotilla_resources::ForgeSpec],
    ) -> Option<Self> {
        let mut revisions = BTreeMap::new();
        match exit {
            InstantiatedExit::Claim => {}
            InstantiatedExit::Table(entries) => {
                for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
                    if !matches!(leaf.field_path.as_str(), ".state" | ".ready") {
                        return None;
                    }
                    let name = record_name(&leaf_address(&leaf.address, forges).ok()?)?;
                    let observation = observations.get(&name)?;
                    if observation.status.as_ref()?.state.value != Some(flotilla_resources::ObservedChangeRequestState::Merged) {
                        return None;
                    }
                    revisions.insert(name, observation.metadata.resource_version.clone());
                }
            }
            InstantiatedExit::None => return None,
        }
        Some(Self {
            convoy: convoy.metadata.resource_version.clone(),
            vessels: selected_versions(vessels),
            checkouts: selected_versions(checkouts),
            observations: revisions,
            forges: forges.to_vec(),
        })
    }
}

#[derive(Default)]
struct RowDependencies {
    by_convoy: HashMap<String, HashSet<String>>,
    by_record: HashMap<String, HashSet<String>>,
}
impl RowDependencies {
    fn replace(&mut self, convoy: &str, records: HashSet<String>) {
        if let Some(previous) = self.by_convoy.remove(convoy) {
            for record in previous {
                if let Some(names) = self.by_record.get_mut(&record) {
                    names.remove(convoy);
                    if names.is_empty() {
                        self.by_record.remove(&record);
                    }
                }
            }
        }
        if !records.is_empty() {
            for record in &records {
                self.by_record.entry(record.clone()).or_default().insert(convoy.to_string());
            }
            self.by_convoy.insert(convoy.to_string(), records);
        }
    }
}

pub(super) struct LeafViews {
    pub(super) defaults: WatchView<flotilla_resources::CrewDefaults>,
    pub(super) fleet: WatchView<flotilla_resources::FleetDesignation>,
    pub(super) projects: WatchView<flotilla_resources::Project>,
    pub(super) convoys: WatchView<Convoy>,
    pub(super) ensures: WatchView<flotilla_resources::ConvoyEnsure>,
    pub(super) sessions: WatchView<TerminalSession>,
    pub(super) change_requests: WatchView<ChangeRequest>,
    pub(super) checkouts: WatchView<Checkout>,
    pub(super) vessels: WatchView<Vessel>,
    pub(super) messages: WatchView<flotilla_resources::Message>,
    pub(super) forges: WatchView<Forge>,
    local_convoys: tokio::sync::Mutex<Option<CachedLocalConvoys>>,
    row_dependencies: tokio::sync::Mutex<RowDependencies>,
    standing_rows: tokio::sync::Mutex<HashMap<String, HashSet<uuid::Uuid>>>,
    final_settlements: std::sync::Mutex<HashMap<String, FinalSettlementInputs>>,
    #[cfg(test)]
    pub(super) settlement_evaluations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(super) row_evaluations: std::sync::atomic::AtomicUsize,
}
impl LeafViews {
    async fn start(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        Ok(Self {
            defaults: WatchView::start(backend.clone(), namespace).await?,
            fleet: WatchView::start(backend.clone(), namespace).await?,
            projects: WatchView::start(backend.clone(), namespace).await?,
            convoys: WatchView::start(backend.clone(), namespace).await?,
            ensures: WatchView::start(backend.clone(), namespace).await?,
            sessions: WatchView::start(backend.clone(), namespace).await?,
            change_requests: WatchView::start(backend.clone(), namespace).await?,
            checkouts: WatchView::start(backend.clone(), namespace).await?,
            vessels: WatchView::start(backend.clone(), namespace).await?,
            messages: WatchView::start(backend.clone(), namespace).await?,
            forges: WatchView::start(backend.clone(), namespace).await?,
            local_convoys: Default::default(),
            row_dependencies: Default::default(),
            standing_rows: Default::default(),
            final_settlements: Default::default(),
            #[cfg(test)]
            settlement_evaluations: Default::default(),
            #[cfg(test)]
            row_evaluations: Default::default(),
        })
    }
    async fn local_convoys(&self) -> Result<Arc<HashMap<String, SharedConvoy>>, String> {
        let listed = self.convoys.shared_local_objects().await.map_err(|error| error.to_string())?;
        let mut cached = self.local_convoys.lock().await;
        if let Some(previous) = cached.as_ref() {
            if Arc::ptr_eq(&previous.generation, &listed) {
                return Ok(previous.objects.clone());
            }
        }
        let objects: Arc<SharedConvoyInventory> =
            Arc::new(listed.iter().map(|item| (item.object.metadata.name.clone(), SharedConvoy(item.clone()))).collect());
        *cached = Some(CachedLocalConvoys { generation: listed, objects: objects.clone() });
        Ok(objects)
    }
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
    pub(super) fn new(subscriptions: LeafSubscriptionTable) -> Self {
        Self { subscriptions, _marker: PhantomData }
    }

    pub(super) async fn views(&self, namespace: &str) -> Result<Arc<OnceCell<LeafViews>>, String> {
        let cell = self.subscriptions.inner.watch_views.lock().await.entry(namespace.into()).or_default().clone();
        cell.get_or_try_init(|| LeafViews::start(&self.subscriptions.inner.backend, namespace)).await.map_err(|error| error.to_string())?;
        Ok(cell)
    }

    pub(super) async fn local_convoys(&self, namespace: &str) -> Result<Arc<HashMap<String, SharedConvoy>>, String> {
        self.views(namespace).await?.get().expect("initialized views").local_convoys().await
    }

    pub(super) async fn report_stale_attention(&self, row: &LeafSubscriptionRow, source: TerminalAttentionSource) {
        if self.subscriptions.inner.stale_attention_reported.lock().await.insert(row.id) {
            tracing::warn!(subscription_id = %row.id, ?source, maker = ?row.maker, "terminal attention evidence stale");
        }
    }

    pub(super) async fn maker_debouncing(
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

    #[cfg(test)]
    pub(super) async fn observe_queued_turns(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let cell = self.views(namespace).await?;
        let messages = cell.get().expect("initialized views").messages.list().await.map_err(|error| error.to_string())?;
        self.observe_queued_turns_from_messages(namespace, convoy, sessions, &messages.items, now).await
    }

    pub(super) async fn observe_queued_turns_from_messages(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        messages: &[flotilla_resources::ReadResourceObject<flotilla_resources::Message>],
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(status) = &convoy.status else { return Ok(()) };
        let mut observations = Vec::new();
        for (source, delivery) in &status.turn_deliveries {
            for episode in &delivery.episodes {
                let TurnDeliveryOutcome::Queued { vessel, role, message_id, .. } = &episode.outcome else { continue };
                let (confirmed, blocking_reason) = match messages.iter().find(|message| message.object.metadata.name == *message_id) {
                    Some(message) => durable_message_evidence(&message.object),
                    None => queued_turn_evidence(queued_turn_session(sessions, vessel, role), message_id),
                };
                observations.push(flotilla_resources::QueuedTurnObservation {
                    source: source.clone(),
                    subject_revision: episode.subject_revision.clone(),
                    confirmed,
                    blocking_reason,
                });
            }
        }
        // Empty evidence is a no-op unless the patch must clear stale
        // queued-turn attention. Preserve that repair across every phase.
        if observations.is_empty()
            && !status
                .attention
                .as_ref()
                .is_some_and(|attention| attention.source == flotilla_resources::ConvoyAttention::QUEUED_TURN_SOURCE)
        {
            return Ok(());
        }
        let patch = flotilla_resources::ConvoyStatusPatch::ObserveQueuedTurnDeliveries { observations, observed_at: now };
        let mut next = status.clone();
        patch.apply(&mut next);
        if next != *status {
            flotilla_store::apply_status_patch(
                &self.subscriptions.inner.backend.clone().using::<Convoy>(namespace),
                &convoy.metadata.name,
                &patch,
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) async fn run(&self, namespace: String, sender: WorkQueueSender) -> Result<(), String> {
        self.subscriptions.inner.controller_queues.lock().await.insert(namespace.clone(), sender.clone());
        let cell = self.views(&namespace).await?;
        let views = cell.get().expect("initialized views");
        let mut convoy_watch = views.convoys.subscribe_local_keys();
        let mut checkout_watch = views.checkouts.subscribe_keys();
        let mut change_request_watch = views.change_requests.subscribe_keys();
        let mut vessel_watch = views.vessels.subscribe_keys();
        let mut forge_watch = views.forges.subscribe();
        let mut convoy_objects = views.local_convoys().await?.as_ref().clone();
        let mut wake_rx = self.subscriptions.inner.reconciler_tx.subscribe();
        self.sync_rows(&namespace, &convoy_objects).await?;
        let mut judge_tick = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = judge_tick.tick() => {
                    self.judge_stalls(&namespace, &convoy_objects).await?;
                }
                event = convoy_watch.recv() => {
                    let selected = match event {
                        Ok(Some(name)) => {
                            if let Some(item) = views.convoys.shared_named(&name).await.map_err(|e| e.to_string())?.into_iter().find(|item| matches!(item.provenance, flotilla_resources::ResourceProvenance::Local)) {
                                convoy_objects.insert(name.clone(), SharedConvoy(item));
                            } else { convoy_objects.remove(&name); }
                            Some(HashSet::from([name]))
                        }
                        Ok(None) | Err(broadcast::error::RecvError::Lagged(_)) => {
                            convoy_objects = views.local_convoys().await?.as_ref().clone();
                            None
                        }
                        Err(broadcast::error::RecvError::Closed) => return Err("convoy view stopped".into()),
                    };
                    self.sync_rows_selected(&namespace, &convoy_objects, selected.as_ref()).await?;
                }
                event = checkout_watch.recv() => {
                    let selected = affected_rows(views, &views.checkouts, event).await?;
                    self.sync_rows_selected(&namespace, &convoy_objects, selected.as_ref()).await?;
                }
                event = change_request_watch.recv() => {
                    let selected = affected_rows(views, &views.change_requests, event).await?;
                    self.sync_rows_selected(&namespace, &convoy_objects, selected.as_ref()).await?;
                }
                event = vessel_watch.recv() => {
                    let selected = affected_rows(views, &views.vessels, event).await?;
                    self.sync_rows_selected(&namespace, &convoy_objects, selected.as_ref()).await?;
                }
                event = forge_watch.changed() => {
                    event.map_err(|_| "forge view stopped".to_string())?;
                    self.sync_rows(&namespace, &convoy_objects).await?;
                }
                wake = wake_rx.recv() => match wake {
                    Ok(convoy) => sender.send(convoy).await.map_err(|_| "convoy controller queue closed".to_string())?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for convoy in convoy_objects.values().filter(|convoy| {
                            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing))
                        }) {
                            sender.send(convoy.metadata.name.clone()).await.map_err(|_| "convoy controller queue closed".to_string())?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err("reconciler wake channel closed".to_string()),
                }
            }
        }
    }

    pub(super) async fn sync_rows(&self, namespace: &str, convoys: &HashMap<String, impl ConvoyRead>) -> Result<(), String> {
        self.sync_rows_selected(namespace, convoys, None).await
    }

    pub(super) async fn sync_rows_selected(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        selected: Option<&HashSet<String>>,
    ) -> Result<(), String> {
        if selected.is_some_and(HashSet::is_empty) {
            return Ok(());
        }
        let cell = self.views(namespace).await?;
        let views = cell.get().expect("initialized views");
        let forges =
            views.forges.definitions().await.map_err(|error| error.to_string())?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
        let children_query = selected.map(|names| StoreQuery {
            any_of: Some(
                names
                    .iter()
                    .map(|name| StoreQuery::labels(&BTreeMap::from([(flotilla_resources::CONVOY_LABEL.into(), name.clone())]), true, true))
                    .collect(),
            ),
            include_replicas: true,
            ..Default::default()
        });
        let checkout_sources = match &children_query {
            Some(query) => views.checkouts.query(query).await,
            None => views.checkouts.list().await,
        }
        .map_err(|error| error.to_string())?
        .items;
        let vessel_sources = match &children_query {
            Some(query) => views.vessels.query(query).await,
            None => views.vessels.list().await,
        }
        .map_err(|error| error.to_string())?
        .items;
        let change_request_list = if let Some(names) = selected {
            let mut references = HashSet::new();
            for convoy in names.iter().filter_map(|name| convoys.get(name)).map(ConvoyRead::convoy) {
                let mut leaves = flotilla_resources::expected_change_request_leaves(convoy, &forges).unwrap_or_default();
                if let Ok(InstantiatedExit::Table(entries)) = instantiate_exit(convoy, &forges) {
                    leaves.extend(entries.into_iter().flat_map(|entry| entry.leaves));
                }
                for leaf in leaves {
                    if let Ok(name) = flotilla_resources::address::leaf_record_name(&leaf.address, &forges) {
                        references.insert(name);
                    }
                }
            }
            views.change_requests.query(&StoreQuery::names(references, true)).await
        } else {
            views.change_requests.list().await
        }
        .map_err(|error| error.to_string())?;
        let observed_change_requests =
            freshest_change_requests(&change_request_sources(change_request_list)).into_iter().collect::<BTreeMap<_, _>>();
        // Revision-keyed final proofs are pruned only for affected parents.
        {
            let mut finals = views.final_settlements.lock().expect("settlement cache lock poisoned");
            let landing = |name: &String| {
                convoys
                    .get(name)
                    .is_some_and(|convoy| convoy.convoy().status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landing))
            };
            if let Some(names) = selected {
                for name in names {
                    if !landing(name) {
                        finals.remove(name);
                    }
                }
            } else {
                finals.retain(|name, _| landing(name));
            }
        }
        let mut desired = Vec::<LeafSubscriptionRow>::new();
        let mut dependencies = HashMap::<String, HashSet<String>>::new();
        let selected_convoys = match selected {
            Some(names) => names.iter().filter_map(|name| convoys.get(name)).collect::<Vec<_>>(),
            None => convoys.values().collect(),
        };
        for convoy in selected_convoys.into_iter().map(ConvoyRead::convoy).filter(|convoy| {
            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing))
        }) {
            #[cfg(test)]
            views.row_evaluations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let inputs = dependencies.entry(convoy.metadata.name.clone()).or_default();
            let status = convoy.status.as_ref().expect("holding convoy has status");
            for leaf in flotilla_resources::expected_change_request_leaves(convoy, &forges).unwrap_or_default() {
                if let Ok(name) = flotilla_resources::address::leaf_record_name(&leaf.address, &forges) {
                    inputs.insert(name);
                }
            }
            if let Ok(InstantiatedExit::Table(entries)) = instantiate_exit(convoy, &forges) {
                for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
                    if let Ok(address) = leaf_address(&leaf.address, &forges) {
                        if let Some(name) = record_name(&address) {
                            inputs.insert(name);
                        }
                    }
                }
            }
            let mut controller_rows = HashSet::<(String, String)>::new();
            let conflicts = subject_relationship_conflicts(convoy);
            if !conflicts.is_empty() {
                let details = conflicts
                    .iter()
                    .map(|subject| {
                        subject_address(subject, &forges)
                            .and_then(|address| leaf_for(&address))
                            .map_or_else(|_| subject.id.clone(), |leaf| leaf.to_string())
                    })
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
            inputs.extend(checkouts.keys().cloned());
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
            let convoy_vessels = select_convoy_children(convoy, &vessel_sources);
            inputs.extend(convoy_vessels.keys().cloned());
            for vessel in convoy_vessels.values() {
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
                // Without an exit declaration this convoy is standing: its
                // roles never owe settlement claims, even after assignment.
                // A missing workflow snapshot likewise declares no exit.
                if status.workflow_snapshot.as_ref().and_then(|snapshot| snapshot.exit.as_ref()).is_some() {
                    for (vessel, crew) in &status.crew_work {
                        for (role, work) in crew {
                            let owes_promise = !flotilla_resources::promises::pending(status, vessel, role).is_empty();
                            let owes_claim = owes_promise
                                || status.workflow_snapshot.as_ref().is_some_and(|snapshot| {
                                    snapshot.vessels.iter().any(|requirement| {
                                        requirement.name == *vessel
                                            && requirement.crew.iter().any(|member| {
                                                member.role == *role
                                                    && (!member.promises.is_empty() || !member.completion_conditions.is_empty())
                                            })
                                    })
                                });
                            // A launch is availability, not an assignment. A delivered task
                            // owes a report even without a workflow claim; a declared stall
                            // remains an obligation until resolved. An initial brief alone
                            // does not assign a report, including for non-standing roles.
                            if !owes_claim
                                && !(work.phase == CrewWorkPhase::Working && work.task_assigned)
                                && work.phase != CrewWorkPhase::Stalled
                            {
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
                                || matches!(work.phase, CrewWorkPhase::Done | CrewWorkPhase::Failed | CrewWorkPhase::HandedBack)
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
                }
                for delivery in instantiate_turn_delivery(convoy, &observed_change_requests, &forges)? {
                    let eligible = is_active_change_request_probe(status, &delivery.rule, &delivery.leaf, &forges);
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
            // Instantiation failure disables caching, but settlement still runs
            // before deciding whether exit-table subscriptions can be derived.
            let exit = instantiate_exit(convoy, &forges);
            let final_inputs = exit
                .as_ref()
                .ok()
                .and_then(|exit| FinalSettlementInputs::new(convoy, exit, &convoy_vessels, &checkouts, &observed_change_requests, &forges));
            let cached = final_inputs.as_ref().is_some_and(|inputs| {
                views.final_settlements.lock().expect("settlement cache lock poisoned").get(&convoy.metadata.name) == Some(inputs)
            });
            let mut settlement_makers = Vec::new();
            let satisfied = if cached {
                true
            } else {
                #[cfg(test)]
                views.settlement_evaluations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Use the same evaluator as the controller. Reuse the child
                // selections above without cloning their resource bodies again.
                let settlement = flotilla_resources::evaluate_landing_settlement(
                    convoy,
                    &convoy_vessels,
                    &checkouts,
                    &observed_change_requests,
                    &forges,
                    self.subscriptions.change_request_stale_after(),
                    flotilla_resources::LANDING_EVIDENCE_TTL,
                    self.subscriptions.inner.clock.now(),
                );
                if settlement.satisfied
                    && matches!(
                        settlement.mode,
                        flotilla_resources::SettlementMode::ClaimExit | flotilla_resources::SettlementMode::WorldTerminal
                    )
                {
                    if let Some(inputs) = final_inputs {
                        views
                            .final_settlements
                            .lock()
                            .expect("settlement cache lock poisoned")
                            .insert(convoy.metadata.name.clone(), inputs);
                    }
                } else {
                    views.final_settlements.lock().expect("settlement cache lock poisoned").remove(&convoy.metadata.name);
                }
                // Empty exit tables still have a producer while discovery or
                // checkout evidence is pending. Invalid/no-exit evaluations do
                // not acquire a blanket dependency: those can be real dead ends.
                use flotilla_resources::UnmetSettlementExpectation as Unmet;
                for unmet in &settlement.unmet {
                    let maker = match unmet {
                        Unmet::SubjectDiscoveryPending { .. } => {
                            Some(LeafMaker::Observed { refresher: "branch_subject_scan".into(), external_party: "forge".into() })
                        }
                        Unmet::MissingCheckout { checkout } => convoy_vessels
                            .values()
                            .find(|vessel| {
                                vessel.status.as_ref().is_some_and(|status| status.checkout_refs.values().any(|name| name == checkout))
                            })
                            .map(|vessel| {
                                let now = self.subscriptions.inner.clock.now();
                                LeafMaker::Controller {
                                    resource_kind: "Vessel".into(),
                                    name: Some(vessel.metadata.name.clone()),
                                    retry: ControllerRetry::retryable_at(None, now, now),
                                    ceiling: RetryCeiling::default(),
                                }
                            }),
                        Unmet::MissingObservedRef { reference } | Unmet::StaleObservedRef { reference, .. } => checkouts
                            .values()
                            .find(|checkout| {
                                let branch = match &checkout.spec {
                                    CheckoutSpec::Worktree(spec) => &spec.r#ref,
                                    CheckoutSpec::FreshClone(spec) => &spec.r#ref,
                                    CheckoutSpec::Observed(spec) => &spec.r#ref,
                                };
                                remote_ref_for_branch(branch) == *reference
                            })
                            .map(|checkout| LeafMaker::Observed {
                                refresher: "checkout_integration".into(),
                                external_party: format!("Checkout/{}", checkout.metadata.name),
                            }),
                        _ => None,
                    };
                    if let Some(maker) = maker {
                        if !settlement_makers.iter().any(|existing| same_standing_maker(existing, &maker)) {
                            settlement_makers.push(maker);
                        }
                    }
                }
                settlement.satisfied
            };
            let now = self.subscriptions.inner.clock.now();
            if satisfied {
                settlement_makers.push(LeafMaker::Controller {
                    resource_kind: "Convoy".into(),
                    name: Some(convoy.metadata.name.clone()),
                    retry: ControllerRetry::retryable_at(None, now, now),
                    ceiling: RetryCeiling::default(),
                });
            }
            for maker in settlement_makers {
                desired.push(LeafSubscriptionRow {
                    id: uuid::Uuid::nil(),
                    namespace: namespace.into(),
                    leaves: vec![Leaf {
                        address: LeafAddress::Convoy { name: convoy.metadata.name.clone() },
                        field_path: ".status.phase".into(),
                        operator: LeafOperator::Equal,
                        literal: "Landed".into(),
                    }],
                    watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                    maker,
                    freshness_demand: None,
                    created_at: now,
                    episode_key: EpisodeKeyFields::default(),
                });
            }
            let exit = match exit {
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
            for delivery in instantiate_turn_delivery(convoy, &observed_change_requests, &forges)? {
                // A merged PR cannot reopen a crew that already claimed settlement.
                if is_merged_settlement_probe(&delivery.leaf) {
                    continue;
                }
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

        for row in &desired {
            let convoy = match &row.watcher {
                LeafWatcher::ReconcilerWake { convoy } | LeafWatcher::TurnDelivery { convoy, .. } => convoy,
                _ => continue,
            };
            for leaf in &row.leaves {
                if let Ok(address) = leaf_address(&leaf.address, &forges) {
                    if let Some(name) = record_name(&address) {
                        dependencies.entry(convoy.clone()).or_default().insert(name);
                    }
                }
            }
        }
        {
            let mut index = views.row_dependencies.lock().await;
            let names = match selected {
                Some(names) => names.clone(),
                None => index.by_convoy.keys().chain(convoys.keys()).cloned().collect(),
            };
            for name in names {
                index.replace(&name, dependencies.remove(&name).unwrap_or_default());
            }
        }
        let existing = if let Some(names) = selected {
            let index = views.standing_rows.lock().await;
            let rows = self.subscriptions.inner.rows.lock().await;
            names.iter().flat_map(|name| index.get(name).into_iter().flatten()).filter_map(|id| rows.get(id)).cloned().collect::<Vec<_>>()
        } else {
            self.subscriptions
                .inner
                .rows
                .lock()
                .await
                .values()
                .filter(|row| row.namespace == namespace && watcher_convoy(&row.watcher).is_some())
                .cloned()
                .collect::<Vec<_>>()
        };
        {
            let mut index = views.standing_rows.lock().await;
            if let Some(names) = selected {
                for name in names {
                    index.remove(name);
                }
            } else {
                index.clear();
            }
        }
        for row in &existing {
            if desired.iter().any(|candidate| same_standing_row(candidate, row)) {
                continue;
            }
            self.subscriptions.inner.rows.lock().await.remove(&row.id);
            if let Some(task) = self.subscriptions.inner.tasks.lock().await.remove(&row.id) {
                task.abort();
            }
            self.subscriptions.remove_routing(row.id).await;
            self.subscriptions.forget_firings(row.id).await;
            self.subscriptions.inner.change_requests.release(row.id).await;
            self.subscriptions.inner.issues.release(row.id).await;
        }

        'desired_rows: for mut row in desired {
            if let Some(existing) = existing.iter().find(|existing| same_standing_row(&row, existing)) {
                if let Some(convoy) = watcher_convoy(&existing.watcher) {
                    views.standing_rows.lock().await.entry(convoy.to_string()).or_default().insert(existing.id);
                }
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
            if let Some(convoy) = watcher_convoy(&row.watcher) {
                views.standing_rows.lock().await.entry(convoy.to_string()).or_default().insert(id);
            }
            if let Err(error) = self.subscriptions.arm_dependencies(&row).await {
                self.subscriptions.finish(id).await;
                tracing::warn!(watcher = ?row.watcher, %error, "arm standing leaf subscription failed");
                continue 'desired_rows;
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
fn watcher_convoy(watcher: &LeafWatcher) -> Option<&str> {
    match watcher {
        LeafWatcher::ReconcilerWake { convoy } | LeafWatcher::TurnDelivery { convoy, .. } => Some(convoy),
        LeafWatcher::WaitCaller { .. } => None,
    }
}

// Reverse dependencies cover disappeared children too. A newly created child
// supplies its parent label before it has ever participated in row evaluation.
async fn affected_rows<T: flotilla_resources::Resource>(
    views: &LeafViews,
    view: &WatchView<T>,
    event: Result<Option<String>, broadcast::error::RecvError>,
) -> Result<Option<HashSet<String>>, String> {
    match event {
        Ok(Some(name)) => {
            let mut names = views.row_dependencies.lock().await.by_record.get(&name).cloned().unwrap_or_default();
            for item in view.shared_named(&name).await.map_err(|error| error.to_string())? {
                if let Some(parent) = item.object.metadata.labels.get(flotilla_resources::CONVOY_LABEL) {
                    names.insert(parent.clone());
                }
            }
            Ok(Some(names))
        }
        Ok(None) | Err(broadcast::error::RecvError::Lagged(_)) => Ok(None),
        Err(broadcast::error::RecvError::Closed) => Err("leaf input view stopped".into()),
    }
}

pub(super) fn same_standing_row(left: &LeafSubscriptionRow, right: &LeafSubscriptionRow) -> bool {
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

pub(super) fn same_standing_maker(left: &LeafMaker, right: &LeafMaker) -> bool {
    match (left, right) {
        (
            LeafMaker::Controller { resource_kind: left_kind, name: left_name, ceiling: left_ceiling, .. },
            LeafMaker::Controller { resource_kind: right_kind, name: right_name, ceiling: right_ceiling, .. },
        ) => left_kind == right_kind && left_name == right_name && left_ceiling == right_ceiling,
        _ => left == right,
    }
}

#[cfg(test)]
mod dependency_tests {
    use super::*;
    // Replacement removes old reverse edges, keeps other parents, and deletes
    // empty buckets. Generated operation sequences cover duplicate and delete.
    #[hegel::test]
    fn reverse_dependencies_follow_replacements(tc: hegel::TestCase) {
        let mut index = RowDependencies::default();
        let mut expected = HashMap::<String, HashSet<String>>::new();
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(32));
        for _ in 0..count {
            let parent = format!("c{}", tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4)));
            let size = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(6));
            let records = (0..size).map(|i| format!("record-{i}")).collect::<HashSet<_>>();
            index.replace(&parent, records.clone());
            expected.insert(parent, records);
            let mut reverse = HashMap::<String, HashSet<String>>::new();
            for (parent, records) in &expected {
                for record in records {
                    reverse.entry(record.clone()).or_default().insert(parent.clone());
                }
            }
            assert_eq!(index.by_record, reverse);
        }
    }
}
