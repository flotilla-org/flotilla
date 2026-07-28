pub mod github;

use std::path::Path;

use async_trait::async_trait;

use crate::providers::types::ChangeRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestAdmission {
    pub id: String,
    pub change_request: ChangeRequest,
    pub base_ref: Option<String>,
}

#[async_trait]
pub trait ChangeRequestTracker: Send + Sync {
    async fn list_change_requests(&self, repo_root: &Path, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String>;
    /// Resolve the newest change request whose head is exactly `branch`.
    /// Provider overrides may include terminal requests so callers can
    /// distinguish open, merged, and closed work. The default implementation
    /// inherits the visibility of [`Self::list_change_requests`].
    async fn find_change_request_by_branch(&self, repo_root: &Path, branch: &str) -> Result<Option<(String, ChangeRequest)>, String> {
        Ok(self.list_change_requests(repo_root, 100).await?.into_iter().find(|(_, request)| request.branch == branch))
    }
    #[allow(dead_code)]
    async fn get_change_request(&self, repo_root: &Path, id: &str) -> Result<(String, ChangeRequest), String>;
    /// Resolve the immutable identity required to admit an existing change
    /// request as a convoy. Providers that expose the base ref should
    /// override this method so admission can provision the exact PR shape.
    async fn get_change_request_for_admission(&self, repo_root: &Path, id: &str) -> Result<ChangeRequestAdmission, String> {
        let (id, change_request) = self.get_change_request(repo_root, id).await?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: None })
    }
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn merge_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn list_merged_branch_names(&self, repo_root: &Path, limit: usize) -> Result<Vec<String>, String>;
}
