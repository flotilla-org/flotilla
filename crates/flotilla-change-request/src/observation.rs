//! Change-request observation interface and GitHub observation decoding.
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_provider_runtime::run;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    address::{record_name, stored_service_scope, Address, AddressKind, GITHUB_FORGE_ID},
    ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability,
    ObservedReviewDecision,
};

pub const DEFAULT_REVIEW_BOT_LOGIN: &str = "claude";

/// One change request observed in one namespace: the observers' key.
///
/// Its identity is the change request's [`Address`], so every spelling of
/// one change request (host or forge id, any repository case) is one
/// observation subject with one record. Its fields are private because it
/// keys maps: [`ChangeRequestKey::new`] is the only way to make one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeRequestKey {
    namespace: String,
    address: Address,
}

impl ChangeRequestKey {
    /// The key for a change-request address; any other address is refused.
    ///
    /// It checks the whole shape the accessors read (a forge, a repository,
    /// a number, a record name and a stored `(service, scope)`), so the
    /// accessors below cannot panic. The address grammar already puts a
    /// change request under a repository under a forge; this check states it
    /// here.
    pub fn new(namespace: impl Into<String>, address: Address) -> Result<Self, String> {
        if address.kind() != AddressKind::ChangeRequest {
            return Err(format!("{address} is not a change request"));
        }
        if address.get(AddressKind::Forge).is_none()
            || address.parent().is_none()
            || address.get(AddressKind::Repo).is_none()
            || address.number().is_none()
            || record_name(&address).is_none()
            || stored_service_scope(&address).is_none()
        {
            return Err(format!("{address} is not a change request under a forge repository"));
        }
        Ok(Self { namespace: namespace.into(), address })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The repository the change request belongs to.
    pub fn repository(&self) -> Address {
        self.address.parent().expect("ChangeRequestKey::new checked the repository")
    }

    /// The forge id: `github` for public GitHub.
    pub fn forge_id(&self) -> &str {
        self.address.get(AddressKind::Forge).expect("ChangeRequestKey::new checked the forge")
    }

    /// The repository's `owner/repo`, in its forge's case.
    pub fn repository_path(&self) -> &str {
        self.address.get(AddressKind::Repo).expect("ChangeRequestKey::new checked the repository")
    }

    pub fn number(&self) -> u64 {
        self.address.number().expect("ChangeRequestKey::new checked the number")
    }

    /// The name of the record that observes this change request.
    pub fn record_name(&self) -> String {
        record_name(&self.address).expect("ChangeRequestKey::new checked the record name")
    }

    /// The `(service, scope)` its record spec stores.
    pub fn stored_service_scope(&self) -> (String, String) {
        stored_service_scope(&self.address).expect("ChangeRequestKey::new checked the stored spelling")
    }
}

#[async_trait]
pub trait ChangeRequestObservationSource: Send + Sync {
    async fn observe(&self, subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError>;

    async fn observe_group(
        &self,
        subjects: &[ChangeRequestKey],
        subject: &ChangeRequestKey,
    ) -> Result<ChangeRequestStatus, ObservationError> {
        let _ = subjects;
        self.observe(subject).await
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
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
    async fn observe(&self, subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
        let repository = github_repository(subject)?;
        let number = subject.number().to_string();
        let output = run!(
            self.runner,
            "gh",
            &["pr", "view", &number, "--repo", repository, "--json", "state,headRefOid,statusCheckRollup,reviewDecision,mergeable",],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now()).map_err(ObservationError::Failed)
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestKey) -> Result<ChangeRequestStatus, ObservationError> {
        let repository = github_repository(subject)?;
        let number = subject.number().to_string();
        let output = run!(
            self.runner,
            "gh",
            &["pr", "view", &number, "--repo", repository, "--json", "state,isDraft,headRefOid,statusCheckRollup,reviewDecision,mergeable"],
            Path::new("/"),
        )?;
        parse_gh_observation(&output, Utc::now()).map_err(ObservationError::Failed)
    }
}

/// `gh` reads only public GitHub, the built-in `github` forge.
fn github_repository(subject: &ChangeRequestKey) -> Result<&str, ObservationError> {
    if subject.forge_id() != GITHUB_FORGE_ID {
        return Err(format!("change request observation forge `{}` is not available on this host", subject.forge_id()).into());
    }
    Ok(subject.repository_path())
}

pub fn parse_gh_observation(json: &str, observed_at: DateTime<Utc>) -> Result<ChangeRequestStatus, String> {
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

pub fn parse_gh_observation_value_with_crew_identity(
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

#[cfg(test)]
mod tests {
    use flotilla_resources::{ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ObservedReviewDecision};

    use super::{
        github_repository, parse_gh_observation, parse_gh_observation_with_crew_identity, parse_gh_observation_with_identity,
        parse_gh_observation_with_review_bot, ChangeRequestKey, DEFAULT_REVIEW_BOT_LOGIN,
    };

    fn key(service: &str, scope: &str) -> ChangeRequestKey {
        let address = flotilla_resources::address::stored_change_request_address(service, scope, 42, &[]).expect("address");
        ChangeRequestKey::new("flotilla", address).expect("key")
    }

    // #3359: a key's identity is its Address, so every spelling of one change
    // request is one key, and a key names only a change request.
    #[test]
    fn spellings_of_one_change_request_are_one_key() {
        assert_eq!(key("github.com", "Flotilla-Org/Flotilla"), key("github", "flotilla-org/flotilla"));
        assert_ne!(key("github.com", "flotilla-org/flotilla"), key("github.com", "flotilla-org/cleat"));
        let issue = flotilla_resources::address::stored_issue_address("github.com", "flotilla-org/flotilla", 42, &[]).expect("issue");
        assert!(ChangeRequestKey::new("flotilla", issue).is_err());
    }

    // `gh` reads only the built-in GitHub forge; another forge is refused
    // before any command runs.
    #[test]
    fn gh_reads_only_the_github_forge() {
        assert_eq!(github_repository(&key("github.com", "Flotilla-Org/Flotilla")).ok(), Some("flotilla-org/flotilla"));
        assert!(github_repository(&key("lab", "team/repo")).is_err());
    }

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
}
