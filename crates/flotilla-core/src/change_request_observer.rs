use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_resources::{
    change_request_record_name, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, InputMeta,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ResourceBackend, ResourceProvenance,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use crate::providers::{run, CommandRunner};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeRequestRef {
    pub namespace: String,
    pub service: String,
    pub scope: String,
    pub number: u64,
}

impl ChangeRequestRef {
    pub fn from_address(namespace: &str, address: &LeafAddress) -> Option<Self> {
        let LeafAddress::ChangeRequest { service, scope, number } = address else { return None };
        Some(Self { namespace: namespace.to_string(), service: service.clone(), scope: scope.clone(), number: *number })
    }

    fn record_name(&self) -> String {
        change_request_record_name(&self.service, &self.scope, self.number)
    }
}

#[async_trait]
pub trait ChangeRequestObservationSource: Send + Sync {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String>;
}

pub struct GhChangeRequestObservationSource {
    runner: Arc<dyn CommandRunner>,
}

impl GhChangeRequestObservationSource {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl ChangeRequestObservationSource for GhChangeRequestObservationSource {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
        if subject.service != "github.com" {
            return Err(format!("change request observation service `{}` is not available on this host", subject.service));
        }
        let number = subject.number.to_string();
        let output = run!(
            self.runner,
            "gh",
            &["pr", "view", &number, "--repo", &subject.scope, "--json", "state,headRefOid,statusCheckRollup,reviewDecision,mergeable",],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now())
    }
}

fn parse_gh_observation(json: &str, observed_at: DateTime<Utc>) -> Result<ChangeRequestStatus, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| format!("decode gh pr observation: {error}"))?;
    let state = match value["state"].as_str() {
        Some("OPEN") => Some(ObservedChangeRequestState::Open),
        Some("MERGED") => Some(ObservedChangeRequestState::Merged),
        Some("CLOSED") => Some(ObservedChangeRequestState::Closed),
        _ => None,
    };
    let head_sha = value["headRefOid"].as_str().map(str::to_string);
    let checks = value["statusCheckRollup"].as_array().map(|checks| {
        if checks.iter().any(check_failed) {
            ObservedChecks::Fail
        } else if checks.iter().any(check_pending) {
            ObservedChecks::Pending
        } else {
            ObservedChecks::Pass
        }
    });
    let actionable_at_head = value.get("reviewDecision").map(|decision| decision.as_str() == Some("CHANGES_REQUESTED"));
    let mergeable = match value["mergeable"].as_str() {
        Some("MERGEABLE") => Some(ObservedMergeability::Mergeable),
        Some("CONFLICTING") => Some(ObservedMergeability::Conflicting),
        _ => None,
    };
    Ok(ChangeRequestStatus {
        state: Observation { value: state, observed_at },
        head_sha: Observation { value: head_sha, observed_at },
        checks: Observation { value: checks, observed_at },
        review: ChangeRequestReviewObservation { actionable_at_head: Observation { value: actionable_at_head, observed_at } },
        mergeable: Observation { value: mergeable, observed_at },
    })
}

fn check_failed(check: &serde_json::Value) -> bool {
    matches!(check["conclusion"].as_str(), Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"))
        || matches!(check["state"].as_str(), Some("FAILURE" | "ERROR"))
}

fn check_pending(check: &serde_json::Value) -> bool {
    check.get("conclusion").is_some_and(serde_json::Value::is_null)
        || matches!(check["status"].as_str(), Some("QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING"))
        || matches!(check["state"].as_str(), Some("PENDING" | "EXPECTED"))
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
    active: Mutex<HashMap<ChangeRequestRef, ActiveRefresh>>,
}

impl ChangeRequestRefresher {
    pub fn new(
        backend: ResourceBackend,
        authority: String,
        source: Arc<dyn ChangeRequestObservationSource>,
        cadence: ChangeRequestRefreshCadence,
    ) -> Self {
        Self { inner: Arc::new(ChangeRequestRefresherInner { backend, authority, source, cadence, active: Mutex::new(HashMap::new()) }) }
    }

    pub fn stale_after(&self) -> Duration {
        self.inner.cadence.stale_after
    }

    pub async fn demand(
        &self,
        subscription_id: uuid::Uuid,
        subject: ChangeRequestRef,
        freshness: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let records =
            self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace).list().await.map_err(|error| error.to_string())?;
        let name = subject.record_name();
        if records
            .items
            .iter()
            .any(|item| item.object.metadata.name == name && matches!(item.provenance, ResourceProvenance::Replica { .. }))
        {
            return Ok(());
        }
        self.ensure_record(&subject, &name).await?;

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
            let result = self.inner.backend.using::<ChangeRequest>(&subject.namespace).delete(&subject.record_name()).await;
            if let Err(error) = result {
                if !matches!(error, flotilla_resources::ResourceError::NotFound { .. }) {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "garbage collect undemanded change request failed");
                }
            }
            drop(active);
        }
    }

    /// A daemon restart has no surviving leaf subscriptions, so locally
    /// authoritative observations from the previous process are all orphans.
    pub async fn garbage_collect_orphans(&self) -> Result<(), String> {
        let namespaces = self.inner.backend.local_namespaces::<ChangeRequest>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            let records = self.inner.backend.using::<ChangeRequest>(&namespace);
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

    async fn refresh_loop(&self, subject: ChangeRequestRef) {
        let record_name = subject.record_name();
        loop {
            match self.inner.source.observe(&subject).await {
                Ok(status) => {
                    if let Err(error) = self.publish(&subject, &record_name, status.clone()).await {
                        tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "publish change request observation failed");
                    }
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    let delay = if demanded {
                        self.inner.cadence.freshness_demanded
                    } else if status.checks.value == Some(ObservedChecks::Pending) {
                        self.inner.cadence.checks_pending
                    } else {
                        self.inner.cadence.state
                    };
                    if !self.wait_for_next(&subject, delay).await {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "change request observation failed");
                    if !self.wait_for_next(&subject, self.inner.cadence.checks_pending).await {
                        break;
                    }
                }
            }
        }
    }

    async fn wait_for_next(&self, subject: &ChangeRequestRef, delay: Duration) -> bool {
        let Some(wake) = self.inner.active.lock().await.get(subject).map(|refresh| Arc::clone(&refresh.wake)) else {
            return false;
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = wake.notified() => {}
        }
        true
    }

    async fn publish(&self, subject: &ChangeRequestRef, name: &str, status: ChangeRequestStatus) -> Result<(), String> {
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        let current = self.get_or_create_record(subject, name).await?;
        if current.status.as_ref().is_some_and(|current| observed_values_equal(current, &status)) {
            return Ok(());
        }
        records.update_status(name, &current.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn ensure_record(&self, subject: &ChangeRequestRef, name: &str) -> Result<(), String> {
        self.get_or_create_record(subject, name).await.map(|_| ())
    }

    async fn get_or_create_record(
        &self,
        subject: &ChangeRequestRef,
        name: &str,
    ) -> Result<flotilla_resources::ResourceObject<ChangeRequest>, String> {
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        match records.get(name).await {
            Ok(current) => Ok(current),
            Err(flotilla_resources::ResourceError::NotFound { .. }) => {
                let spec = ChangeRequestSpec::builder()
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
    use flotilla_resources::{InMemoryBackend, ResourceBackend};

    use super::*;

    struct UnavailableSource;

    #[async_trait]
    impl ChangeRequestObservationSource for UnavailableSource {
        async fn observe(&self, _subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
            Err("unavailable".to_string())
        }
    }

    struct CountingSource(Arc<AtomicUsize>);

    #[async_trait]
    impl ChangeRequestObservationSource for CountingSource {
        async fn observe(&self, _subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let observed_at = Utc::now();
            Ok(ChangeRequestStatus {
                state: Observation::known(ObservedChangeRequestState::Open, observed_at),
                head_sha: Observation::known("abc".to_string(), observed_at),
                checks: Observation::known(ObservedChecks::Pass, observed_at),
                review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
                mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
            })
        }
    }

    #[test]
    fn parses_gh_observation_vocabulary() {
        let status = parse_gh_observation(
            r#"{"state":"MERGED","headRefOid":"abc","statusCheckRollup":[{"conclusion":"SUCCESS","status":"COMPLETED"}],"reviewDecision":"CHANGES_REQUESTED","mergeable":"CONFLICTING"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Merged));
        assert_eq!(status.checks.value, Some(ObservedChecks::Pass));
        assert_eq!(status.review.actionable_at_head.value, Some(true));
        assert_eq!(status.mergeable.value, Some(ObservedMergeability::Conflicting));
    }

    #[test]
    fn classic_success_status_without_check_run_fields_passes() {
        let status = parse_gh_observation(
            r#"{"state":"OPEN","headRefOid":"abc","statusCheckRollup":[{"state":"SUCCESS"}],"reviewDecision":"APPROVED","mergeable":"MERGEABLE"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.checks.value, Some(ObservedChecks::Pass));
    }

    #[test]
    fn explicit_null_review_decision_is_not_actionable() {
        let status = parse_gh_observation(
            r#"{"state":"OPEN","headRefOid":"abc","statusCheckRollup":[],"reviewDecision":null,"mergeable":"MERGEABLE"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse");
        assert_eq!(status.review.actionable_at_head.value, Some(false));
    }

    #[tokio::test]
    async fn concurrent_first_demands_converge_on_one_authority_record() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        let (first, second) = tokio::join!(
            refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None),
            refresher.demand(uuid::Uuid::new_v4(), subject, None),
        );
        first.expect("first demand");
        second.expect("concurrent demand");
        assert_eq!(backend.using::<ChangeRequest>("flotilla").list().await.expect("list CRs").items.len(), 1);
        assert_eq!(refresher.active_demands().await, 2);
    }

    #[tokio::test]
    async fn last_released_demand_garbage_collects_observed_record() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "ops".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject, None).await.expect("non-default namespace demand");
        assert_eq!(backend.local_namespaces::<ChangeRequest>().await.expect("local namespaces"), vec!["ops"]);

        refresher.garbage_collect_orphans().await.expect("startup garbage collection");

        assert!(backend.using::<ChangeRequest>("ops").list().await.expect("list ops CRs").items.is_empty());
        assert!(backend.local_namespaces::<ChangeRequest>().await.expect("local namespaces after GC").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn identical_polls_do_not_write_status() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.expect("demand");
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        let records = backend.using::<ChangeRequest>("flotilla");
        let first = records.get(&subject.record_name()).await.expect("first observation");

        const IDENTICAL_POLLS: usize = 4;
        for _ in 0..IDENTICAL_POLLS {
            tokio::time::advance(Duration::from_secs(90)).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        }

        let after = records.get(&subject.record_name()).await.expect("observation after identical polls");
        assert_eq!(calls.load(Ordering::SeqCst), IDENTICAL_POLLS + 1);
        assert_eq!(after.metadata.resource_version, first.metadata.resource_version, "identical observed values must produce no writes");
        assert_eq!(after.status, first.status, "poll timestamps are not persisted unless an observed value changes");
    }

    #[tokio::test(start_paused = true)]
    async fn late_freshness_demand_preempts_existing_state_cadence_sleep() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.expect("initial demand");
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(5)).await;
        refresher.demand(uuid::Uuid::new_v4(), subject, Some(Utc::now())).await.expect("late freshness demand");
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must preempt the remaining 85-second sleep");
    }
}
