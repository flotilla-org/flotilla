pub mod github;

use std::path::Path;
use async_trait::async_trait;
use crate::providers::types::Issue;

#[async_trait]
pub trait IssueTracker: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<Issue>, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
}
