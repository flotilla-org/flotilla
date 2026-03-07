pub mod github;

use crate::providers::types::ChangeRequest;
use async_trait::async_trait;
use std::path::Path;

#[async_trait]
pub trait CodeReview: Send + Sync {
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str {
        "Change Requests"
    }
    fn item_noun(&self) -> &str {
        "change request"
    }
    fn abbreviation(&self) -> &str {
        "CR"
    }
    async fn list_change_requests(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<ChangeRequest>, String>;
    #[allow(dead_code)]
    async fn get_change_request(&self, repo_root: &Path, id: &str)
        -> Result<ChangeRequest, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
    async fn list_merged_branch_names(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<String>, String>;
}
