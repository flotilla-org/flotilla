pub mod forgejo;
pub mod github;

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState};

use crate::providers::{github_api::GithubRateLimit, types::ChangeRequest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestAdmission {
    pub id: String,
    pub change_request: ChangeRequest,
    pub base_ref: Option<String>,
}

/// Observation failures retain forge classification until a presentation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    Forge(String),
    RateLimited { budget: String, limit: GithubRateLimit },
}

impl ObservationError {
    pub fn retry_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::RateLimited { limit, .. } => limit.retry_at,
            Self::Forge(_) => None,
        }
    }
}

impl From<String> for ObservationError {
    fn from(error: String) -> Self {
        Self::Forge(error)
    }
}
impl From<&str> for ObservationError {
    fn from(error: &str) -> Self {
        Self::Forge(error.to_string())
    }
}
impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forge(error) => f.write_str(error),
            Self::RateLimited { budget, limit } => write!(
                f,
                "github rate limited (budget={budget}, identity=host gh login, kind={}, retry_source={}, retry_at={})",
                limit.kind.as_str(),
                limit.retry_source,
                limit.retry_at.map(|at| at.to_rfc3339()).unwrap_or_else(|| "unavailable".into())
            ),
        }
    }
}
impl std::error::Error for ObservationError {}

pub type BoundObservations = HashMap<u64, Result<ChangeRequestStatus, ObservationError>>;
pub type CrewGithubLoginsByRequest = BTreeMap<u64, Vec<String>>;

#[async_trait]
pub trait ChangeRequestTracker: Send + Sync {
    /// Background servicing must not renew the demand that scheduled it.
    fn for_background_refresh(&self) -> Option<std::sync::Arc<dyn ChangeRequestTracker>> {
        None
    }
    /// Observe bound requests together. The default uses individual provider
    /// reads and reports state and title; GitHub overrides this with one query
    /// that also includes checks, review, mergeability, and head SHA. Feedback
    /// uses a matching force-push event when available, then the head's GitHub
    /// push time. When GitHub exposes neither, it falls back to committedDate;
    /// delayed or edited commit timestamps can then over- or under-report
    /// feedback until an explicit response or a newer head is observed.
    /// Crew identities are GraphQL App logins, scoped by PR number and derived
    /// from bound convoys' credential declarations. Other forges can ignore them.
    async fn observe_bound(&self, numbers: &[u64], crew_logins: &CrewGithubLoginsByRequest) -> Result<BoundObservations, ObservationError> {
        let _ = crew_logins;
        let mut statuses = HashMap::new();
        for number in numbers {
            let (_, request) = match self.get_change_request(&number.to_string()).await {
                Ok(request) => request,
                Err(error) => {
                    statuses.insert(*number, Err(error.into()));
                    continue;
                }
            };
            let state = match request.status {
                flotilla_protocol::ChangeRequestStatus::Open => ObservedChangeRequestState::Open,
                flotilla_protocol::ChangeRequestStatus::Draft => ObservedChangeRequestState::Draft,
                flotilla_protocol::ChangeRequestStatus::Merged => ObservedChangeRequestState::Merged,
                flotilla_protocol::ChangeRequestStatus::Closed => ObservedChangeRequestState::Closed,
            };
            let observed_at = Utc::now();
            statuses.insert(
                *number,
                Ok(ChangeRequestStatus {
                    title: Observation::known(request.title, observed_at),
                    author: Observation::unknown(observed_at),
                    review_decision: Observation::unknown(observed_at),
                    review_requested_from_owner: Observation::unknown(observed_at),
                    state: Observation::known(state, observed_at),
                    head_sha: Observation::unknown(observed_at),
                    checks: Observation::unknown(observed_at),
                    review: ChangeRequestReviewObservation { actionable_at_head: Observation::unknown(observed_at) },
                    mergeable: Observation::unknown(observed_at),
                }),
            );
        }
        Ok(statuses)
    }
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String>;
    /// Resolve the newest change request whose head is exactly `branch`.
    /// Provider overrides may include terminal requests so callers can
    /// distinguish open, merged, and closed work. The default implementation
    /// inherits the visibility of [`Self::list_change_requests`].
    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        Ok(self.list_change_requests(100).await?.into_iter().find(|(_, request)| request.branch == branch))
    }
    #[allow(dead_code)]
    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String>;
    /// Resolve the immutable identity required to admit an existing change
    /// request as a convoy. Providers that expose the base ref should
    /// override this method so admission can provision the exact PR shape.
    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        let (id, change_request) = self.get_change_request(id).await?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: None })
    }
    /// Replace the forge request's body without relying on a working copy.
    async fn update_body(&self, _id: &str, _body: &str) -> Result<(), String> {
        Err("change request provider does not support editing bodies".into())
    }

    /// Preserve the existing body and append missing issue-closing references.
    /// This read-modify-write can race with concurrent forge edits: the provider
    /// API offers no conditional update, so callers should avoid parallel edits.
    async fn link_issues(&self, id: &str, issue_ids: &[String]) -> Result<(), String> {
        if issue_ids.is_empty() {
            return Ok(());
        }
        let (_, request) = self.get_change_request(id).await?;
        let current = request.body.unwrap_or_default();
        let mut body = if current.trim().is_empty() { String::new() } else { current.trim_end().to_string() };
        for issue in issue_ids {
            let line = format!("Fixes #{issue}");
            if !body.lines().any(|existing| existing == line) {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&line);
            }
        }
        if body != current.trim_end() {
            self.update_body(id, &body).await?;
        }
        Ok(())
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String>;
    async fn close_change_request(&self, id: &str) -> Result<(), String>;
    async fn merge_change_request(&self, id: &str) -> Result<(), String>;
    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::test_support::FakeChangeRequest;

    #[tokio::test]
    async fn default_bound_observation_preserves_healthy_requests_after_a_missing_one() {
        let provider = FakeChangeRequest::new();
        provider
            .add_change_requests(vec![("2".to_string(), ChangeRequest {
                title: "Healthy PR".to_string(),
                branch: "healthy".to_string(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "fake".to_string(),
                provider_display_name: "Fake".to_string(),
            })])
            .await;
        let observed = provider.observe_bound(&[1, 2], &Default::default()).await.expect("independent reads");
        assert!(observed.get(&1).expect("missing PR result").as_ref().is_err());
        assert_eq!(
            observed.get(&2).expect("healthy PR result").as_ref().expect("healthy PR").state.value,
            Some(ObservedChangeRequestState::Open)
        );
        let status = observed.get(&2).expect("healthy PR result").as_ref().expect("healthy PR");
        assert_eq!(status.author.observed_at, status.state.observed_at);
        assert_eq!(status.review_decision.observed_at, status.state.observed_at);
        assert_eq!(status.review_requested_from_owner.observed_at, status.state.observed_at);
    }
}
