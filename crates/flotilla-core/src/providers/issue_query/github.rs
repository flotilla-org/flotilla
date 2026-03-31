//! GitHub implementation of the IssueQueryService.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::provider_data::Issue;

use super::{IssueQuery, IssueQueryService, IssueResultPage};
use crate::providers::{
    gh_api_get, gh_api_get_with_headers,
    github_api::{clamp_per_page, GhApi},
    issue_tracker::github::parse_issue,
    run, CommandRunner,
};

/// Provider name used for association keys on parsed issues.
const PROVIDER_NAME: &str = "github";

pub struct GitHubIssueQueryService {
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
}

impl GitHubIssueQueryService {
    pub fn new(repo_slug: String, api: Arc<dyn GhApi>, runner: Arc<dyn CommandRunner>) -> Self {
        Self { repo_slug, api, runner }
    }
}

#[async_trait]
impl IssueQueryService for GitHubIssueQueryService {
    async fn query(&self, repo: &Path, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        let per_page = clamp_per_page(count);
        let (items, has_more, total) = match &params.search {
            None => {
                let endpoint = format!("repos/{}/issues?state=open&per_page={}&page={}", self.repo_slug, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, repo)?;
                let raw_items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
                let issues: Vec<(String, Issue)> = raw_items
                    .into_iter()
                    .filter(|v| !v.as_object().map(|o| o.contains_key("pull_request")).unwrap_or(false))
                    .filter_map(|v| parse_issue(PROVIDER_NAME, &v))
                    .collect();
                (issues, response.has_next_page, None)
            }
            Some(search_term) => {
                let raw_query = format!("repo:{} is:issue is:open {}", self.repo_slug, search_term);
                let encoded_query = urlencoding::encode(&raw_query);
                let endpoint = format!("search/issues?q={}&per_page={}&page={}", encoded_query, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, repo)?;
                let parsed: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
                let total_count = parsed["total_count"].as_u64().map(|n| n as u32);
                let items_array = parsed["items"].as_array().ok_or("no items array in search response")?;
                let issues: Vec<(String, Issue)> = items_array.iter().filter_map(|v| parse_issue(PROVIDER_NAME, v)).collect();
                (issues, response.has_next_page, total_count)
            }
        };
        Ok(IssueResultPage { items, total, has_more })
    }

    async fn fetch_by_ids(&self, repo: &Path, ids: &[String]) -> Result<Vec<(String, Issue)>, String> {
        use futures::stream::{
            StreamExt, {self},
        };

        let repo_root = repo.to_path_buf();
        let futs: Vec<_> = ids
            .iter()
            .map(|id| {
                let endpoint = format!("repos/{}/issues/{}", self.repo_slug, id);
                let api = Arc::clone(&self.api);
                let id = id.clone();
                let repo_root = repo_root.clone();
                async move {
                    let body = gh_api_get!(api, &endpoint, &repo_root)?;
                    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
                    parse_issue(PROVIDER_NAME, &v).ok_or_else(|| format!("failed to parse issue {}", id))
                }
            })
            .collect();

        let results: Vec<_> = stream::iter(futs).buffer_unordered(10).collect().await;
        let mut issues = Vec::new();
        for result in results {
            match result {
                Ok(issue) => issues.push(issue),
                Err(e) => tracing::warn!(provider = "github", err = %e, "failed to fetch issue by id"),
            }
        }
        Ok(issues)
    }

    async fn open_in_browser(&self, repo: &Path, id: &str) -> Result<(), String> {
        run!(self.runner, "gh", &["issue", "view", id, "--web"], repo)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex as StdMutex};

    use super::*;
    use crate::providers::{
        github_api::{GhApi, GhApiResponse},
        testing::MockRunner,
        ChannelLabel,
    };

    struct MockGhApi {
        responses: StdMutex<VecDeque<Result<GhApiResponse, String>>>,
    }

    impl MockGhApi {
        fn new(responses: Vec<Result<GhApiResponse, String>>) -> Self {
            Self { responses: StdMutex::new(responses.into()) }
        }
    }

    #[async_trait]
    impl GhApi for MockGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root, label).await.map(|r| r.body)
        }
        async fn get_with_headers(&self, _endpoint: &str, _repo_root: &Path, _label: &ChannelLabel) -> Result<GhApiResponse, String> {
            self.responses.lock().unwrap().pop_front().expect("MockGhApi: no more responses")
        }
    }

    fn make_issues_json(count: usize) -> String {
        let issues: Vec<String> = (1..=count).map(|n| format!(r#"{{"number": {}, "title": "Issue {}", "labels": []}}"#, n, n)).collect();
        format!("[{}]", issues.join(","))
    }

    fn make_search_json(count: usize, total: usize) -> String {
        let issues: Vec<String> =
            (1..=count).map(|n| format!(r#"{{"number": {}, "title": "Search result {}", "labels": []}}"#, n, n)).collect();
        format!(r#"{{"total_count": {}, "items": [{}]}}"#, total, issues.join(","))
    }

    fn mock_service(responses: Vec<Result<GhApiResponse, String>>) -> GitHubIssueQueryService {
        let api = Arc::new(MockGhApi::new(responses));
        let runner = Arc::new(MockRunner::new(vec![]));
        GitHubIssueQueryService::new("owner/repo".into(), api, runner)
    }

    fn ok_response(body: &str, has_next_page: bool) -> Result<GhApiResponse, String> {
        Ok(GhApiResponse { status: 200, etag: None, body: body.to_string(), has_next_page, total_count: None })
    }

    #[tokio::test]
    async fn query_returns_issues_from_list_endpoint() {
        let body = make_issues_json(3);
        let service = mock_service(vec![ok_response(&body, false)]);
        let params = IssueQuery::default();
        let page = service.query(Path::new("/repo"), &params, 1, 10).await.unwrap();
        assert_eq!(page.items.len(), 3);
        assert!(!page.has_more);
        assert_eq!(page.items[0].0, "1");
        assert_eq!(page.items[0].1.title, "Issue 1");
    }

    #[tokio::test]
    async fn query_with_search_uses_search_endpoint() {
        let body = make_search_json(2, 5);
        let service = mock_service(vec![ok_response(&body, true)]);
        let params = IssueQuery { search: Some("bug".into()) };
        let page = service.query(Path::new("/repo"), &params, 1, 10).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.total, Some(5));
    }

    #[tokio::test]
    async fn query_filters_pull_requests() {
        let body = r#"[
            {"number": 1, "title": "Real issue", "labels": []},
            {"number": 2, "title": "A PR", "labels": [], "pull_request": {"url": "..."}},
            {"number": 3, "title": "Another issue", "labels": []}
        ]"#;
        let service = mock_service(vec![ok_response(body, false)]);
        let params = IssueQuery::default();
        let page = service.query(Path::new("/repo"), &params, 1, 10).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].0, "1");
        assert_eq!(page.items[1].0, "3");
    }

    #[tokio::test]
    async fn query_pagination_uses_page_param() {
        let body1 = make_issues_json(2);
        let body2 = make_issues_json(1);
        let service = mock_service(vec![ok_response(&body1, true), ok_response(&body2, false)]);
        let params = IssueQuery::default();

        let page1 = service.query(Path::new("/repo"), &params, 1, 2).await.unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);

        let page2 = service.query(Path::new("/repo"), &params, 2, 2).await.unwrap();
        assert_eq!(page2.items.len(), 1);
        assert!(!page2.has_more);
    }

    #[tokio::test]
    async fn fetch_by_ids_returns_matching_issues() {
        let body1 = r#"{"number": 42, "title": "The answer", "labels": [{"name": "bug"}]}"#;
        let body2 = r#"{"number": 99, "title": "Another one", "labels": []}"#;
        let api = Arc::new(MockGhApi::new(vec![
            Ok(GhApiResponse { status: 200, etag: None, body: body1.into(), has_next_page: false, total_count: None }),
            Ok(GhApiResponse { status: 200, etag: None, body: body2.into(), has_next_page: false, total_count: None }),
        ]));
        let runner = Arc::new(MockRunner::new(vec![]));
        let svc = GitHubIssueQueryService::new("owner/repo".into(), api, runner);

        let issues = svc.fetch_by_ids(Path::new("/repo"), &["42".into(), "99".into()]).await.unwrap();
        assert_eq!(issues.len(), 2);
        // buffer_unordered may reorder, so check by collecting ids
        let ids: Vec<&str> = issues.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"42"));
        assert!(ids.contains(&"99"));
    }

    #[tokio::test]
    async fn open_in_browser_calls_gh_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let api = Arc::new(MockGhApi::new(vec![]));
        let svc = GitHubIssueQueryService::new("owner/repo".into(), api, runner.clone());

        svc.open_in_browser(Path::new("/repo"), "42").await.unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "gh");
        assert_eq!(calls[0].1, vec!["issue", "view", "42", "--web"]);
    }
}
