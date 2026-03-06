pub mod github;

use crate::providers::types::Issue;
use async_trait::async_trait;
use std::path::Path;

#[async_trait]
pub trait IssueTracker: Send + Sync {
    #[allow(dead_code)]
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str {
        "Issues"
    }
    fn item_noun(&self) -> &str {
        "issue"
    }
    fn abbreviation(&self) -> &str {
        "#"
    }
    async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<Issue>, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;
}
