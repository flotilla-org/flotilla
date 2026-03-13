pub mod github;

use std::path::Path;

use async_trait::async_trait;

use crate::providers::types::ChangeRequest;

#[async_trait]
pub trait CodeReview: Send + Sync {
    async fn list_change_requests(&self, repo_root: &Path, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String>;
    #[allow(dead_code)]
    async fn get_change_request(&self, repo_root: &Path, id: &str) -> Result<(String, ChangeRequest), String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn list_merged_branch_names(&self, repo_root: &Path, limit: usize) -> Result<Vec<String>, String>;
}
