pub mod github;

use crate::providers::types::{Issue, IssuePage};
use async_trait::async_trait;
use std::path::Path;

#[async_trait]
pub trait IssueTracker: Send + Sync {
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
    async fn list_issues(
        &self,
        repo_root: &Path,
        limit: usize,
    ) -> Result<Vec<(String, Issue)>, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;

    async fn list_issues_page(
        &self,
        repo_root: &Path,
        page: u32,
        per_page: usize,
    ) -> Result<IssuePage, String> {
        // Default: delegate to list_issues for page 1 only
        if page > 1 {
            return Ok(IssuePage {
                issues: vec![],
                total_count: None,
                has_more: false,
            });
        }
        let issues = self.list_issues(repo_root, per_page).await?;
        let has_more = issues.len() >= per_page;
        Ok(IssuePage {
            issues,
            total_count: None,
            has_more,
        })
    }

    async fn fetch_issues_by_id(
        &self,
        _repo_root: &Path,
        _ids: &[String],
    ) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }

    async fn search_issues(
        &self,
        _repo_root: &Path,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }
}
