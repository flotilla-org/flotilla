pub mod github;

use std::path::Path;

use async_trait::async_trait;
pub use flotilla_protocol::issue_query::{IssueQuery, IssueResultPage};
use flotilla_protocol::provider_data::Issue;

/// Stateless paged query interface for issue listing and search.
#[async_trait]
pub trait IssueQueryService: Send + Sync {
    /// Fetch a page of issues. `params.search` of `None` lists open issues.
    async fn query(&self, repo: &Path, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String>;

    /// Fetch specific issues by ID (for linked/pinned issue resolution).
    async fn fetch_by_ids(&self, repo: &Path, ids: &[String]) -> Result<Vec<(String, Issue)>, String>;

    /// Open an issue in the browser.
    async fn open_in_browser(&self, repo: &Path, id: &str) -> Result<(), String>;
}
