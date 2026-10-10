use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress, LeafOperator};
use flotilla_resources::{
    actor_obligation, instantiate_exit, instantiate_turn_delivery, select_convoy_children, subject_relationship_conflicts, ChangeRequest,
    Checkout, CheckoutSpec, ControllerRetry, Convoy, ConvoyPhase, Forge, InstantiatedExit, LeafMaker, ResourceError, ResourceObject,
    RetryCeiling, StatusPatch, TerminalAttentionSource, TerminalSession, TurnDeliveryOutcome, Vessel, WatchEvent, WatchStart, WorkPhase,
};
use flotilla_store::{
    controller::{SecondaryWatch, WorkQueueSender},
    ResourceBackend,
};
use futures::StreamExt;
use tokio::sync::broadcast;

use super::durable_message_evidence;
use super::sources::{change_request_sources, freshest_change_requests};
use super::stalls::{is_active_change_request_probe, is_merged_settlement_probe};
use super::turn_delivery::{queued_turn_evidence, queued_turn_session};
use super::{EpisodeKeyFields, LeafSubscriptionRow, LeafSubscriptionTable, LeafWatcher, UnableEvidenceKey};

#[derive(Clone)]
pub(super) struct ReconcilerWake {
    pub(super) subscriptions: LeafSubscriptionTable,
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
    pub(super) fn new(subscriptions: LeafSubscriptionTable) -> Self {
        Self { subscriptions, _marker: PhantomData }
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

    pub(super) async fn observe_queued_turns(
        &self,
        namespace: &str,
        convoy: &ResourceObject<Convoy>,
        sessions: &BTreeMap<String, ResourceObject<TerminalSession>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let Some(status) = &convoy.status else { return Ok(()) };
        let mut observations = Vec::new();
        for (source, delivery) in &status.turn_deliveries {
            for episode in &delivery.episodes {
                let TurnDeliveryOutcome::Queued { vessel, role, message_id, .. } = &episode.outcome else { continue };
                let (confirmed, blocking_reason) = match self
                    .subscriptions
                    .inner
                    .backend
                    .including_replicas::<flotilla_resources::Message>(namespace)
                    .get(message_id)
                    .await
                {
                    Ok(message) => durable_message_evidence(&message.object),
                    Err(ResourceError::NotFound { .. }) => queued_turn_evidence(queued_turn_session(sessions, vessel, role), message_id),
                    Err(error) => return Err(error.to_string()),
                };
                observations.push(flotilla_resources::QueuedTurnObservation {
                    source: source.clone(),
                    subject_revision: episode.subject_revision.clone(),
                    confirmed,
                    blocking_reason,
                });
            }
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

    pub(super) async fn sync_rows(&self, namespace: &str, convoys: &HashMap<String, ResourceObject<Convoy>>) -> Result<(), String> {
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
            convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing))
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
                                    && requirement.crew.iter().any(|member| {
                                        member.role == *role && (!member.promises.is_empty() || !member.completion_conditions.is_empty())
                                    })
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
