use std::{
    collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_change_request::observation::{ChangeRequestKey, ChangeRequestObservationSource};
use flotilla_protocol::{IssueSource, ReferenceContext, Relationship, SubjectKind as ConvoySubjectKind};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    address::{stored_spelling_names, subject_address},
    checkout_records::{CheckoutChange, CheckoutHolder, Launches},
    convoy_subject_rows,
    lifecycle_reads::LifecycleReads,
    merge_change_request_history, retain_change_request, select_change_requests, ChangeRequest, ChangeRequestSpec, ChangeRequestStatus,
    Convoy, ConvoyPhase, Forge, ForgeSpec, InputMeta, ObservedChangeRequestState, ObservedChecks, ResourceError, ResourceObject,
    ResourceProvenance,
};
use flotilla_store::{
    load_metrics::{attributed, caller, carry},
    watch_resource_kind_including_replicas, ResourceBackend, StoreQuery,
};

use crate::engine_observers::EngineObservers;

const KIND: &str = "ChangeRequest";
/// The caller a subject's refresh loop reads as.
const REFRESHER: &str = "change request refresh";
#[cfg(test)]
use flotilla_store_http::HttpBackend;
#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use futures::{FutureExt, StreamExt};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

const RETAINED_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// How often a live convoy's branch is looked up on its forge while the convoy produces no change
/// request: the forge observer's edge probe (#3108). The webhook relay's change request hints are
/// to replace this cadence.
const BRANCH_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);

// Deadline arithmetic on the judge path must remain total even for a cadence
// outside chrono's duration range or an observation near its final timestamp.
fn cadence_deadline(at: DateTime<Utc>, cadence: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(cadence).ok().and_then(|cadence| at.checked_add_signed(cadence)).unwrap_or(DateTime::<Utc>::MAX_UTC)
}

#[derive(Debug, Clone, Copy)]
pub struct ChangeRequestRefreshCadence {
    pub state: Duration,
    pub checks_pending: Duration,
    pub freshness_demanded: Duration,
    pub stale_after: Duration,
}

impl Default for ChangeRequestRefreshCadence {
    fn default() -> Self {
        Self {
            state: Duration::from_secs(90),
            checks_pending: Duration::from_secs(15),
            freshness_demanded: Duration::from_secs(10),
            stale_after: Duration::from_secs(180),
        }
    }
}

/// Looks `branch` of `repository` up on its forge and records the change requests found on their
/// records (#3108). Finding none records nothing; an error is a lookup that did not complete.
#[async_trait::async_trait]
pub trait BranchProbe: Send + Sync {
    async fn probe(&self, repository: &Address, branch: &str) -> Result<(), String>;
}

/// The change requests the convoys demand, as the convoy demand watch holds them.
///
/// An ended convoy wants its change requests while it holds a checkout not yet released: that
/// release is judged against them. Which ended convoys do is read by holder once per convoy, and
/// again only as one of its checkouts changes.
#[derive(Debug, Default)]
pub struct ConvoyDemands {
    /// Each change request wanted, with its demand.
    demands: HashMap<ChangeRequestKey, uuid::Uuid>,
    /// Each ended convoy, by launch, holding checkouts not yet released: their launches when last
    /// read. A change to one of them reads the convoy's again.
    holding: HashMap<String, BTreeSet<String>>,
    /// Each ended convoy, by launch, found holding no checkout still to be released. Nothing is
    /// held for an ended convoy again, so it is not read again.
    released: HashSet<String>,
}

impl ConvoyDemands {
    /// The change requests demanded.
    pub fn demanded(&self) -> BTreeSet<&ChangeRequestKey> {
        self.demands.keys().collect()
    }
}

/// What woke the convoy demand watch.
enum DemandEvent {
    /// A convoy, or what resolves its change requests, changed.
    Convoys,
    /// A checkout changed: an ended convoy may have had its last checkout released.
    Checkout(CheckoutChange),
}

struct ActiveRefresh {
    demands: HashMap<uuid::Uuid, Option<DateTime<Utc>>>,
    wake: Arc<Notify>,
    task: JoinHandle<()>,
    next_refresh_at: DateTime<Utc>,
    scheduled_cadence: Duration,
}

#[derive(Clone)]
pub struct ChangeRequestRefresher {
    inner: Arc<ChangeRequestRefresherInner>,
}

struct ChangeRequestRefresherInner {
    backend: ResourceBackend,
    authority: String,
    source: Arc<dyn ChangeRequestObservationSource>,
    cadence: ChangeRequestRefreshCadence,
    active: Mutex<HashMap<ChangeRequestKey, ActiveRefresh>>,
    observation_errors: Mutex<HashMap<ChangeRequestKey, ObservationError>>,
    subject_locks: Mutex<HashMap<ChangeRequestKey, Weak<Mutex<()>>>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
    grace_schedule_reads: std::sync::atomic::AtomicUsize,
    engine: EngineObservers,
    branch_probe: std::sync::OnceLock<Arc<dyn BranchProbe>>,
    /// The convoy branches this observer looks up, each with when its next lookup is due.
    branch_schedule: std::sync::Mutex<HashMap<(Address, String), tokio::time::Instant>>,
    /// Wakes the branch scheduler when a branch is added.
    branch_wake: Arc<Notify>,
    /// The one task that runs the branch lookups as they fall due; it ends with the observer.
    branch_scheduler: std::sync::OnceLock<tokio_util::task::AbortOnDropHandle<()>>,
    /// The engine's checkouts, by which a convoy's demand is held until its checkouts are
    /// released; without them a convoy's demand ends with the convoy.
    lifecycle: std::sync::OnceLock<Arc<dyn LifecycleReads>>,
}

impl ChangeRequestRefresher {
    pub fn new(
        backend: ResourceBackend,
        authority: String,
        source: Arc<dyn ChangeRequestObservationSource>,
        cadence: ChangeRequestRefreshCadence,
    ) -> Self {
        Self {
            inner: Arc::new(ChangeRequestRefresherInner {
                backend,
                authority,
                source,
                cadence,
                active: Mutex::new(HashMap::new()),
                observation_errors: Mutex::new(HashMap::new()),
                subject_locks: Mutex::new(HashMap::new()),
                relay_healthy: std::sync::atomic::AtomicBool::new(false),
                relay_wake: Notify::new(),
                grace_schedule_reads: Default::default(),
                engine: EngineObservers::default(),
                branch_probe: std::sync::OnceLock::new(),
                branch_schedule: std::sync::Mutex::new(HashMap::new()),
                branch_wake: Arc::new(Notify::new()),
                branch_scheduler: std::sync::OnceLock::new(),
                lifecycle: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Installs the forge lookup the branch probes use, once the daemon that serves it is composed.
    pub fn install_branch_probe(&self, probe: Arc<dyn BranchProbe>) -> Result<(), String> {
        self.inner.branch_probe.set(probe).map_err(|_| "the branch probe is already installed".to_owned())?;
        let scheduler = tokio::spawn(attributed(
            caller("branch probes"),
            run_branch_probes(Arc::downgrade(&self.inner), Arc::clone(&self.inner.branch_wake)),
        ));
        let _ = self.inner.branch_scheduler.set(tokio_util::task::AbortOnDropHandle::new(scheduler));
        Ok(())
    }

    /// Convoys are durable observation demand, including those homed on peers.
    /// Subscribe before listing so startup and watch recovery cannot lose demand.
    /// Only the source owner arms refreshes; forge reads stay in the ordinary
    /// observer loop and observations reach consumers through store replication.
    ///
    /// A convoy's demand outlives the convoy while it holds a checkout not yet released: that
    /// release is judged against the change request, so its observation must still be there.
    pub async fn watch_convoy_demands(&self, namespace: &str) {
        // Keep demands across resubscriptions: only a successful re-list can
        // prove that a consumer disappeared and release its standing demand.
        let mut demands = ConvoyDemands::default();
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let mut streams = Vec::new();
            // Subscribed before the convoys are listed, so a release between the two is not missed.
            if let Some(changes) = self.checkout_changes() {
                streams.push(checkout_events(changes));
            }
            let mut subscription_error = None;
            for kind in ["Convoy", "Project", "Repository", "Host", "Forge"] {
                match watch_resource_kind_including_replicas(&self.inner.backend, namespace, kind).await {
                    Ok(watch) => streams.push(
                        watch
                            .stream
                            .map(|event| event.map(|_| DemandEvent::Convoys))
                            .chain(futures::stream::once(
                                async move { Err(ResourceError::other(format!("{kind} convoy demand watch closed"))) },
                            ))
                            .boxed(),
                    ),
                    Err(error) => {
                        subscription_error = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = subscription_error {
                tracing::warn!(%error, "change request convoy demand watch subscription failed");
            } else {
                let mut events = futures::stream::select_all(streams);
                // A fresh subscription may have missed anything: every holding convoy's checkouts
                // are read again, and every convoy is listed.
                let mut convoys_changed = true;
                let mut checkout_changes = vec![CheckoutChange::Rescanned];
                'reconcile: loop {
                    // A convoy whose last checkout was released wants its change requests no more.
                    if !checkout_changes.is_empty() && self.recheck_holding(&mut demands, &checkout_changes).await {
                        convoys_changed = true;
                    }
                    if convoys_changed {
                        if let Err(error) = self.reconcile_convoy_demands(namespace, &mut demands).await {
                            tracing::warn!(%error, "change request convoy demand reconciliation failed");
                            break;
                        }
                    }
                    retry_delay = Duration::from_secs(1);
                    convoys_changed = false;
                    checkout_changes.clear();
                    match events.next().await {
                        Some(Ok(event)) => {
                            absorb(event, &demands, &mut convoys_changed, &mut checkout_changes);
                            // Collapse bursts into one re-list without delaying isolated
                            // changes. Bound the drain so a busy source cannot starve it.
                            for _ in 0..64 {
                                match events.next().now_or_never() {
                                    Some(Some(Ok(event))) => absorb(event, &demands, &mut convoys_changed, &mut checkout_changes),
                                    Some(Some(Err(error))) => {
                                        tracing::warn!(%error, "change request convoy demand watch failed; relisting");
                                        break 'reconcile;
                                    }
                                    _ => break,
                                }
                            }
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "change request convoy demand watch failed; relisting");
                            break;
                        }
                        None => break,
                    }
                }
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
        }
    }

    /// Installs the engine's checkout reads, by which a convoy's change request stays demanded
    /// until the checkouts the convoy holds are released.
    pub fn install_lifecycle_reads(&self, reads: Arc<dyn LifecycleReads>) -> Result<(), String> {
        self.inner.lifecycle.set(reads).map_err(|_| "the change request observer's lifecycle reads are already installed".to_owned())
    }

    /// The checkouts' changes from now on, where the engine's checkouts are read.
    fn checkout_changes(&self) -> Option<tokio::sync::broadcast::Receiver<CheckoutChange>> {
        self.inner.lifecycle.get().and_then(|reads| reads.checkouts().ok()).map(|checkouts| checkouts.changes())
    }

    /// The launches held for convoy `convoy` (by launch) that have not ended, read by holder; none
    /// where the engine's checkouts are not read.
    async fn unreleased_launches(&self, convoy: &str) -> Result<BTreeSet<String>, String> {
        let Some(reads) = self.inner.lifecycle.get().cloned() else { return Ok(BTreeSet::new()) };
        let holder = CheckoutHolder::Convoy(convoy.to_owned());
        tokio::task::spawn_blocking(carry(move || {
            // A port that serves no checkouts holds none for the convoy.
            let Ok(checkouts) = reads.checkouts() else { return Ok(BTreeSet::new()) };
            Ok(checkouts
                .launches(Launches::Held(&holder))?
                .into_iter()
                .filter(|launch| !launch.ended())
                .map(|launch| launch.name)
                .collect())
        }))
        .await
        .map_err(|error| error.to_string())?
    }

    /// Reads again the checkouts of each holding convoy that `changes` may concern. True when one
    /// of them now holds none to be released, so wants its change requests no more.
    async fn recheck_holding(&self, demands: &mut ConvoyDemands, changes: &[CheckoutChange]) -> bool {
        let rescanned = changes.iter().any(|change| matches!(change, CheckoutChange::Rescanned));
        let changed: HashSet<&str> = changes
            .iter()
            .filter_map(|change| match change {
                CheckoutChange::Changed { name, .. } => Some(name.as_str()),
                CheckoutChange::Rescanned => None,
            })
            .collect();
        let concerned = demands
            .holding
            .iter()
            .filter(|(_, launches)| rescanned || launches.iter().any(|launch| changed.contains(launch.as_str())))
            .map(|(convoy, _)| convoy.clone())
            .collect::<Vec<_>>();
        let mut released = false;
        for convoy in concerned {
            match self.unreleased_launches(&convoy).await {
                Ok(launches) if !launches.is_empty() => {
                    demands.holding.insert(convoy, launches);
                }
                Ok(_) => {
                    demands.holding.remove(&convoy);
                    demands.released.insert(convoy);
                    released = true;
                }
                // Kept as it was: the next change to one of its checkouts reads it again.
                Err(error) => tracing::warn!(%convoy, %error, "an ended convoy's checkouts could not be read"),
            }
        }
        released
    }

    /// Whether ended convoy `convoy` (by launch) holds a checkout still to be released: memoised
    /// in `demands`, read by holder only the first time it is asked.
    async fn holds_unreleased(&self, demands: &mut ConvoyDemands, convoy: &str) -> bool {
        if demands.holding.contains_key(convoy) {
            return true;
        }
        if demands.released.contains(convoy) {
            return false;
        }
        match self.unreleased_launches(convoy).await {
            Ok(launches) if !launches.is_empty() => {
                demands.holding.insert(convoy.to_owned(), launches);
                true
            }
            Ok(_) => {
                demands.released.insert(convoy.to_owned());
                false
            }
            Err(error) => {
                // Held, not memoised: the next pass reads it again.
                tracing::warn!(%convoy, %error, "an ended convoy's checkouts could not be read; its change requests are held meanwhile");
                true
            }
        }
    }

    async fn reconcile_convoy_demands(&self, namespace: &str, demands: &mut ConvoyDemands) -> Result<(), String> {
        let cleanup_result = self.release_unlinked_history(namespace).await;
        if let Err(error) = &cleanup_result {
            tracing::warn!(%error, "release unlinked change request history failed");
        }
        let convoys = self
            .inner
            .backend
            .query_declared::<Convoy>(namespace, &flotilla_store::live_convoys_query())
            .await
            .map_err(|error| error.to_string())?;
        let forges = self.forges(namespace).await?;
        let mut wanted = HashSet::new();
        let mut owners = HashMap::new();
        let mut branches = HashSet::new();
        for copy in &convoys.items {
            branches.extend(unproduced_branches(&copy.object, &forges));
        }
        self.reconcile_branch_probes(namespace, branches).await?;
        let mut addresses = Vec::new();
        for copy in convoys.items {
            addresses.extend(change_request_subjects(&copy.object, &forges));
        }
        // A landed convoy can hold only the engine's checkouts; without them it holds nothing, and
        // the old store's retained landed convoys are not read.
        let ended_query = match self.inner.lifecycle.get() {
            Some(_) => flotilla_store::ended_convoys_query(),
            None => flotilla_store::abandoned_convoys_query(),
        };
        let ended = self.inner.backend.query_declared::<Convoy>(namespace, &ended_query).await.map_err(|error| error.to_string())?;
        let mut still_held = HashSet::new();
        for copy in ended.items {
            let convoy = copy.object.metadata.name.clone();
            // An abandoned convoy still held is observed for the change requests its Submitted pr
            // promises name, until each resolves (#3108 ruling 1): the resolution releases the demand.
            if let Some(status) = copy.object.status.as_ref().filter(|status| status.phase != ConvoyPhase::Landed) {
                for address in flotilla_resources::convoy::promises::ended_submissions(status, &forges) {
                    addresses.push((convoy.clone(), address));
                }
            }
            // Any ended convoy is observed for its change requests while it holds a checkout still
            // to be released: that release is judged against them.
            let subjects = change_request_subjects(&copy.object, &forges);
            if !subjects.is_empty() && self.holds_unreleased(demands, &convoy).await {
                addresses.extend(subjects);
            }
            still_held.insert(convoy);
        }
        // A convoy no longer listed has retired.
        demands.holding.retain(|convoy, _| still_held.contains(convoy));
        demands.released.retain(|convoy| still_held.contains(convoy));
        for (convoy, address) in addresses {
            let subject = match ChangeRequestKey::new(namespace, address) {
                Ok(subject) => subject,
                Err(error) => {
                    tracing::debug!(%convoy, %error, "convoy change request subject has no observation key");
                    continue;
                }
            };
            let source = stored_source(&subject);
            let owned = if let Some(owned) = owners.get(&source) {
                *owned
            } else {
                let owned = crate::forge_observation::source_owner(&self.inner.backend, namespace, &source)
                    .await?
                    .is_some_and(|owner| owner.to_string() == self.inner.authority);
                owners.insert(source, owned);
                owned
            };
            if owned {
                wanted.insert(subject);
            }
        }
        for subject in &wanted {
            if let Entry::Vacant(entry) = demands.demands.entry(subject.clone()) {
                let id = uuid::Uuid::new_v4();
                self.demand(id, subject.clone(), None).await?;
                entry.insert(id);
            }
        }
        let released = demands.demands.keys().filter(|subject| !wanted.contains(*subject)).cloned().collect::<Vec<_>>();
        for subject in released {
            if let Some(id) = demands.demands.remove(&subject) {
                self.release(id).await;
            }
        }
        // Complete independent demand updates before retrying failed cleanup.
        cleanup_result
    }

    /// Schedules each wanted branch whose repository this observer observes, a new one due at once,
    /// and drops the branches no longer wanted.
    async fn reconcile_branch_probes(&self, namespace: &str, wanted: HashSet<(Address, String)>) -> Result<(), String> {
        if self.inner.branch_probe.get().is_none() {
            return Ok(());
        }
        let mut owned = HashSet::new();
        let mut owners = HashMap::new();
        for (repository, branch) in wanted {
            let Some((service, scope)) = flotilla_resources::address::stored_service_scope(&repository) else { continue };
            let source = IssueSource { service, scope };
            let observes = match owners.get(&source) {
                Some(observes) => *observes,
                None => {
                    let observes = crate::forge_observation::owns_source(&self.inner.backend, namespace, &source).await?;
                    owners.insert(source, observes);
                    observes
                }
            };
            if observes {
                owned.insert((repository, branch));
            }
        }
        let mut schedule = self.inner.branch_schedule.lock().expect("branch schedule");
        schedule.retain(|key, _| owned.contains(key));
        let mut added = false;
        for key in owned {
            if let Entry::Vacant(entry) = schedule.entry(key) {
                entry.insert(tokio::time::Instant::now());
                added = true;
            }
        }
        if added {
            self.inner.branch_wake.notify_one();
        }
        Ok(())
    }

    /// Release only explicit unlink assertions. Missing convoys retain history.
    pub async fn release_unlinked_history(&self, namespace: &str) -> Result<(), String> {
        let records = self.inner.backend.using::<ChangeRequest>(namespace);
        let inventory = records.list().await.map_err(|error| error.to_string())?;
        if inventory.items.iter().all(|record| record.spec.subject_of.is_empty()) {
            return Ok(());
        }
        let forges = self.forges(namespace).await?;
        let convoys = self.inner.backend.including_replicas::<Convoy>(namespace).list().await.map_err(|e| e.to_string())?;
        let mut cleanup_error = None;
        for record in inventory.items {
            if record.spec.subject_of.is_empty() {
                continue;
            }
            let subject = match record.spec.address(&forges).and_then(|address| ChangeRequestKey::new(namespace, address)) {
                Ok(subject) => subject,
                Err(error) => {
                    tracing::debug!(record = %record.metadata.name, %error, "change request history has no address; unlink not processed");
                    continue;
                }
            };
            let result: Result<(), String> = async {
                let lock = self.subject_lock(&subject).await;
                let _guard = lock.lock().await;
                loop {
                    let current = match records.get(&record.metadata.name).await {
                        Ok(current) => current,
                        Err(ResourceError::NotFound { .. }) => break,
                        Err(error) => return Err(error.to_string()),
                    };
                    let mut spec = current.spec.clone();
                    spec.subject_of.retain(|entry| {
                        !convoys.items.iter().any(|copy| {
                            let convoy = &copy.object;
                            convoy.metadata.namespace == entry.namespace
                                && convoy.metadata.name == entry.convoy
                                && convoy.status.as_ref().is_some_and(|status| {
                                    status.unlinked_subjects.iter().any(|unlinked| {
                                        unlinked.kind == ConvoySubjectKind::ChangeRequest
                                            && subject_address(unlinked, &forges).is_ok_and(|address| &address == subject.address())
                                    })
                                })
                        })
                    });
                    if spec == current.spec {
                        break;
                    }
                    let demanded = self.inner.active.lock().await.get(&subject).is_some_and(|refresh| !refresh.demands.is_empty());
                    if !demanded && spec.subject_of.is_empty() {
                        // Delete before dropping the final history entry. A failed
                        // deletion leaves that entry available for the next retry.
                        let held_elsewhere = self.subject_held_elsewhere(&subject, &current.metadata.name).await?;
                        records.delete(&current.metadata.name).await.map_err(|e| e.to_string())?;
                        if !held_elsewhere {
                            if let Some(refresh) = self.inner.active.lock().await.remove(&subject) {
                                refresh.task.abort();
                            }
                            self.inner.observation_errors.lock().await.remove(&subject);
                        }
                        break;
                    }
                    match records.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await {
                        Ok(_) => break,
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(error.to_string()),
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(record = %record.metadata.name, %error, "release unlinked history record failed");
                cleanup_error.get_or_insert(error);
            }
        }
        cleanup_error.map_or(Ok(()), Err)
    }

    // Check aliases before deletion so a read failure leaves retryable history.
    // Unrelated history-less records are never deleted by unlink reconciliation.
    async fn subject_held_elsewhere(&self, subject: &ChangeRequestKey, name: &str) -> Result<bool, String> {
        let records = self.inner.backend.using::<ChangeRequest>(subject.namespace()).list().await.map_err(|e| e.to_string())?;
        Ok(records
            .items
            .iter()
            .any(|record| record.metadata.name != name && spec_names(&record.spec, subject) && !record.spec.subject_of.is_empty()))
    }

    async fn subject_lock(&self, subject: &ChangeRequestKey) -> Arc<Mutex<()>> {
        let mut locks = self.inner.subject_locks.lock().await;
        if let Some(lock) = locks.get(subject).and_then(Weak::upgrade) {
            return lock;
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(Mutex::new(()));
        locks.insert(subject.clone(), Arc::downgrade(&lock));
        lock
    }

    pub fn stale_after(&self) -> Duration {
        self.inner.cadence.stale_after
    }

    /// The engine's observation records, which composition installs behind `[primitives]`.
    pub fn engine(&self) -> &EngineObservers {
        &self.inner.engine
    }

    /// Deletes this home's records nothing demands: under the engine, records outlive a restart
    /// and their demand returns within one lease, after which the rest are orphans.
    pub async fn sweep_undemanded(&self) -> Result<(), String> {
        let namespaces = self.inner.backend.local_namespaces::<ChangeRequest>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            let records = self.inner.backend.using::<ChangeRequest>(&namespace);
            let forges = self.forges(&namespace).await?;
            for record in records.list().await.map_err(|error| error.to_string())?.items {
                let Ok(subject) = record.spec.address(&forges).and_then(|address| ChangeRequestKey::new(namespace.clone(), address)) else {
                    continue;
                };
                let lock = self.subject_lock(&subject).await;
                let _guard = lock.lock().await;
                if self.inner.active.lock().await.contains_key(&subject) {
                    continue;
                }
                match records.delete(&record.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    /// A consumer gets the observer's next scheduled refresh plus one cadence.
    /// Missing local demand (for example a remote owner) starts with the ordinary
    /// configured cadence; no judge-specific fixed delay is invented.
    pub async fn refresh_grace_deadline(
        &self,
        subject: &ChangeRequestKey,
        freshness: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
    ) -> DateTime<Utc> {
        self.inner.grace_schedule_reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let active = self.inner.active.lock().await;
        let (next, cadence) = active.get(subject).map_or_else(
            || {
                let cadence = if freshness.is_some() { self.inner.cadence.freshness_demanded } else { self.inner.cadence.state };
                (cadence_deadline(created_at, cadence), cadence)
            },
            |refresh| (refresh.next_refresh_at, refresh.scheduled_cadence),
        );
        cadence_deadline(next, cadence)
    }

    /// Number of grace schedule lookups, for observation diagnostics.
    pub fn grace_schedule_reads(&self) -> usize {
        self.inner.grace_schedule_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn retry_grace_deadline(&self, retry_at: DateTime<Utc>) -> DateTime<Utc> {
        cadence_deadline(retry_at, self.inner.cadence.checks_pending)
    }

    pub async fn observation_error(&self, subject: &ChangeRequestKey) -> Option<ObservationError> {
        self.inner.observation_errors.lock().await.get(subject).cloned()
    }

    /// The namespace's declared Forges, which resolve a host-spelled service.
    async fn forges(&self, namespace: &str) -> Result<Vec<ForgeSpec>, String> {
        let forges = self.inner.backend.definitions::<Forge>(namespace).list().await.map_err(|error| error.to_string())?;
        Ok(forges.into_iter().map(|forge| forge.spec).collect())
    }

    /// Refresh a claim-time observation even before Landing has armed its
    /// standing leaf subscriptions.
    pub async fn refresh_once(&self, subject: &ChangeRequestKey) -> Result<(), ObservationError> {
        self.refresh_once_with_creation(subject, true).await
    }

    async fn selected_merged(&self, subject: &ChangeRequestKey) -> Result<bool, ObservationError> {
        // Numeric identity is indexed; keep spelling/alias selection in spec_names.
        let sources = self
            .inner
            .backend
            .query_declared::<ChangeRequest>(
                subject.namespace(),
                &StoreQuery {
                    fields: BTreeMap::from([("spec.number".into(), subject.number().to_string())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| ObservationError::Failed(error.to_string()))?;
        let selected = select_change_requests(sources.items.iter().map(|source| &source.object));
        Ok(selected.values().any(|record| {
            spec_names(&record.spec, subject)
                && record.status.as_ref().is_some_and(|status| status.state.value == Some(ObservedChangeRequestState::Merged))
        }))
    }

    async fn refresh_once_with_creation(&self, subject: &ChangeRequestKey, create_missing: bool) -> Result<(), ObservationError> {
        if self.selected_merged(subject).await? {
            self.inner.observation_errors.lock().await.remove(subject);
            return Ok(());
        }
        let record_name = subject.record_name();
        let lock = self.subject_lock(subject).await;
        let _guard = lock.lock().await;
        // Ownership and publication use the resource store's string errors, not
        // forge observations. Tag them explicitly as ordinary failures.
        if !self.owns_record(subject, false, create_missing).await.map_err(ObservationError::Failed)? {
            return Ok(());
        }
        let status = if create_missing {
            self.inner.source.observe_for_completion(subject).await
        } else {
            let group = self
                .inner
                .active
                .lock()
                .await
                .iter()
                .filter(|(candidate, refresh)| same_repository(candidate, subject) && !refresh.demands.is_empty())
                .map(|(candidate, _)| candidate.clone())
                .collect::<Vec<_>>();
            self.inner.source.observe_group(&group, subject).await
        };
        let status = match status {
            Ok(status) => {
                self.inner.observation_errors.lock().await.remove(subject);
                status
            }
            Err(error) => {
                self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                self.inner.engine.failed(KIND, &record_name).await;
                return Err(error);
            }
        };
        self.publish(subject, &record_name, status, true, false, create_missing).await.map_err(ObservationError::Failed)
    }

    pub async fn demand(
        &self,
        subscription_id: uuid::Uuid,
        subject: ChangeRequestKey,
        freshness: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let lock = self.subject_lock(&subject).await;
        let _guard = lock.lock().await;
        let mut active = self.inner.active.lock().await;
        if let Some(refresh) = active.get_mut(&subject) {
            let was_retained = refresh.demands.is_empty();
            refresh.demands.insert(subscription_id, freshness);
            if freshness.is_some() || was_retained {
                refresh.next_refresh_at = Utc::now();
                refresh.scheduled_cadence = self.inner.cadence.freshness_demanded;
                refresh.wake.notify_one();
            }
            return Ok(());
        }
        let this = self.clone();
        let task_subject = subject.clone();
        let task = tokio::spawn(attributed(caller(REFRESHER), async move { this.refresh_loop(task_subject).await }));
        active.insert(
            subject,
            ActiveRefresh {
                demands: HashMap::from([(subscription_id, freshness)]),
                wake: Arc::new(Notify::new()),
                task,
                next_refresh_at: Utc::now(),
                scheduled_cadence: self.inner.cadence.freshness_demanded,
            },
        );
        Ok(())
    }

    pub async fn release(&self, subscription_id: uuid::Uuid) {
        let mut active = self.inner.active.lock().await;
        let empty = active
            .iter_mut()
            .filter_map(|(subject, refresh)| {
                let removed = refresh.demands.remove(&subscription_id).is_some();
                (removed && refresh.demands.is_empty()).then_some(subject.clone())
            })
            .collect::<Vec<_>>();
        let stopped =
            empty.into_iter().filter_map(|subject| active.remove(&subject).map(|refresh| (subject, refresh.task))).collect::<Vec<_>>();
        drop(active);

        for (subject, task) in stopped {
            task.abort();
            let _ = task.await;
            let lock = self.subject_lock(&subject).await;
            let _guard = lock.lock().await;
            if self.inner.active.lock().await.contains_key(&subject) {
                continue;
            }
            self.inner.observation_errors.lock().await.remove(&subject);
            let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
            let result = match records.get(&subject.record_name()).await {
                Ok(record) if retain_change_request(&record) => {
                    self.start_retained(subject.clone()).await;
                    continue;
                }
                Ok(record)
                    if record.spec.observing_authority == self.inner.authority
                        || record.spec.subject_of.iter().any(|entry| entry.relationship == Relationship::Produces) =>
                {
                    self.collect_local_subject(&subject).await
                }
                Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if !matches!(error, ResourceError::NotFound { .. }) {
                    tracing::warn!(change_request = %subject.address(), %error, "garbage collect undemanded change request failed");
                }
            }
        }
    }

    /// The relay only acts on live demand, and the same authority check used by
    /// refresh_once is repeated just before the forge read.
    pub async fn demanded_owned(&self) -> Result<Vec<ChangeRequestKey>, String> {
        let subjects = self
            .inner
            .active
            .lock()
            .await
            .iter()
            .filter(|(_, refresh)| !refresh.demands.is_empty())
            .map(|(subject, _)| subject.clone())
            .collect::<Vec<_>>();
        let mut owned = Vec::new();
        for subject in subjects {
            if self.owns_record(&subject, false, false).await? {
                owned.push(subject);
            }
        }
        Ok(owned)
    }

    /// Match a relay address across all namespaces with live demand. The wire
    /// subject has no namespace, while one daemon can serve several.
    pub async fn refresh_hint(&self, hint: &Address) -> Result<(), String> {
        if hint.kind() != Kind::ChangeRequest {
            return Ok(());
        }
        let subjects = self
            .inner
            .active
            .lock()
            .await
            .iter()
            .filter(|(subject, refresh)| !refresh.demands.is_empty() && subject.address() == hint)
            .map(|(subject, _)| subject.clone())
            .collect::<Vec<_>>();
        let mut first_error = None;
        for subject in subjects {
            if let Err(error) = self.refresh_once_with_creation(&subject, false).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.to_string()))
    }

    pub async fn refresh_demanded_owned(&self) -> Result<(), String> {
        let mut first_error = None;
        for subject in self.demanded_owned().await? {
            if let Err(error) = self.refresh_once_with_creation(&subject, false).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(error.to_string()))
    }

    pub fn set_relay_healthy(&self, healthy: bool) {
        use std::sync::atomic::Ordering;
        if self.inner.relay_healthy.swap(healthy, Ordering::SeqCst) != healthy {
            self.inner.relay_wake.notify_waiters();
        }
    }

    /// Recover produced requests after restart; ordinary demand-only observations
    /// are collected. Retention has no time expiry while the forge is open:
    /// one task per subject, polling at most once every fifteen minutes.
    pub async fn garbage_collect_orphans(&self) -> Result<(), String> {
        let namespaces = self.inner.backend.local_namespaces::<ChangeRequest>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            self.release_unlinked_history(&namespace).await?;
            let records = self.inner.backend.using::<ChangeRequest>(&namespace);
            let forges = self.forges(&namespace).await?;
            for record in records.list().await.map_err(|error| error.to_string())?.items {
                if retain_change_request(&record) {
                    match record.spec.address(&forges).and_then(|address| ChangeRequestKey::new(namespace.clone(), address)) {
                        Ok(subject) => self.start_retained(subject).await,
                        Err(error) => tracing::warn!(record = %record.metadata.name, %error, "retained change request has no address"),
                    }
                } else {
                    records.delete(&record.metadata.name).await.map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    async fn collect_retained_if_terminal(&self, subject: &ChangeRequestKey) -> Result<bool, ResourceError> {
        let lock = self.subject_lock(subject).await;
        let _guard = lock.lock().await;
        let retained = self.inner.active.lock().await.get(subject).is_some_and(|refresh| refresh.demands.is_empty());
        if !retained {
            return Ok(false);
        }
        let sources = self.inner.backend.including_replicas::<ChangeRequest>(subject.namespace()).list().await?;
        let selected = select_change_requests(sources.items.iter().map(|source| &source.object));
        let terminal = selected.values().find(|record| spec_names(&record.spec, subject)).is_some_and(|record| {
            record.status.as_ref().is_some_and(|status| {
                matches!(status.state.value, Some(ObservedChangeRequestState::Merged | ObservedChangeRequestState::Closed))
            })
        });
        if !terminal {
            return Ok(false);
        }
        self.collect_local_subject(subject).await?;
        self.inner.active.lock().await.remove(subject);
        Ok(true)
    }

    async fn collect_local_subject(&self, subject: &ChangeRequestKey) -> Result<(), ResourceError> {
        let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
        for record in records.list().await?.items.iter().filter(|record| spec_names(&record.spec, subject)) {
            match records.delete(&record.metadata.name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn start_retained(&self, subject: ChangeRequestKey) {
        let mut active = self.inner.active.lock().await;
        if active.contains_key(&subject) {
            return;
        }
        let this = self.clone();
        let task_subject = subject.clone();
        let task = tokio::spawn(attributed(caller(REFRESHER), async move { this.refresh_loop(task_subject).await }));
        active.insert(
            subject,
            ActiveRefresh {
                demands: HashMap::new(),
                wake: Arc::new(Notify::new()),
                task,
                next_refresh_at: Utc::now(),
                scheduled_cadence: RETAINED_REFRESH_INTERVAL,
            },
        );
    }

    /// Number of currently retained observation demands.
    pub async fn active_demands(&self) -> usize {
        self.inner.active.lock().await.values().map(|refresh| refresh.demands.len()).sum()
    }

    async fn refresh_delay(&self, subject: &ChangeRequestKey, demanded_delay: Duration) -> Duration {
        if self.inner.active.lock().await.get(subject).is_some_and(|refresh| refresh.demands.is_empty()) {
            RETAINED_REFRESH_INTERVAL
        } else {
            demanded_delay
        }
    }

    async fn refresh_loop(&self, subject: ChangeRequestKey) {
        let record_name = subject.record_name();
        loop {
            match self.collect_retained_if_terminal(&subject).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(change_request = %subject.address(), %error, "collect retained change request failed")
                }
            }
            match self.selected_merged(&subject).await {
                Ok(true) => {
                    self.inner.observation_errors.lock().await.remove(&subject);
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error);
                    if !self.wait_for_next(&subject, self.inner.cadence.checks_pending, self.inner.cadence.checks_pending).await {
                        return;
                    }
                    continue;
                }
            }
            match self.owns_record(&subject, true, true).await {
                Ok(false) => {
                    if !self
                        .wait_for_next(&subject, self.refresh_delay(&subject, self.inner.cadence.state).await, self.inner.cadence.state)
                        .await
                    {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), ObservationError::Failed(error));
                    if !self
                        .wait_for_next(
                            &subject,
                            self.refresh_delay(&subject, self.inner.cadence.checks_pending).await,
                            self.inner.cadence.checks_pending,
                        )
                        .await
                    {
                        break;
                    }
                    continue;
                }
                Ok(true) => {}
            }
            let group = self
                .inner
                .active
                .lock()
                .await
                .iter()
                .filter(|(candidate, refresh)| same_repository(candidate, &subject) && !refresh.demands.is_empty())
                .map(|(candidate, _)| candidate.clone())
                .collect::<Vec<_>>();
            match self.inner.source.observe_group(&group, &subject).await {
                Ok(status) => {
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    let lock = self.subject_lock(&subject).await;
                    let guard = lock.lock().await;
                    if !self.inner.active.lock().await.contains_key(&subject) {
                        return;
                    }
                    if let Err(error) = self.publish(&subject, &record_name, status.clone(), demanded, true, true).await {
                        self.inner.observation_errors.lock().await.insert(subject.clone(), ObservationError::Failed(error.clone()));
                        tracing::warn!(change_request = %subject.address(), %error, "publish change request observation failed");
                    } else {
                        self.inner.observation_errors.lock().await.remove(&subject);
                    }
                    drop(guard);
                    match self.collect_retained_if_terminal(&subject).await {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(change_request = %subject.address(), %error, "collect retained change request failed")
                        }
                    }
                    let retained = self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.is_empty());
                    let delay = if retained {
                        RETAINED_REFRESH_INTERVAL
                    } else if demanded {
                        self.inner.cadence.freshness_demanded
                    } else if status.checks.value == Some(ObservedChecks::Pending) {
                        self.inner.cadence.checks_pending
                    } else {
                        self.inner.cadence.state
                    };
                    let delay = if self.inner.relay_healthy.load(std::sync::atomic::Ordering::SeqCst) {
                        delay.max(RETAINED_REFRESH_INTERVAL)
                    } else {
                        delay
                    };
                    if !self.wait_for_next(&subject, delay, delay).await {
                        break;
                    }
                }
                Err(error) => {
                    if matches!(error, ObservationError::NotFound(_)) {
                        let lock = self.subject_lock(&subject).await;
                        let _guard = lock.lock().await;
                        let retained = self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.is_empty());
                        if retained {
                            match self.collect_local_subject(&subject).await {
                                Ok(()) => {
                                    self.inner.active.lock().await.remove(&subject);
                                    self.inner.observation_errors.lock().await.remove(&subject);
                                    // Returning ends this refresh task; it must not abort itself.
                                    return;
                                }
                                Err(error) => tracing::warn!(%error, "collect nonexistent retained change request failed"),
                            }
                        }
                    }
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                    self.inner.engine.failed(KIND, &record_name).await;
                    tracing::warn!(change_request = %subject.address(), %error, "change request observation failed");
                    let retained = self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.is_empty());
                    let ordinary_delay = if retained { RETAINED_REFRESH_INTERVAL } else { self.inner.cadence.checks_pending };
                    let delay = error
                        .retry_at()
                        .and_then(|retry_at| retry_at.signed_duration_since(Utc::now()).to_std().ok())
                        .filter(|delay| !delay.is_zero())
                        .unwrap_or(ordinary_delay);
                    if !self.wait_for_next(&subject, delay, ordinary_delay).await {
                        break;
                    }
                }
            }
        }
    }

    async fn owns_record(&self, subject: &ChangeRequestKey, allow_takeover: bool, create_missing: bool) -> Result<bool, String> {
        let source = stored_source(subject);
        if let Some(owner) = crate::forge_observation::source_owner(&self.inner.backend, subject.namespace(), &source).await? {
            if owner.to_string() != self.inner.authority {
                return Ok(false);
            }
            let name = subject.record_name();
            let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
            if create_missing {
                self.get_or_create_record(subject, &name).await?;
            }
            loop {
                let current = match records.get(&name).await {
                    Ok(current) => current,
                    Err(ResourceError::NotFound { .. }) => return Ok(false),
                    Err(error) => return Err(error.to_string()),
                };
                if current.spec.observing_authority == self.inner.authority {
                    return Ok(true);
                }
                // Declared source ownership is authoritative immediately. The
                // legacy stale-record election below applies only without it.
                // Even a fresher foreign claim cannot override the declared owner.
                // Hosts may briefly disagree while source ownership replicates;
                // claims converge once their views agree on the declared owner.
                let mut spec = current.spec.clone();
                spec.observing_authority = self.inner.authority.clone();
                match records.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await {
                    Ok(_) => return Ok(true),
                    Err(ResourceError::Conflict { .. }) => continue,
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        let name = subject.record_name();
        let records = self.inner.backend.including_replicas::<ChangeRequest>(subject.namespace());
        // A former owner may retain its local copy. Status-less copies rank
        // last; observed copies rank by freshness, then authority, with local
        // preference only when both observation time and authority tie exactly.
        let record =
            flotilla_resources::pick_replica_read(records.get_all(&name).await.map_err(|error| error.to_string())?.items, |item| {
                (&item.object, matches!(item.provenance, ResourceProvenance::Local))
            });
        let Some(record) = record else {
            if !create_missing {
                return Ok(false);
            }
            let created = self.get_or_create_record(subject, &name).await?;
            return Ok(created.spec.observing_authority == self.inner.authority);
        };
        if record.object.spec.observing_authority == self.inner.authority {
            return Ok(matches!(record.provenance, ResourceProvenance::Local));
        }
        if !allow_takeover {
            return Ok(false);
        }
        let observed_at =
            record.object.status.as_ref().map_or(record.object.metadata.creation_timestamp, |status| status.state.observed_at);
        let takeover_after =
            chrono::Duration::from_std(self.inner.cadence.stale_after.saturating_mul(2)).map_err(|error| error.to_string())?;
        if Utc::now().signed_duration_since(observed_at) <= takeover_after {
            return Ok(false);
        }
        let local = self.inner.backend.using::<ChangeRequest>(subject.namespace());
        let mut spec = record.object.spec.clone();
        spec.observing_authority = self.inner.authority.clone();
        if let Ok(existing) = local.get(&name).await {
            merge_change_request_history(&mut spec.subject_of, &existing.spec.subject_of);
        }
        // A former owner can keep its local record while another host owns a
        // fresher replica. Reclaim through that local record when it exists.
        let result = match local.get(&name).await {
            Ok(existing) if existing.spec.observing_authority == self.inner.authority => Ok(existing),
            Ok(existing) => local.update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &spec).await,
            Err(ResourceError::NotFound { .. }) => {
                // A single conditional create claims a local copy, so an
                // interruption cannot strand a foreign shadow.
                local.create(&InputMeta::builder().name(name).build(), &spec).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(_) => Ok(true),
            Err(ResourceError::Conflict { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn wait_for_next(&self, subject: &ChangeRequestKey, delay: Duration, cadence: Duration) -> bool {
        let Some(wake) = self.inner.active.lock().await.get_mut(subject).map(|refresh| {
            refresh.next_refresh_at = cadence_deadline(Utc::now(), delay);
            refresh.scheduled_cadence = cadence;
            Arc::clone(&refresh.wake)
        }) else {
            return false;
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = wake.notified() => {}
            () = self.inner.relay_wake.notified() => {}
        }
        true
    }

    async fn publish(
        &self,
        subject: &ChangeRequestKey,
        name: &str,
        status: ChangeRequestStatus,
        heartbeat: bool,
        allow_takeover: bool,
        create_missing: bool,
    ) -> Result<(), String> {
        if !self.owns_record(subject, allow_takeover, create_missing).await? {
            return Ok(());
        }
        let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
        let current = records.get(name).await.map_err(|error| error.to_string())?;
        if current.spec.observing_authority != self.inner.authority {
            return Ok(());
        }
        let stale_after = chrono::Duration::from_std(self.inner.cadence.stale_after).map_err(|error| error.to_string())?;
        if !heartbeat
            && current.status.as_ref().is_some_and(|current| {
                observed_values_equal(current, &status) && Utc::now().signed_duration_since(current.state.observed_at) < stale_after
            })
        {
            return Ok(());
        }
        records.update_status(name, &current.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn get_or_create_record(
        &self,
        subject: &ChangeRequestKey,
        name: &str,
    ) -> Result<flotilla_resources::ResourceObject<ChangeRequest>, String> {
        let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
        match records.get(name).await {
            Ok(current) => Ok(current),
            Err(ResourceError::NotFound { .. }) => {
                let source = stored_source(subject);
                // Without declared ownership, preserve the legacy local authority election.
                let authority = crate::forge_observation::source_owner(&self.inner.backend, subject.namespace(), &source)
                    .await?
                    .map_or_else(|| self.inner.authority.clone(), |owner| owner.to_string());
                let candidates = self
                    .inner
                    .backend
                    .including_replicas::<ChangeRequest>(subject.namespace())
                    .list()
                    .await
                    .map_err(|error| error.to_string())?;
                let selected = select_change_requests(candidates.items.iter().map(|source| &source.object));
                let history = selected
                    .values()
                    .find(|record| spec_names(&record.spec, subject))
                    .map(|record| record.spec.subject_of.clone())
                    .unwrap_or_default();
                let (service, scope) = subject.stored_service_scope();
                let spec = ChangeRequestSpec::builder()
                    .subject_of(history)
                    .service(service)
                    .scope(scope)
                    .number(subject.number())
                    .observing_authority(authority)
                    .build();
                match records.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await {
                    Ok(created) => Ok(created),
                    Err(ResourceError::Conflict { .. }) => records.get(name).await.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The stored `(service, scope)` source that declares the subject's owner.
/// The branch of each repository of an active convoy that produces no change request yet: the
/// branches whose change requests nothing else looks for.
/// The change requests convoy `convoy` names as its subjects, by address, with its name.
fn change_request_subjects(convoy: &ResourceObject<Convoy>, forges: &[ForgeSpec]) -> Vec<(String, Address)> {
    let name = &convoy.metadata.name;
    convoy_subject_rows(convoy, &ReferenceContext::default())
        .into_iter()
        .filter(|row| row.subject.kind == ConvoySubjectKind::ChangeRequest)
        // Debug, not warn: this re-runs on every convoy, project and Forge change, so an undeclared
        // host would repeat each pass.
        .filter_map(|row| match subject_address(&row.subject, forges) {
            Ok(address) => Some((name.clone(), address)),
            Err(error) => {
                tracing::debug!(convoy = %name, %error, "convoy change request subject has no address; not observed");
                None
            }
        })
        .collect()
}

/// Takes `event` into the next pass: a convoy change lists the convoys again, and a checkout
/// change is kept only while an ended convoy holds checkouts to be released.
fn absorb(event: DemandEvent, demands: &ConvoyDemands, convoys_changed: &mut bool, checkout_changes: &mut Vec<CheckoutChange>) {
    match event {
        DemandEvent::Convoys => *convoys_changed = true,
        DemandEvent::Checkout(change) => {
            if !demands.holding.is_empty() {
                checkout_changes.push(change);
            }
        }
    }
}

/// The checkouts' changes as demand events: a lagged receiver may have missed any, so it reads as a
/// rescan. A closed channel follows no checkouts and ends.
fn checkout_events(
    changes: tokio::sync::broadcast::Receiver<CheckoutChange>,
) -> futures::stream::BoxStream<'static, Result<DemandEvent, ResourceError>> {
    futures::stream::unfold(changes, |mut changes| async move {
        let change = match changes.recv().await {
            Ok(change) => change,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => CheckoutChange::Rescanned,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok(DemandEvent::Checkout(change)), changes))
    })
    .boxed()
}

fn unproduced_branches(convoy: &ResourceObject<Convoy>, forges: &[ForgeSpec]) -> Vec<(Address, String)> {
    let Some(branch) = convoy.spec.r#ref.as_ref() else { return Vec::new() };
    let active_without_change_request = convoy.status.as_ref().is_some_and(|status| {
        status.phase == flotilla_resources::ConvoyPhase::Active
            && !status
                .subjects
                .iter()
                .any(|entry| entry.subject.kind == ConvoySubjectKind::ChangeRequest && entry.relationship == Relationship::Produces)
    });
    if !active_without_change_request {
        return Vec::new();
    }
    convoy
        .spec
        .repositories
        .iter()
        .filter_map(|repository| flotilla_resources::address::repository_address(&repository.url, forges).ok())
        .map(|repository| (repository, branch.clone()))
        .collect()
}

/// The branch scheduler: sleeps to the earliest due lookup, or until a branch is added, and looks
/// up each due branch in turn. A branch dropped meanwhile is not scheduled again.
async fn run_branch_probes(inner: Weak<ChangeRequestRefresherInner>, wake: Arc<Notify>) {
    loop {
        let next = {
            let Some(inner) = inner.upgrade() else { return };
            let schedule = inner.branch_schedule.lock().expect("branch schedule");
            schedule.values().min().copied()
        };
        match next {
            Some(at) if at > tokio::time::Instant::now() => {
                tokio::select! {
                    () = tokio::time::sleep_until(at) => {}
                    () = wake.notified() => continue,
                }
            }
            Some(_) => {}
            None => {
                wake.notified().await;
                continue;
            }
        }
        let Some(inner) = inner.upgrade() else { return };
        let Some(probe) = inner.branch_probe.get().cloned() else { return };
        let now = tokio::time::Instant::now();
        let due = inner
            .branch_schedule
            .lock()
            .expect("branch schedule")
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in due {
            let (repository, branch) = &key;
            match probe.probe(repository, branch).await {
                Ok(()) => tracing::debug!(%repository, %branch, "convoy branch looked up"),
                Err(error) => tracing::warn!(%repository, %branch, %error, "convoy branch lookup failed"),
            }
            if let Some(at) = inner.branch_schedule.lock().expect("branch schedule").get_mut(&key) {
                *at = tokio::time::Instant::now() + BRANCH_PROBE_INTERVAL;
            }
        }
    }
}

fn stored_source(subject: &ChangeRequestKey) -> IssueSource {
    let (service, scope) = subject.stored_service_scope();
    IssueSource { service, scope }
}

fn same_repository(candidate: &ChangeRequestKey, subject: &ChangeRequestKey) -> bool {
    candidate.namespace() == subject.namespace() && candidate.repository() == subject.repository()
}

/// Whether a record's stored spelling names the subject's change request.
/// Records are written in `stored_service_scope`'s spelling (`github.com` or
/// a forge id), which needs no Forge directory to read back.
fn spec_names(spec: &ChangeRequestSpec, subject: &ChangeRequestKey) -> bool {
    stored_spelling_names(subject.address(), &spec.service, &spec.scope, spec.number, &[])
}

fn observed_values_equal(left: &ChangeRequestStatus, right: &ChangeRequestStatus) -> bool {
    left.state.value == right.state.value
        && left.head_sha.value == right.head_sha.value
        && left.checks.value == right.checks.value
        && left.review.actionable_at_head.value == right.review.actionable_at_head.value
        && left.mergeable.value == right.mergeable.value
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use async_trait::async_trait;
    use flotilla_manifest::{
        entity,
        projection::{project_catalog, CatalogInput, SubjectCatalogInput},
        recipe::FlotillaRecipes,
        wire::{MetadataTarget, MetadataValue},
    };
    use flotilla_resources::{ChangeRequestReviewObservation, ChangeRequestSubjectHistory, Observation, ObservedMergeability};
    use flotilla_store::{ResourceBackend, TypedResolver};

    use super::*;
    fn cr_name(service: &str, scope: &str, number: u64) -> String {
        flotilla_resources::address::record_name(
            &flotilla_resources::address::stored_change_request_address(service, scope, number, &[]).expect("address"),
        )
        .expect("record name")
    }
    use flotilla_change_request::observation::parse_gh_observation;

    // Convoy demand discovery decodes live inventory only, regardless of retained history.
    #[tokio::test]
    async fn convoy_demands_read_only_live_convoys() {
        use flotilla_store_testkit::{measure_reads, ReadCountsBackendExt};
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();

        flotilla_store_testkit::fixtures::seed_convoy_history()
            .backend(&backend)
            .namespace("flotilla")
            .landed(300)
            .active(10)
            .call()
            .await
            .expect("convoy history");

        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let (result, reads) = measure_reads(refresher.reconcile_convoy_demands("flotilla", &mut ConvoyDemands::default())).await;
        result.expect("reconcile");
        assert_eq!(reads.of(&backend).objects_by_kind().get("Convoy"), Some(&10));
    }

    // #3108 ruling 1: an abandoned convoy that something still holds is observed only for the
    // change request its Submitted pr promise names, until the promise resolves; the resolution
    // releases the demand. A landed convoy, which delivered with nothing open, demands nothing.
    #[tokio::test]
    async fn an_abandoned_convoys_submitted_pr_is_demanded_until_it_resolves() {
        use flotilla_resources::{
            promises::{Promise, PromiseKind, PromiseSource, PromiseState, Submission},
            ConvoyPhase, ConvoySpec, ConvoyStatus, Host, HostSpec, HostStatus,
        };
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(flotilla_protocol::NodeId::new("node"));
        let hosts = backend.using::<Host>("ops");
        let host = hosts.create(&InputMeta::builder().name("node-host".into()).build(), &HostSpec::default()).await.expect("host");
        let ready = HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() };
        hosts.update_status("node-host", &host.metadata.resource_version, &ready).await.expect("heartbeat");
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let promise = |number: u64, state: PromiseState| Promise {
            id: "template/pr".into(),
            vessel: "work".into(),
            role: "coder".into(),
            source: PromiseSource::Template,
            kind: PromiseKind::Pr,
            state,
            retraction_reason: None,
            submissions: vec![Submission {
                reference: format!("cr/github.com/org/repo/{number}"),
                metadata: BTreeMap::new(),
                submitted_at: Some(Utc::now()),
                verdict: None,
            }],
        };
        let convoys = backend.using::<Convoy>("ops");
        for (name, phase, number) in [("abandoned", ConvoyPhase::Abandoned, 42), ("landed", ConvoyPhase::Landed, 43)] {
            let spec = ConvoySpec::builder().workflow_ref("dev".into()).project_ref("flotilla".into()).role("coder".into()).build();
            let convoy = convoys.create(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("convoy");
            let mut status = ConvoyStatus { phase, ..Default::default() };
            status.promises.entry("work".into()).or_default().insert("coder".into(), vec![promise(number, PromiseState::Submitted)]);
            convoys.update_status(name, &convoy.metadata.resource_version, &status).await.expect("status");
        }
        let mut demands = ConvoyDemands::default();
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        let demanded: Vec<String> = demands.demanded().into_iter().map(ChangeRequestKey::record_name).collect();
        assert_eq!(demanded, [cr_name("github.com", "org/repo", 42)]);

        let convoy = convoys.get("abandoned").await.expect("abandoned");
        let mut status = convoy.status.expect("status");
        status.promises.get_mut("work").expect("work").insert("coder".into(), vec![promise(42, PromiseState::Kept)]);
        convoys.update_status("abandoned", &convoy.metadata.resource_version, &status).await.expect("kept");
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        assert!(demands.demanded().is_empty(), "the merge released the demand: {demands:?}");
        for (_, refresh) in refresher.inner.active.lock().await.drain() {
            refresh.task.abort();
        }
    }

    /// Checkout launches held for convoys, read by holder, with their changes.
    struct HeldCheckouts {
        launches: std::sync::Mutex<Vec<flotilla_resources::checkout_records::CheckoutLaunch>>,
        changes: tokio::sync::broadcast::Sender<CheckoutChange>,
        reads: AtomicUsize,
    }

    impl HeldCheckouts {
        fn new() -> Self {
            Self { launches: std::sync::Mutex::new(Vec::new()), changes: tokio::sync::broadcast::channel(16).0, reads: AtomicUsize::new(0) }
        }

        /// How many times launches were read by holder.
        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Launch `name` held for `convoy`, read as `reading`.
        fn set(&self, name: &str, convoy: &str, reading: flotilla_protocol::LifecycleReading) {
            let mut launches = self.launches.lock().expect("launches");
            launches.retain(|launch| launch.name != name);
            launches.push(flotilla_resources::checkout_records::CheckoutRecord {
                name: name.into(),
                home: "node".into(),
                spec: flotilla_resources::CheckoutSpec::Observed(flotilla_resources::ObservedCheckoutSpec {
                    r#ref: "work".into(),
                    path: "/work".into(),
                    repo_ref: flotilla_resources::RepositoryKey("repo".into()),
                    host_ref: "node-host".into(),
                    is_main: false,
                }),
                holder: Some(CheckoutHolder::Convoy(convoy.into())),
                requested_at: Utc::now(),
                reading,
                degraded: None,
                integration: (),
            });
        }
    }

    impl flotilla_resources::checkout_records::CheckoutRecords for HeldCheckouts {
        fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
            Vec::new()
        }
        fn get(&self, _: &str, _: Option<&str>) -> Result<Option<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer reads checkouts by holder")
        }
        fn held(&self, _: &CheckoutHolder) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer reads launches without integration")
        }
        fn of_repository(
            &self,
            _: &flotilla_resources::RepositoryKey,
        ) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer reads checkouts by holder")
        }
        fn in_environment(&self, _: &str, _: Option<&str>) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer reads checkouts by holder")
        }
        fn here(&self) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer reads checkouts by holder")
        }
        fn home(&self) -> &str {
            "node"
        }
        fn every(&self) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            unreachable!("the observer never lists every checkout")
        }
        fn launches(&self, which: Launches<'_>) -> Result<Vec<flotilla_resources::checkout_records::CheckoutLaunch>, String> {
            let Launches::Held(holder) = which else { unreachable!("the observer reads checkouts by holder") };
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.launches.lock().expect("launches").iter().filter(|launch| launch.holder.as_ref() == Some(holder)).cloned().collect())
        }
        fn request(&self, _: &str, _: &flotilla_resources::CheckoutSpec, _: Option<&CheckoutHolder>) -> Result<String, String> {
            unreachable!("the observer writes no checkouts")
        }
        fn inspected(&self, _: &str, _: flotilla_resources::checkout_records::Inspection) -> Result<bool, String> {
            unreachable!("the observer writes no checkouts")
        }
        fn inspection_failed(&self, _: &str) -> Result<(), String> {
            unreachable!("the observer writes no checkouts")
        }
        fn changes(&self) -> tokio::sync::broadcast::Receiver<CheckoutChange> {
            self.changes.subscribe()
        }
    }

    struct CheckoutReads(HeldCheckouts);

    impl LifecycleReads for CheckoutReads {
        fn home(&self) -> Result<&str, String> {
            Ok("node")
        }
        fn try_reading(
            &self,
            _: flotilla_resources::lifecycle_reads::ProvisioningKind,
            _: &str,
            _: Option<&str>,
        ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
            Ok(None)
        }
        fn joins(&self) -> Result<&flotilla_resources::held_joins::HeldJoins, String> {
            Err("no joins".into())
        }
        fn checkouts(&self) -> Result<&dyn flotilla_resources::checkout_records::CheckoutRecords, String> {
            Ok(&self.0)
        }
    }

    /// An observer of `ops` whose convoy `work`, in `phase`, produces change request 42, its
    /// checkouts read from `checkouts`.
    async fn producing_convoy_observer(
        checkouts: Arc<CheckoutReads>,
        phase: ConvoyPhase,
    ) -> (ChangeRequestRefresher, TypedResolver<Convoy>) {
        use flotilla_resources::{ConvoySpec, ConvoyStatus, Host, HostSpec, HostStatus, SubjectDiscoverySource};
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(flotilla_protocol::NodeId::new("node"));
        let hosts = backend.using::<Host>("ops");
        let host = hosts.create(&InputMeta::builder().name("node-host".into()).build(), &HostSpec::default()).await.expect("host");
        let ready = HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() };
        hosts.update_status("node-host", &host.metadata.resource_version, &ready).await.expect("heartbeat");
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        refresher.install_lifecycle_reads(checkouts).expect("install the checkouts");
        let convoys = backend.using::<Convoy>("ops");
        let spec = ConvoySpec::builder().workflow_ref("dev".into()).project_ref("flotilla".into()).role("coder".into()).build();
        let convoy = convoys.create(&InputMeta::builder().name("work".into()).build(), &spec).await.expect("convoy");
        let mut status = ConvoyStatus { phase, ..Default::default() };
        let produced = flotilla_protocol::Subject {
            kind: ConvoySubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "org/repo".into() },
            id: "42".into(),
        };
        status.discover_subject(produced, Relationship::Produces, SubjectDiscoverySource::Claim, Utc::now());
        convoys.update_status("work", &convoy.metadata.resource_version, &status).await.expect("status");
        (refresher, convoys)
    }

    /// Convoy `work` lands.
    async fn land(convoys: &TypedResolver<Convoy>) {
        let convoy = convoys.get("work").await.expect("work");
        let mut status = convoy.status.expect("status");
        status.phase = ConvoyPhase::Landed;
        convoys.update_status("work", &convoy.metadata.resource_version, &status).await.expect("landed");
    }

    async fn stop_refreshes(refresher: &ChangeRequestRefresher) {
        for (_, refresh) in refresher.inner.active.lock().await.drain() {
            refresh.task.abort();
        }
    }

    fn demanded(demands: &ConvoyDemands) -> Vec<String> {
        demands.demanded().into_iter().map(ChangeRequestKey::record_name).collect()
    }

    // A landed convoy still holds a checkout to be released, and that release is judged against
    // the change request it produced: it stays demanded until the checkout ends. The convoy's
    // checkouts are read by holder once, then again only as one of them changes.
    #[tokio::test]
    async fn a_landed_convoys_change_request_stays_demanded_until_its_checkouts_are_released() {
        use flotilla_protocol::LifecycleReading;
        let reads = Arc::new(CheckoutReads(HeldCheckouts::new()));
        reads.0.set("co#1", "work", LifecycleReading::Ready { identity: "/work".into() });
        let (refresher, convoys) = producing_convoy_observer(Arc::clone(&reads), ConvoyPhase::Active).await;
        let mut demands = ConvoyDemands::default();
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        let wanted = [cr_name("github.com", "org/repo", 42)];
        assert_eq!(demanded(&demands), wanted);
        assert_eq!(reads.0.reads(), 0, "a live convoy wants its change request whatever it holds");

        land(&convoys).await;
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        assert_eq!(demanded(&demands), wanted, "the convoy's checkout is still to be released");
        assert_eq!(refresher.active_demands().await, 1);
        assert_eq!(reads.0.reads(), 1, "its checkouts are read once");

        // Another checkout's change reads nothing; this one's, still live, keeps it.
        let changed = |name: &str| [CheckoutChange::Changed { name: name.into(), home: "node".into() }];
        assert!(!refresher.recheck_holding(&mut demands, &changed("other#1")).await);
        assert_eq!(reads.0.reads(), 1);
        assert!(!refresher.recheck_holding(&mut demands, &changed("co#1")).await);
        assert_eq!(reads.0.reads(), 2);

        reads.0.set("co#1", "work", LifecycleReading::Released);
        assert!(refresher.recheck_holding(&mut demands, &changed("co#1")).await, "its last checkout was released");
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        assert!(demanded(&demands).is_empty(), "the release ended the demand: {demands:?}");
        assert_eq!(refresher.active_demands().await, 0);
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        assert_eq!(reads.0.reads(), 3, "a convoy that holds nothing more is not read again");
        stop_refreshes(&refresher).await;
    }

    // An observer that starts after the convoy ended, its checkout not yet released (a daemon
    // restart), demands the change request again: the hold is read from the checkouts, not
    // remembered.
    #[tokio::test]
    async fn after_a_restart_an_ended_convoys_unreleased_checkout_still_holds_its_change_request() {
        use flotilla_protocol::LifecycleReading;
        let reads = Arc::new(CheckoutReads(HeldCheckouts::new()));
        reads.0.set("co#1", "work", LifecycleReading::Ready { identity: "/work".into() });
        let (refresher, _convoys) = producing_convoy_observer(Arc::clone(&reads), ConvoyPhase::Landed).await;
        let mut demands = ConvoyDemands::default();
        refresher.reconcile_convoy_demands("ops", &mut demands).await.expect("reconcile");
        let demanded = demanded(&demands);
        stop_refreshes(&refresher).await;
        assert_eq!(demanded, [cr_name("github.com", "org/repo", 42)], "the unreleased checkout holds its change request");
    }

    struct UnavailableSource;

    struct RecordingProbe(std::sync::Mutex<Vec<(String, String)>>);

    #[async_trait::async_trait]
    impl BranchProbe for RecordingProbe {
        async fn probe(&self, repository: &Address, branch: &str) -> Result<(), String> {
            self.0.lock().expect("probes").push((repository.to_string(), branch.to_owned()));
            Ok(())
        }
    }

    // The forge observer's branch probe (#3108): an active convoy that produces no change request
    // has its branch looked up at once and again at each probe interval. A convoy that is not
    // active is not probed, and once the convoy produces a change request its probe stops.
    #[tokio::test(start_paused = true)]
    async fn an_active_convoy_without_a_change_request_has_its_branch_probed() {
        use flotilla_resources::{ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, SubjectDiscoverySource};
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let probe = Arc::new(RecordingProbe(Default::default()));
        refresher.install_branch_probe(probe.clone()).expect("probe");
        let probes = || probe.0.lock().expect("probes").clone();
        let settle = || async {
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        };
        let convoys = backend.using::<Convoy>("ops");
        for (name, phase) in [("active", ConvoyPhase::Active), ("pending", ConvoyPhase::Pending)] {
            let repository = ConvoyRepositorySpec {
                url: format!("https://github.com/org/{name}"),
                repo_ref: flotilla_protocol::RepositoryKey(name.into()),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: name.into(),
                subpaths: Vec::new(),
            };
            let spec = ConvoySpec::builder()
                .workflow_ref("dev".into())
                .project_ref("flotilla".into())
                .role("coder".into())
                .r#ref("feature/work".into())
                .repositories(vec![repository])
                .build();
            let convoy = convoys.create(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("convoy");
            let status = ConvoyStatus { phase, ..Default::default() };
            convoys.update_status(name, &convoy.metadata.resource_version, &status).await.expect("status");
        }

        refresher.reconcile_convoy_demands("ops", &mut ConvoyDemands::default()).await.expect("reconcile");
        settle().await;
        let looked_up = |count: usize| vec![("forge:github/repo:org%2Factive".to_owned(), "feature/work".to_owned()); count];
        assert_eq!(probes(), looked_up(1), "only the active convoy's branch, at once");
        tokio::time::advance(BRANCH_PROBE_INTERVAL).await;
        settle().await;
        assert_eq!(probes(), looked_up(2), "and again at the probe interval");

        let convoy = convoys.get("active").await.expect("convoy");
        let mut status = convoy.status.clone().expect("status");
        let produced = flotilla_protocol::Subject {
            kind: ConvoySubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "org/active".into() },
            id: "7".into(),
        };
        status.discover_subject(produced, Relationship::Produces, SubjectDiscoverySource::Branch, Utc::now());
        convoys.update_status("active", &convoy.metadata.resource_version, &status).await.expect("produces");
        refresher.reconcile_convoy_demands("ops", &mut ConvoyDemands::default()).await.expect("reconcile");
        tokio::time::advance(BRANCH_PROBE_INTERVAL * 2).await;
        settle().await;
        assert_eq!(probes(), looked_up(2), "a convoy that produces a change request is not probed");
        for (_, refresh) in refresher.inner.active.lock().await.drain() {
            refresh.task.abort();
        }
    }

    #[async_trait]
    impl ChangeRequestObservationSource for UnavailableSource {
        async fn observe(&self, _subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            Err("unavailable".into())
        }
    }

    // Later failures replace prior diagnostics and success clears every kind,
    // including NotFound, so collection cannot reuse superseded absence evidence.
    #[tokio::test]
    async fn claim_time_recovery_clears_observation_errors() {
        struct RecoveringSource(Mutex<Result<ChangeRequestStatus, ObservationError>>);
        #[async_trait]
        impl ChangeRequestObservationSource for RecoveringSource {
            async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
                self.0.lock().await.clone()
            }
        }
        for error in [
            ObservationError::NotFound("missing".into()),
            ObservationError::Failed("temporary failure".into()),
            ObservationError::RateLimited {
                budget: "GraphQL".into(),
                retry_at: Some("2026-10-03T12:00:00Z".parse().expect("deadline")),
                detail: "rate limited".into(),
            },
        ] {
            let source = Arc::new(RecoveringSource(Mutex::new(Err(error.clone()))));
            let refresher = ChangeRequestRefresher::new(
                ResourceBackend::new(InMemoryBackend::default()),
                "node".into(),
                source.clone(),
                ChangeRequestRefreshCadence::default(),
            );
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            assert_eq!(refresher.refresh_once(&subject).await.unwrap_err(), error);
            assert_eq!(refresher.observation_error(&subject).await, Some(error.clone()));
            let replacement = ObservationError::Failed("new read failed".into());
            *source.0.lock().await = Err(replacement.clone());
            assert_eq!(refresher.refresh_once(&subject).await.unwrap_err(), replacement);
            assert_eq!(refresher.observation_error(&subject).await, Some(replacement));
            *source.0.lock().await = Ok(parse_gh_observation(
                r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
                Utc::now(),
            )
            .expect("healthy observation"));
            refresher.refresh_once(&subject).await.expect("recovered observation");
            assert!(refresher.observation_error(&subject).await.is_none());
        }
    }

    // Deadline conversion is small arithmetic glue. These cases cover zero,
    // ordinary cadence, duration overflow, timestamp overflow, and the limit.
    #[test]
    fn cadence_deadline_saturates_at_chrono_limits() {
        let at: DateTime<Utc> = "2026-10-17T00:00:00Z".parse().unwrap();
        assert_eq!(cadence_deadline(at, Duration::ZERO), at);
        assert_eq!(cadence_deadline(at, Duration::from_secs(60)), at + chrono::Duration::seconds(60));
        assert_eq!(cadence_deadline(at, Duration::MAX), DateTime::<Utc>::MAX_UTC);
        assert_eq!(cadence_deadline(DateTime::<Utc>::MAX_UTC, Duration::from_secs(1)), DateTime::<Utc>::MAX_UTC);
        assert_eq!(cadence_deadline(DateTime::<Utc>::MAX_UTC, Duration::ZERO), DateTime::<Utc>::MAX_UTC);
    }

    struct CountingSource(Arc<AtomicUsize>);

    #[async_trait]
    impl ChangeRequestObservationSource for CountingSource {
        async fn observe(&self, _subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let observed_at = Utc::now();
            Ok(ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: Observation::known(ObservedChangeRequestState::Open, observed_at),
                head_sha: Observation::known("abc".to_string(), observed_at),
                checks: Observation::known(ObservedChecks::Pass, observed_at),
                review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
                mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
            })
        }
    }

    #[tokio::test]
    async fn concurrent_first_demands_share_one_refresh_and_write_no_placeholder() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1366, &[]).expect("address"),
        )
        .expect("key");
        let (first, second) = tokio::join!(
            refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None),
            refresher.demand(uuid::Uuid::new_v4(), subject, None),
        );
        first.expect("first demand");
        second.expect("concurrent demand");
        assert!(
            backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.is_empty(),
            "the observer writes no placeholder"
        );
        assert_eq!(refresher.inner.active.lock().await.len(), 1);
        assert_eq!(refresher.active_demands().await, 2);
    }

    // Forge boundary: inject observations instead of invoking a live forge API.
    struct MutableSource(Mutex<ChangeRequestStatus>, AtomicUsize);
    #[async_trait]
    impl ChangeRequestObservationSource for MutableSource {
        async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            let status = self.0.lock().await.clone();
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(status)
        }
    }

    fn test_subject(number: u64) -> ChangeRequestKey {
        ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", number, &[]).expect("address"),
        )
        .expect("subject")
    }
    // #3710: an already merged demanded record never reaches the forge, even
    // after ten minutes or when explicitly refreshed for completion.
    #[tokio::test(start_paused = true)]
    async fn demanded_merged_subject_not_polled() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let source = Arc::new(MutableSource(
            Mutex::new(
                parse_gh_observation(
                    r#"{"state":"MERGED","headRefOid":"head","statusCheckRollup":[],"mergeable":"MERGEABLE"}"#,
                    Utc::now(),
                )
                .expect("merged"),
            ),
            AtomicUsize::new(0),
        ));
        let refresher = ChangeRequestRefresher::new(backend.clone(), "node".into(), source.clone(), ChangeRequestRefreshCadence::default());
        let subject = test_subject(42);
        refresher.refresh_once(&subject).await.expect("initial observation");
        assert_eq!(source.1.load(Ordering::SeqCst), 1);
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject.clone(), None).await.expect("demand");
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        refresher.refresh_once(&subject).await.expect("completion uses merged record");
        assert_eq!(source.1.load(Ordering::SeqCst), 1);
        refresher.release(demand).await;
    }

    // Forge boundary fake: expose exactly which subjects a batch asks to read.
    struct GroupSource(Mutex<Vec<Vec<u64>>>);
    #[async_trait]
    impl ChangeRequestObservationSource for GroupSource {
        async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            Ok(parse_gh_observation(r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[],"mergeable":"MERGEABLE"}"#, Utc::now())
                .expect("open"))
        }
        async fn observe_group(
            &self,
            subjects: &[ChangeRequestKey],
            subject: &ChangeRequestKey,
        ) -> Result<ChangeRequestStatus, ObservationError> {
            self.0.lock().await.push(subjects.iter().map(ChangeRequestKey::number).collect());
            self.observe(subject).await
        }
    }
    // #3710: retained history has its own cadence and cannot ride a live sibling's batch.
    #[tokio::test(start_paused = true)]
    async fn retained_subject_does_not_ride_along_with_demanded_sibling() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let source = Arc::new(GroupSource(Mutex::new(Vec::new())));
        let refresher = ChangeRequestRefresher::new(backend, "node".into(), source.clone(), ChangeRequestRefreshCadence::default());
        let retained = test_subject(41);
        refresher.refresh_once(&retained).await.expect("seed retained");
        refresher.start_retained(retained).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        source.0.lock().await.clear();
        let sibling = test_subject(42);
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, sibling.clone(), None).await.expect("sibling demand");
        // The sibling's record is the refresh loop's first write; its next refresh is the batch read.
        refresher.get_or_create_record(&sibling, &sibling.record_name()).await.expect("sibling record");
        refresher.refresh_once_with_creation(&sibling, false).await.expect("sibling batch");
        let groups = source.0.lock().await.clone();
        assert!(!groups.is_empty());
        assert!(groups.iter().all(|group| !group.contains(&41)), "{groups:?}");
        refresher.release(demand).await;
    }

    async fn wait_for_request_state(records: &TypedResolver<ChangeRequest>, name: &str, expected: Option<ObservedChangeRequestState>) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let found = records.get(name).await;
                let matches = match expected {
                    Some(state) => {
                        found.as_ref().is_ok_and(|record| record.status.as_ref().is_some_and(|status| status.state.value == Some(state)))
                    }
                    None => matches!(found, Err(ResourceError::NotFound { .. })),
                };
                if matches {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("request reached expected state before deadline");
    }

    // Wait for observable progress, with a deadline shorter than any refresh cadence.
    async fn wait_for_calls(calls: &AtomicUsize, count: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("observer call before deadline");
        // Let the completed observation publish and arm its next timer.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    async fn wait_for_observation(source: &MutableSource, count: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.1.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("retained observation before deadline");
    }

    // Network boundary fake: return explicit absence, throttling, authentication,
    // or transport failure. Only explicit absence may collect a retained record.
    struct ErrorSource(ObservationError);
    #[async_trait::async_trait]
    impl ChangeRequestObservationSource for ErrorSource {
        async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            Err(self.0.clone())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retained_missing_request_is_collected_but_read_failures_are_kept() {
        for error in [
            ObservationError::NotFound("missing".into()),
            ObservationError::Failed("401 unauthenticated".into()),
            ObservationError::Failed("transport unavailable".into()),
            ObservationError::RateLimited { budget: "forge".into(), retry_at: None, detail: "throttled".into() },
        ] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let refresher = ChangeRequestRefresher::new(
                backend.clone(),
                "node".into(),
                Arc::new(ErrorSource(error.clone())),
                ChangeRequestRefreshCadence::default(),
            );
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            refresher.get_or_create_record(&subject, &subject.record_name()).await.expect("record");
            let records = backend.using::<ChangeRequest>("ops");
            let record = records.get(&subject.record_name()).await.expect("record");
            let mut spec = record.spec.clone();
            spec.subject_of.push(
                ChangeRequestSubjectHistory::builder()
                    .namespace("ops".into())
                    .convoy("gone".into())
                    .origin("kiwi".into())
                    .relationship(Relationship::Produces)
                    .role("coder".into())
                    .last_seen(Utc::now())
                    .build(),
            );
            records.update(&InputMeta::from(&record.metadata), &record.metadata.resource_version, &spec).await.expect("history");
            refresher.start_retained(subject.clone()).await;
            for _ in 0..30 {
                tokio::task::yield_now().await;
            }
            let missing = matches!(error, ObservationError::NotFound(_));
            assert_eq!(backend.using::<ChangeRequest>("ops").list().await.expect("records").items.is_empty(), missing, "{error}");
            assert_eq!(refresher.inner.active.lock().await.contains_key(&subject), !missing);
            for (_, refresh) in refresher.inner.active.lock().await.drain() {
                refresh.task.abort();
            }
        }
    }

    // Explicit unlink releases only that convoy's history. A second producer
    // keeps the request retained; duplicate unlink is harmless.
    #[hegel::test]
    fn unlinked_request_releases_only_the_unlinking_convoy(tc: hegel::TestCase) {
        use flotilla_resources::{ConvoySpec, ConvoyStatus};
        // Generate one to four holders, either unlink order and duplicate reconciliations.
        // This observer-local lifecycle uses the real store; no request routing changes.
        let holders = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(4));
        let reverse = tc.draw(hegel::generators::booleans());
        let repeats = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
        runtime.block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let refresher = ChangeRequestRefresher::new(
                backend.clone(),
                "node".into(),
                Arc::new(UnavailableSource),
                ChangeRequestRefreshCadence::default(),
            );
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            let record = refresher.get_or_create_record(&subject, &subject.record_name()).await.expect("record");
            let convoys = backend.using::<Convoy>("ops");
            let linked = flotilla_protocol::Subject {
                kind: ConvoySubjectKind::ChangeRequest,
                source: IssueSource { service: "github.com".into(), scope: "org/repo".into() },
                id: "42".into(),
            };
            for i in 0..holders {
                let name = format!("producer-{i}");
                let convoy = convoys
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &ConvoySpec::builder().workflow_ref("dev".into()).project_ref("flotilla".into()).role("coder".into()).build(),
                    )
                    .await
                    .expect("convoy");
                let mut status = ConvoyStatus::default();
                status.discover_subject(
                    linked.clone(),
                    Relationship::Produces,
                    flotilla_resources::SubjectDiscoverySource::Operator,
                    Utc::now(),
                );
                convoys.update_status(&name, &convoy.metadata.resource_version, &status).await.expect("link");
            }
            // Each holder's history, as the record held it when written beside the convoys.
            let mut spec = record.spec.clone();
            for i in 0..holders {
                spec.subject_of.push(
                    ChangeRequestSubjectHistory::builder()
                        .namespace("ops".into())
                        .convoy(format!("producer-{i}"))
                        .origin("kiwi".into())
                        .relationship(Relationship::Produces)
                        .role("coder".into())
                        .last_seen(Utc::now())
                        .build(),
                );
            }
            let records = backend.using::<ChangeRequest>("ops");
            records.update(&InputMeta::from(&record.metadata), &record.metadata.resource_version, &spec).await.expect("history");
            let mut order = (0..holders).collect::<Vec<_>>();
            if reverse {
                order.reverse();
            }
            for (step, i) in order.iter().enumerate() {
                let name = format!("producer-{i}");
                let convoy = convoys.get(&name).await.expect("convoy");
                let mut status = convoy.status.expect("status");
                status.unlink_subject(&linked);
                convoys.update_status(&name, &convoy.metadata.resource_version, &status).await.expect("unlink");
                // Reconciliation observes explicit unlink assertions, including duplicates.
                for _ in 0..repeats {
                    refresher.reconcile_convoy_demands("ops", &mut ConvoyDemands::default()).await.expect("reconcile");
                }
                let records = backend.using::<ChangeRequest>("ops").list().await.expect("records").items;
                if step + 1 == holders {
                    assert!(records.is_empty(), "last unlink collects the record");
                } else {
                    assert_eq!(records.len(), 1);
                    assert_eq!(records[0].spec.subject_of.len(), holders - step - 1);
                    for remaining in &order[step + 1..] {
                        assert!(records[0].spec.subject_of.iter().any(|entry| entry.convoy == format!("producer-{remaining}")));
                    }
                }
            }
            for (_, refresh) in refresher.inner.active.lock().await.drain() {
                refresh.task.abort();
            }
        });
    }

    // An explicit-unlink pass must leave unrelated observations alone, regardless
    // of their authority. Startup GC is a separate policy for history-less rows.
    #[tokio::test]
    async fn unlink_reconciliation_keeps_unrelated_historyless_records() {
        for authority in ["node", "peer"] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let refresher = ChangeRequestRefresher::new(
                backend.clone(),
                "node".into(),
                Arc::new(UnavailableSource),
                ChangeRequestRefreshCadence::default(),
            );
            let records = backend.using::<ChangeRequest>("ops");
            records
                .create(
                    &InputMeta::builder().name("unrelated".into()).build(),
                    &ChangeRequestSpec::builder()
                        .service("github.com".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority(authority.into())
                        .build(),
                )
                .await
                .unwrap();
            refresher.reconcile_convoy_demands("ops", &mut ConvoyDemands::default()).await.unwrap();
            assert_eq!(records.get("unrelated").await.unwrap().spec.observing_authority, authority);
        }
    }

    // Network boundary: pause a missing-PR response so a new demand can be
    // admitted before collection. The demand must keep its record and diagnostics.
    struct PausedMissingSource {
        started: Notify,
        proceed: Notify,
    }
    #[async_trait::async_trait]
    impl ChangeRequestObservationSource for PausedMissingSource {
        async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            self.started.notify_one();
            self.proceed.notified().await;
            Err(ObservationError::NotFound("missing".into()))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn demand_arriving_during_missing_collection_preserves_observation() {
        for collection_first in [false, true] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let source = Arc::new(PausedMissingSource { started: Notify::new(), proceed: Notify::new() });
            let refresher =
                ChangeRequestRefresher::new(backend.clone(), "node".into(), source.clone(), ChangeRequestRefreshCadence::default());
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).unwrap(),
            )
            .unwrap();
            refresher.get_or_create_record(&subject, &subject.record_name()).await.unwrap();
            refresher.start_retained(subject.clone()).await;
            source.started.notified().await;
            let demand = uuid::Uuid::new_v4();
            if collection_first {
                let lock = refresher.subject_lock(&subject).await;
                let guard = lock.lock().await;
                source.proceed.notify_one();
                // The missing response queues cleanup on the subject lock.
                for _ in 0..30 {
                    tokio::task::yield_now().await;
                }
                let resumed = refresher.clone();
                let demanded = subject.clone();
                let admission = tokio::spawn(async move { resumed.demand(demand, demanded, None).await });
                tokio::task::yield_now().await;
                drop(guard);
                admission.await.unwrap().unwrap();
                // Cleanup ran first; admission must recreate and re-arm demand.
                source.started.notified().await;
            } else {
                refresher.demand(demand, subject.clone(), None).await.unwrap();
            }
            source.proceed.notify_one();
            for _ in 0..30 {
                tokio::task::yield_now().await;
            }
            assert_eq!(refresher.active_demands().await, 1);
            assert!(backend.using::<ChangeRequest>("ops").get(&subject.record_name()).await.is_ok());
            assert!(matches!(refresher.observation_error(&subject).await, Some(ObservationError::NotFound(_))));
            refresher.release(demand).await;
        }
    }

    // #3633: explicit unlink releases only that convoy's claim. Other convoy
    // claims keep the request retained until the last explicit unlink.
    #[tokio::test]
    async fn explicit_unlinks_release_only_their_own_retention_claims() {
        use flotilla_resources::{ConvoySpec, ConvoyStatus, SubjectDiscoverySource};
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let subject = ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 70, &[]).unwrap(),
        )
        .unwrap();
        let link = flotilla_resources::change_request_subject(
            &ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("org/repo".into())
                .number(70)
                .observing_authority("node".into())
                .build(),
        );
        let records = backend.using::<ChangeRequest>("ops");
        let mut spec = ChangeRequestSpec::builder()
            .service("github.com".into())
            .scope("org/repo".into())
            .number(70)
            .observing_authority("node".into())
            .build();
        for name in ["one", "two"] {
            spec.subject_of.push(
                ChangeRequestSubjectHistory::builder()
                    .namespace("ops".into())
                    .convoy(name.into())
                    .origin("kiwi".into())
                    .relationship(Relationship::Produces)
                    .role("coder".into())
                    .last_seen(Utc::now())
                    .build(),
            );
        }
        records.create(&InputMeta::builder().name(subject.record_name()).build(), &spec).await.unwrap();
        let convoys = backend.using::<Convoy>("ops");
        // An earlier convoy's unresolvable unlink must not skip
        // the independent cleanup for the following convoys.
        let broken = convoys
            .create(
                &InputMeta::builder().name("00-broken".into()).build(),
                &ConvoySpec::builder().workflow_ref("dev".into()).project_ref("flotilla".into()).role("coder".into()).build(),
            )
            .await
            .unwrap();
        let mut malformed = link.clone();
        malformed.source.service = "undeclared.invalid".into();
        let mut broken_status = ConvoyStatus::default();
        broken_status.unlink_subject(&malformed);
        convoys.update_status("00-broken", &broken.metadata.resource_version, &broken_status).await.unwrap();
        for name in ["one", "two"] {
            let convoy = convoys
                .create(
                    &InputMeta::builder().name(name.into()).build(),
                    &ConvoySpec::builder().workflow_ref("dev".into()).project_ref("flotilla".into()).role("coder".into()).build(),
                )
                .await
                .unwrap();
            let mut status = ConvoyStatus::default();
            status.discover_subject(link.clone(), Relationship::Produces, SubjectDiscoverySource::Operator, Utc::now());
            status.unlink_subject(&link);
            convoys.update_status(name, &convoy.metadata.resource_version, &status).await.unwrap();
            let refresher = ChangeRequestRefresher::new(
                backend.clone(),
                "node".into(),
                Arc::new(UnavailableSource),
                ChangeRequestRefreshCadence::default(),
            );
            refresher.reconcile_convoy_demands("ops", &mut ConvoyDemands::default()).await.unwrap();
            if name == "one" {
                let remaining = records.get(&subject.record_name()).await.unwrap();
                assert_eq!(remaining.spec.subject_of.len(), 1);
                assert_eq!(remaining.spec.subject_of[0].convoy, "two");
            } else {
                assert!(matches!(records.get(&subject.record_name()).await, Err(ResourceError::NotFound { .. })));
            }
        }
    }

    // HTTP boundary: an invalid endpoint deterministically refuses backend I/O,
    // without a server or a live request. Cleanup errors must not retire monitoring.
    #[tokio::test]
    async fn failed_collection_keeps_retained_monitoring() {
        let backend = ResourceBackend::new(HttpBackend::new(rigging_tls::client(), "invalid://resource-store"));
        let refresher =
            ChangeRequestRefresher::new(backend, "node".into(), Arc::new(UnavailableSource), ChangeRequestRefreshCadence::default());
        let subject = ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
        )
        .expect("key");
        refresher.start_retained(subject.clone()).await;
        assert!(refresher.collect_local_subject(&subject).await.is_err());
        assert!(refresher.collect_retained_if_terminal(&subject).await.is_err());
        assert!(refresher.inner.active.lock().await.contains_key(&subject));
        for (_, refresh) in refresher.inner.active.lock().await.drain() {
            refresh.task.abort();
        }
    }

    // Behaviour (#2445): a request a convoy produced, with that link in its history (as an
    // imported record holds it), survives demand release and daemon restart, then terminal
    // polling collects it.
    #[tokio::test(start_paused = true)]
    async fn produced_request_survives_release_and_restart_until_terminal() {
        for terminal in [ObservedChangeRequestState::Merged, ObservedChangeRequestState::Closed] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let observed_at = Utc::now();
            let opened = parse_gh_observation(
                r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
                observed_at,
            )
            .expect("open");
            let source = Arc::new(MutableSource(Mutex::new(opened), AtomicUsize::new(0)));
            let refresher =
                ChangeRequestRefresher::new(backend.clone(), "node".into(), source.clone(), ChangeRequestRefreshCadence::default());
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            let record = refresher.get_or_create_record(&subject, &subject.record_name()).await.expect("record");
            let mut spec = record.spec.clone();
            spec.subject_of.push(
                ChangeRequestSubjectHistory::builder()
                    .namespace("ops".into())
                    .convoy("producer".into())
                    .origin("kiwi".into())
                    .relationship(Relationship::Produces)
                    .role("coder".into())
                    .project("platform".into())
                    .last_seen(observed_at)
                    .build(),
            );
            let records = backend.using::<ChangeRequest>("ops");
            records.update(&InputMeta::from(&record.metadata), &record.metadata.resource_version, &spec).await.expect("history");
            let demand = uuid::Uuid::new_v4();
            refresher.demand(demand, subject.clone(), None).await.expect("demand");
            refresher.refresh_once(&subject).await.expect("initial observation");
            let record = records.get(&subject.record_name()).await.expect("request");
            assert_eq!(record.spec.subject_of.len(), 1);
            assert_eq!(record.spec.subject_of[0].convoy, "producer");
            let next_observation = source.1.load(Ordering::SeqCst) + 1;
            refresher.release(demand).await;
            assert!(records.get(&subject.record_name()).await.is_ok());
            assert_eq!(refresher.active_demands().await, 0);
            wait_for_observation(&source, next_observation).await;
            source.0.lock().await.state = Observation::known(ObservedChangeRequestState::Draft, observed_at);
            // Releasing an unrelated demand must not restart retained polling.
            refresher.release(uuid::Uuid::new_v4()).await;
            tokio::time::advance(Duration::from_secs(14 * 60)).await;
            assert_eq!(
                records.get(&subject.record_name()).await.expect("slow poll").status.expect("status").state.value,
                Some(ObservedChangeRequestState::Open)
            );
            let resumed = uuid::Uuid::new_v4();
            refresher.demand(resumed, subject.clone(), None).await.expect("reactivate ordinary demand");
            wait_for_request_state(&records, &subject.record_name(), Some(ObservedChangeRequestState::Draft)).await;
            assert_eq!(
                records.get(&subject.record_name()).await.expect("reactivated").status.expect("status").state.value,
                Some(ObservedChangeRequestState::Draft)
            );
            refresher.release(resumed).await;
            let observations = SubjectCatalogInput {
                change_requests: vec![records.get(&subject.record_name()).await.expect("retained")],
                ..Default::default()
            };
            let catalog = project_catalog(
                &CatalogInput {
                    subjects: Some(&observations),
                    awareness: None,
                    convoys: &[],
                    independents: &[],
                    standing_roles: &[],
                    project_repositories: &[],
                },
                &FlotillaRecipes::new("flotilla"),
            );
            let patches = catalog.reassert_patches();
            let cr = entity::change_request("github.com", "org/repo", "42");
            let patch = patches.iter().find(|patch| patch.target == MetadataTarget::Entity(cr.clone())).expect("orphan");
            assert_eq!(patch.set["flotilla.orphaned"].value, MetadataValue::Bool(true));
            assert_eq!(
                patch.set["flotilla.subject_of.produces"].value,
                MetadataValue::EntityRefs(vec![entity::convoy("ops", "producer", "kiwi")])
            );
            // A restart loses all in-memory demand/tasks but retains the store.
            for (_, refresh) in refresher.inner.active.lock().await.drain() {
                refresh.task.abort();
            }
            let recovered =
                ChangeRequestRefresher::new(backend.clone(), "node".into(), source.clone(), ChangeRequestRefreshCadence::default());
            recovered.garbage_collect_orphans().await.expect("recover");
            assert!(records.get(&subject.record_name()).await.is_ok());
            source.0.lock().await.state = Observation::known(terminal, observed_at + chrono::Duration::seconds(1));
            wait_for_request_state(&records, &subject.record_name(), None).await;
            assert!(matches!(records.get(&subject.record_name()).await, Err(ResourceError::NotFound { .. })));
            let observations = SubjectCatalogInput { change_requests: records.list().await.expect("list").items, ..Default::default() };
            let catalog = project_catalog(
                &CatalogInput {
                    subjects: Some(&observations),
                    awareness: None,
                    convoys: &[],
                    independents: &[],
                    standing_roles: &[],
                    project_repositories: &[],
                },
                &FlotillaRecipes::new("flotilla"),
            );
            assert!(!catalog.reassert_patches().iter().any(|patch| patch.target == MetadataTarget::Entity(cr.clone())));
        }
    }

    #[tokio::test]
    async fn last_released_demand_garbage_collects_observed_record() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1366, &[]).expect("address"),
        )
        .expect("key");
        refresher.get_or_create_record(&subject, &subject.record_name()).await.expect("observed record");
        let first = uuid::Uuid::new_v4();
        let last = uuid::Uuid::new_v4();
        refresher.demand(first, subject.clone(), None).await.expect("first demand");
        refresher.demand(last, subject.clone(), None).await.expect("last demand");
        assert_eq!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.len(), 1);

        refresher.release(first).await;
        assert_eq!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.len(), 1);

        refresher.release(last).await;
        assert!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.is_empty());
    }

    #[tokio::test]
    async fn startup_garbage_collection_covers_non_default_namespaces() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1366, &[]).expect("address"),
        )
        .expect("key");
        refresher.get_or_create_record(&subject, &subject.record_name()).await.expect("observed record");
        refresher.demand(uuid::Uuid::new_v4(), subject, None).await.expect("non-default namespace demand");
        assert_eq!(backend.local_namespaces::<ChangeRequest>().await.expect("local namespaces"), vec!["ops"]);

        refresher.garbage_collect_orphans().await.expect("startup garbage collection");

        assert!(backend.using::<ChangeRequest>("ops").list().await.expect("list ops CRs").items.is_empty());
        assert!(backend.local_namespaces::<ChangeRequest>().await.expect("local namespaces after GC").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn identical_polls_do_not_write_status() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1366, &[]).expect("address"),
        )
        .expect("key");
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.expect("demand");
        wait_for_calls(&calls, 1).await;
        let records = backend.using::<ChangeRequest>("flotilla");
        let first = records.get(&subject.record_name()).await.expect("first observation");

        const IDENTICAL_POLLS: usize = 4;
        for poll in 0..IDENTICAL_POLLS {
            tokio::time::advance(Duration::from_secs(90)).await;
            wait_for_calls(&calls, poll + 2).await;
        }

        let after = records.get(&subject.record_name()).await.expect("observation after identical polls");
        assert_eq!(calls.load(Ordering::SeqCst), IDENTICAL_POLLS + 1);
        assert_eq!(after.metadata.resource_version, first.metadata.resource_version, "identical observed values must produce no writes");
        assert_eq!(after.status, first.status, "poll timestamps are not persisted unless an observed value changes");
    }

    #[tokio::test(start_paused = true)]
    async fn relay_health_slows_polling_and_disconnect_restores_normal_cadence() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        refresher.set_relay_healthy(true);
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("GitHub.com", "Flotilla-Org/Flotilla", 2051, &[]).expect("address"),
        )
        .expect("key");
        refresher.demand(uuid::Uuid::new_v4(), subject, None).await.expect("demand");
        wait_for_calls(&calls, 1).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(90)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "healthy relay uses slow backstop");
        refresher.set_relay_healthy(false);
        wait_for_calls(&calls, 2).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "disconnect wakes the normal refresher");
        tokio::time::advance(Duration::from_secs(90)).await;
        wait_for_calls(&calls, 3).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn relay_hint_matches_demands_in_each_namespace_without_recreating_missing_records() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        for namespace in ["flotilla", "ops"] {
            refresher
                .demand(
                    uuid::Uuid::new_v4(),
                    ChangeRequestKey::new(
                        namespace,
                        flotilla_resources::address::stored_change_request_address("GitHub.com", "Flotilla-Org/Flotilla", 2051, &[])
                            .expect("address"),
                    )
                    .expect("key"),
                    None,
                )
                .await
                .expect("demand");
        }
        wait_for_calls(&calls, 2).await;
        let before = calls.load(Ordering::SeqCst);
        let hint =
            flotilla_address::Address::change_request(&flotilla_address::Forge::github(), "flotilla-org/flotilla", 2051).expect("hint");
        refresher.refresh_hint(&hint).await.expect("hint");
        assert_eq!(calls.load(Ordering::SeqCst), before + 2, "both namespaces refresh once");

        let name = cr_name("github.com", "flotilla-org/flotilla", 2051);
        backend.using::<ChangeRequest>("ops").delete(&name).await.expect("delete record during live demand");
        refresher.refresh_hint(&hint).await.expect("hint after record deletion");
        assert!(
            matches!(backend.using::<ChangeRequest>("ops").get(&name).await, Err(ResourceError::NotFound { .. })),
            "hint does not recreate a missing record"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before + 3, "remaining namespace still refreshes");
    }

    // A hint spells public GitHub by host (`cr/github.com/...`) while a
    // stored subject may spell it by forge id (`github`) in any repository
    // case. Both are one address, so the hint refreshes that demand.
    #[tokio::test(start_paused = true)]
    async fn host_spelled_relay_hint_matches_a_forge_id_spelled_demand() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        let address =
            flotilla_resources::address::stored_change_request_address("github", "Flotilla-Org/Flotilla", 2053, &[]).expect("address");
        refresher.demand(uuid::Uuid::new_v4(), ChangeRequestKey::new("flotilla", address).expect("key"), None).await.expect("demand");
        wait_for_calls(&calls, 1).await;
        let before = calls.load(Ordering::SeqCst);

        let other = flotilla_address::Address::change_request(&flotilla_address::Forge::github(), "flotilla-org/flotilla", 2054)
            .expect("other hint");
        refresher.refresh_hint(&other).await.expect("other hint");
        assert_eq!(calls.load(Ordering::SeqCst), before, "another change request's hint refreshes nothing");

        let hint =
            flotilla_address::Address::change_request(&flotilla_address::Forge::github(), "flotilla-org/flotilla", 2053).expect("hint");
        refresher.refresh_hint(&hint).await.expect("hint");
        assert_eq!(calls.load(Ordering::SeqCst), before + 1, "the host-spelled hint names the forge-id-spelled demand");
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_demand_heartbeats_identical_observations() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1699, &[]).expect("address"),
        )
        .expect("key");
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), Some(Utc::now())).await.expect("freshness demand");
        wait_for_calls(&calls, 1).await;
        let records = backend.using::<ChangeRequest>("flotilla");
        let first = records.get(&subject.record_name()).await.expect("first observation");

        tokio::time::advance(Duration::from_secs(10)).await;
        wait_for_calls(&calls, 2).await;
        let heartbeat = records.get(&subject.record_name()).await.expect("heartbeat observation");

        assert_ne!(heartbeat.metadata.resource_version, first.metadata.resource_version);
        assert!(heartbeat.status.expect("heartbeat status").state.observed_at > first.status.expect("first status").state.observed_at);
    }

    #[tokio::test(start_paused = true)]
    async fn late_freshness_demand_preempts_existing_state_cadence_sleep() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence {
                state: Duration::from_secs(90),
                checks_pending: Duration::from_secs(15),
                freshness_demanded: Duration::from_secs(10),
                stale_after: Duration::from_secs(180),
            },
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1366, &[]).expect("address"),
        )
        .expect("key");
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.expect("initial demand");
        wait_for_calls(&calls, 1).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // #3412: grace follows the observer's scheduled refresh plus its own
        // cadence; a new demand must replace the slow schedule with the fast one.
        let now = Utc::now();
        let deadline = refresher.refresh_grace_deadline(&subject, None, now).await;
        assert!(deadline >= now + chrono::Duration::seconds(179));
        assert!(deadline <= now + chrono::Duration::seconds(181));

        tokio::time::advance(Duration::from_secs(5)).await;
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), Some(Utc::now())).await.expect("late freshness demand");
        wait_for_calls(&calls, 2).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must preempt the remaining 85-second sleep");
        let now = Utc::now();
        let deadline = refresher.refresh_grace_deadline(&subject, Some(now), now).await;
        assert!(deadline >= now + chrono::Duration::seconds(19));
        assert!(deadline <= now + chrono::Duration::seconds(21));
    }

    #[derive(Debug, Default)]
    struct RecordedFailures(std::sync::Mutex<Vec<(String, String)>>);

    #[async_trait]
    impl flotilla_resources::engine_records::ObservationFailures for RecordedFailures {
        async fn failing(&self, kind: &str, name: &str) -> Result<(), String> {
            self.0.lock().expect("failures").push((kind.to_owned(), name.to_owned()));
            Ok(())
        }
    }

    // Behaviour (observation ruling): under the engine, a host that does not observe
    // a change request writes nothing for it: no local copy and no convoy history, however a
    // local convoy links it.
    #[tokio::test]
    async fn under_the_engine_only_the_observer_writes() {
        use flotilla_protocol::{IssueSource, NodeId, Relationship, Subject, SubjectKind};
        use flotilla_resources::{Convoy, ConvoySpec, DeclaredSubject};
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let remote = ResourceBackend::new(InMemoryBackend::default());
        let subject = ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
        )
        .expect("key");
        let remote_records = remote.using::<ChangeRequest>("ops");
        let spec = ChangeRequestSpec::builder()
            .service(subject.stored_service_scope().0)
            .scope(subject.stored_service_scope().1)
            .number(subject.number())
            .observing_authority("remote".into())
            .build();
        let record = remote_records.create(&InputMeta::builder().name(subject.record_name()).build(), &spec).await.expect("remote record");
        let observed = parse_gh_observation(r#"{"state":"OPEN"}"#, Utc::now()).expect("observation");
        remote_records.update_status(&subject.record_name(), &record.metadata.resource_version, &observed).await.expect("observation");
        backend
            .replica_writer::<ChangeRequest>(NodeId::new("remote"), "ops")
            .replace(&remote_records.list().await.expect("remote list"), Utc::now())
            .await
            .expect("replica");
        let produces = DeclaredSubject {
            subject: Subject {
                kind: SubjectKind::ChangeRequest,
                source: IssueSource { service: subject.stored_service_scope().0, scope: subject.stored_service_scope().1 },
                id: "42".into(),
            },
            relationship: Relationship::Produces,
            issue: None,
            change_request: None,
        };
        let convoy = ConvoySpec::builder()
            .workflow_ref("dev".into())
            .project_ref("flotilla".into())
            .role("coder".into())
            .subjects(vec![produces])
            .build();
        backend.using::<Convoy>("ops").create(&InputMeta::builder().name("producer".into()).build(), &convoy).await.expect("convoy");
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "local".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        refresher.engine().install(Arc::new(RecordedFailures::default())).expect("engine");
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject.clone(), None).await.expect("demand");

        // A change request no home holds yet, whose source a Project at `remote` declares: the
        // demand leaves its record to `remote`.
        let binding = flotilla_resources::IssueSourceBindingSpec::builder()
            .source(IssueSource { service: "https://github.com".into(), scope: "org/repo".into() })
            .build();
        let project = flotilla_resources::ProjectSpec::builder().display_name("ops".into()).issue_source_bindings(vec![binding]).build();
        let projects = remote.using::<flotilla_resources::Project>("ops");
        projects.create(&InputMeta::builder().name("ops".into()).build(), &project).await.expect("remote project");
        backend
            .replica_writer::<flotilla_resources::Project>(NodeId::new("remote"), "ops")
            .replace(&projects.list().await.expect("remote projects"), Utc::now())
            .await
            .expect("project replica");
        // Only a host that has sent a heartbeat is a candidate observer (Q3): `remote` has.
        let hosts = remote.using::<flotilla_resources::Host>("ops");
        let host = hosts.create(&InputMeta::builder().name("remote-host".into()).build(), &Default::default()).await.expect("remote host");
        let status = flotilla_resources::HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() };
        hosts.update_status("remote-host", &host.metadata.resource_version, &status).await.expect("remote heartbeat");
        backend
            .replica_writer::<flotilla_resources::Host>(NodeId::new("remote"), "ops")
            .replace(&hosts.list().await.expect("remote hosts"), Utc::now())
            .await
            .expect("host replica");
        let unheld = ChangeRequestKey::new(
            "ops",
            flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 43, &[]).expect("address"),
        )
        .expect("key");
        let unheld_demand = uuid::Uuid::new_v4();
        refresher.demand(unheld_demand, unheld, None).await.expect("demand");
        tokio::task::yield_now().await;
        assert!(backend.using::<ChangeRequest>("ops").list().await.expect("local records").items.is_empty());
        refresher.release(demand).await;
        refresher.release(unheld_demand).await;
    }

    // Behaviour: the sweep deletes this home's records nothing demands, and keeps a demanded one.
    #[tokio::test]
    async fn the_sweep_keeps_demanded_records_and_deletes_the_rest() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        refresher.engine().install(Arc::new(RecordedFailures::default())).expect("engine");
        let records = backend.using::<ChangeRequest>("flotilla");
        let key = |number| {
            ChangeRequestKey::new(
                "flotilla",
                flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", number, &[])
                    .expect("address"),
            )
            .expect("key")
        };
        let (demanded, orphan) = (key(1), key(2));
        for subject in [&demanded, &orphan] {
            let spec = ChangeRequestSpec::builder()
                .service(subject.stored_service_scope().0)
                .scope(subject.stored_service_scope().1)
                .number(subject.number())
                .observing_authority("authority".into())
                .build();
            records.create(&InputMeta::builder().name(subject.record_name()).build(), &spec).await.expect("record");
        }
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, demanded.clone(), None).await.expect("demand");
        refresher.sweep_undemanded().await.expect("sweep");
        let left: Vec<_> = records.list().await.expect("list").items.into_iter().map(|record| record.metadata.name).collect();
        assert_eq!(left, [demanded.record_name()]);
        refresher.release(demand).await;
    }

    // Behaviour (age as a judgement): under the engine, the observer records a failed
    // read on its record, so readers age its values from the first failure.
    #[tokio::test]
    async fn under_the_engine_a_failed_read_is_recorded() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let failures = Arc::new(RecordedFailures::default());
        refresher.engine().install(failures.clone()).expect("engine");
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1699, &[]).expect("address"),
        )
        .expect("key");
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject.clone(), Some(Utc::now())).await.expect("demand");
        tokio::time::timeout(Duration::from_secs(1), async {
            while failures.0.lock().expect("failures").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the failed read is recorded");
        assert_eq!(failures.0.lock().expect("failures")[0], ("ChangeRequest".to_owned(), subject.record_name()));
        refresher.release(demand).await;
    }

    #[tokio::test]
    async fn observation_failure_is_available_to_settlement_diagnostics() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "flotilla-org/flotilla", 1699, &[]).expect("address"),
        )
        .expect("key");
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject.clone(), Some(Utc::now())).await.expect("demand");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if refresher.observation_error(&subject).await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed observation should become diagnostic evidence");
        assert_eq!(refresher.observation_error(&subject).await, Some(ObservationError::Failed("unavailable".into())));

        refresher.release(demand).await;
        assert_eq!(refresher.observation_error(&subject).await, None);
    }
}
