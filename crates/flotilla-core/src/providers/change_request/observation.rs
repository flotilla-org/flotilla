//! Change-request observation interface and GitHub observation decoding.
use crate::providers::{forge::observation_error::ObservationError, run, CommandRunner};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_relay_protocol::Subject;
use flotilla_resources::{
    change_request_record_name, ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState,
    ObservedChecks, ObservedMergeability, ObservedReviewDecision,
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

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

    pub(crate) fn normalized(mut self) -> Self {
        (self.service, self.scope) = Subject::normalize_scope(&self.service, &self.scope);
        self
    }
}

#[async_trait]
pub trait ChangeRequestObservationSource: Send + Sync {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError>;

    async fn observe_group(
        &self,
        subjects: &[ChangeRequestRef],
        subject: &ChangeRequestRef,
    ) -> Result<ChangeRequestStatus, ObservationError> {
        let _ = subjects;
        self.observe(subject).await
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
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
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
        if subject.service != "github.com" {
            return Err(format!("change request observation service `{}` is not available on this host", subject.service).into());
        }
        let number = subject.number.to_string();
        let output = run!(
            self.runner,
            "gh",
            &["pr", "view", &number, "--repo", &subject.scope, "--json", "state,headRefOid,statusCheckRollup,reviewDecision,mergeable",],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now()).map_err(ObservationError::Forge)
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestRef) -> Result<ChangeRequestStatus, ObservationError> {
        if subject.service != "github.com" {
            return Err(format!("change request observation service `{}` is not available on this host", subject.service).into());
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
        parse_gh_observation(&output, Utc::now()).map_err(ObservationError::Forge)
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
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| format!("decode gh pr observation: {error}"))?;
    Ok(parse_gh_observation_value_with_crew_identity(&value, observed_at, review_bot_login, operator_login, crew_logins))
}

pub(crate) fn parse_gh_observation_value_with_crew_identity(
    value: &serde_json::Value,
    observed_at: DateTime<Utc>,
    review_bot_login: &str,
    operator_login: Option<&str>,
    crew_logins: &[String],
) -> ChangeRequestStatus {
    let crew_logins = crew_logins.iter().map(String::as_str).collect::<HashSet<_>>();
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
    ChangeRequestStatus {
        title: Observation { value: value["title"].as_str().map(str::to_string), observed_at },
        author: Observation { value: value["author"]["login"].as_str().map(str::to_string), observed_at },
        review_decision: Observation { value: review_decision, observed_at },
        review_requested_from_owner: Observation { value: review_requested_from_owner, observed_at },
        state: Observation { value: state, observed_at },
        head_sha: Observation { value: head_sha, observed_at },
        checks: Observation { value: checks, observed_at },
        review: ChangeRequestReviewObservation { actionable_at_head: Observation { value: actionable_at_head, observed_at } },
        mergeable: Observation { value: mergeable, observed_at },
    }
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
