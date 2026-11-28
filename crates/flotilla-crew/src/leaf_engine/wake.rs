use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress, LeafOperator, LifecycleReading};
use flotilla_resources::checkout_integration::remote_ref_for_branch;
use flotilla_resources::convoy::engine::OldPromises;
use flotilla_resources::{
    actor_obligation,
    address::{leaf_address, leaf_for, record_name, subject_address},
    checkout_records::{latest_launches, CheckoutChange, CheckoutRecord},
    clone_records::{launched, CloneChange, CloneRecord},
    instantiate_exit, instantiate_turn_delivery,
    promises::{PromiseKind, PromiseState},
    select_convoy_children, subject_relationship_conflicts, ChangeRequest, CheckoutSpec, ControllerRetry, ControllerRetryDisposition,
    Convoy, ConvoyPhase, CrewWorkPhase, Forge, InstantiatedExit, LeafMaker, ResourceError, ResourceObject, RetryCeiling, StatusPatch,
    TerminalAttentionSource, TerminalSession, TurnDeliveryOutcome, Vessel, WorkPhase,
};
use flotilla_store::{
    controller::{SecondaryWatch, WorkQueueSender},
    load_metrics::carry,
    ResourceBackend, StoreQuery, WatchView,
};
use futures::StreamExt;
use tokio::sync::{broadcast, OnceCell};

use super::schedule::StallSchedule;
use super::sources::{change_request_sources, freshest_change_requests};
use super::stalls::{declared_stall, is_active_change_request_probe, is_merged_settlement_probe, launch_underway};
use super::{EpisodeKeyFields, LeafSubscriptionRow, LeafSubscriptionTable, LeafWatcher, UnableEvidenceKey};
use flotilla_resources::{queued_message_evidence, queued_session_evidence, queued_turn_session};

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
        checkouts: &BTreeMap<String, CheckoutRecord>,
        observations: &BTreeMap<String, ResourceObject<ChangeRequest>>,
        forges: &[flotilla_resources::ForgeSpec],
    ) -> Option<Self> {
        let mut revisions = BTreeMap::new();
        match exit {
            InstantiatedExit::Claim => {}
            InstantiatedExit::Table(entries) => {
                for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
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
            // A checkout's launch, its reading and its inspection stand for its revision.
            checkouts: checkouts
                .iter()
                .map(|(name, record)| (name.clone(), format!("{}:{:?}:{:?}", record.name, record.reading, record.inspection())))
                .collect(),
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
    pub(super) sessions: WatchView<TerminalSession>,
    pub(super) change_requests: WatchView<ChangeRequest>,
    pub(super) vessels: WatchView<Vessel>,
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
            sessions: WatchView::start(backend.clone(), namespace).await?,
            change_requests: WatchView::start(backend.clone(), namespace).await?,
            vessels: WatchView::start(backend.clone(), namespace).await?,
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

    /// Until when the evidence `key` about row `row_id`, first seen at `first_seen`, is still
    /// debounced by `delay`; none once it has held for that long.
    pub(super) async fn maker_debouncing(
        &self,
        row_id: uuid::Uuid,
        key: UnableEvidenceKey,
        first_seen: DateTime<Utc>,
        delay: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let mut episodes = self.subscriptions.inner.unable_since.lock().await;
        let episode = episodes.entry(row_id).or_insert((key, first_seen));
        if episode.0 != key {
            *episode = (key, first_seen);
        }
        Some(episode.1 + delay).filter(|until| now < *until)
    }

    #[cfg(test)]
    pub(super) async fn observe_queued_turns(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let primitives = self.subscriptions.message_primitives();
        let seats = flotilla_resources::convoy_crew_seats(convoy);
        let named = flotilla_resources::convoy_seat_turns(convoy);
        let messages = primitives.seat_standings(namespace, &seats, &named).await.map_err(|error| error.to_string())?;
        self.observe_queued_turns_from_messages(namespace, convoy, sessions, &messages, now).await
    }

    pub(super) async fn observe_queued_turns_from_messages(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        messages: &[flotilla_messaging::SeatMessage],
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(status) = &convoy.status else { return Ok(()) };
        let mut observations = Vec::new();
        for (source, delivery) in &status.turn_deliveries {
            for episode in &delivery.episodes {
                let TurnDeliveryOutcome::Queued { vessel, role, message_id, .. } = &episode.outcome else { continue };
                let (confirmed, blocking_reason) = match messages.iter().find(|message| message.name == *message_id) {
                    Some(message) => queued_message_evidence(message.phase, message.reason.as_deref()),
                    None => queued_session_evidence(queued_turn_session(sessions, vessel, role)),
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
            flotilla_store::note_convoy(
                &self.subscriptions.inner.convoy_engine,
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
        let mut change_request_watch = views.change_requests.subscribe_keys();
        // The observer confirms an unchanged record without writing it, so no watch sees that.
        let mut change_request_confirmations = self.subscriptions.inner.change_requests.confirmations();
        let mut vessel_watch = views.vessels.subscribe_keys();
        let mut forge_watch = views.forges.subscribe();
        // Clones are the engine's (#3108): where it serves none, no worktree's clone row is armed.
        let mut clone_changes = self.subscriptions.inner.lifecycle_reads.clones().ok().map(|clones| clones.changes());
        // So are checkouts: a convoy's rows read the checkouts held for it.
        let mut checkout_changes = self.subscriptions.inner.lifecycle_reads.checkouts().ok().map(|checkouts| checkouts.changes());
        // A convoy's promises are its engine record's: no old record is written for their edges,
        // so its rows follow the engine's changes too (#3108).
        let mut engine_changes = Some(self.subscriptions.inner.convoy_engine.changes());
        let mut convoy_objects = views.local_convoys().await?.as_ref().clone();
        let mut wake_rx = self.subscriptions.inner.reconciler_tx.subscribe();
        // What the stall judge reads besides the rows' inputs: every convoy, a supervisor's
        // included; the sessions; the seats' messages and their agents' sessions; and the
        // supervision and delivery declarations.
        let mut every_convoy_watch = views.convoys.subscribe_keys();
        let mut session_watch = views.sessions.subscribe_keys();
        let mut project_watch = views.projects.subscribe();
        let mut defaults_watch = views.defaults.subscribe();
        let mut fleet_watch = views.fleet.subscribe();
        let mut inbox_changes = self.subscriptions.message_primitives().inbox_changes().map_err(|error| error.to_string())?;
        let mut agent_changes = self.subscriptions.inner.lifecycle_reads.agent_sessions().ok().and_then(|reads| reads.changes().ok());
        let clock = Arc::clone(&self.subscriptions.inner.clock);
        let mut schedule = StallSchedule::default();
        self.sync_rows(&namespace, &convoy_objects).await?;

        loop {
            self.judge_scheduled(&namespace, &convoy_objects, clock.now(), &mut schedule).await?;
            // Measured on the daemon's clock.
            let due_wait = schedule.next_due().map(|due| (due - clock.now()).to_std().unwrap_or_default());
            tokio::select! {
                () = tokio::time::sleep(due_wait.unwrap_or_default()), if due_wait.is_some() => {}
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
                    schedule.convoys_changed(selected.as_ref());
                }
                event = every_convoy_watch.recv() => {
                    if let Err(broadcast::error::RecvError::Closed) = event {
                        return Err("convoy view stopped".into());
                    }
                    // Any launch, another home's included, may be a stalled seat's supervisor.
                    schedule.changed(Some(&stalled(&convoy_objects)));
                    schedule.charter_changed();
                }
                event = session_watch.recv() => {
                    let selected = match event {
                        Ok(Some(name)) => Some(session_convoys(views.sessions.shared_named(&name).await.map_err(|error| error.to_string())?)),
                        Ok(None) | Err(broadcast::error::RecvError::Lagged(_)) => None,
                        Err(broadcast::error::RecvError::Closed) => return Err("session view stopped".into()),
                    };
                    self.seats_changed(&mut schedule, &convoy_objects, selected).await;
                    schedule.charter_changed();
                }
                change = async {
                    match agent_changes.as_mut() {
                        Some(changes) => changes.next().await,
                        None => std::future::pending().await,
                    }
                } => match change {
                    Some(flotilla_resources::agent_activity::AgentSessionChange::Changed { name, origin }) => {
                        schedule.session_changed(&(name.clone(), origin.clone()));
                        let sessions = views.sessions.shared_list().await.map_err(|error| error.to_string())?;
                        let running = sessions.items.iter().filter(|session| {
                            flotilla_resources::terminal_records::agent_session_of(&session.object) == Some(name.as_str())
                                && session.object.metadata.annotations.get(flotilla_resources::ORIGIN_ROOT_ANNOTATION) == origin.as_ref()
                        });
                        let selected = running.filter_map(|session| session.object.metadata.labels.get(flotilla_resources::CONVOY_LABEL).cloned()).collect();
                        self.seats_changed(&mut schedule, &convoy_objects, Some(selected)).await;
                    }
                    Some(flotilla_resources::agent_activity::AgentSessionChange::Heard { origin }) => schedule.heard(&origin),
                    Some(flotilla_resources::agent_activity::AgentSessionChange::Rescanned) => schedule.sessions_rescanned(),
                    None => agent_changes = None,
                },
                change = inbox_changes.next() => match change {
                    Some(flotilla_messaging::InboxChange::Touched(inboxes)) => {
                        let selected = inboxes
                            .iter()
                            .flat_map(flotilla_resources::convoy::engine::convoy_records)
                            .filter(|name| convoy_objects.contains_key(name))
                            .collect();
                        schedule.inboxes_changed(&inboxes, &selected);
                    }
                    Some(flotilla_messaging::InboxChange::Rescanned) => schedule.messages_rescanned(),
                    None => return Err("the Message inbox changes stopped".into()),
                },
                changed = project_watch.changed() => {
                    changed.map_err(|_| "project view stopped".to_string())?;
                    // A project declares its convoys' supervision and its charter.
                    schedule.changed(Some(&stalled(&convoy_objects)));
                    schedule.charter_changed();
                }
                changed = defaults_watch.changed() => {
                    changed.map_err(|_| "crew defaults view stopped".to_string())?;
                    schedule.changed(Some(&stalled(&convoy_objects)));
                    schedule.charter_changed();
                }
                changed = fleet_watch.changed() => {
                    // The designation decides whether messages are delivered at all.
                    changed.map_err(|_| "fleet designation view stopped".to_string())?;
                    schedule.changed(None);
                    schedule.charter_changed();
                }
                change = async {
                    match checkout_changes.as_mut() {
                        Some(changes) => changes.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let selected = match change {
                        Ok(CheckoutChange::Changed { name, .. }) => Some(
                            views.row_dependencies.lock().await.by_record.get(&checkout_dependency(launched(&name))).cloned().unwrap_or_default(),
                        ),
                        Ok(CheckoutChange::Rescanned) | Err(broadcast::error::RecvError::Lagged(_)) => None,
                        Err(broadcast::error::RecvError::Closed) => {
                            checkout_changes = None;
                            continue;
                        }
                    };
                    self.rows_changed(&namespace, &convoy_objects, selected.as_ref(), &mut schedule).await?;
                }
                change = async {
                    match engine_changes.as_mut() {
                        Some(changes) => changes.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let selected = match change {
                        Ok(name) => Some(HashSet::from([name])),
                        Err(broadcast::error::RecvError::Lagged(_)) => None,
                        Err(broadcast::error::RecvError::Closed) => {
                            engine_changes = None;
                            continue;
                        }
                    };
                    self.rows_changed(&namespace, &convoy_objects, selected.as_ref(), &mut schedule).await?;
                }
                event = change_request_watch.recv() => {
                    let selected = affected_rows(views, &views.change_requests, event).await?;
                    self.rows_changed(&namespace, &convoy_objects, selected.as_ref(), &mut schedule).await?;
                }
                // A record confirmed is current again, though nothing in it changed: the convoys
                // whose rows read it are judged again, and a stall on its staleness ends (run 5
                // R5-1). Their rows read its values, which did not change, so they stand.
                confirmed = change_request_confirmations.recv() => match confirmed {
                    Ok(name) => {
                        let selected = views.row_dependencies.lock().await.by_record.get(&name).cloned().unwrap_or_default();
                        if !selected.is_empty() {
                            schedule.changed(Some(&selected));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => schedule.changed(None),
                    Err(broadcast::error::RecvError::Closed) => return Err("change request confirmations stopped".into()),
                },
                event = vessel_watch.recv() => {
                    let selected = affected_rows(views, &views.vessels, event).await?;
                    self.rows_changed(&namespace, &convoy_objects, selected.as_ref(), &mut schedule).await?;
                }
                event = forge_watch.changed() => {
                    event.map_err(|_| "forge view stopped".to_string())?;
                    self.rows_changed(&namespace, &convoy_objects, None, &mut schedule).await?;
                }
                change = async {
                    match clone_changes.as_mut() {
                        Some(changes) => changes.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let selected = match change {
                        Ok(CloneChange::Changed(name)) => Some(
                            views.row_dependencies.lock().await.by_record.get(&clone_dependency(launched(&name))).cloned().unwrap_or_default(),
                        ),
                        Ok(CloneChange::Rescanned) | Err(broadcast::error::RecvError::Lagged(_)) => None,
                        // Where clones are not followed, their channel is closed: nothing wakes on them.
                        Err(broadcast::error::RecvError::Closed) => {
                            clone_changes = None;
                            continue;
                        }
                    };
                    self.rows_changed(&namespace, &convoy_objects, selected.as_ref(), &mut schedule).await?;
                }
                // A woken convoy's controller row is judged by the queue's progress.
                wake = wake_rx.recv() => match wake {
                    Ok(convoy) => {
                        sender.send(convoy.clone()).await.map_err(|_| "convoy controller queue closed".to_string())?;
                        schedule.changed(Some(&HashSet::from([convoy])));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for convoy in convoy_objects.values().filter(|convoy| {
                            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landing))
                        }) {
                            sender.send(convoy.metadata.name.clone()).await.map_err(|_| "convoy controller queue closed".to_string())?;
                        }
                        schedule.changed(None);
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err("reconciler wake channel closed".to_string()),
                }
            }
        }
    }

    /// Rebuilds the rows of `selected` convoys, or every convoy's for `None`, after their inputs
    /// changed; the stall judge judges them again.
    async fn rows_changed(
        &self,
        namespace: &str,
        convoys: &HashMap<String, impl ConvoyRead>,
        selected: Option<&HashSet<String>>,
        schedule: &mut StallSchedule,
    ) -> Result<(), String> {
        self.sync_rows_selected(namespace, convoys, selected).await?;
        schedule.changed(selected);
        Ok(())
    }

    /// The crew sessions of `selected` convoys changed, or any convoy's for `None`: those convoys
    /// are judged again, with each convoy one of them supervises and each stall awaiting a
    /// supervisor.
    async fn seats_changed(
        &self,
        schedule: &mut StallSchedule,
        convoys: &HashMap<String, SharedConvoy>,
        selected: Option<HashSet<String>>,
    ) {
        let Some(selected) = selected else {
            schedule.changed(None);
            return;
        };
        let supervised = self.subscriptions.rows().await.into_iter().filter_map(|row| match (&row.maker, row.watcher) {
            (LeafMaker::Supervisor { convoy, .. }, LeafWatcher::ReconcilerWake { convoy: supervised }) if selected.contains(convoy) => {
                Some(supervised)
            }
            _ => None,
        });
        let mut judged: HashSet<String> = supervised.collect();
        judged.extend(stalled(convoys));
        judged.extend(selected);
        schedule.changed(Some(&judged));
    }

    /// This home's `convoy` with its promises as its log records them: a keyed read of one
    /// convoy, made only for a seat that has submitted every open promise.
    async fn recorded_promises(&self, convoy: &ResourceObject<Convoy>) -> Result<OldPromises, String> {
        let engine = Arc::clone(&self.subscriptions.inner.convoy_engine);
        let convoy = convoy.clone();
        let name = convoy.metadata.name.clone();
        tokio::task::spawn_blocking(carry(move || engine.recorded_promises(&convoy, None)))
            .await
            .map_err(|error| format!("recorded promises: {error}"))??
            .ok_or_else(|| format!("the engine does not hold convoy `{name}`"))
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
        let mut clone_readings = HashMap::<flotilla_resources::RepositoryKey, Vec<CloneRecord>>::new();
        let selected_convoys = match selected {
            Some(names) => names.iter().filter_map(|name| convoys.get(name)).collect::<Vec<_>>(),
            None => convoys.values().collect(),
        };
        for convoy in selected_convoys.into_iter().map(ConvoyRead::convoy).filter(|convoy| {
            convoy.status.as_ref().is_some_and(|status| launch_underway(status.phase) || status.phase == ConvoyPhase::Landing)
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
            let checkouts = match flotilla_resources::convoy::engine::checkout_holder(convoy) {
                Some(holder) => {
                    let reads = Arc::clone(&self.subscriptions.inner.lifecycle_reads);
                    let held = tokio::task::spawn_blocking(carry(move || reads.checkouts()?.held(&holder)))
                        .await
                        .map_err(|error| format!("held checkouts: {error}"))??;
                    latest_launches(held)
                }
                None => BTreeMap::new(),
            };
            inputs.extend(checkouts.keys().map(|name| checkout_dependency(name)));
            // A worktree's clone stands as a controller row while the engine's reading of it is a
            // failure (#3108): the leaf shows its cause, or that its attempts are spent.
            for checkout in checkouts.values() {
                let CheckoutSpec::Worktree(spec) = &checkout.spec else { continue };
                inputs.insert(clone_dependency(launched(&spec.clone_ref)));
                if !controller_rows.insert(("Clone".into(), spec.clone_ref.clone())) {
                    continue;
                }
                if !clone_readings.contains_key(&spec.repo_ref) {
                    let (reads, repository) = (Arc::clone(&self.subscriptions.inner.lifecycle_reads), spec.repo_ref.clone());
                    let records = tokio::task::spawn_blocking(carry(move || reads.clones()?.of_repository(&repository)))
                        .await
                        .map_err(|error| format!("clone readings: {error}"))??;
                    clone_readings.insert(spec.repo_ref.clone(), records);
                }
                let Some(retry) = clone_readings[&spec.repo_ref]
                    .iter()
                    .filter(|record| record.name == spec.clone_ref || record.clone_name() == spec.clone_ref)
                    .max_by_key(|record| launch_number(&record.name))
                    .and_then(|record| clone_retry(&record.reading, self.subscriptions.inner.clock.now()))
                else {
                    continue;
                };
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
                    created_at: flotilla_resources::clock::now(),
                    episode_key: EpisodeKeyFields::default(),
                });
            }
            let convoy_vessels = select_convoy_children(convoy, &vessel_sources);
            inputs.extend(convoy_vessels.keys().cloned());
            if launch_underway(status.phase) {
                for stalled in status.stalls.iter().filter(|stalled| stalled.supervisor.is_some()) {
                    if let Some(maker @ LeafMaker::Supervisor { .. }) = &stalled.maker {
                        desired.push(LeafSubscriptionRow {
                            id: uuid::Uuid::nil(),
                            namespace: namespace.to_string(),
                            leaves: stalled.leaves.clone(),
                            watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                            maker: maker.clone(),
                            freshness_demand: None,
                            created_at: flotilla_resources::clock::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
                }
                // Without an exit declaration this convoy is standing: its
                // roles never owe settlement claims, even after assignment.
                // A missing workflow snapshot likewise declares no exit.
                if status.workflow_snapshot.as_ref().and_then(|snapshot| snapshot.exit.as_ref()).is_some() {
                    // The convoy's promises with their logged submission times, read once and
                    // only for a seat that has submitted every open promise.
                    let mut recorded: Option<Result<OldPromises, String>> = None;
                    let mut heads: Option<TurnHeads> = None;
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
                            // A seat whose every open promise is submitted has handed over all it
                            // owes: each promise waits on its kept rule's observation (a PR on its
                            // merge or close), not on the seat, which is not nudged for it. A turn a
                            // rule delivered after those submissions (a review fix) is work the seat
                            // owes whatever its promises, until the seat answers it (a submission
                            // after it, or a push past its head), and a rejection opens the promise
                            // again. The seat's own stall declaration stays an obligation; a stall
                            // judged from its idleness (a nudge) does not, or one nudge would keep it
                            // armed.
                            let open: Vec<_> = flotilla_resources::promises::owned(status, vessel, role)
                                .iter()
                                .filter(|promise| !promise.state.terminal())
                                .collect();
                            if declared_stall(status, vessel, role).is_none()
                                && !open.is_empty()
                                && open.iter().all(|promise| promise.state == flotilla_resources::promises::PromiseState::Submitted)
                            {
                                if recorded.is_none() {
                                    recorded = Some(self.recorded_promises(convoy).await);
                                }
                                let latest = match recorded.as_ref().expect("read above") {
                                    Ok(promises) => latest_submission(promises, vessel, role, &open),
                                    Err(error) => Err(error.clone()),
                                };
                                let heads = heads
                                    .get_or_insert_with(|| TurnHeads::read(convoy, &forges, |name| observed_change_requests.get(name)));
                                let unanswered = match latest {
                                    Ok(latest) => self.unanswered_turns(namespace, convoy, vessel, role, Some(latest), heads).await,
                                    Err(error) => Err(error),
                                };
                                match unanswered {
                                    Ok(turns) if turns.is_empty() => continue,
                                    Ok(_) => {}
                                    // Unread, the seat stays armed for this pass.
                                    Err(error) => tracing::debug!(
                                        convoy = %convoy.metadata.name,
                                        vessel = %vessel,
                                        role = %role,
                                        %error,
                                        "a submitted seat's submission times or turns are unread; it stays armed this pass"
                                    ),
                                }
                            }
                            if status.stalls.iter().any(|stalled| {
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
                                created_at: flotilla_resources::clock::now(),
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
                        created_at: flotilla_resources::clock::now(),
                        episode_key: EpisodeKeyFields::default(),
                    });
                }
                // A submitted PR promise is kept or broken by its record's state:
                // that record's change wakes the controller to observe it.
                for submission in status
                    .promises
                    .values()
                    .flat_map(BTreeMap::values)
                    .flatten()
                    .filter(|promise| promise.kind == PromiseKind::Pr && promise.state == PromiseState::Submitted)
                    .filter_map(|promise| promise.submissions.last())
                {
                    let Some(address) = submission.subject_address(&forges).ok() else { continue };
                    let Some(leaf_address @ LeafAddress::ChangeRequest { .. }) = leaf_for(&address).ok() else { continue };
                    if let Some(name) = record_name(&address) {
                        inputs.insert(name);
                    }
                    for literal in ["merged", "closed"] {
                        desired.push(LeafSubscriptionRow {
                            id: uuid::Uuid::nil(),
                            namespace: namespace.to_string(),
                            leaves: vec![Leaf {
                                address: leaf_address.clone(),
                                field_path: ".state".into(),
                                operator: LeafOperator::Equal,
                                literal: literal.into(),
                            }],
                            watcher: LeafWatcher::ReconcilerWake { convoy: convoy.metadata.name.clone() },
                            maker: LeafMaker::Observed { refresher: "change_request".into(), external_party: "forge".into() },
                            freshness_demand: None,
                            created_at: flotilla_resources::clock::now(),
                            episode_key: EpisodeKeyFields::default(),
                        });
                    }
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
                // The launch's vessel records, whose ends discharge an unread checkout.
                let launch_vessels = if status.engine_launch.is_some() {
                    let (reads, name) = (Arc::clone(&self.subscriptions.inner.lifecycle_reads), convoy.metadata.name.clone());
                    tokio::task::spawn_blocking(carry(move || reads.vessels()?.of_convoy(&name)))
                        .await
                        .map_err(|error| format!("vessel readings: {error}"))??
                } else {
                    Vec::new()
                };
                // Use the same evaluator as the controller. Reuse the child
                // selections above without cloning their resource bodies again.
                let settlement = flotilla_resources::evaluate_landing_settlement(
                    convoy,
                    &checkouts,
                    &launch_vessels,
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
                            // An ended checkout is no longer inspected: it produces no evidence.
                            .find(|checkout| !checkout.ended() && remote_ref_for_branch(checkout.branch()) == *reference)
                            .map(|checkout| LeafMaker::Observed {
                                refresher: "checkout_integration".into(),
                                external_party: format!("Checkout/{}", checkout.checkout_name()),
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
                            freshness_demand: Some(flotilla_resources::clock::now()),
                            created_at: flotilla_resources::clock::now(),
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
                    created_at: flotilla_resources::clock::now(),
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
            let task = tokio::spawn(flotilla_store::load_metrics::inherit(async move {
                if let Err(error) = subscriptions.watch_row(row).await {
                    tracing::warn!(subscription_id = %id, %error, "reconciler leaf subscription watch ended");
                    subscriptions.finish(id).await;
                }
            }));
            self.subscriptions.inner.tasks.lock().await.insert(id, task);
        }
        Ok(())
    }
}

/// The latest submission time of seat `vessel`/`role`'s `submitted` promises, as `recorded` (the
/// convoy's promises read with their log) holds them. `Err` when a promise or its latest
/// submission's time is not there: the record alone does not carry it (run 3 F7).
fn latest_submission(
    recorded: &OldPromises,
    vessel: &str,
    role: &str,
    submitted: &[&flotilla_resources::promises::Promise],
) -> Result<DateTime<Utc>, String> {
    let seat = recorded.get(vessel).and_then(|roles| roles.get(role)).map(Vec::as_slice).unwrap_or_default();
    let mut latest = None;
    for promise in submitted {
        let at = seat
            .iter()
            .find(|logged| logged.id == promise.id)
            .and_then(|logged| logged.submissions.last())
            .and_then(|submission| submission.submitted_at)
            .ok_or_else(|| format!("promise `{}` has no logged submission time", promise.id))?;
        latest = latest.max(Some(at));
    }
    latest.ok_or_else(|| "no submitted promise".to_string())
}

/// A new turn a turn rule admitted to a seat that asked it to act (run 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OwedTurn {
    /// Its rule's source.
    pub(super) source: String,
    pub(super) accepted_at: DateTime<Utc>,
    /// The revision of its subject it was delivered for.
    pub(super) revision: String,
    /// Its message's record name.
    message: String,
}

/// The new turns a turn rule targeting seat `vessel`/`role` admitted that asked it to act (run 4),
/// as the home's notes on turn delivery hold them since it started. A turn that only informed
/// (checks passed) is not among them, and stall nudges and hand-offs record no turn delivery, so
/// they never are. Whether the seat has answered one is [`ReconcilerWake::unanswered_turns`]'s to
/// say.
fn noted_turns<'a>(status: &'a flotilla_resources::ConvoyStatus, vessel: &'a str, role: &'a str) -> impl Iterator<Item = OwedTurn> + 'a {
    status
        .workflow_snapshot
        .iter()
        .flat_map(|snapshot| &snapshot.turn_delivery)
        .filter(move |(_, rule)| rule.to.vessel == vessel && rule.to.role == role)
        .filter_map(|(source, _)| status.turn_deliveries.get(source).map(|delivery| (source.as_str(), delivery)))
        .flat_map(|(source, delivery)| delivery.episodes.iter().map(move |episode| (source, episode)))
        .filter(|(_, episode)| episode.owes)
        .filter_map(|(source, episode)| match &episode.outcome {
            TurnDeliveryOutcome::MessageAccepted { new_turn: true, accepted_at, message, .. } => Some(OwedTurn {
                source: source.to_owned(),
                accepted_at: *accepted_at,
                revision: episode.subject_revision.clone(),
                message: message.name.clone(),
            }),
            _ => None,
        })
}

/// The change requests each turn rule of a convoy fires on, as observed: a turn about a head none
/// of them is at any longer was answered by a push (GitHub run GH2-1), one whose rule no longer
/// asks at that head or whose change request ended was answered by the world (GS-1), and the turn a rule fires
/// at their current state is the one a seat may owe that the home's notes no longer hold (GH2-2).
#[derive(Default)]
pub(super) struct TurnHeads(Vec<TurnSubject>);

/// One change request a turn rule fires on, and its observation.
struct TurnSubject {
    delivery: flotilla_resources::InstantiatedTurnDelivery,
    observed: Option<ResourceObject<ChangeRequest>>,
}

impl TurnHeads {
    /// `convoy`'s rules' change requests, as `observed` reads each record by its name.
    pub(super) fn read<'a>(
        convoy: &ResourceObject<Convoy>,
        forges: &[flotilla_resources::ForgeSpec],
        observed: impl Fn(&str) -> Option<&'a ResourceObject<ChangeRequest>>,
    ) -> Self {
        // A change request rule's subjects are the convoy's own; no observation is needed to bind them.
        let subjects = instantiate_turn_delivery(convoy, &BTreeMap::new(), forges)
            .unwrap_or_default()
            .into_iter()
            .filter(|delivery| matches!(delivery.leaf.address, LeafAddress::ChangeRequest { .. }))
            .filter_map(|delivery| {
                let name = flotilla_resources::address::leaf_record_name(&delivery.leaf.address, forges).ok()?;
                Some(TurnSubject { observed: observed(&name).cloned(), delivery })
            })
            .collect();
        Self(subjects)
    }

    /// Whether a turn rule `source` fired at `revision` is still owed at its change requests'
    /// current observations, read at `now`: some change request it fires on is unobserved, or is
    /// observed at the head the turn was about with the rule still asking to act there. A turn is
    /// answered by a push past its head (GH2-1), by its rule no longer asking at the same head (an
    /// approval, checks now passing, the conflict resolved: GitHub smoke GS-1) and by its change
    /// request's merge or close. A rule on no change request is not judged here.
    fn still_owed(&self, source: &str, revision: &str, now: DateTime<Utc>) -> bool {
        let mut subjects = self.0.iter().filter(|subject| subject.delivery.source == source).peekable();
        subjects.peek().is_none()
            || subjects.any(|subject| match subject.standing(now) {
                Standing::Unobserved => true,
                Standing::Ended => false,
                Standing::At { head, asks, .. } => asks && super::turn_delivery::same_progress(revision, &head),
            })
    }

    /// The turns rules targeting seat `vessel`/`role` fire at their change requests' current
    /// state that ask it to act, as rule source and subject revision: the turns it may owe now,
    /// whatever the home has noted, read at `now`. A merged settlement is not among them.
    fn current<'a>(&'a self, vessel: &'a str, role: &'a str, now: DateTime<Utc>) -> impl Iterator<Item = (&'a str, String)> + 'a {
        self.0
            .iter()
            .filter(move |subject| subject.delivery.rule.to.vessel == vessel && subject.delivery.rule.to.role == role)
            .filter(|subject| !is_merged_settlement_probe(&subject.delivery.leaf))
            .filter_map(move |subject| match subject.standing(now) {
                Standing::At { asks: true, revision: Some(revision), .. } => Some((subject.delivery.source.as_str(), revision)),
                _ => None,
            })
    }
}

/// What a change request a turn rule fires on asks of the rule's seat, as observed.
enum Standing {
    /// Its state or head is unknown: a turn about it may still be owed.
    Unobserved,
    /// It merged or closed: it asks nothing more.
    Ended,
    /// It is open at `head`, where a turn of its rule fires at `revision` (unknown while a checks
    /// leaf's checks are) and still asks the seat to act when `asks`.
    At { head: String, revision: Option<String>, asks: bool },
}

impl TurnSubject {
    /// What this change request asks of its rule's seat at its current observation, read at
    /// `now`, as delivery reads its rule when it fires. A merged settlement's turn is about the
    /// merge, so its subject's merge does not end it; it is judged by its head alone, as before.
    fn standing(&self, now: DateTime<Utc>) -> Standing {
        let Some(record) = self.observed.as_ref() else { return Standing::Unobserved };
        let Some(status) = record.status.as_ref() else { return Standing::Unobserved };
        let merged_settlement = is_merged_settlement_probe(&self.delivery.leaf);
        if !merged_settlement
            && matches!(
                status.state.value,
                Some(flotilla_resources::ObservedChangeRequestState::Merged | flotilla_resources::ObservedChangeRequestState::Closed)
            )
        {
            return Standing::Ended;
        }
        let Some(head) = status.head_sha.value.clone() else { return Standing::Unobserved };
        if merged_settlement {
            return Standing::At { head, revision: None, asks: true };
        }
        let revision = super::turn_delivery::change_request_turn_revision(&self.delivery.leaf, status);
        let observation =
            flotilla_resources::ChangeRequestLeafSubject { change_request: record, now, stale_after: std::time::Duration::MAX };
        let asks = super::turn_delivery::asks_now(&self.delivery.rule, &self.delivery.leaf, &observation);
        Standing::At { head, revision, asks }
    }
}

impl ReconcilerWake {
    /// The turns seat `vessel`/`role` of `convoy` owes and has not answered, accepted after
    /// `since` (its latest submission) when given. A turn is answered by a submission after it, a
    /// push past the head it was about, its rule no longer asking to act at that head or its change
    /// request's merge or close (`heads`, GS-1), or guidance delivered to the seat after it (an
    /// operator's resume, N3-4), and one superseded before it was typed was never put to the seat,
    /// which owes the turn that superseded it instead (GitHub run GH2-1).
    ///
    /// The home's notes hold the turns delivered since it started. A turn delivered before is
    /// found from what is durable (GH2-2): its rule's turn at its change request's current state
    /// (`heads`) has a Message under a name its producer key fixes, read by that name with when
    /// it was accepted. A turn at an earlier head was answered by the push past it. The messages
    /// are read by their exact identities, once, only for turns left after the rest.
    pub(super) async fn unanswered_turns(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        since: Option<DateTime<Utc>>,
        heads: &TurnHeads,
    ) -> Result<Vec<OwedTurn>, String> {
        let Some(status) = convoy.status.as_ref() else { return Ok(Vec::new()) };
        let now = self.subscriptions.inner.clock.now();
        let noted: Vec<_> = noted_turns(status, vessel, role)
            .filter(|turn| since.is_none_or(|since| turn.accepted_at > since))
            .filter(|turn| heads.still_owed(&turn.source, &turn.revision, now))
            .collect();
        let current: Vec<_> = heads.current(vessel, role, now).collect();
        if noted.is_empty() && current.is_empty() {
            return Ok(noted);
        }
        let primitives = self.subscriptions.message_primitives();
        // The current turns' names: a turn is sent from the fleet's turn-rules role to the seat.
        // A fleet with no such role admitted none.
        let project = &convoy.spec.project_ref;
        let seat = super::Party::Seat { project: project.clone(), convoy: convoy.metadata.name.clone(), role: role.to_owned() };
        let current: Vec<_> = match super::Party::fleet("turn-rules").inbox(primitives.as_ref(), namespace, project).await {
            Ok(sender) => {
                let inbox = seat.inbox(primitives.as_ref(), namespace, project).await?;
                current
                    .into_iter()
                    .filter(|(source, revision)| !noted.iter().any(|turn| turn.source == *source && turn.revision == *revision))
                    .map(|(source, revision)| {
                        let key = super::turn_delivery::turn_message_producer_key(source, &revision);
                        let message = flotilla_resources::message_record_name(inbox.as_str(), sender.as_str(), &key);
                        (source.to_owned(), revision, message)
                    })
                    .collect()
            }
            Err(error) => {
                tracing::debug!(convoy = %convoy.metadata.name, %error, "no fleet turn-rules role: no turn was sent");
                Vec::new()
            }
        };
        let receiver = flotilla_resources::crew_role_address(project, &convoy.metadata.name, vessel, role);
        let named: Vec<_> = noted
            .iter()
            .map(|turn| turn.message.clone())
            .chain(current.iter().map(|(_, _, message)| message.clone()))
            .map(|message| (receiver.clone(), message))
            .collect();
        if named.is_empty() {
            return Ok(noted);
        }
        let read: BTreeMap<String, flotilla_messaging::SeatMessage> = primitives
            .sent_messages(namespace, &named)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|message| (message.standing.name.clone(), message.standing))
            .collect();
        let superseded = |message: &str| read.get(message).is_some_and(|read| read.phase == flotilla_resources::MessagePhase::Superseded);
        let found = current.into_iter().filter_map(|(source, revision, message)| {
            let accepted_at = read.get(&message)?.created;
            since.is_none_or(|since| accepted_at > since).then_some(OwedTurn { source, accepted_at, revision, message })
        });
        let owed: Vec<_> = noted.into_iter().chain(found).filter(|turn| !superseded(&turn.message)).collect();
        // An operator's resume or a supervisor's guidance delivered to the seat answers the turns
        // owed before it: the operator is the authority (GitHub run N3-4). Only one delivered
        // since the oldest owed turn can answer any.
        let Some(oldest) = owed.iter().map(|turn| turn.accepted_at).min() else { return Ok(owed) };
        let guidance = primitives.latest_guidance(namespace, &receiver, Some(oldest)).await.map_err(|error| error.to_string())?;
        Ok(match guidance {
            Some(guidance) => owed.into_iter().filter(|turn| turn.accepted_at > guidance.created).collect(),
            None => owed,
        })
    }
}

/// The reverse-dependency key under which a convoy's rows depend on clone `clone`, kept apart from
/// the stored records' names.
fn clone_dependency(clone: &str) -> String {
    format!("Clone/{clone}")
}

/// The row-dependency key of checkout `checkout`: the engine's checkouts are keyed apart from the
/// stored records a row reads.
fn checkout_dependency(checkout: &str) -> String {
    format!("Checkout/{checkout}")
}

/// The launch number of `<clone>#<n>`; a name without one sorts first.
fn launch_number(name: &str) -> u32 {
    name.rsplit_once('#').and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
}

/// A clone's engine reading as a controller row's retry, while it is a failure. The engine's own
/// policy ends a clone's retries (it gives up with its cause), so a launch whose attempts are spent
/// reads as retrying without progress until it does. A reading carries no first failure time, so
/// its next try stands for it: a retrying clone stalls once it has not tried again within the
/// ceiling's duration (it waits on a fact that has not come).
fn clone_retry(reading: &LifecycleReading, now: DateTime<Utc>) -> Option<ControllerRetry> {
    match reading {
        LifecycleReading::Retrying { failures, next, .. } => Some(ControllerRetry {
            attempts: *failures,
            first_failure_at: *next,
            disposition: ControllerRetryDisposition::Retryable { next_attempt_at: *next },
        }),
        LifecycleReading::Exhausted { .. } => Some(ControllerRetry {
            attempts: RetryCeiling::default().attempts,
            first_failure_at: now,
            disposition: ControllerRetryDisposition::Retryable { next_attempt_at: now },
        }),
        LifecycleReading::GaveUp { cause } => Some(ControllerRetry::terminal(None, now, cause.clone())),
        _ => None,
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
/// The convoys holding a stall: a change to any launch, session or declaration may move its
/// supervision.
fn stalled(convoys: &HashMap<String, SharedConvoy>) -> HashSet<String> {
    convoys
        .values()
        .filter(|convoy| convoy.status.as_ref().is_some_and(|status| !status.stalls.is_empty()))
        .map(|convoy| convoy.metadata.name.clone())
        .collect()
}

/// The convoys sessions `named` belong to.
fn session_convoys(named: Vec<Arc<flotilla_resources::ReadResourceObject<TerminalSession>>>) -> HashSet<String> {
    named.iter().filter_map(|session| session.object.metadata.labels.get(flotilla_resources::CONVOY_LABEL).cloned()).collect()
}

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
mod clone_retry_tests {
    use super::*;

    // Each clone reading's stall evidence: a fresh retry none, a retry that has not tried again
    // within the ceiling's duration and a launch whose attempts are spent "retrying without
    // progress", a launch that gave up its cause, and a ready clone no row.
    #[test]
    fn clone_readings_judge_as_controller_retries() {
        let now = Utc::now();
        let ceiling = RetryCeiling::default();
        let stall = |reading: LifecycleReading| clone_retry(&reading, now).map(|retry| retry.stall_reason(now, ceiling));
        assert_eq!(stall(LifecycleReading::Retrying { failures: 1, next: now, cause: "network".into() }), Some(None));
        let long_ago = now - chrono::Duration::from_std(ceiling.total_duration).expect("duration");
        assert_eq!(
            stall(LifecycleReading::Retrying { failures: 1, next: long_ago, cause: "network".into() }),
            Some(Some("retrying without progress".into()))
        );
        assert_eq!(stall(LifecycleReading::Exhausted { cause: "network".into() }), Some(Some("retrying without progress".into())));
        assert_eq!(stall(LifecycleReading::GaveUp { cause: "repository not found".into() }), Some(Some("repository not found".into())));
        assert_eq!(stall(LifecycleReading::Ready { identity: "/clone".into() }), None);
        assert_eq!(stall(LifecycleReading::InProgress), None);
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
