use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_relay_protocol::{Subject, SubjectKind};
use flotilla_resources::{
    change_request_record_name, ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, InputMeta,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ResourceBackend, ResourceProvenance,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use crate::providers::{run, CommandRunner};

// GraphQL's Author.login for the crew App omits the REST `[bot]` suffix.
const CREW_GITHUB_LOGIN: &str = "flotilla-crew";

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

    pub(crate) fn record_name(&self) -> String {
        let (service, scope) = Subject::normalize_scope(&self.service, &self.scope);
        change_request_record_name(&service, &scope, self.number)
    }

    fn normalized(mut self) -> Self {
        (self.service, self.scope) = Subject::normalize_scope(&self.service, &self.scope);
        self
    }
}

#[async_trait]
pub trait ChangeRequestObservationSource: Send + Sync {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String>;

    async fn observe_group(&self, subjects: &[ChangeRequestRef], subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
        let _ = subjects;
        self.observe(subject).await
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
        self.observe(subject).await
    }
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

    async fn observe_for_completion(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
        if subject.service != "github.com" {
            return Err(format!("change request observation service `{}` is not available on this host", subject.service));
        }
        let number = subject.number.to_string();
        let output = run!(
            self.runner,
            "gh",
            &[
                "pr",
                "view",
                &number,
                "--repo",
                &subject.scope,
                "--json",
                "state,isDraft,headRefOid,statusCheckRollup,reviewDecision,mergeable"
            ],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now())
    }
}

pub(crate) fn parse_gh_observation(json: &str, observed_at: DateTime<Utc>) -> Result<ChangeRequestStatus, String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| format!("decode gh pr observation: {error}"))?;
    let state = match value["state"].as_str() {
        Some("OPEN") if value["isDraft"] == true => Some(ObservedChangeRequestState::Draft),
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
    let comments = value["comments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            value["reviewThreads"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|thread| thread["comments"]["nodes"].as_array().into_iter().flatten()),
        )
        .collect::<Vec<_>>();
    let addressed = comments
        .iter()
        .copied()
        .filter(|comment| comment["author"]["login"] == CREW_GITHUB_LOGIN)
        .filter_map(|comment| comment["body"].as_str())
        .flat_map(|body| {
            body.split("<!--")
                .skip(1)
                .filter_map(|part| part.split_once("-->")?.0.trim().strip_prefix("pr-shepherd-addresses:")?.parse::<u64>().ok())
        })
        .collect::<HashSet<_>>();
    let head_at = value["commits"]["nodes"][0]["commit"]["committedDate"].as_str();
    let reviews = value["reviews"]["nodes"].as_array();
    let actionable_at_head = value.get("reviewDecision").map(|decision| {
        let unaddressed = |item: &serde_json::Value| {
            item["author"]["login"].as_str() != Some(CREW_GITHUB_LOGIN) && github_comment_id(item).is_none_or(|id| !addressed.contains(&id))
        };
        reviews.is_some_and(|items| items.iter().any(|review| review["state"] == "CHANGES_REQUESTED" && unaddressed(review)))
            || head_at.is_some_and(|head_at| {
                comments.iter().copied().chain(reviews.into_iter().flatten()).any(|item| {
                    unaddressed(item)
                        && item["createdAt"]
                            .as_str()
                            .or_else(|| item["submittedAt"].as_str())
                            .is_some_and(|created_at| created_at > head_at)
                })
            })
            || (reviews.is_none() && decision.as_str() == Some("CHANGES_REQUESTED"))
    });
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

fn github_comment_id(comment: &serde_json::Value) -> Option<u64> {
    comment["databaseId"]
        .as_u64()
        .or_else(|| comment["fullDatabaseId"].as_u64())
        .or_else(|| comment["fullDatabaseId"].as_str()?.parse().ok())
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
    observation_errors: Mutex<HashMap<ChangeRequestRef, String>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
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
                relay_healthy: std::sync::atomic::AtomicBool::new(false),
                relay_wake: Notify::new(),
            }),
        }
    }

    pub fn stale_after(&self) -> Duration {
        self.inner.cadence.stale_after
    }

    pub async fn observation_error(&self, subject: &ChangeRequestRef) -> Option<String> {
        self.inner.observation_errors.lock().await.get(subject).cloned()
    }

    /// Refresh a claim-time observation even before Landing has armed its
    /// standing leaf subscriptions.
    pub async fn refresh_once(&self, subject: &ChangeRequestRef) -> Result<(), String> {
        self.refresh_once_with_creation(subject, true).await
    }

    async fn refresh_once_with_creation(&self, subject: &ChangeRequestRef, create_missing: bool) -> Result<(), String> {
        let subject = &subject.clone().normalized();
        if !self.owns_record(subject, false, create_missing).await? {
            return Ok(());
        }
        let status = if create_missing {
            self.inner.source.observe_for_completion(subject).await?
        } else {
            let group = self
                .inner
                .active
                .lock()
                .await
                .keys()
                .filter(|candidate| {
                    candidate.namespace == subject.namespace && candidate.service == subject.service && candidate.scope == subject.scope
                })
                .cloned()
                .collect::<Vec<_>>();
            self.inner.source.observe_group(&group, subject).await?
        };
        self.publish(subject, &subject.record_name(), status, true, false, create_missing).await
    }

    pub async fn demand(
        &self,
        subscription_id: uuid::Uuid,
        subject: ChangeRequestRef,
        freshness: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let subject = subject.normalized();
        let name = subject.record_name();
        match self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace).get(&name).await {
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
            let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
            let result = match records.get(&subject.record_name()).await {
                Ok(record) if record.spec.observing_authority == self.inner.authority => records.delete(&subject.record_name()).await,
                Ok(_) | Err(flotilla_resources::ResourceError::NotFound { .. }) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if !matches!(error, flotilla_resources::ResourceError::NotFound { .. }) {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "garbage collect undemanded change request failed");
                }
            }
            drop(active);
        }
    }

    /// The relay only acts on live demand, and the same authority check used by
    /// refresh_once is repeated just before the forge read.
    pub async fn demanded_owned(&self) -> Result<Vec<ChangeRequestRef>, String> {
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
        if hint.kind != SubjectKind::ChangeRequest {
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
            match self.owns_record(&subject, true, true).await {
                Ok(false) => {
                    if !self.wait_for_next(&subject, self.inner.cadence.state).await {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error);
                    if !self.wait_for_next(&subject, self.inner.cadence.checks_pending).await {
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
                .keys()
                .filter(|candidate| {
                    candidate.namespace == subject.namespace && candidate.service == subject.service && candidate.scope == subject.scope
                })
                .cloned()
                .collect::<Vec<_>>();
            match self.inner.source.observe_group(&group, &subject).await {
                Ok(status) => {
                    let demanded =
                        self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.values().any(Option::is_some));
                    if let Err(error) = self.publish(&subject, &record_name, status.clone(), demanded, true, true).await {
                        self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                        tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "publish change request observation failed");
                    } else {
                        self.inner.observation_errors.lock().await.remove(&subject);
                    }
                    let delay = if demanded {
                        self.inner.cadence.freshness_demanded
                    } else if status.checks.value == Some(ObservedChecks::Pending) {
                        self.inner.cadence.checks_pending
                    } else {
                        self.inner.cadence.state
                    };
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
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "change request observation failed");
                    if !self.wait_for_next(&subject, self.inner.cadence.checks_pending).await {
                        break;
                    }
                }
            }
        }
    }

    async fn owns_record(&self, subject: &ChangeRequestRef, allow_takeover: bool, create_missing: bool) -> Result<bool, String> {
        let name = subject.record_name();
        let records = self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace);
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
        let local = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
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

    async fn wait_for_next(&self, subject: &ChangeRequestRef, delay: Duration) -> bool {
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
        subject: &ChangeRequestRef,
        name: &str,
        status: ChangeRequestStatus,
        heartbeat: bool,
        allow_takeover: bool,
        create_missing: bool,
    ) -> Result<(), String> {
        if !self.owns_record(subject, allow_takeover, create_missing).await? {
            return Ok(());
        }
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
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
    fn draft_pr_is_not_ready_for_a_crew_claim() {
        let status = parse_gh_observation(
            r#"{"state":"OPEN","isDraft":true,"headRefOid":"abc","statusCheckRollup":[],"reviewDecision":null,"mergeable":"MERGEABLE"}"#,
            "2026-08-03T20:00:00Z".parse().expect("time"),
        )
        .expect("parse draft");
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Draft));
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
    async fn relay_health_slows_polling_and_disconnect_restores_normal_cadence() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        refresher.set_relay_healthy(true);
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "GitHub.com".to_string(),
            scope: "Flotilla-Org/Flotilla".to_string(),
            number: 2051,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject, None).await.expect("demand");
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(90)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "healthy relay uses slow backstop");
        refresher.set_relay_healthy(false);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "disconnect wakes the normal refresher");
        tokio::time::advance(Duration::from_secs(90)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn relay_hint_matches_demands_in_each_namespace_without_recreating_missing_records() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
                    ChangeRequestRef {
                        namespace: namespace.to_string(),
                        service: "GitHub.com".to_string(),
                        scope: "Flotilla-Org/Flotilla".to_string(),
                        number: 2051,
                    },
                    None,
                )
                .await
                .expect("demand");
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let before = calls.load(Ordering::SeqCst);
        let hint = Subject::new(SubjectKind::ChangeRequest, "github.com", "flotilla-org/flotilla", 2051);
        refresher.refresh_hint(&hint).await.expect("hint");
        assert_eq!(calls.load(Ordering::SeqCst), before + 2, "both namespaces refresh once");

        let name = change_request_record_name("github.com", "flotilla-org/flotilla", 2051);
        backend.using::<ChangeRequest>("ops").delete(&name).await.expect("delete record during live demand");
        refresher.refresh_hint(&hint).await.expect("hint after record deletion");
        assert!(
            matches!(backend.using::<ChangeRequest>("ops").get(&name).await, Err(flotilla_resources::ResourceError::NotFound { .. })),
            "hint does not recreate a missing record"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before + 3, "remaining namespace still refreshes");
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_demand_heartbeats_identical_observations() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::new(AtomicUsize::new(0)))),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1699,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), Some(Utc::now())).await.expect("freshness demand");
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        let records = backend.using::<ChangeRequest>("flotilla");
        let first = records.get(&subject.record_name()).await.expect("first observation");

        tokio::time::advance(Duration::from_secs(10)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        let heartbeat = records.get(&subject.record_name()).await.expect("heartbeat observation");

        assert_ne!(heartbeat.metadata.resource_version, first.metadata.resource_version);
        assert!(heartbeat.status.expect("heartbeat status").state.observed_at > first.status.expect("first status").state.observed_at);
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

    #[tokio::test]
    async fn observation_failure_is_available_to_settlement_diagnostics() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            backend,
            "authority".to_string(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1699,
        };
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
        assert_eq!(refresher.observation_error(&subject).await.as_deref(), Some("unavailable"));

        refresher.release(demand).await;
        assert_eq!(refresher.observation_error(&subject).await, None);
    }
}
