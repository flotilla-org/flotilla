#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use chrono::Utc;
use flotilla_protocol::{DaemonEvent, LeafAddress, LeafFire, WaitSubscriptionRequest};
use flotilla_resources::{
    admit_leaf, evaluate_leaf, Artifact, ArtifactLeafSubject, ChangeRequest, ChangeRequestLeafSubject, Convoy, ConvoyLeafSubject, Issue,
    IssueLeafSubject, LeafMaker, ResourceError, ResourceObject, ThreeValue, Usage, UsageLeafSubject, Vessel, VesselLeafSubject,
    WorkLeafSubject,
};

use super::routing::{ObjectAddress, RowDependencies};
use super::sources::{change_request_sources, freshest_change_requests, freshest_issues, issue_sources, LeafObservationStaleness};
use super::{EpisodeKeyFields, LeafFiringRecord, LeafSubscriptionRow, LeafSubscriptionTable, LeafWatcher};

pub(super) const LEAF_WATCH_RECOVERY_INITIAL_DELAY: Duration = Duration::from_millis(25);
pub(super) const LEAF_WATCH_RECOVERY_MAX_DELAY: Duration = Duration::from_secs(1);
pub(super) const LEAF_WATCH_RECOVERY_RESET_AFTER: Duration = Duration::from_secs(5);

// One immediate burst recovery, then repeated expiries back off from 25 ms
// to one second. Five healthy seconds restore immediate burst recovery.
#[derive(Default)]
pub(super) struct LeafWatchRecovery {
    pub(super) delay: Duration,
}

impl LeafWatchRecovery {
    pub(super) fn expired(&mut self, healthy_for: Duration) -> Duration {
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
        if let Err(error) = self.arm_dependencies(&row).await {
            self.finish(id).await;
            return Err(error);
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
            // Abort before taking the routing lock: watch registration may be
            // awaiting a store while holding it.
            if let Some(task) = self.inner.tasks.lock().await.remove(&id) {
                task.abort();
            }
            self.remove_routing(id).await;
            self.forget_firings(id).await;
            self.inner.change_requests.release(id).await;
            self.inner.issues.release(id).await;
        }
    }

    pub(super) async fn finish(&self, id: uuid::Uuid) {
        self.inner.rows.lock().await.remove(&id);
        self.remove_routing(id).await;
        self.forget_firings(id).await;
        self.inner.tasks.lock().await.remove(&id);
        self.inner.change_requests.release(id).await;
        self.inner.issues.release(id).await;
    }

    pub(super) async fn forget_firings(&self, id: uuid::Uuid) {
        self.inner.last_firings.lock().await.retain(|(subscription_id, _), _| *subscription_id != id);
        self.inner.unable_since.lock().await.remove(&id);
        self.inner.stale_attention_reported.lock().await.remove(&id);
    }

    // One dependency pass supplies both the store index and observation demand.
    pub(super) async fn arm_dependencies(&self, row: &LeafSubscriptionRow) -> Result<(), String> {
        let dependencies = RowDependencies::derive(row);
        for subject in &dependencies.change_requests {
            self.inner.change_requests.demand(row.id, subject.clone(), row.freshness_demand).await?;
        }
        for subject in &dependencies.issues {
            self.inner.issues.demand(row.id, subject.clone(), row.freshness_demand).await?;
        }
        self.inner.routing.lock().await.dependencies.insert(row.id, dependencies);
        Ok(())
    }

    pub(super) async fn watch_row(&self, row: LeafSubscriptionRow) -> Result<(), String> {
        let id = row.id;
        let result = self.watch_row_once(row).await.map_err(|error| error.to_string());
        self.remove_routing(id).await;
        result
    }

    pub(super) async fn watch_row_once(&self, row: LeafSubscriptionRow) -> Result<(), ResourceError> {
        let (dependencies, mut changes) = self.register_routing(&row).await?;
        let staleness = LeafObservationStaleness { change_request: self.change_request_stale_after(), issue: self.issue_stale_after() };
        let mut last_retry_deadline = None;
        loop {
            changes.borrow_and_update();
            let Some(current_row) = self.inner.rows.lock().await.get(&row.id).cloned() else { return Ok(()) };
            let subjects = self.load_subjects(&row.namespace, &dependencies.objects).await?;
            #[cfg(test)]
            self.inner.evaluations.fetch_add(1, Ordering::SeqCst);
            if let Some(fire) = evaluate_row(&current_row, &subjects.borrowed(), staleness).map_err(ResourceError::other)? {
                self.fire(row.id, fire).await;
                if !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }) {
                    return Ok(());
                }
            }
            // Delivery retry deadlines live in the convoy, an implicit dependency
            // indexed alongside the condition's concrete addresses.
            let retry_at = match &row.watcher {
                LeafWatcher::TurnDelivery { convoy, source, .. } => subjects
                    .convoys
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
                _ = tokio::time::sleep(retry_delay), if retry_at.is_some() => { last_retry_deadline = retry_at; }
                result = changes.changed() => {
                    if result.is_err() { return Ok(()) }
                }
            }
        }
    }

    async fn load_subjects(&self, namespace: &str, objects: &HashSet<ObjectAddress>) -> Result<OwnedLeafSubjects, ResourceError> {
        let mut subjects = OwnedLeafSubjects::default();
        #[cfg(test)]
        self.inner.snapshot_loads.fetch_add(1, Ordering::SeqCst);
        for address in objects {
            #[cfg(test)]
            self.inner.store_reads.fetch_add(1, Ordering::SeqCst);
            macro_rules! load {
                ($kind:ty, $field:ident) => {
                    match self.inner.backend.including_replicas::<$kind>(namespace).get(&address.name).await {
                        Ok(item) => {
                            subjects.$field.insert(address.name.clone(), item.object);
                        }
                        Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error),
                    }
                };
            }
            match address.kind {
                "Convoy" => load!(Convoy, convoys),
                "Vessel" => load!(Vessel, vessels),
                "Usage" => load!(Usage, usages),
                "Artifact" => load!(Artifact, artifacts),
                "ChangeRequest" => {
                    let copies = self.inner.backend.including_replicas::<ChangeRequest>(namespace).get_all(&address.name).await?;
                    subjects.change_requests.extend(freshest_change_requests(&change_request_sources(copies)));
                }
                "Issue" => {
                    let copies = self.inner.backend.including_replicas::<Issue>(namespace).get_all(&address.name).await?;
                    subjects.issues.extend(freshest_issues(&issue_sources(copies)));
                }
                _ => unreachable!("leaf resource kind"),
            }
        }
        Ok(subjects)
    }

    pub(super) async fn fire(&self, subscription_id: uuid::Uuid, mut fire: LeafFire) {
        fire.subscription_id = subscription_id;
        self.inner.last_firings.lock().await.insert(
            (subscription_id, fire.leaf.clone()),
            LeafFiringRecord { leaf: fire.leaf.clone(), value: fire.value.clone(), fired_at: Utc::now() },
        );
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
}
#[derive(Default)]
struct OwnedLeafSubjects {
    convoys: HashMap<String, ResourceObject<Convoy>>,
    vessels: HashMap<String, ResourceObject<Vessel>>,
    change_requests: HashMap<String, ResourceObject<ChangeRequest>>,
    usages: HashMap<String, ResourceObject<Usage>>,
    issues: HashMap<String, ResourceObject<Issue>>,
    artifacts: HashMap<String, ResourceObject<Artifact>>,
}

impl OwnedLeafSubjects {
    fn borrowed(&self) -> LeafSubjects<'_> {
        LeafSubjects {
            convoys: &self.convoys,
            vessels: &self.vessels,
            change_requests: &self.change_requests,
            usages: &self.usages,
            issues: &self.issues,
            artifacts: &self.artifacts,
        }
    }
}

pub(super) struct LeafSubjects<'a> {
    pub(super) convoys: &'a HashMap<String, ResourceObject<Convoy>>,
    pub(super) vessels: &'a HashMap<String, ResourceObject<Vessel>>,
    pub(super) change_requests: &'a HashMap<String, ResourceObject<ChangeRequest>>,
    pub(super) usages: &'a HashMap<String, ResourceObject<Usage>>,
    pub(super) issues: &'a HashMap<String, ResourceObject<Issue>>,
    pub(super) artifacts: &'a HashMap<String, ResourceObject<Artifact>>,
}

pub(super) fn evaluate_row(
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
