pub mod github;

use std::path::Path;
use async_trait::async_trait;
use crate::providers::types::ChangeRequest;

#[async_trait]
pub trait CodeReview: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_change_requests(&self, repo_root: &Path, limit: usize) -> Result<Vec<ChangeRequest>, String>;
    async fn get_change_request(&self, repo_root: &Path, id: &str) -> Result<ChangeRequest, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
}
