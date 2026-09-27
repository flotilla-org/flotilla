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

#[async_trait]
pub trait ChangeRequestTracker: Send + Sync {
    /// Observe bound requests together. The default uses individual provider
    /// reads and reports only the state; GitHub overrides this with one query
    /// that also includes checks, review, mergeability, and head SHA.
    async fn observe_bound(&self, numbers: &[u64]) -> Result<HashMap<u64, ChangeRequestStatus>, String> {
        let mut statuses = HashMap::new();
        for number in numbers {
            let (_, request) = self.get_change_request(&number.to_string()).await?;
            let state = match request.status {
                flotilla_protocol::ChangeRequestStatus::Open => ObservedChangeRequestState::Open,
                flotilla_protocol::ChangeRequestStatus::Draft => ObservedChangeRequestState::Draft,
                flotilla_protocol::ChangeRequestStatus::Merged => ObservedChangeRequestState::Merged,
                flotilla_protocol::ChangeRequestStatus::Closed => ObservedChangeRequestState::Closed,
            };
            let observed_at = Utc::now();
            statuses.insert(*number, ChangeRequestStatus {
                state: Observation::known(state, observed_at),
                head_sha: Observation::unknown(observed_at),
                checks: Observation::unknown(observed_at),
                review: ChangeRequestReviewObservation { actionable_at_head: Observation::unknown(observed_at) },
                mergeable: Observation::unknown(observed_at),
            });
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
