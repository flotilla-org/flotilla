use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    IssueSource, LeafAddress, ReferenceContext, Relationship, Subject as ConvoySubject, SubjectKind as ConvoySubjectKind,
};
use flotilla_relay_protocol::{Subject, SubjectKind};
use flotilla_resources::{
    change_request_record_name, convoy_subject_rows, merge_change_request_history, retain_change_request, select_change_requests,
    ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, ChangeRequestSubjectHistory, Convoy, InputMeta,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ObservedReviewDecision, ResourceBackend, ResourceError,
    ResourceProvenance,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use crate::providers::{github_api::rate_limit_reset, run, CommandRunner};

const RETAINED_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const HISTORY_REFRESH_INTERVAL_SECS: i64 = 60 * 60;

pub(crate) const DEFAULT_REVIEW_BOT_LOGIN: &str = "claude";

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
    parse_gh_observation_with_review_bot(json, observed_at, DEFAULT_REVIEW_BOT_LOGIN)
}

pub(crate) fn parse_gh_observation_with_review_bot(
    json: &str,
    observed_at: DateTime<Utc>,
    review_bot_login: &str,
) -> Result<ChangeRequestStatus, String> {
    parse_gh_observation_with_identity(json, observed_at, review_bot_login, None)
}

pub(crate) fn parse_gh_observation_with_identity(
    json: &str,
    observed_at: DateTime<Utc>,
    review_bot_login: &str,
    operator_login: Option<&str>,
) -> Result<ChangeRequestStatus, String> {
    parse_gh_observation_with_crew_identity(json, observed_at, review_bot_login, operator_login, &[])
}

pub(crate) fn parse_gh_observation_with_crew_identity(
    json: &str,
    observed_at: DateTime<Utc>,
    review_bot_login: &str,
    operator_login: Option<&str>,
    crew_logins: &[String],
) -> Result<ChangeRequestStatus, String> {
    let crew_logins = crew_logins.iter().map(String::as_str).collect::<HashSet<_>>();
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
                .filter(|thread| thread["isResolved"] != true)
                .flat_map(|thread| thread["comments"]["nodes"].as_array().into_iter().flatten()),
        )
        .collect::<Vec<_>>();
    let addressed = comments
        .iter()
        .copied()
        .filter(|comment| comment["author"]["login"].as_str().is_some_and(|login| crew_logins.contains(login)))
        .filter_map(|comment| comment["body"].as_str())
        .flat_map(|body| {
            body.split("<!--")
                .skip(1)
                .filter_map(|part| part.split_once("-->")?.0.trim().strip_prefix("pr-shepherd-addresses:")?.parse::<u64>().ok())
        })
        .collect::<HashSet<_>>();
    // GitHub returns these timestamps in UTC ISO-8601 form, so lexical order is chronological.
    // pushedDate is often null. A force-push event identifies the current head only
    // when its afterCommit matches headRefOid. If neither is available, retain the
    // committedDate fallback for ordinary pushes; it can over-report feedback for
    // a delayed push, so callers must still handle such feedback explicitly.
    let commit = &value["commits"]["nodes"][0]["commit"];
    let head_oid = value["headRefOid"].as_str();
    let force_push_at = value["timelineItems"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|event| head_oid.is_some() && event["afterCommit"]["oid"].as_str() == head_oid)
        .and_then(|event| event["createdAt"].as_str());
    let head_at = force_push_at.or_else(|| commit["pushedDate"].as_str()).or_else(|| commit["committedDate"].as_str());
    let reviews = value["reviews"]["nodes"].as_array();
    let actionable_at_head = value.get("reviewDecision").map(|decision| {
        let unaddressed = |item: &serde_json::Value| {
            actionable_author(item, value["author"]["login"].as_str(), review_bot_login, &crew_logins)
                && github_comment_id(item).is_none_or(|id| !addressed.contains(&id))
        };
        let mut latest_formal = HashMap::new();
        if let Some(items) = reviews {
            for review in items {
                if review["state"].as_str().is_some_and(|state| state != "COMMENTED" && state != "PENDING") {
                    if let Some(login) = review["author"]["login"].as_str() {
                        latest_formal.insert(login, review);
                    }
                }
            }
        }
        // A formal change request remains open across new head commits until the reviewer
        // approves or the crew acknowledges it. It deliberately has no head-time cutoff.
        latest_formal.values().any(|review| review["state"] == "CHANGES_REQUESTED" && unaddressed(review))
            || head_at.is_some_and(|head_at| {
                comments.iter().copied().any(|item| {
                    unaddressed(item)
                        && substantive_feedback(item)
                        && item["createdAt"]
                            .as_str()
                            .or_else(|| item["submittedAt"].as_str())
                            .is_some_and(|created_at| created_at > head_at)
                }) || reviews.into_iter().flatten().any(|review| {
                    unaddressed(review)
                        && substantive_feedback(review)
                        && (review["state"] == "COMMENTED"
                            || review["author"]["login"]
                                .as_str()
                                .and_then(|login| latest_formal.get(login))
                                .is_some_and(|latest| std::ptr::eq(*latest, review)))
                        && review["submittedAt"].as_str().is_some_and(|submitted_at| submitted_at > head_at)
                })
            })
            || (reviews.is_none() && decision.as_str() == Some("CHANGES_REQUESTED"))
    });
    let mergeable = match value["mergeable"].as_str() {
        Some("MERGEABLE") => Some(ObservedMergeability::Mergeable),
        Some("CONFLICTING") => Some(ObservedMergeability::Conflicting),
        _ => None,
    };
    let review_decision = match value["reviewDecision"].as_str() {
        Some("APPROVED") => Some(ObservedReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ObservedReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ObservedReviewDecision::Required),
        // An explicit null means GitHub has no review decision; an absent field
        // means the provider did not observe that fact.
        None if value.get("reviewDecision").is_some() => Some(ObservedReviewDecision::None),
        _ => None,
    };
    let requested = value["reviewRequests"]["nodes"].as_array();
    let review_requested_from_owner = operator_login.and_then(|operator| {
        let requests = requested?;
        if value["reviewRequests"]["pageInfo"]["hasNextPage"] == true {
            return None;
        }
        let mut unidentified_reviewer = false;
        for request in requests {
            let login = request["requestedReviewer"]["login"].as_str();
            match login {
                Some(login) if login.eq_ignore_ascii_case(operator) => return Some(true),
                Some(_) => {}
                // A team request does not reveal whether the operator belongs to it.
                None => unidentified_reviewer = true,
            }
        }
        (!unidentified_reviewer).then_some(false)
    });
    Ok(ChangeRequestStatus {
        title: Observation { value: value["title"].as_str().map(str::to_string), observed_at },
        author: Observation { value: value["author"]["login"].as_str().map(str::to_string), observed_at },
        review_decision: Observation { value: review_decision, observed_at },
        review_requested_from_owner: Observation { value: review_requested_from_owner, observed_at },
        state: Observation { value: state, observed_at },
        head_sha: Observation { value: head_sha, observed_at },
        checks: Observation { value: checks, observed_at },
        review: ChangeRequestReviewObservation { actionable_at_head: Observation { value: actionable_at_head, observed_at } },
        mergeable: Observation { value: mergeable, observed_at },
    })
}

fn actionable_author(item: &serde_json::Value, pr_author: Option<&str>, review_bot_login: &str, crew_logins: &HashSet<&str>) -> bool {
    let Some(login) = item["author"]["login"].as_str() else { return false };
    if crew_logins.contains(login) || Some(login) == pr_author {
        return false;
    }
    // Only the configured review bot may contribute bot feedback.
    let is_bot = item["author"]["__typename"] == "Bot" || login.ends_with("[bot]");
    !is_bot || (item["author"]["__typename"] == "Bot" && login == review_bot_login)
}

fn substantive_feedback(item: &serde_json::Value) -> bool {
    let body = item["body"].as_str().unwrap_or_default().trim().trim_end_matches(['!', '.', '?']).trim_end();
    !body.is_empty() && !matches!(body.to_ascii_lowercase().as_str(), "thanks" | "thank you" | "lgtm" | "+1" | "approved")
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
    origin: String,
    source: Arc<dyn ChangeRequestObservationSource>,
    cadence: ChangeRequestRefreshCadence,
    active: Mutex<HashMap<ChangeRequestRef, ActiveRefresh>>,
    observation_errors: Mutex<HashMap<ChangeRequestRef, String>>,
    subject_locks: Mutex<HashMap<ChangeRequestRef, Weak<Mutex<()>>>>,
    relay_healthy: std::sync::atomic::AtomicBool,
    relay_wake: Notify,
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
            }),
        }
    }

    async fn subject_lock(&self, subject: &ChangeRequestRef) -> Arc<Mutex<()>> {
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
        let lock = self.subject_lock(subject).await;
        let _guard = lock.lock().await;
        if !self.owns_record(subject, false, create_missing).await? {
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
                .keys()
                .filter(|candidate| {
                    candidate.namespace == subject.namespace && candidate.service == subject.service && candidate.scope == subject.scope
                })
                .cloned()
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
                return Err(error);
            }
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
        let lock = self.subject_lock(&subject).await;
        let _guard = lock.lock().await;
        let name = subject.record_name();
        match self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace).get(&name).await {
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
            let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
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
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "garbage collect undemanded change request failed");
                }
            }
        }
    }

    /// The relay only acts on live demand, and the same authority check used by
    /// refresh_once is repeated just before the forge read.
    pub async fn demanded_owned(&self) -> Result<Vec<ChangeRequestRef>, String> {
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
    pub async fn refresh_hint(&self, hint: &Subject) -> Result<(), String> {
        if hint.kind != SubjectKind::ChangeRequest {
            return Ok(());
        }
        let subjects = self
            .inner
            .active
            .lock()
            .await
            .iter()
            .filter(|(subject, refresh)| {
                !refresh.demands.is_empty()
                    && subject.service == hint.service
                    && subject.scope == hint.scope
                    && subject.number == hint.number
            })
            .map(|(subject, _)| subject.clone())
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

    /// Recover produced requests after restart; ordinary demand-only observations
    /// are collected. Retention has no time expiry while the forge is open:
    /// one task per subject, polling at most once every fifteen minutes.
    pub async fn garbage_collect_orphans(&self) -> Result<(), String> {
        let namespaces = self.inner.backend.local_namespaces::<ChangeRequest>().await.map_err(|error| error.to_string())?;
        for namespace in namespaces {
            let records = self.inner.backend.using::<ChangeRequest>(&namespace);
            for record in records.list().await.map_err(|error| error.to_string())?.items {
                if retain_change_request(&record) {
                    self.start_retained(ChangeRequestRef {
                        namespace: namespace.clone(),
                        service: record.spec.service.clone(),
                        scope: record.spec.scope.clone(),
                        number: record.spec.number,
                    })
                    .await;
                } else {
                    records.delete(&record.metadata.name).await.map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    async fn collect_retained_if_terminal(&self, subject: &ChangeRequestRef) -> Result<bool, ResourceError> {
        let lock = self.subject_lock(subject).await;
        let _guard = lock.lock().await;
        let retained = self.inner.active.lock().await.get(subject).is_some_and(|refresh| refresh.demands.is_empty());
        if !retained {
            return Ok(false);
        }
        let sources = self.inner.backend.including_replicas::<ChangeRequest>(&subject.namespace).list().await?;
        let selected = select_change_requests(sources.items.iter().map(|source| &source.object));
        let terminal = selected
            .values()
            .find(|record| {
                record.spec.service == subject.service && record.spec.scope == subject.scope && record.spec.number == subject.number
            })
            .is_some_and(|record| {
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

    async fn collect_local_subject(&self, subject: &ChangeRequestRef) -> Result<(), ResourceError> {
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        for record in records.list().await?.items.iter().filter(|record| {
            record.spec.service == subject.service && record.spec.scope == subject.scope && record.spec.number == subject.number
        }) {
            match records.delete(&record.metadata.name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn start_retained(&self, subject: ChangeRequestRef) {
        let mut active = self.inner.active.lock().await;
        if active.contains_key(&subject) {
            return;
        }
        let this = self.clone();
        let task_subject = subject.clone();
        let task = tokio::spawn(async move { this.refresh_loop(task_subject).await });
        active.insert(subject, ActiveRefresh { demands: HashMap::new(), wake: Arc::new(Notify::new()), task });
    }

    /// Capture links at admission, before a convoy can disappear. History is in
    /// the spec so ordinary forge status refreshes cannot erase it, and it follows
    /// the record's existing observation replication class.
    async fn record_convoy_history(&self, subject: &ChangeRequestRef) -> Result<(), String> {
        let convoys = self.inner.backend.using::<Convoy>(&subject.namespace).list().await.map_err(|error| error.to_string())?;
        let target = ConvoySubject {
            kind: ConvoySubjectKind::ChangeRequest,
            source: IssueSource { service: subject.service.clone(), scope: subject.scope.clone() },
            id: subject.number.to_string(),
        };
        let mut history = Vec::new();
        let references = ReferenceContext::default();
        for convoy in convoys.items {
            for row in convoy_subject_rows(&convoy, &references) {
                if row.subject != target {
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
        let records = self.inner.backend.using::<ChangeRequest>(&subject.namespace);
        loop {
            let current = match records.get(&subject.record_name()).await {
                Ok(current) => current,
                Err(ResourceError::NotFound { .. }) => {
                    // The observing authority may be remote. Keep a local copy
                    // of its record so this convoy's history can replicate too.
                    let observed = self
                        .inner
                        .backend
                        .including_replicas::<ChangeRequest>(&subject.namespace)
                        .get(&subject.record_name())
                        .await
                        .map_err(|error| error.to_string())?
                        .object;
                    let mut spec = observed.spec.clone();
                    merge_change_request_history(&mut spec.subject_of, &history);
                    match records.create(&InputMeta::builder().name(subject.record_name()).build(), &spec).await {
                        Ok(created) => {
                            if let Some(status) = observed.status {
                                records
                                    .update_status(&subject.record_name(), &created.metadata.resource_version, &status)
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

    #[cfg(test)]
    pub async fn active_demands(&self) -> usize {
        self.inner.active.lock().await.values().map(|refresh| refresh.demands.len()).sum()
    }

    async fn refresh_delay(&self, subject: &ChangeRequestRef, demanded_delay: Duration) -> Duration {
        if self.inner.active.lock().await.get(subject).is_some_and(|refresh| refresh.demands.is_empty()) {
            RETAINED_REFRESH_INTERVAL
        } else {
            demanded_delay
        }
    }

    async fn refresh_loop(&self, subject: ChangeRequestRef) {
        let record_name = subject.record_name();
        loop {
            match self.collect_retained_if_terminal(&subject).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "collect retained change request failed")
                }
            }
            match self.owns_record(&subject, true, true).await {
                Ok(false) => {
                    if !self.wait_for_next(&subject, self.refresh_delay(&subject, self.inner.cadence.state).await).await {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error);
                    if !self.wait_for_next(&subject, self.refresh_delay(&subject, self.inner.cadence.checks_pending).await).await {
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
                    match self.collect_retained_if_terminal(&subject).await {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "collect retained change request failed")
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
                    if !self.wait_for_next(&subject, delay).await {
                        break;
                    }
                }
                Err(error) => {
                    self.inner.observation_errors.lock().await.insert(subject.clone(), error.clone());
                    tracing::warn!(service = %subject.service, scope = %subject.scope, number = subject.number, %error, "change request observation failed");
                    let retained = self.inner.active.lock().await.get(&subject).is_some_and(|refresh| refresh.demands.is_empty());
                    let ordinary_delay = if retained { RETAINED_REFRESH_INTERVAL } else { self.inner.cadence.checks_pending };
                    let delay = rate_limit_reset(&error)
                        .and_then(|retry_at| retry_at.signed_duration_since(Utc::now()).to_std().ok())
                        .filter(|delay| !delay.is_zero())
                        .unwrap_or(ordinary_delay);
                    if !self.wait_for_next(&subject, delay).await {
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
        self.record_convoy_history(subject).await?;
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
            Err(ResourceError::NotFound { .. }) => {
                let candidates = self
                    .inner
                    .backend
                    .including_replicas::<ChangeRequest>(&subject.namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?;
                let selected = select_change_requests(candidates.items.iter().map(|source| &source.object));
                let history = selected
                    .values()
                    .find(|record| {
                        record.spec.service == subject.service && record.spec.scope == subject.scope && record.spec.number == subject.number
                    })
                    .map(|record| record.spec.subject_of.clone())
                    .unwrap_or_default();
                let spec = ChangeRequestSpec::builder()
                    .subject_of(history)
                    .service(subject.service.clone())
                    .scope(subject.scope.clone())
                    .number(subject.number)
                    .observing_authority(self.inner.authority.clone())
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
    use flotilla_resources::{HttpBackend, InMemoryBackend, ResourceBackend, TypedResolver};

    use super::*;
    use crate::tls;

    struct UnavailableSource;

    #[async_trait]
    impl ChangeRequestObservationSource for UnavailableSource {
        async fn observe(&self, _subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
            Err("unavailable".to_string())
        }
    }

    struct CountingSource(Arc<AtomicUsize>);

    fn review_actionable(request: serde_json::Value) -> bool {
        parse_gh_observation(&request.to_string(), "2026-09-30T12:00:00Z".parse().expect("time"))
            .expect("parse review")
            .review
            .actionable_at_head
            .value
            .expect("review observation")
    }

    #[test]
    fn approval_supersedes_requested_changes_even_when_old_review_has_feedback() {
        let request = serde_json::json!({
            "reviewDecision": "APPROVED", "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "reviews": {"nodes": [
                {"fullDatabaseId": "1", "submittedAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"},
                 "state": "CHANGES_REQUESTED", "body": "Please fix this"},
                {"fullDatabaseId": "2", "submittedAt": "2026-09-30T11:30:00Z", "author": {"login": "reviewer"},
                 "state": "APPROVED", "body": ""}
            ]}
        });
        assert!(!review_actionable(request));
    }

    #[test]
    fn commented_feedback_after_approval_is_actionable() {
        let mut request = serde_json::json!({
            "reviewDecision": "APPROVED", "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "reviews": {"nodes": [
                {"fullDatabaseId": "1", "submittedAt": "2026-09-30T09:00:00Z", "author": {"login": "reviewer"},
                 "state": "CHANGES_REQUESTED", "body": "Please fix this"},
                {"fullDatabaseId": "2", "submittedAt": "2026-09-30T10:30:00Z", "author": {"login": "reviewer"},
                 "state": "APPROVED", "body": ""},
                {"fullDatabaseId": "3", "submittedAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"},
                 "state": "COMMENTED", "body": "One more concern"}
            ]}
        });
        assert!(review_actionable(request.clone()));
        request["reviews"]["nodes"][2]["body"] = "LGTM.".into();
        assert!(!review_actionable(request));
    }

    #[test]
    fn resolved_review_thread_does_not_wake_crew() {
        let mut request = serde_json::json!({
            "reviewDecision": null, "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "reviewThreads": {"nodes": [{"isResolved": true, "comments": {"nodes": [{
                "fullDatabaseId": "42", "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "body": "Please fix"
            }]}}]}
        });
        assert!(!review_actionable(request.clone()));
        request["reviewThreads"]["nodes"][0]["isResolved"] = false.into();
        assert!(review_actionable(request));
    }

    #[test]
    fn only_configured_bot_identity_is_actionable() {
        let request = serde_json::json!({
            "reviewDecision": null, "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "comments": {"nodes": [{"databaseId": 42, "createdAt": "2026-09-30T11:00:00Z",
                "author": {"login": "review-helper", "__typename": "Bot"}, "body": "Please fix"}]}
        });
        let observed_at = "2026-09-30T12:00:00Z".parse().expect("time");
        assert!(!review_actionable(request.clone()));
        assert_eq!(
            parse_gh_observation_with_review_bot(&request.to_string(), observed_at, "review-helper")
                .expect("parse")
                .review
                .actionable_at_head
                .value,
            Some(true)
        );
    }

    #[test]
    fn crew_markers_are_accepted_only_from_configured_credential_actor() {
        let mut request = serde_json::json!({
            "reviewDecision": null, "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "reviews": {"nodes": [{"fullDatabaseId": "77", "submittedAt": "2026-09-30T11:00:00Z",
                "author": {"login": "reviewer"}, "state": "COMMENTED", "body": "Please fix"}]},
            "comments": {"nodes": [{"databaseId": 88, "createdAt": "2026-09-30T12:00:00Z",
                "author": {"login": "other-app"}, "body": "<!-- pr-shepherd-addresses:77 -->"}]}
        });
        let observed_at = "2026-09-30T13:00:00Z".parse().expect("time");
        let observe = |request: &serde_json::Value, crew_logins: &[String]| {
            parse_gh_observation_with_crew_identity(&request.to_string(), observed_at, DEFAULT_REVIEW_BOT_LOGIN, None, crew_logins)
                .expect("parse")
                .review
                .actionable_at_head
                .value
        };
        let configured = ["crew-app".to_string()];
        assert_eq!(observe(&request, &configured), Some(true));
        assert_eq!(observe(&request, &[]), Some(true));
        assert_eq!(observe(&request, &["crew-app".to_string(), "other-app".to_string()]), Some(false));
        request["comments"]["nodes"][0]["author"]["login"] = "crew-app".into();
        assert_eq!(observe(&request, &configured), Some(false));
    }

    #[test]
    fn bot_chatter_empty_reviews_and_author_acknowledgments_do_not_wake_crew() {
        let mut request = serde_json::json!({
            "reviewDecision": null, "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "comments": {"nodes": [
                {"databaseId": 1, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "codecov", "__typename": "Bot"}, "body": "Coverage dropped"},
                {"databaseId": 2, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "author", "__typename": "User"}, "body": "Thanks"},
                {"databaseId": 6, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer", "__typename": "User"}, "body": "Thanks!"}
            ]},
            "reviews": {"nodes": [
                {"fullDatabaseId": "3", "submittedAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"}, "state": "APPROVED", "body": ""},
                {"fullDatabaseId": "4", "submittedAt": "2026-09-30T11:00:00Z", "author": {"login": "other"}, "state": "COMMENTED", "body": ""}
            ]}
        });
        assert!(!review_actionable(request.clone()));
        request["comments"]["nodes"].as_array_mut().expect("comments").push(serde_json::json!({
            "databaseId": 5, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "claude", "__typename": "Bot"},
            "body": "Please fix the race"
        }));
        assert!(review_actionable(request));
    }

    #[test]
    fn marker_in_non_crew_comment_cannot_suppress_feedback() {
        let request = serde_json::json!({
            "reviewDecision": null, "author": {"login": "author"},
            "commits": {"nodes": [{"commit": {"committedDate": "2026-09-30T10:00:00Z"}}]},
            "comments": {"nodes": [
                {"databaseId": 42, "createdAt": "2026-09-30T11:00:00Z", "author": {"login": "reviewer"}, "body": "Please fix"},
                {"databaseId": 43, "createdAt": "2026-09-30T11:30:00Z", "author": {"login": "other"},
                 "body": "<!-- pr-shepherd-addresses:42 -->"}
            ]}
        });
        assert!(review_actionable(request));
    }

    #[async_trait]
    impl ChangeRequestObservationSource for CountingSource {
        async fn observe(&self, _subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
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
    fn observes_presentation_fields_and_configured_operator_request() {
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        let status = parse_gh_observation_with_identity(
            r#"{"title":"Keep metadata current","state":"OPEN","isDraft":true,"author":{"login":"contributor"},"reviewDecision":"APPROVED","reviewRequests":{"nodes":[{"requestedReviewer":{"login":"owner"}}]}}"#,
            observed_at,
            DEFAULT_REVIEW_BOT_LOGIN,
            Some("OWNER"),
        )
        .expect("parse");
        assert_eq!(status.title.value.as_deref(), Some("Keep metadata current"));
        assert_eq!(status.author.value.as_deref(), Some("contributor"));
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Draft));
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::Approved));
        assert_eq!(status.review_requested_from_owner.value, Some(true));
        assert_eq!(status.title.observed_at, observed_at);
    }

    #[test]
    fn operator_request_is_unknown_without_identity_or_with_team_request() {
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        let request = r#"{"state":"OPEN","reviewRequests":{"nodes":[{"requestedReviewer":{"__typename":"Team"}}]}}"#;
        let unset = parse_gh_observation_with_identity(request, observed_at, DEFAULT_REVIEW_BOT_LOGIN, None).expect("parse");
        assert_eq!(unset.review_requested_from_owner.value, None);
        let team = parse_gh_observation_with_identity(request, observed_at, DEFAULT_REVIEW_BOT_LOGIN, Some("owner")).expect("parse");
        assert_eq!(team.review_requested_from_owner.value, None);
    }

    #[test]
    fn review_decision_distinguishes_changes_requested_from_review_required() {
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        for (decision, expected) in
            [("CHANGES_REQUESTED", ObservedReviewDecision::ChangesRequested), ("REVIEW_REQUIRED", ObservedReviewDecision::Required)]
        {
            let request = serde_json::json!({"state": "OPEN", "reviewDecision": decision});
            let status = parse_gh_observation(&request.to_string(), observed_at).expect("parse");
            assert_eq!(status.review_decision.value, Some(expected));
        }
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
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::None));
        let absent = parse_gh_observation(r#"{"state":"OPEN"}"#, "2026-08-03T20:00:00Z".parse().expect("time")).expect("parse");
        assert_eq!(absent.review_decision.value, None);
    }

    #[tokio::test]
    async fn concurrent_first_demands_converge_on_one_authority_record() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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

    // Behaviour (#2445): a local convoy keeps history even when a different
    // host observes the forge. A terminal history-bearing local copy is collectible.
    #[tokio::test(start_paused = true)]
    async fn remote_observer_does_not_lose_local_convoy_history() {
        use flotilla_protocol::{IssueSource, NodeId, Relationship, Subject, SubjectKind};
        use flotilla_resources::{Convoy, ConvoySpec, DeclaredSubject};
        for via_replica in [false, true] {
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let remote = ResourceBackend::InMemory(InMemoryBackend::default());
            let subject = ChangeRequestRef { namespace: "ops".into(), service: "github.com".into(), scope: "org/repo".into(), number: 42 };
            let remote_records = remote.using::<ChangeRequest>("ops");
            let record = remote_records
                .create(
                    &InputMeta::builder().name(subject.record_name()).build(),
                    &ChangeRequestSpec::builder()
                        .service(subject.service.clone())
                        .scope(subject.scope.clone())
                        .number(subject.number)
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
                                source: IssueSource { service: subject.service.clone(), scope: subject.scope.clone() },
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
        async fn observe(&self, _: &ChangeRequestRef) -> Result<ChangeRequestStatus, String> {
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
        let backend = ResourceBackend::Http(HttpBackend::new(tls::client(), "invalid://resource-store"));
        let refresher = ChangeRequestRefresher::new(
            "kiwi".into(),
            backend,
            "node".into(),
            Arc::new(UnavailableSource),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef { namespace: "ops".into(), service: "github.com".into(), scope: "org/repo".into(), number: 42 };
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
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
            let subject = ChangeRequestRef { namespace: "ops".into(), service: "github.com".into(), scope: "org/repo".into(), number: 42 };
            let mut status = ConvoyStatus::default();
            status.discover_subject(
                ConvoySubject {
                    kind: ConvoySubjectKind::ChangeRequest,
                    source: IssueSource { service: subject.service.clone(), scope: subject.scope.clone() },
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
            "fleet".to_string(),
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
            "fleet".to_string(),
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        wait_for_calls(&calls, 2).await;
        let before = calls.load(Ordering::SeqCst);
        let hint = Subject::new(SubjectKind::ChangeRequest, "github.com", "flotilla-org/flotilla", 2051);
        refresher.refresh_hint(&hint).await.expect("hint");
        assert_eq!(calls.load(Ordering::SeqCst), before + 2, "both namespaces refresh once");

        let name = change_request_record_name("github.com", "flotilla-org/flotilla", 2051);
        backend.using::<ChangeRequest>("ops").delete(&name).await.expect("delete record during live demand");
        refresher.refresh_hint(&hint).await.expect("hint after record deletion");
        assert!(
            matches!(backend.using::<ChangeRequest>("ops").get(&name).await, Err(ResourceError::NotFound { .. })),
            "hint does not recreate a missing record"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before + 3, "remaining namespace still refreshes");
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_demand_heartbeats_identical_observations() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
            backend.clone(),
            "authority".to_string(),
            Arc::new(CountingSource(Arc::clone(&calls))),
            ChangeRequestRefreshCadence::default(),
        );
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1699,
        };
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "flotilla-org/flotilla".to_string(),
            number: 1366,
        };
        refresher.demand(uuid::Uuid::new_v4(), subject.clone(), None).await.expect("initial demand");
        wait_for_calls(&calls, 1).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(5)).await;
        refresher.demand(uuid::Uuid::new_v4(), subject, Some(Utc::now())).await.expect("late freshness demand");
        wait_for_calls(&calls, 2).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "freshness demand must preempt the remaining 85-second sleep");
    }

    #[tokio::test]
    async fn observation_failure_is_available_to_settlement_diagnostics() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let refresher = ChangeRequestRefresher::new(
            "fleet".to_string(),
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
