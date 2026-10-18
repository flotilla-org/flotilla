use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
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
    convoy_subject_rows, merge_change_request_history, retain_change_request, select_change_requests, ChangeRequest, ChangeRequestSpec,
    ChangeRequestStatus, ChangeRequestSubjectHistory, Convoy, Forge, ForgeSpec, InputMeta, ObservedChangeRequestState, ObservedChecks,
    ResourceError, ResourceProvenance,
};
use flotilla_store::{watch_resource_kind_including_replicas, ResourceBackend};
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
const HISTORY_REFRESH_INTERVAL_SECS: i64 = 60 * 60;

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
    origin: String,
    source: Arc<dyn ChangeRequestObservationSource>,
    cadence: ChangeRequestRefreshCadence,
    active: Mutex<HashMap<ChangeRequestKey, ActiveRefresh>>,
    observation_errors: Mutex<HashMap<ChangeRequestKey, ObservationError>>,
    subject_locks: Mutex<HashMap<ChangeRequestKey, Weak<Mutex<()>>>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
    grace_schedule_reads: std::sync::atomic::AtomicUsize,
}

impl ChangeRequestRefresher {
    pub fn new(
        origin: String,
        backend: ResourceBackend,
        authority: String,
        source: Arc<dyn ChangeRequestObservationSource>,
        cadence: ChangeRequestRefreshCadence,
    ) -> Self {
        Self {
            inner: Arc::new(ChangeRequestRefresherInner {
                backend,
                authority,
                origin,
                source,
                cadence,
                active: Mutex::new(HashMap::new()),
                observation_errors: Mutex::new(HashMap::new()),
                subject_locks: Mutex::new(HashMap::new()),
                relay_healthy: std::sync::atomic::AtomicBool::new(false),
                relay_wake: Notify::new(),
                grace_schedule_reads: Default::default(),
            }),
        }
    }

    /// Convoys are durable observation demand, including those homed on peers.
    /// Subscribe before listing so startup and watch recovery cannot lose demand.
    /// Only the source owner arms refreshes; forge reads stay in the ordinary
    /// observer loop and observations reach consumers through store replication.
    pub async fn watch_convoy_demands(&self, namespace: &str) {
        // Keep demands across resubscriptions: only a successful re-list can
        // prove that a consumer disappeared and release its standing demand.
        let mut demands = HashMap::new();
        let mut retry_delay = Duration::from_secs(1);
        loop {
            let mut streams = Vec::new();
            let mut subscription_error = None;
            for kind in ["Convoy", "Project", "Repository", "Host", "Forge"] {
                match watch_resource_kind_including_replicas(&self.inner.backend, namespace, kind).await {
                    Ok(watch) => streams.push(
                        watch
                            .stream
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
                'reconcile: loop {
                    if let Err(error) = self.reconcile_convoy_demands(namespace, &mut demands).await {
                        tracing::warn!(%error, "change request convoy demand reconciliation failed");
                        break;
                    }
                    retry_delay = Duration::from_secs(1);
                    match events.next().await {
                        Some(Ok(_)) => {
                            // Collapse bursts into one re-list without delaying isolated
                            // changes. Bound the drain so a busy source cannot starve it.
                            for _ in 0..64 {
                                match events.next().now_or_never() {
                                    Some(Some(Ok(_))) => {}
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

    async fn reconcile_convoy_demands(&self, namespace: &str, demands: &mut HashMap<ChangeRequestKey, uuid::Uuid>) -> Result<(), String> {
        let convoys = self
            .inner
            .backend
            .query_declared::<Convoy>(namespace, &flotilla_store::live_convoys_query())
            .await
            .map_err(|error| error.to_string())?;
        let forges = self.forges(namespace).await?;
        let mut wanted = HashSet::new();
        let mut owners = HashMap::new();
        for copy in convoys.items {
            for row in convoy_subject_rows(&copy.object, &ReferenceContext::default()) {
                if row.subject.kind != ConvoySubjectKind::ChangeRequest {
                    continue;
                }
                // Debug, not warn: this re-runs on every convoy, project and
                // Forge change, so an undeclared host would repeat each pass.
                let address = match subject_address(&row.subject, &forges) {
                    Ok(address) => address,
                    Err(error) => {
                        tracing::debug!(convoy = %copy.object.metadata.name, %error, "convoy change request subject has no address; not observed");
                        continue;
                    }
                };
                let subject = match ChangeRequestKey::new(namespace, address) {
                    Ok(subject) => subject,
                    Err(error) => {
                        tracing::debug!(convoy = %copy.object.metadata.name, %error, "convoy change request subject has no observation key");
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
        }
        for subject in &wanted {
            if let Entry::Vacant(entry) = demands.entry(subject.clone()) {
                let id = uuid::Uuid::new_v4();
                self.demand(id, subject.clone(), None).await?;
                entry.insert(id);
            }
        }
        let released = demands.keys().filter(|subject| !wanted.contains(*subject)).cloned().collect::<Vec<_>>();
        for subject in released {
            if let Some(id) = demands.remove(&subject) {
                self.release(id).await;
            }
        }
        Ok(())
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

    async fn refresh_once_with_creation(&self, subject: &ChangeRequestKey, create_missing: bool) -> Result<(), ObservationError> {
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
            let group =
                self.inner.active.lock().await.keys().filter(|candidate| same_repository(candidate, subject)).cloned().collect::<Vec<_>>();
            self.inner.source.observe_group(&group, subject).await
        };
        let status = match status {
            Ok(status) => {
                self.inner.observation_errors.lock().await.remove(subject);
                status
            }
            Err(error) => {
                self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
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
        let name = subject.record_name();
        match self.inner.backend.including_replicas::<ChangeRequest>(subject.namespace()).get(&name).await {
            Ok(_) => {}
            Err(ResourceError::NotFound { .. }) => self.ensure_record(&subject, &name).await?,
            Err(error) => return Err(error.to_string()),
        }

        self.record_convoy_history(&subject).await?;
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
        let task = tokio::spawn(async move { this.refresh_loop(task_subject).await });
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
        let task = tokio::spawn(async move { this.refresh_loop(task_subject).await });
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

    /// Capture links at admission, before a convoy can disappear. History is in
    /// the spec so ordinary forge status refreshes cannot erase it, and it follows
    /// the record's existing observation replication class.
    async fn record_convoy_history(&self, subject: &ChangeRequestKey) -> Result<(), String> {
        let convoys = self.inner.backend.using::<Convoy>(subject.namespace()).list().await.map_err(|error| error.to_string())?;
        let forges = self.forges(subject.namespace()).await?;
        let mut history = Vec::new();
        let references = ReferenceContext::default();
        for convoy in convoys.items {
            for row in convoy_subject_rows(&convoy, &references) {
                // Convoy rows keep the spelling they were admitted with; one
                // change request matches whatever its spelling.
                if row.subject.kind != ConvoySubjectKind::ChangeRequest {
                    continue;
                }
                let address = match subject_address(&row.subject, &forges) {
                    Ok(address) => address,
                    Err(error) => {
                        tracing::debug!(convoy = %convoy.metadata.name, %error, "convoy change request subject has no address; no history recorded");
                        continue;
                    }
                };
                if &address != subject.address() {
                    continue;
                }
                history.push(
                    ChangeRequestSubjectHistory::builder()
                        .namespace(convoy.metadata.namespace.clone())
                        .convoy(convoy.metadata.name.clone())
                        .origin(self.inner.origin.clone())
                        .relationship(row.relationship)
                        .role(convoy.spec.role.clone())
                        .maybe_project(convoy.spec.project_ref.clone())
                        .last_seen(Utc::now())
                        .build(),
                );
            }
        }
        if history.is_empty() {
            return Ok(());
        }
        let name = subject.record_name();
        let records = self.inner.backend.using::<ChangeRequest>(subject.namespace());
        loop {
            let current = match records.get(&name).await {
                Ok(current) => current,
                Err(ResourceError::NotFound { .. }) => {
                    // The observing authority may be remote. Keep a local copy
                    // of its record so this convoy's history can replicate too.
                    let observed = self
                        .inner
                        .backend
                        .including_replicas::<ChangeRequest>(subject.namespace())
                        .get(&name)
                        .await
                        .map_err(|error| error.to_string())?
                        .object;
                    let mut spec = observed.spec.clone();
                    merge_change_request_history(&mut spec.subject_of, &history);
                    match records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await {
                        Ok(created) => {
                            if let Some(status) = observed.status {
                                records
                                    .update_status(&name, &created.metadata.resource_version, &status)
                                    .await
                                    .map_err(|error| error.to_string())?;
                            }
                            continue;
                        }
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(error.to_string()),
                    }
                }
                Err(error) => return Err(error.to_string()),
            };
            let mut spec = current.spec.clone();
            let mut incoming = history.clone();
            for entry in &mut incoming {
                if let Some(prior) = spec.subject_of.iter().find(|prior| {
                    prior.namespace == entry.namespace
                        && prior.convoy == entry.convoy
                        && prior.origin == entry.origin
                        && prior.relationship == entry.relationship
                        && prior.role == entry.role
                        && prior.project == entry.project
                }) {
                    if entry.last_seen.signed_duration_since(prior.last_seen).num_seconds() < HISTORY_REFRESH_INTERVAL_SECS {
                        entry.last_seen = prior.last_seen;
                    }
                }
            }
            merge_change_request_history(&mut spec.subject_of, &incoming);
            if spec == current.spec {
                return Ok(());
            }
            match records.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
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
            let group =
                self.inner.active.lock().await.keys().filter(|candidate| same_repository(candidate, &subject)).cloned().collect::<Vec<_>>();
            match self.inner.source.observe_group(&group, &subject).await {
                Ok(status) => {
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    if let Err(error) = self.publish(&subject, &record_name, status.clone(), demanded, true, true).await {
                        self.inner.observation_errors.lock().await.insert(subject.clone(), ObservationError::Failed(error.clone()));
                        tracing::warn!(change_request = %subject.address(), %error, "publish change request observation failed");
                    } else {
                        self.inner.observation_errors.lock().await.remove(&subject);
                    }
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
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
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
        // A former owner can still hold its local copy after another host has
        // claimed the subject. Prefer the freshest observation across copies;
        // an exact tie is resolved deterministically by authority name.
        let record = records.get_all(&name).await.map_err(|error| error.to_string())?.items.into_iter().max_by_key(|item| {
            (
                item.object.status.as_ref().map_or(item.object.metadata.creation_timestamp, |status| status.state.observed_at),
                item.object.spec.observing_authority.clone(),
                matches!(item.provenance, ResourceProvenance::Local),
            )
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
        self.record_convoy_history(subject).await?;
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

    async fn ensure_record(&self, subject: &ChangeRequestKey, name: &str) -> Result<(), String> {
        self.get_or_create_record(subject, name).await.map(|_| ())
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
    use flotilla_resources::{ChangeRequestReviewObservation, Observation, ObservedMergeability};
    use flotilla_store::{ResourceBackend, TypedResolver};

    use super::*;
    fn cr_name(service: &str, scope: &str, number: u64) -> String {
        flotilla_resources::address::record_name(
            &flotilla_resources::address::stored_change_request_address(service, scope, number, &[]).expect("address"),
        )
        .expect("record name")
    }
    use flotilla_change_request::observation::parse_gh_observation;
    use flotilla_protocol::Subject as ConvoySubject;

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
            "fleet".into(),
            backend.clone(),
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let (result, reads) = measure_reads(refresher.reconcile_convoy_demands("flotilla", &mut HashMap::new())).await;
        result.expect("reconcile");
        assert_eq!(reads.of(&backend).objects_by_kind().get("Convoy"), Some(&10));
    }

    struct UnavailableSource;

    #[async_trait]
    impl ChangeRequestObservationSource for UnavailableSource {
        async fn observe(&self, _subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
            Err("unavailable".into())
        }
    }

    #[tokio::test]
    async fn claim_time_recovery_clears_rate_limit_error() {
        struct RecoveringSource(Mutex<Result<ChangeRequestStatus, ObservationError>>);
        #[async_trait]
        impl ChangeRequestObservationSource for RecoveringSource {
            async fn observe(&self, _: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
                self.0.lock().await.clone()
            }
        }
        let error = ObservationError::RateLimited {
            budget: "GraphQL".into(),
            retry_at: Some("2026-10-03T12:00:00Z".parse().expect("deadline")),
            detail: "rate limited".into(),
        };
        let source = Arc::new(RecoveringSource(Mutex::new(Err(error.clone()))));
        let refresher = ChangeRequestRefresher::new(
            "fleet".into(),
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
        *source.0.lock().await = Ok(parse_gh_observation(
            r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
            Utc::now(),
        )
        .expect("healthy observation"));
        refresher.refresh_once(&subject).await.expect("recovered observation");
        assert!(refresher.observation_error(&subject).await.is_none());
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
    async fn concurrent_first_demands_converge_on_one_authority_record() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
        assert_eq!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.len(), 1);
        assert_eq!(refresher.active_demands().await, 2);
    }

    // Behaviour (#2445): a local convoy keeps history even when a different
    // host observes the forge. A terminal history-bearing local copy is collectible.
    #[tokio::test(start_paused = true)]
    async fn remote_observer_does_not_lose_local_convoy_history() {
        use flotilla_protocol::{IssueSource, NodeId, Relationship, Subject, SubjectKind};
        use flotilla_resources::{Convoy, ConvoySpec, DeclaredSubject};
        for via_replica in [false, true] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let remote = ResourceBackend::new(InMemoryBackend::default());
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            let remote_records = remote.using::<ChangeRequest>("ops");
            let record = remote_records
                .create(
                    &InputMeta::builder().name(subject.record_name()).build(),
                    &ChangeRequestSpec::builder()
                        .service(subject.stored_service_scope().0)
                        .scope(subject.stored_service_scope().1)
                        .number(subject.number())
                        .observing_authority("remote".into())
                        .build(),
                )
                .await
                .expect("remote record");
            let observed = parse_gh_observation(r#"{"state":"OPEN"}"#, Utc::now()).expect("observation");
            remote_records
                .update_status(&subject.record_name(), &record.metadata.resource_version, &observed)
                .await
                .expect("remote observation");
            backend
                .replica_writer::<ChangeRequest>(NodeId::new("remote"), "ops")
                .replace(&remote_records.list().await.expect("remote list"), Utc::now())
                .await
                .expect("replica");
            backend
                .using::<Convoy>("ops")
                .create(
                    &InputMeta::builder().name("producer".into()).build(),
                    &ConvoySpec::builder()
                        .workflow_ref("dev".into())
                        .role("coder".into())
                        .subjects(vec![DeclaredSubject {
                            subject: Subject {
                                kind: SubjectKind::ChangeRequest,
                                source: IssueSource { service: subject.stored_service_scope().0, scope: subject.stored_service_scope().1 },
                                id: "42".into(),
                            },
                            relationship: Relationship::Produces,
                            issue: None,
                            change_request: None,
                        }])
                        .build(),
                )
                .await
                .expect("local convoy");
            let refresher = ChangeRequestRefresher::new(
                "kiwi".into(),
                backend.clone(),
                "local".into(),
                Arc::new(UnavailableSource),
                ChangeRequestRefreshCadence::default(),
            );
            let demand = uuid::Uuid::new_v4();
            refresher.demand(demand, subject.clone(), None).await.expect("demand");
            let records = backend.using::<ChangeRequest>("ops");
            let record = records.get(&subject.record_name()).await.expect("local history copy");
            assert_eq!(record.spec.observing_authority, "remote");
            assert_eq!(record.spec.subject_of.len(), 1);
            assert_eq!(record.spec.subject_of[0].origin, "kiwi");
            assert_eq!(record.status.as_ref().expect("copied observation").state.value, Some(ObservedChangeRequestState::Open));
            let second = uuid::Uuid::new_v4();
            refresher.demand(second, subject.clone(), None).await.expect("second demand");
            assert_eq!(
                records.get(&subject.record_name()).await.expect("history unchanged").metadata.resource_version,
                record.metadata.resource_version
            );
            backend.using::<Convoy>("ops").delete("producer").await.expect("delete producer");
            refresher.release(demand).await;
            if via_replica {
                refresher.release(second).await;
                assert!(records.get(&subject.record_name()).await.is_ok());
                let current = remote_records.get(&subject.record_name()).await.expect("remote current");
                let mut terminal = current.status.expect("remote status");
                terminal.state = Observation::known(ObservedChangeRequestState::Merged, Utc::now() + chrono::Duration::seconds(1));
                remote_records
                    .update_status(&subject.record_name(), &current.metadata.resource_version, &terminal)
                    .await
                    .expect("remote merge");
                backend
                    .replica_writer::<ChangeRequest>(NodeId::new("remote"), "ops")
                    .replace(&remote_records.list().await.expect("remote terminal list"), Utc::now())
                    .await
                    .expect("terminal replica");
                tokio::time::advance(RETAINED_REFRESH_INTERVAL).await;
                wait_for_request_state(&records, &subject.record_name(), None).await;
                assert!(!refresher.inner.active.lock().await.contains_key(&subject));
                continue;
            }
            let mut terminal = record.status.expect("status");
            terminal.state = Observation::known(ObservedChangeRequestState::Closed, Utc::now());
            records
                .update_status(&subject.record_name(), &record.metadata.resource_version, &terminal)
                .await
                .expect("terminal observation");
            refresher.release(second).await;
            assert!(records.list().await.expect("local records").items.is_empty());
        }
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

    // HTTP boundary: an invalid endpoint deterministically refuses backend I/O,
    // without a server or a live request. Cleanup errors must not retire monitoring.
    #[tokio::test]
    async fn failed_collection_keeps_retained_monitoring() {
        let backend = ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), "invalid://resource-store"));
        let refresher = ChangeRequestRefresher::new(
            "kiwi".into(),
            backend,
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
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

    // Behaviour (#2445): link history replicates with the request, survives convoy
    // deletion, demand release, and daemon restart, then terminal polling collects it.
    #[tokio::test(start_paused = true)]
    async fn produced_request_survives_release_and_restart_until_terminal() {
        use flotilla_resources::{Convoy, ConvoySpec, ConvoyStatus, SubjectDiscoverySource};
        for terminal in [ObservedChangeRequestState::Merged, ObservedChangeRequestState::Closed] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let observed_at = Utc::now();
            let opened = parse_gh_observation(
                r#"{"state":"OPEN","headRefOid":"head","statusCheckRollup":[],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
                observed_at,
            )
            .expect("open");
            let source = Arc::new(MutableSource(Mutex::new(opened), AtomicUsize::new(0)));
            let refresher = ChangeRequestRefresher::new(
                "kiwi".into(),
                backend.clone(),
                "node".into(),
                source.clone(),
                ChangeRequestRefreshCadence::default(),
            );
            let convoys = backend.using::<Convoy>("ops");
            let convoy = convoys
                .create(
                    &InputMeta::builder().name("producer".into()).build(),
                    &ConvoySpec::builder().workflow_ref("dev".into()).role("coder".into()).project_ref("platform".into()).build(),
                )
                .await
                .expect("convoy");
            let subject = ChangeRequestKey::new(
                "ops",
                flotilla_resources::address::stored_change_request_address("github.com", "org/repo", 42, &[]).expect("address"),
            )
            .expect("key");
            let mut status = ConvoyStatus::default();
            status.discover_subject(
                ConvoySubject {
                    kind: ConvoySubjectKind::ChangeRequest,
                    source: IssueSource { service: subject.stored_service_scope().0, scope: subject.stored_service_scope().1 },
                    id: "42".into(),
                },
                Relationship::Produces,
                SubjectDiscoverySource::Claim,
                observed_at,
            );
            convoys.update_status("producer", &convoy.metadata.resource_version, &status).await.expect("link");
            let demand = uuid::Uuid::new_v4();
            refresher.demand(demand, subject.clone(), None).await.expect("demand");
            refresher.refresh_once(&subject).await.expect("initial observation");
            let records = backend.using::<ChangeRequest>("ops");
            let record = records.get(&subject.record_name()).await.expect("request");
            assert_eq!(record.spec.subject_of.len(), 1);
            assert_eq!(record.spec.subject_of[0].convoy, "producer");
            assert_eq!(record.spec.subject_of[0].origin, "kiwi");
            assert_eq!(record.spec.subject_of[0].role, "coder");
            convoys.delete("producer").await.expect("delete convoy");
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
            let recovered = ChangeRequestRefresher::new(
                "kiwi".into(),
                backend.clone(),
                "node".into(),
                source.clone(),
                ChangeRequestRefreshCadence::default(),
            );
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
            "fleet".to_string(),
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
            "fleet".to_string(),
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
            "fleet".to_string(),
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
            "fleet".to_string(),
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
            "fleet".to_string(),
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
        let hint = flotilla_relay_protocol::parse_hint_subject("cr/github.com/flotilla-org/flotilla/2051").expect("hint");
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
            "fleet".to_string(),
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

        let other = flotilla_relay_protocol::parse_hint_subject("cr/github.com/flotilla-org/flotilla/2054").expect("other hint");
        refresher.refresh_hint(&other).await.expect("other hint");
        assert_eq!(calls.load(Ordering::SeqCst), before, "another change request's hint refreshes nothing");

        let hint = flotilla_relay_protocol::parse_hint_subject("cr/github.com/flotilla-org/flotilla/2053").expect("hint");
        refresher.refresh_hint(&hint).await.expect("hint");
        assert_eq!(calls.load(Ordering::SeqCst), before + 1, "the host-spelled hint names the forge-id-spelled demand");
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_demand_heartbeats_identical_observations() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
            "fleet".to_string(),
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

    #[tokio::test]
    async fn observation_failure_is_available_to_settlement_diagnostics() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
