//! GitHub implementation of the IssueQueryService.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::provider_data::Issue;
use tokio::sync::Mutex;

use super::{CursorId, IssueQuery, IssueQueryService, IssueResultPage};
use crate::providers::{
    gh_api_get, gh_api_get_with_headers,
    github_api::{clamp_per_page, GhApi},
    issue_tracker::github::parse_issue,
    run, CommandRunner,
};

/// How long a cursor can be idle before it is swept away.
const CURSOR_EXPIRY_SECS: u64 = 300;

/// Provider name used for association keys on parsed issues.
const PROVIDER_NAME: &str = "github";

struct CursorState {
    query: IssueQuery,
    repo_slug: String,
    repo_root: PathBuf,
    next_page: u32,
    has_more: bool,
    total: Option<u32>,
    last_accessed: tokio::time::Instant,
    session_id: uuid::Uuid,
}

pub struct GitHubIssueQueryService {
    repo_slug: String,
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
    cursors: Mutex<HashMap<CursorId, CursorState>>,
    /// Tracks which session owns which cursors for disconnect cleanup.
    session_cursors: Mutex<HashMap<uuid::Uuid, Vec<CursorId>>>,
    next_cursor_id: std::sync::atomic::AtomicU64,
}

impl GitHubIssueQueryService {
    pub fn new(repo_slug: String, api: Arc<dyn GhApi>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            repo_slug,
            api,
            runner,
            cursors: Mutex::new(HashMap::new()),
            session_cursors: Mutex::new(HashMap::new()),
            next_cursor_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

/// Opportunistically remove cursors that have not been accessed for `CURSOR_EXPIRY_SECS`.
/// Also cleans `session_cursors` so expired cursor IDs don't accumulate in the secondary index.
fn expire_stale_cursors(cursors: &mut HashMap<CursorId, CursorState>, session_cursors: &mut HashMap<uuid::Uuid, Vec<CursorId>>) {
    let threshold = tokio::time::Instant::now() - std::time::Duration::from_secs(CURSOR_EXPIRY_SECS);
    let expired: Vec<CursorId> = cursors.iter().filter(|(_, state)| state.last_accessed <= threshold).map(|(id, _)| id.clone()).collect();
    for id in &expired {
        if let Some(state) = cursors.remove(id) {
            if let Some(session_ids) = session_cursors.get_mut(&state.session_id) {
                session_ids.retain(|c| c != id);
                if session_ids.is_empty() {
                    session_cursors.remove(&state.session_id);
                }
            }
        }
    }
}

#[async_trait]
impl IssueQueryService for GitHubIssueQueryService {
    async fn open_query(&self, repo: &Path, params: IssueQuery, session_id: uuid::Uuid) -> Result<CursorId, String> {
        let mut cursors = self.cursors.lock().await;
        let mut session_map = self.session_cursors.lock().await;
        expire_stale_cursors(&mut cursors, &mut session_map);

        let id = self.next_cursor_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cursor_id = CursorId::new(format!("gh-{id}"));
        let state = CursorState {
            query: params,
            repo_slug: self.repo_slug.clone(),
            repo_root: repo.to_path_buf(),
            next_page: 1,
            has_more: true,
            total: None,
            last_accessed: tokio::time::Instant::now(),
            session_id,
        };
        cursors.insert(cursor_id.clone(), state);
        session_map.entry(session_id).or_default().push(cursor_id.clone());
        Ok(cursor_id)
    }

    async fn fetch_page(&self, cursor: &CursorId, count: usize) -> Result<IssueResultPage, String> {
        let mut cursors = self.cursors.lock().await;
        {
            let mut session_map = self.session_cursors.lock().await;
            expire_stale_cursors(&mut cursors, &mut session_map);
        }

        let state = cursors.get_mut(cursor).ok_or_else(|| format!("unknown cursor: {:?}", cursor.as_str()))?;
        state.last_accessed = tokio::time::Instant::now();

        if !state.has_more {
            return Ok(IssueResultPage { items: vec![], total: state.total, has_more: false });
        }

        let per_page = clamp_per_page(count);
        let page = state.next_page;
        let repo_slug = state.repo_slug.clone();
        let repo_root = state.repo_root.clone();
        let query = state.query.clone();

        // Drop the lock while doing the network call.
        drop(cursors);

        let (items, has_more, total) = match &query.search {
            None => {
                let endpoint = format!("repos/{}/issues?state=open&per_page={}&page={}", repo_slug, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &repo_root)?;
                let raw_items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
                let issues: Vec<(String, Issue)> = raw_items
                    .into_iter()
                    .filter(|v| !v.as_object().map(|o| o.contains_key("pull_request")).unwrap_or(false))
                    .filter_map(|v| parse_issue(PROVIDER_NAME, &v))
                    .collect();
                (issues, response.has_next_page, None)
            }
            Some(search_term) => {
                let raw_query = format!("repo:{} is:issue is:open {}", repo_slug, search_term);
                let encoded_query = urlencoding::encode(&raw_query);
                let endpoint = format!("search/issues?q={}&per_page={}&page={}", encoded_query, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &repo_root)?;
                let parsed: serde_json::Value = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
                let total_count = parsed["total_count"].as_u64().map(|n| n as u32);
                let items_array = parsed["items"].as_array().ok_or("no items array in search response")?;
                let issues: Vec<(String, Issue)> = items_array.iter().filter_map(|v| parse_issue(PROVIDER_NAME, v)).collect();
                (issues, response.has_next_page, total_count)
            }
        };

        // Re-acquire and update the cursor state.
        let mut cursors = self.cursors.lock().await;
        if let Some(state) = cursors.get_mut(cursor) {
            state.has_more = has_more;
            state.next_page = page + 1;
            if total.is_some() {
                state.total = total;
            }
            Ok(IssueResultPage { items, total: state.total, has_more })
        } else {
            // Cursor was closed or expired while we were fetching — still return the data.
            Ok(IssueResultPage { items, total, has_more })
        }
    }

    async fn close_query(&self, cursor: &CursorId) {
        // Lock order: cursors first, then session_cursors (same as open_query/fetch_page).
        let mut cursors = self.cursors.lock().await;
        let mut session_map = self.session_cursors.lock().await;
        if let Some(state) = cursors.remove(cursor) {
            if let Some(list) = session_map.get_mut(&state.session_id) {
                list.retain(|c| c != cursor);
                if list.is_empty() {
                    session_map.remove(&state.session_id);
                }
            }
        }
    }

    async fn disconnect_session(&self, session_id: uuid::Uuid) -> Vec<CursorId> {
        // Lock order: cursors first, then session_cursors (same as open_query/fetch_page).
        let mut cursors = self.cursors.lock().await;
        let mut session_map = self.session_cursors.lock().await;
        let cursor_ids = session_map.remove(&session_id).unwrap_or_default();
        for cursor_id in &cursor_ids {
            cursors.remove(cursor_id);
        }
        if !cursor_ids.is_empty() {
            tracing::debug!(count = cursor_ids.len(), %session_id, "cleaned up cursors for disconnected session");
        }
        cursor_ids
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
    async fn open_query_returns_valid_cursor_id() {
        let service = mock_service(vec![]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();
        assert!(cursor.as_str().starts_with("gh-"), "cursor id should start with gh- prefix");
    }

    #[tokio::test]
    async fn close_query_removes_cursor() {
        let service = mock_service(vec![]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();
        service.close_query(&cursor).await;

        let result = service.fetch_page(&cursor, 10).await;
        assert!(result.is_err(), "fetching from closed cursor should error");
        assert!(result.unwrap_err().contains("unknown cursor"));
    }

    #[tokio::test]
    async fn fetch_from_unknown_cursor_returns_error() {
        let service = mock_service(vec![]);
        let bogus = CursorId::new("nonexistent");
        let result = service.fetch_page(&bogus, 10).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown cursor"));
    }

    #[tokio::test]
    async fn fetch_page_returns_issues_from_list_endpoint() {
        let body = make_issues_json(3);
        let service = mock_service(vec![ok_response(&body, false)]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();
        let page = service.fetch_page(&cursor, 10).await.unwrap();
        assert_eq!(page.items.len(), 3);
        assert!(!page.has_more);
        assert_eq!(page.items[0].0, "1");
        assert_eq!(page.items[0].1.title, "Issue 1");
    }

    #[tokio::test]
    async fn fetch_page_with_search_uses_search_endpoint() {
        let body = make_search_json(2, 5);
        let service = mock_service(vec![ok_response(&body, true)]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery { search: Some("bug".into()) }, uuid::Uuid::nil()).await.unwrap();
        let page = service.fetch_page(&cursor, 10).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(page.total, Some(5));
    }

    #[tokio::test]
    async fn fetch_page_pagination_advances() {
        let body1 = make_issues_json(2);
        let body2 = make_issues_json(1);
        let service = mock_service(vec![ok_response(&body1, true), ok_response(&body2, false)]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();

        let page1 = service.fetch_page(&cursor, 2).await.unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.has_more);

        let page2 = service.fetch_page(&cursor, 2).await.unwrap();
        assert_eq!(page2.items.len(), 1);
        assert!(!page2.has_more);
    }

    #[tokio::test]
    async fn fetch_page_when_exhausted_returns_empty() {
        let body = make_issues_json(1);
        let service = mock_service(vec![ok_response(&body, false)]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();

        let _page1 = service.fetch_page(&cursor, 10).await.unwrap();
        let page2 = service.fetch_page(&cursor, 10).await.unwrap();
        assert!(page2.items.is_empty());
        assert!(!page2.has_more);
    }

    #[tokio::test]
    async fn fetch_page_filters_pull_requests() {
        let body = r#"[
            {"number": 1, "title": "Real issue", "labels": []},
            {"number": 2, "title": "A PR", "labels": [], "pull_request": {"url": "..."}},
            {"number": 3, "title": "Another issue", "labels": []}
        ]"#;
        let service = mock_service(vec![ok_response(body, false)]);
        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();
        let page = service.fetch_page(&cursor, 10).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].0, "1");
        assert_eq!(page.items[1].0, "3");
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

    #[tokio::test]
    async fn cursor_expiry_removes_stale_cursors() {
        // Test the expire_stale_cursors function directly
        let mut cursors = HashMap::new();
        let mut session_cursors: HashMap<uuid::Uuid, Vec<CursorId>> = HashMap::new();
        let fresh_id = CursorId::new("fresh");
        let stale_id = CursorId::new("stale");
        let session = uuid::Uuid::new_v4();

        cursors.insert(fresh_id.clone(), CursorState {
            query: IssueQuery::default(),
            repo_slug: "owner/repo".into(),
            repo_root: PathBuf::from("/repo"),
            next_page: 1,
            has_more: true,
            total: None,
            last_accessed: tokio::time::Instant::now(),
            session_id: session,
        });

        // Create a stale cursor by setting last_accessed far in the past
        cursors.insert(stale_id.clone(), CursorState {
            query: IssueQuery::default(),
            repo_slug: "owner/repo".into(),
            repo_root: PathBuf::from("/repo"),
            next_page: 1,
            has_more: true,
            total: None,
            last_accessed: tokio::time::Instant::now() - std::time::Duration::from_secs(CURSOR_EXPIRY_SECS + 1),
            session_id: session,
        });

        session_cursors.insert(session, vec![fresh_id.clone(), stale_id.clone()]);

        assert_eq!(cursors.len(), 2);
        assert_eq!(session_cursors.get(&session).map(|v| v.len()), Some(2));

        expire_stale_cursors(&mut cursors, &mut session_cursors);

        assert_eq!(cursors.len(), 1);
        assert!(cursors.contains_key(&fresh_id));
        assert!(!cursors.contains_key(&stale_id));
        // session_cursors should only contain the fresh cursor now
        let remaining = session_cursors.get(&session).expect("session should still exist");
        assert_eq!(remaining.len(), 1);
        assert!(remaining.contains(&fresh_id));
    }

    #[tokio::test]
    async fn disconnect_session_removes_all_session_cursors() {
        let service = mock_service(vec![]);
        let session_a = uuid::Uuid::new_v4();
        let session_b = uuid::Uuid::new_v4();

        let cursor1 = service.open_query(Path::new("/repo"), IssueQuery::default(), session_a).await.unwrap();
        let cursor2 = service.open_query(Path::new("/repo"), IssueQuery::default(), session_a).await.unwrap();
        let cursor3 = service.open_query(Path::new("/repo"), IssueQuery::default(), session_b).await.unwrap();

        // Disconnect session A — its cursors should be gone
        service.disconnect_session(session_a).await;

        assert!(service.fetch_page(&cursor1, 10).await.is_err(), "cursor1 should be cleaned up");
        assert!(service.fetch_page(&cursor2, 10).await.is_err(), "cursor2 should be cleaned up");

        // Session B's cursor is unaffected (but has no mock response, so just check the cursor map)
        assert!(service.cursors.lock().await.contains_key(&cursor3), "cursor3 should still exist");
    }

    #[tokio::test]
    async fn close_query_removes_from_session_cursors() {
        let service = mock_service(vec![]);
        let session = uuid::Uuid::new_v4();

        let cursor = service.open_query(Path::new("/repo"), IssueQuery::default(), session).await.unwrap();
        assert_eq!(service.session_cursors.lock().await.get(&session).map(|v| v.len()), Some(1));

        service.close_query(&cursor).await;
        // Session entry should be cleaned up since it had only one cursor
        assert!(service.session_cursors.lock().await.get(&session).is_none());
    }

    #[tokio::test]
    async fn multiple_cursors_are_independent() {
        let body1 = make_issues_json(2);
        let body2 = make_search_json(1, 1);
        let service = mock_service(vec![ok_response(&body1, false), ok_response(&body2, false)]);

        let cursor1 = service.open_query(Path::new("/repo"), IssueQuery::default(), uuid::Uuid::nil()).await.unwrap();
        let cursor2 = service.open_query(Path::new("/repo"), IssueQuery { search: Some("bug".into()) }, uuid::Uuid::nil()).await.unwrap();

        let page1 = service.fetch_page(&cursor1, 10).await.unwrap();
        let page2 = service.fetch_page(&cursor2, 10).await.unwrap();

        assert_eq!(page1.items.len(), 2);
        assert_eq!(page2.items.len(), 1);

        // Closing one doesn't affect the other
        service.close_query(&cursor1).await;
        assert!(service.fetch_page(&cursor1, 10).await.is_err());
        // cursor2 still works (but exhausted)
        let page2b = service.fetch_page(&cursor2, 10).await.unwrap();
        assert!(page2b.items.is_empty());
    }
}
