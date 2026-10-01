pub mod forgejo;
pub mod github;

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_resources::{ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState};

use crate::providers::types::ChangeRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestAdmission {
    pub id: String,
    pub change_request: ChangeRequest,
    pub base_ref: Option<String>,
}

pub type BoundObservations = HashMap<u64, Result<ChangeRequestStatus, String>>;

#[async_trait]
pub trait ChangeRequestTracker: Send + Sync {
    async fn observe_bound_with_crew_identity(&self, numbers: &[u64], crew_login: Option<&str>) -> Result<BoundObservations, String> {
        let _ = crew_login;
        self.observe_bound(numbers).await
    }

    /// Observe bound requests together. The default uses individual provider
    /// reads and reports state and title; GitHub overrides this with one query
    /// that also includes checks, review, mergeability, and head SHA.
    async fn observe_bound(&self, numbers: &[u64]) -> Result<BoundObservations, String> {
        let mut statuses = HashMap::new();
        for number in numbers {
            let (_, request) = match self.get_change_request(&number.to_string()).await {
                Ok(request) => request,
                Err(error) => {
                    statuses.insert(*number, Err(error));
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
    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, String> {
        Ok(self.list_change_requests(100).await?.into_iter().find(|(_, request)| request.branch == branch))
    }
    #[allow(dead_code)]
    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String>;
    /// Resolve the immutable identity required to admit an existing change
    /// request as a convoy. Providers that expose the base ref should
    /// override this method so admission can provision the exact PR shape.
    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, String> {
        let (id, change_request) = self.get_change_request(id).await?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: None })
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
        let observed = provider.observe_bound(&[1, 2]).await.expect("independent reads");
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
