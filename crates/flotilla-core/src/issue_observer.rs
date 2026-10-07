use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_relay_protocol::{Subject, SubjectKind};
use flotilla_resources::{issue_record_name, InputMeta, Issue, IssueSpec, IssueStatus, ResourceBackend, ResourceProvenance};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IssueRef {
    pub namespace: String,
    pub service: String,
    pub scope: String,
    pub number: u64,
}

impl IssueRef {
    pub fn from_address(namespace: &str, address: &LeafAddress) -> Option<Self> {
        let LeafAddress::Issue { service, scope, number } = address else { return None };
        Some(Self { namespace: namespace.to_string(), service: service.clone(), scope: scope.clone(), number: *number })
    }

    pub(crate) fn record_name(&self) -> String {
        let (service, scope) = Subject::normalize_scope(&self.service, &self.scope);
        issue_record_name(&service, &scope, self.number)
    }

    fn normalized(mut self) -> Self {
        (self.service, self.scope) = Subject::normalize_scope(&self.service, &self.scope);
        self
    }
}

#[async_trait]
pub trait IssueObservationSource: Send + Sync {
    async fn observe(&self, subject: &IssueRef) -> Result<IssueStatus, String>;
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
    active: Mutex<HashMap<IssueRef, ActiveRefresh>>,
    observation_errors: Mutex<HashMap<IssueRef, String>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
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
            }),
        }
    }

    pub fn stale_after(&self) -> Duration {
        self.inner.cadence.stale_after
    }

    pub async fn observation_error(&self, subject: &IssueRef) -> Option<String> {
        self.inner.observation_errors.lock().await.get(subject).cloned()
    }

    /// Refresh a claim-time observation even before Landing has armed its
    /// standing leaf subscriptions.
    pub async fn refresh_once(&self, subject: &IssueRef) -> Result<(), String> {
        self.refresh_once_with_creation(subject, true).await
    }

    async fn refresh_once_with_creation(&self, subject: &IssueRef, create_missing: bool) -> Result<(), String> {
        let subject = &subject.clone().normalized();
        if !self.owns_record(subject, false, create_missing).await? {
            return Ok(());
        }
        let status = self.inner.source.observe(subject).await?;
        self.publish(subject, &subject.record_name(), status, true, false, create_missing).await
    }

    pub async fn demand(&self, subscription_id: uuid::Uuid, subject: IssueRef, freshness: Option<DateTime<Utc>>) -> Result<(), String> {
        let subject = subject.normalized();
        let name = subject.record_name();
        match self.inner.backend.including_replicas::<Issue>(&subject.namespace).get(&name).await {
            Ok(_) => {}
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
        active.insert(subject, ActiveRefresh {
            demands: HashMap::from([(subscription_id, freshness)]),
            wake: Arc::new(Notify::new()),
            task,
        });
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
            let result = match records.get(&subject.record_name()).await {
                Ok(record) if record.spec.observing_authority == self.inner.authority => records.delete(&subject.record_name()).await,
                Ok(_) | Err(flotilla_resources::ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if !matches!(error, flotilla_resources::ResourceError::NotFound { .. }) {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "garbage collect undemanded issue failed");
                }
            }
            drop(active);
        }
    }

    /// The relay only acts on live demand, and the same authority check used by
    /// refresh_once is repeated just before the forge read.
    pub async fn demanded_owned(&self) -> Result<Vec<IssueRef>, String> {
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
    pub async fn refresh_hint(&self, hint: &Subject) -> Result<(), String> {
        if hint.kind != SubjectKind::Issue {
            return Ok(());
        }
        let subjects = self
            .inner
            .active
            .lock()
            .await
            .keys()
            .filter(|subject| subject.service == hint.service && subject.scope == hint.scope && subject.number == hint.number)
            .cloned()
            .collect::<Vec<_>>();
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

    async fn refresh_loop(&self, subject: IssueRef) {
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
                        tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "publish issue observation failed");
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
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "issue observation failed");
                    if !self.wait_for_next(&subject, self.inner.cadence.state).await {
                        break;
                    }
                }
            }
        }
    }

    async fn owns_record(&self, subject: &IssueRef, allow_takeover: bool, create_missing: bool) -> Result<bool, String> {
        let source = flotilla_protocol::IssueSource { service: subject.service.clone(), scope: subject.scope.clone() };
        if let Some(owner) = crate::forge_observation::source_owner(&self.inner.backend, &subject.namespace, &source).await? {
            if owner.to_string() != self.inner.authority {
                return Ok(false);
            }
            if !create_missing {
                return Ok(self.inner.backend.using::<Issue>(&subject.namespace).get(&subject.record_name()).await.is_ok());
            }
            self.get_or_create_record(subject, &subject.record_name()).await?;
            return Ok(true);
        }
        let name = subject.record_name();
        let records = self.inner.backend.including_replicas::<Issue>(&subject.namespace);
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

    async fn wait_for_next(&self, subject: &IssueRef, delay: Duration) -> bool {
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
        subject: &IssueRef,
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

    async fn ensure_record(&self, subject: &IssueRef, name: &str) -> Result<(), String> {
        self.get_or_create_record(subject, name).await.map(|_| ())
    }

    async fn get_or_create_record(&self, subject: &IssueRef, name: &str) -> Result<flotilla_resources::ResourceObject<Issue>, String> {
        let records = self.inner.backend.using::<Issue>(&subject.namespace);
        match records.get(name).await {
            Ok(current) => Ok(current),
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let spec = IssueSpec::builder()
                    .service(subject.service.clone())
                    .scope(subject.scope.clone())
                    .number(subject.number)
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

    use flotilla_resources::{InMemoryBackend, Observation, ObservedIssueState};

    use super::*;

    struct CountingSource(Arc<AtomicUsize>);

    #[async_trait]
    impl IssueObservationSource for CountingSource {
        async fn observe(&self, _subject: &IssueRef) -> Result<IssueStatus, String> {
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

    fn subject() -> IssueRef {
        IssueRef { namespace: "flotilla".into(), service: "github.com".into(), scope: "flotilla-org/flotilla".into(), number: 2052 }
    }

    #[tokio::test]
    async fn unowned_hint_does_not_fetch_and_stale_demand_takes_over() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let records = backend.using::<Issue>("flotilla");
        let subject = subject();
        let spec = IssueSpec::builder()
            .service(subject.service.clone())
            .scope(subject.scope.clone())
            .number(subject.number)
            .observing_authority("former".to_string())
            .build();
        let name = subject.record_name();
        let created = records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("create issue");
        let old = Utc::now() - chrono::Duration::hours(1);
        records
            .update_status(&name, &created.metadata.resource_version, &IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: flotilla_resources::Observation::known(ObservedIssueState::Open, old),
                labels: flotilla_resources::Observation::known(vec![], old),
                updated_at: flotilla_resources::Observation::known(old, old),
            })
            .await
            .expect("old status");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = IssueRefresher::new(
            backend.clone(),
            "new-owner".into(),
            Arc::new(CountingSource(calls.clone())),
            IssueRefreshCadence::default(),
        );
        let hint: Subject = "issue/github.com/flotilla-org/flotilla/2052".parse().expect("hint");
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let records = backend.using::<Issue>("flotilla");
        let subject = subject();
        let name = subject.record_name();
        let spec = IssueSpec::builder()
            .service(subject.service.clone())
            .scope(subject.scope.clone())
            .number(subject.number)
            .observing_authority("owner".to_string())
            .build();
        let created = records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.expect("create issue");
        let now = Utc::now();
        records
            .update_status(&name, &created.metadata.resource_version, &IssueStatus {
                title: Default::default(),
                assignees: Default::default(),
                state: Observation::known(ObservedIssueState::Open, now),
                labels: Observation::known(vec![], now),
                updated_at: Observation::known(now, now),
            })
            .await
            .expect("fresh status");
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher =
            IssueRefresher::new(backend.clone(), "reader".into(), Arc::new(CountingSource(calls.clone())), IssueRefreshCadence::default());
        let demand = uuid::Uuid::new_v4();
        refresher.demand(demand, subject, None).await.expect("demand");
        let hint: Subject = "issue/github.com/flotilla-org/flotilla/2052".parse().expect("hint");
        refresher.refresh_hint(&hint).await.expect("foreign hint");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        refresher.release(demand).await;
        assert_eq!(records.get(&name).await.expect("foreign record preserved").spec.observing_authority, "owner");
    }
}
