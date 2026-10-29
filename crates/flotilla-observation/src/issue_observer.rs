use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_address::{Address, Kind};
use flotilla_resources::{
    address::{record_name, stored_service_scope},
    InputMeta, Issue, IssueSpec, IssueStatus, ResourceProvenance,
};
use flotilla_store::ResourceBackend;
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use crate::engine_observers::EngineObservers;

const KIND: &str = "Issue";

/// One issue observed in one namespace: the issue observer's key.
///
/// Its identity is the issue's [`Address`], so every spelling of one issue
/// is one observation subject with one record. Its fields are private
/// because it keys maps: [`IssueKey::new`] is the only way to make one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IssueKey {
    namespace: String,
    address: Address,
}

impl IssueKey {
    /// The key for an issue address; any other address is refused.
    ///
    /// It checks the whole shape the accessors read (a forge, a repository,
    /// a number, a record name and a stored `(service, scope)`), so the
    /// accessors below cannot panic. The address grammar already puts an
    /// issue under a repository under a forge; this check states it here.
    pub fn new(namespace: impl Into<String>, address: Address) -> Result<Self, String> {
        if address.kind() != Kind::Issue {
            return Err(format!("{address} is not an issue"));
        }
        if address.get(Kind::Forge).is_none()
            || address.get(Kind::Repo).is_none()
            || address.number().is_none()
            || record_name(&address).is_none()
            || stored_service_scope(&address).is_none()
        {
            return Err(format!("{address} is not an issue under a forge repository"));
        }
        Ok(Self { namespace: namespace.into(), address })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The forge id: `github` for public GitHub.
    pub fn forge_id(&self) -> &str {
        self.address.get(Kind::Forge).expect("IssueKey::new checked the forge")
    }

    /// The repository's `owner/repo`, in its forge's case.
    pub fn repository_path(&self) -> &str {
        self.address.get(Kind::Repo).expect("IssueKey::new checked the repository")
    }

    pub fn number(&self) -> u64 {
        self.address.number().expect("IssueKey::new checked the number")
    }

    /// The name of the record that observes this issue.
    pub(crate) fn record_name(&self) -> String {
        record_name(&self.address).expect("IssueKey::new checked the record name")
    }

    /// The `(service, scope)` its record spec stores.
    pub(crate) fn stored_service_scope(&self) -> (String, String) {
        stored_service_scope(&self.address).expect("IssueKey::new checked the stored spelling")
    }
}

#[async_trait]
pub trait IssueObservationSource: Send + Sync {
    async fn observe(&self, subject: &IssueKey) -> Result<IssueStatus, String>;
}

#[derive(Debug, Clone, Copy)]
pub struct IssueRefreshCadence {
    pub state: Duration,
    pub freshness_demanded: Duration,
    pub stale_after: Duration,
}

impl Default for IssueRefreshCadence {
    fn default() -> Self {
        Self { state: Duration::from_secs(90), freshness_demanded: Duration::from_secs(10), stale_after: Duration::from_secs(180) }
    }
}

struct ActiveRefresh {
    demands: HashMap<uuid::Uuid, Option<DateTime<Utc>>>,
    wake: Arc<Notify>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct IssueRefresher {
    inner: Arc<IssueRefresherInner>,
}

struct IssueRefresherInner {
    backend: ResourceBackend,
    authority: String,
    source: Arc<dyn IssueObservationSource>,
    cadence: IssueRefreshCadence,
    active: Mutex<HashMap<IssueKey, ActiveRefresh>>,
    observation_errors: Mutex<HashMap<IssueKey, String>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
    engine: EngineObservers,
}

impl IssueRefresher {
    pub fn new(backend: ResourceBackend, authority: String, source: Arc<dyn IssueObservationSource>, cadence: IssueRefreshCadence) -> Self {
        Self {
            inner: Arc::new(IssueRefresherInner {
                backend,
                authority,
                source,
                cadence,
                active: Mutex::new(HashMap::new()),
                observation_errors: Mutex::new(HashMap::new()),
                relay_healthy: std::sync::atomic::AtomicBool::new(false),
                relay_wake: Notify::new(),
                engine: EngineObservers::default(),
            }),
        }
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
        let namespaces = self.inner.backend.local_namespaces::<Issue>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            let records = self.inner.backend.using::<Issue>(&namespace);
            let demanded: std::collections::HashSet<String> = self.inner.active.lock().await.keys().map(IssueKey::record_name).collect();
            for record in records.list().await.map_err(|error| error.to_string())?.items {
                if demanded.contains(&record.metadata.name) {
                    continue;
                }
                match records.delete(&record.metadata.name).await {
                    Ok(()) | Err(flotilla_resources::ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    pub async fn observation_error(&self, subject: &IssueKey) -> Option<String> {
        self.inner.observation_errors.lock().await.get(subject).cloned()
    }

    /// Refresh a claim-time observation even before Landing has armed its
    /// standing leaf subscriptions.
    pub async fn refresh_once(&self, subject: &IssueKey) -> Result<(), String> {
        self.refresh_once_with_creation(subject, true).await
    }

    async fn refresh_once_with_creation(&self, subject: &IssueKey, create_missing: bool) -> Result<(), String> {
        let name = subject.record_name();
        if !self.owns_record(subject, false, create_missing).await? {
            return Ok(());
        }
        let status = match self.inner.source.observe(subject).await {
            Ok(status) => status,
            Err(error) => {
                self.inner.engine.failed(KIND, &name).await;
                return Err(error);
            }
        };
        self.publish(subject, &name, status, true, false, create_missing).await
    }

    pub async fn demand(&self, subscription_id: uuid::Uuid, subject: IssueKey, freshness: Option<DateTime<Utc>>) -> Result<(), String> {
        let name = subject.record_name();
        match self.inner.backend.including_replicas::<Issue>(&subject.namespace).get(&name).await {
            Ok(_) => {}
            Err(flotilla_resources::ResourceError::NotFound { .. }) if self.inner.engine.installed() => {}
            Err(flotilla_resources::ResourceError::NotFound { .. }) => self.ensure_record(&subject, &name).await?,
            Err(error) => return Err(error.to_string()),
        }

        let mut active = self.inner.active.lock().await;
        if let Some(refresh) = active.get_mut(&subject) {
            refresh.demands.insert(subscription_id, freshness);
            if freshness.is_some() {
                refresh.wake.notify_one();
            }
            return Ok(());
        }
        let this = self.clone();
        let task_subject = subject.clone();
        let task = tokio::spawn(async move { this.refresh_loop(task_subject).await });
        active
            .insert(subject, ActiveRefresh { demands: HashMap::from([(subscription_id, freshness)]), wake: Arc::new(Notify::new()), task });
        Ok(())
    }

    pub async fn release(&self, subscription_id: uuid::Uuid) {
        let mut active = self.inner.active.lock().await;
        let empty = active
            .iter_mut()
            .filter_map(|(subject, refresh)| {
                refresh.demands.remove(&subscription_id);
                refresh.demands.is_empty().then_some(subject.clone())
            })
            .collect::<Vec<_>>();
        let stopped =
            empty.into_iter().filter_map(|subject| active.remove(&subject).map(|refresh| (subject, refresh.task))).collect::<Vec<_>>();
        drop(active);

        for (subject, task) in stopped {
            task.abort();
            let _ = task.await;
            let active = self.inner.active.lock().await;
            if active.contains_key(&subject) {
                continue;
            }
            self.inner.observation_errors.lock().await.remove(&subject);
            let records = self.inner.backend.using::<Issue>(&subject.namespace);
            let name = subject.record_name();
            let result = match records.get(&name).await {
                Ok(record) if record.spec.observing_authority == self.inner.authority => records.delete(&name).await,
                Ok(_) | Err(flotilla_resources::ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if !matches!(error, flotilla_resources::ResourceError::NotFound { .. }) {
                    tracing::warn!(issue = %subject.address(), %error, "garbage collect undemanded issue failed");
                }
            }
            drop(active);
        }
    }

    /// The relay only acts on live demand, and the same authority check used by
    /// refresh_once is repeated just before the forge read.
    pub async fn demanded_owned(&self) -> Result<Vec<IssueKey>, String> {
        let subjects = self.inner.active.lock().await.keys().cloned().collect::<Vec<_>>();
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
        if hint.kind() != Kind::Issue {
            return Ok(());
        }
        let subjects = self.inner.active.lock().await.keys().filter(|subject| subject.address() == hint).cloned().collect::<Vec<_>>();
        let mut first_error = None;
        for subject in subjects {
            if let Err(error) = self.refresh_once_with_creation(&subject, false).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn refresh_demanded_owned(&self) -> Result<(), String> {
        let mut first_error = None;
        for subject in self.demanded_owned().await? {
            if let Err(error) = self.refresh_once_with_creation(&subject, false).await {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn set_relay_healthy(&self, healthy: bool) {
        use std::sync::atomic::Ordering;
        if self.inner.relay_healthy.swap(healthy, Ordering::SeqCst) != healthy {
            self.inner.relay_wake.notify_waiters();
        }
    }

    /// A daemon restart has no surviving leaf subscriptions, so locally
    /// authoritative observations from the previous process are all orphans.
    pub async fn garbage_collect_orphans(&self) -> Result<(), String> {
        let namespaces = self.inner.backend.local_namespaces::<Issue>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            let records = self.inner.backend.using::<Issue>(&namespace);
            for record in records.list().await.map_err(|error| error.to_string())?.items {
                records.delete(&record.metadata.name).await.map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn active_demands(&self) -> usize {
        self.inner.active.lock().await.values().map(|refresh| refresh.demands.len()).sum()
    }

    async fn refresh_loop(&self, subject: IssueKey) {
        let record_name = subject.record_name();
        loop {
            match self.owns_record(&subject, true, true).await {
                Ok(false) => {
                    if !self.wait_for_next(&subject, self.inner.cadence.state).await {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error);
                    if !self.wait_for_next(&subject, self.inner.cadence.state).await {
                        break;
                    }
                    continue;
                }
                Ok(true) => {}
            }
            match self.inner.source.observe(&subject).await {
                Ok(status) => {
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    if let Err(error) = self.publish(&subject, &record_name, status.clone(), demanded, true, true).await {
                        self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                        tracing::warn!(issue = %subject.address(), %error, "publish issue observation failed");
                    } else {
                        self.inner.observation_errors.lock().await.remove(&subject);
                    }
                    let delay = if demanded { self.inner.cadence.freshness_demanded } else { self.inner.cadence.state };
                    let delay = if self.inner.relay_healthy.load(std::sync::atomic::Ordering::SeqCst) {
                        delay.max(Duration::from_secs(15 * 60))
                    } else {
                        delay
                    };
                    if !self.wait_for_next(&subject, delay).await {
                        break;
                    }
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                    self.inner.engine.failed(KIND, &record_name).await;
                    tracing::warn!(issue = %subject.address(), %error, "issue observation failed");
                    if !self.wait_for_next(&subject, self.inner.cadence.state).await {
                        break;
                    }
                }
            }
        }
    }

    async fn owns_record(&self, subject: &IssueKey, allow_takeover: bool, create_missing: bool) -> Result<bool, String> {
        let (service, scope) = subject.stored_service_scope();
        let source = flotilla_protocol::IssueSource { service, scope };
        if let Some(owner) = crate::forge_observation::source_owner(&self.inner.backend, &subject.namespace, &source).await? {
            if owner.to_string() != self.inner.authority {
                return Ok(false);
            }
            let name = subject.record_name();
            if !create_missing {
                return Ok(self.inner.backend.using::<Issue>(&subject.namespace).get(&name).await.is_ok());
            }
            self.get_or_create_record(subject, &name).await?;
            return Ok(true);
        }
        let name = subject.record_name();
        let records = self.inner.backend.including_replicas::<Issue>(&subject.namespace);
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
        let local = self.inner.backend.using::<Issue>(&subject.namespace);
        let mut spec = record.object.spec.clone();
        spec.observing_authority = self.inner.authority.clone();
        // A former owner can keep its local record while another host owns a
        // fresher replica. Reclaim through that local record when it exists.
        let result = match local.get(&name).await {
            Ok(existing) if existing.spec.observing_authority == self.inner.authority => Ok(existing),
            Ok(existing) => local.update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &spec).await,
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                // A single conditional create claims a local copy, so an
                // interruption cannot strand a foreign shadow.
                local.create(&InputMeta::builder().name(name).build(), &spec).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(_) => Ok(true),
            Err(flotilla_resources::ResourceError::Conflict { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn wait_for_next(&self, subject: &IssueKey, delay: Duration) -> bool {
        let Some(wake) = self.inner.active.lock().await.get(subject).map(|refresh| Arc::clone(&refresh.wake)) else {
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
        subject: &IssueKey,
        name: &str,
        status: IssueStatus,
        heartbeat: bool,
        allow_takeover: bool,
        create_missing: bool,
    ) -> Result<(), String> {
        if !self.owns_record(subject, allow_takeover, create_missing).await? {
            return Ok(());
        }
        let records = self.inner.backend.using::<Issue>(&subject.namespace);
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

    async fn ensure_record(&self, subject: &IssueKey, name: &str) -> Result<(), String> {
        self.get_or_create_record(subject, name).await.map(|_| ())
    }

    async fn get_or_create_record(&self, subject: &IssueKey, name: &str) -> Result<flotilla_resources::ResourceObject<Issue>, String> {
        let records = self.inner.backend.using::<Issue>(&subject.namespace);
        match records.get(name).await {
            Ok(current) => Ok(current),
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let (service, scope) = subject.stored_service_scope();
                let spec = IssueSpec::builder()
                    .service(service)
                    .scope(scope)
                    .number(subject.number())
                    .observing_authority(self.inner.authority.clone())
                    .build();
                match records.create(&InputMeta::builder().name(name.to_string()).build(), &spec).await {
                    Ok(created) => Ok(created),
                    Err(flotilla_resources::ResourceError::Conflict { .. }) => records.get(name).await.map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn observed_values_equal(left: &IssueStatus, right: &IssueStatus) -> bool {
    left.state.value == right.state.value && left.labels.value == right.labels.value && left.updated_at.value == right.updated_at.value
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_resources::{Observation, ObservedIssueState};
    use flotilla_store_memory::InMemoryBackend;

    use super::*;

    struct CountingSource(Arc<AtomicUsize>);

    #[async_trait]
    impl IssueObservationSource for CountingSource {
        async fn observe(&self, _subject: &IssueKey) -> Result<IssueStatus, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let now = Utc::now();
            Ok(IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: Observation::known(ObservedIssueState::Open, now),
                labels: Observation::known(vec!["ready".into()], now),
                updated_at: Observation::known(now, now),
            })
        }
    }

    fn subject() -> IssueKey {
        let address = flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 2052, &[]).expect("address");
        IssueKey::new("flotilla", address).expect("issue key")
    }

    #[tokio::test]
    async fn unowned_hint_does_not_fetch_and_stale_demand_takes_over() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let records = backend.using::<Issue>("flotilla");
        let subject = subject();
        let spec = IssueSpec::builder()
            .service(subject.stored_service_scope().0)
            .scope(subject.stored_service_scope().1)
            .number(subject.number())
            .observing_authority("former".to_string())
            .build();
        let name = subject.record_name();
        let created = records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("create issue");
        let old = Utc::now() - chrono::Duration::hours(1);
        records
            .update_status(
                &name,
                &created.metadata.resource_version,
                &IssueStatus {
                    title: Default::default(),
                    assignees: Default::default(),
                    state: flotilla_resources::Observation::known(ObservedIssueState::Open, old),
                    labels: flotilla_resources::Observation::known(vec![], old),
                    updated_at: flotilla_resources::Observation::known(old, old),
                },
            )
            .await
            .expect("old status");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = IssueRefresher::new(
            backend.clone(),
            "new-owner".into(),
            Arc::new(CountingSource(calls.clone())),
            IssueRefreshCadence::default(),
        );
        let hint = flotilla_relay_protocol::parse_hint_subject("issue/github.com/flotilla-org/flotilla/2052").expect("hint");
        refresher.refresh_hint(&hint).await.expect("no demand");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject, None).await.expect("demand");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if calls.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stale takeover");
        assert_eq!(records.get(&name).await.expect("claimed").spec.observing_authority, "new-owner");
        refresher.release(demand).await;
        assert!(matches!(records.get(&name).await, Err(flotilla_resources::ResourceError::NotFound { .. })));
    }

    #[tokio::test]
    async fn fresh_foreign_authority_is_not_refreshed() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let records = backend.using::<Issue>("flotilla");
        let subject = subject();
        let name = subject.record_name();
        let spec = IssueSpec::builder()
            .service(subject.stored_service_scope().0)
            .scope(subject.stored_service_scope().1)
            .number(subject.number())
            .observing_authority("owner".to_string())
            .build();
        let created = records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("create issue");
        let now = Utc::now();
        records
            .update_status(
                &name,
                &created.metadata.resource_version,
                &IssueStatus {
                    title: Default::default(),
                    assignees: Default::default(),
                    state: Observation::known(ObservedIssueState::Open, now),
                    labels: Observation::known(vec![], now),
                    updated_at: Observation::known(now, now),
                },
            )
            .await
            .expect("fresh status");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher =
            IssueRefresher::new(backend.clone(), "reader".into(), Arc::new(CountingSource(calls.clone())), IssueRefreshCadence::default());
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject, None).await.expect("demand");
        let hint = flotilla_relay_protocol::parse_hint_subject("issue/github.com/flotilla-org/flotilla/2052").expect("hint");
        refresher.refresh_hint(&hint).await.expect("foreign hint");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        refresher.release(demand).await;
        assert_eq!(records.get(&name).await.expect("foreign record preserved").spec.observing_authority, "owner");
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

    struct FailingSource;

    #[async_trait]
    impl IssueObservationSource for FailingSource {
        async fn observe(&self, _subject: &IssueKey) -> Result<IssueStatus, String> {
            Err("unavailable".into())
        }
    }

    // Behaviour (age as a judgement): under the engine, the observer records a failed read on its
    // record, from its refresh loop and from a claim-time refresh alike.
    #[tokio::test]
    async fn under_the_engine_a_failed_read_is_recorded() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let refresher = IssueRefresher::new(backend, "authority".into(), Arc::new(FailingSource), IssueRefreshCadence::default());
        let failures = Arc::new(RecordedFailures::default());
        refresher.engine().install(failures.clone()).expect("engine");
        let subject = subject();
        assert!(refresher.refresh_once(&subject).await.is_err());
        assert_eq!(failures.0.lock().expect("failures").as_slice(), [("Issue".to_owned(), subject.record_name())]);
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject.clone(), Some(Utc::now())).await.expect("demand");
        tokio::time::timeout(Duration::from_secs(1), async {
            while failures.0.lock().expect("failures").len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the refresh loop's failed read is recorded");
        refresher.release(demand).await;
    }
}
