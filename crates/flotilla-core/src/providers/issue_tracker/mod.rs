pub mod github;

use std::path::Path;

use async_trait::async_trait;

use crate::providers::types::{Issue, IssueChangeset, IssuePage};

#[async_trait]
pub trait IssueTracker: Send + Sync {
    async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<(String, Issue)>, String>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<(), String>;

    async fn list_issues_page(&self, repo_root: &Path, page: u32, per_page: usize) -> Result<IssuePage, String> {
        // Default: delegate to list_issues for page 1 only
        if page > 1 {
            return Ok(IssuePage { issues: vec![], total_count: None, has_more: false });
        }
        let issues = self.list_issues(repo_root, per_page).await?;
        let has_more = issues.len() >= per_page;
        Ok(IssuePage { issues, total_count: None, has_more })
    }

    async fn fetch_issues_by_id(&self, _repo_root: &Path, _ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }

    async fn search_issues(&self, _repo_root: &Path, _query: &str, _limit: usize) -> Result<Vec<(String, Issue)>, String> {
        Ok(vec![])
    }

    /// Incremental sync: returns issues changed since the given ISO 8601 timestamp.
    /// Default implementation falls back to a full page-1 fetch (no evictions).
    async fn list_issues_changed_since(&self, repo_root: &Path, _since: &str, per_page: usize) -> Result<IssueChangeset, String> {
        let page = self.list_issues_page(repo_root, 1, per_page).await?;
        Ok(IssueChangeset { updated: page.issues, closed_ids: vec![], has_more: page.has_more })
    }
}
