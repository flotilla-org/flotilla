pub mod github;

use std::path::Path;

use async_trait::async_trait;

use crate::providers::types::ChangeRequest;

#[async_trait]
pub trait ChangeRequestTracker: Send + Sync {
    async fn list_change_requests(&self, repo_root: &Path, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String>;
    /// Resolve the newest change request whose head is exactly `branch`.
    /// Providers should include terminal requests so callers can distinguish
    /// open, merged, and closed work.
    async fn find_change_request_by_branch(&self, repo_root: &Path, branch: &str) -> Result<Option<(String, ChangeRequest)>, String> {
        Ok(self.list_change_requests(repo_root, 100).await?.into_iter().find(|(_, request)| request.branch == branch))
    }
    #[allow(dead_code)]
    async fn get_change_request(&self, repo_root: &Path, id: &str) -> Result<(String, ChangeRequest), String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn list_merged_branch_names(&self, repo_root: &Path, limit: usize) -> Result<Vec<String>, String>;
}
