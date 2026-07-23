use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    AssociationKey, Issue, IssueChangeset, IssueRef, IssueSource, IssueState,
};

use crate::providers::{
    gh_api_get, gh_api_get_with_headers,
    github_api::{clamp_per_page, GhApi},
    run, CommandRunner,
};

pub struct GitHubIssueProvider {
    api: Arc<dyn GhApi>,
    runner: Arc<dyn CommandRunner>,
    host_root: Box<Path>,
}

impl GitHubIssueProvider {
    pub fn new(api: Arc<dyn GhApi>, runner: Arc<dyn CommandRunner>, host_root: impl Into<Box<Path>>) -> Self {
        Self { api, runner, host_root: host_root.into() }
    }
}

fn is_github_source(source: &IssueSource) -> bool {
    // Bare service names are valid for explicit Project issue-source overrides;
    // Forge-derived sources use the canonical URL form.
    matches!(source.service.trim_end_matches('/'), "github" | "github.com" | "https://github.com")
}

fn parse_issue(source: &IssueSource, v: &serde_json::Value, fetched_at: DateTime<Utc>) -> Option<Issue> {
    let number = v["number"].as_i64()?;
    let title = v["title"].as_str()?.to_string();
    let body = v["body"].as_str().map(str::to_string);
    let state = match v["state"].as_str()? {
        "open" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return None,
    };
    let labels: Vec<String> = v["labels"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|l| l["name"].as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let id = number.to_string();
    let as_of = v["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).unwrap_or(fetched_at);
    let reference = IssueRef { source: source.clone(), id: id.clone() };
    let association_keys = vec![AssociationKey::IssueRef("github".to_string(), id)];
    Some(
        Issue::builder()
            .reference(reference)
            .title(title)
            .maybe_body(body)
            .state(state)
            .labels(labels)
            .as_of(as_of)
            .observed_at(fetched_at)
            .association_keys(association_keys)
            .provider_name("github".into())
            .provider_display_name("GitHub".into())
            .build(),
    )
}

#[async_trait]
impl super::IssueProvider for GitHubIssueProvider {
    fn supports(&self, source: &IssueSource) -> bool {
        is_github_source(source)
    }

    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        let per_page = clamp_per_page(count);
        let as_of = Utc::now();
        let (items, has_more, total) = match &params.search {
            None => {
                let endpoint =
                    format!("repos/{}/issues?state=open&sort=updated&direction=desc&per_page={}&page={}", source.scope, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
                let raw_items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
                let issues = raw_items
                    .into_iter()
                    .filter(|value| !value.as_object().is_some_and(|object| object.contains_key("pull_request")))
                    .filter_map(|value| parse_issue(source, &value, as_of))
                    .collect();
                (issues, response.has_next_page, None)
            }
            Some(search_term) => {
                let raw_query = format!("repo:{} is:issue is:open {}", source.scope, search_term);
                let encoded_query = urlencoding::encode(&raw_query);
                let endpoint = format!("search/issues?q={}&sort=updated&order=desc&per_page={}&page={}", encoded_query, per_page, page);
                let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
                let parsed: serde_json::Value = serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
                let total = parsed["total_count"].as_u64().and_then(|value| u32::try_from(value).ok());
                let raw_items = parsed["items"].as_array().ok_or("no items array in search response")?;
                let issues = raw_items.iter().filter_map(|value| parse_issue(source, value, as_of)).collect();
                (issues, response.has_next_page, total)
            }
        };
        Ok(IssueResultPage { items, total, has_more })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        let endpoint = format!("repos/{}/issues/{}", reference.source.scope, reference.id);
        let body = gh_api_get!(self.api, &endpoint, &self.host_root)?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        parse_issue(&reference.source, &value, Utc::now()).ok_or_else(|| format!("failed to parse issue {}", reference.id))
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String> {
        let per_page = clamp_per_page(count);
        let encoded_since = urlencoding::encode(since);
        let endpoint =
            format!("repos/{}/issues?state=all&since={}&sort=updated&direction=desc&per_page={}", source.scope, encoded_since, per_page);
        let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        let as_of = Utc::now();

        let mut updated = Vec::new();
        let mut closed = Vec::new();

        for v in &items {
            if v.as_object().map(|o| o.contains_key("pull_request")).unwrap_or(false) {
                continue;
            }
            let state = v["state"].as_str().unwrap_or("open");
            if state == "open" {
                if let Some(issue) = parse_issue(source, v, as_of) {
                    updated.push(issue);
                }
            } else if let Some(number) = v["number"].as_i64() {
                closed.push(IssueRef { source: source.clone(), id: number.to_string() });
            }
        }

        // A raw page containing only pull requests says nothing about later
        // pages. Escalate whenever GitHub has another page so the caller does
        // not advance its cursor past unseen issue changes.
        Ok(IssueChangeset { updated, closed, has_more: response.has_next_page })
    }

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        run!(self.runner, "gh", &["issue", "view", &reference.id, "--repo", &reference.source.scope, "--web"], &self.host_root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, path::PathBuf, sync::Mutex};

    use super::*;
    use crate::providers::{
        github_api::{GhApi, GhApiResponse},
        github_test_support::{build_api_and_runner, repo_root_for_recording},
        issue_tracker::{tests::assert_provider_contract, IssueProvider},
        replay::{self, Masks},
        testing::MockRunner,
        ChannelLabel,
    };

    struct MockGhApi {
        responses: Mutex<VecDeque<Result<GhApiResponse, String>>>,
        requests: Mutex<Vec<(String, PathBuf)>>,
    }

    impl MockGhApi {
        fn new(responses: Vec<Result<GhApiResponse, String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) }
        }

        fn requests(&self) -> Vec<(String, PathBuf)> {
            self.requests.lock().expect("GitHub request lock poisoned").clone()
        }
    }

    #[async_trait]
    impl GhApi for MockGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root, label).await.map(|r| r.body)
        }
        async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, _label: &ChannelLabel) -> Result<GhApiResponse, String> {
            self.requests.lock().expect("GitHub request lock poisoned").push((endpoint.to_string(), repo_root.to_path_buf()));
            self.responses.lock().expect("GitHub response lock poisoned").pop_front().expect("MockGhApi: no more responses")
        }
    }

    fn source() -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() }
    }

    fn ok_response(body: &str, has_next_page: bool) -> Result<GhApiResponse, String> {
        Ok(GhApiResponse { status: 200, etag: None, body: body.to_string(), has_next_page, total_count: None })
    }

    fn mock_provider(responses: Vec<Result<GhApiResponse, String>>) -> GitHubIssueProvider {
        let api = Arc::new(MockGhApi::new(responses));
        let runner = Arc::new(MockRunner::new(vec![]));
        GitHubIssueProvider::new(api, runner, Path::new("/neutral"))
    }

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("issue_tracker", name)
    }

    #[tokio::test]
    async fn record_replay_satisfies_provider_contract() {
        let session = replay::test_session(&fixture("github_issues.yaml"), Masks::new());
        let repo_root = if session.is_live() { repo_root_for_recording() } else { PathBuf::from("/test/repo") };
        let (api, runner) = build_api_and_runner(&session);
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let provider = GitHubIssueProvider::new(api, runner, repo_root);

        assert_provider_contract(&provider, &source, "747", "2026-07-01T00:00:00Z").await;

        session.finish();
    }

    #[tokio::test]
    async fn fetch_by_id_uses_source_identity_without_a_checkout() {
        let before = Utc::now();
        let api = Arc::new(MockGhApi::new(vec![ok_response(
            r#"{"number":42,"title":"The answer","body":"Details","state":"closed","labels":[{"name":"bug"}],"updated_at":"2026-07-20T12:34:56Z"}"#,
            false,
        )]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/host-capability"));
        let reference = IssueRef { source: source(), id: "42".into() };

        let issue = provider.fetch_by_id(&reference).await.expect("fetch-by-id should succeed");

        assert_eq!(issue.reference, reference);
        assert_eq!(issue.body.as_deref(), Some("Details"));
        assert_eq!(issue.state, IssueState::Closed);
        assert_eq!(issue.as_of, "2026-07-20T12:34:56Z".parse::<DateTime<Utc>>().expect("timestamp"));
        assert!(issue.observed_at.is_some_and(|observed_at| observed_at >= before && observed_at <= Utc::now()));
        assert_eq!(api.requests(), vec![("repos/owner/repo/issues/42".into(), PathBuf::from("/host-capability"))]);
    }

    #[tokio::test]
    async fn query_filters_pull_requests_and_preserves_source_refs() {
        let body = r#"[
            {"number":1,"title":"Real issue","state":"open","labels":[]},
            {"number":2,"title":"A PR","state":"open","labels":[],"pull_request":{"url":"..."}},
            {"number":3,"title":"Another issue","state":"open","labels":[]}
        ]"#;
        let provider = mock_provider(vec![ok_response(body, false)]);

        let page = provider.query(&source(), &IssueQuery::default(), 1, 10).await.expect("issue query should succeed");

        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].reference, IssueRef { source: source(), id: "1".into() });
        assert_eq!(page.items[1].reference.id, "3");
    }

    #[tokio::test]
    async fn search_query_returns_total_and_pagination() {
        let body = r#"{"total_count":5,"items":[{"number":1,"title":"Bug","state":"open","labels":[]}]}"#;
        let provider = mock_provider(vec![ok_response(body, true)]);

        let page = provider.query(&source(), &IssueQuery { search: Some("bug".into()) }, 2, 10).await.expect("issue search should succeed");

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, Some(5));
        assert!(page.has_more);
    }

    #[tokio::test]
    async fn open_in_browser_passes_source_scope_explicitly() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let provider = GitHubIssueProvider::new(Arc::new(MockGhApi::new(vec![])), runner.clone(), Path::new("/neutral"));

        provider.open_in_browser(&IssueRef { source: source(), id: "42".into() }).await.expect("open-in-browser should succeed");

        assert_eq!(runner.calls(), vec![(
            "gh".into(),
            vec!["issue", "view", "42", "--repo", "owner/repo", "--web"].into_iter().map(str::to_string).collect()
        )]);
    }

    #[tokio::test]
    async fn changed_since_partitions_open_and_closed() {
        let body = r#"[
            {"number": 1, "title": "Open issue", "state": "open", "labels": []},
            {"number": 2, "title": "Closed issue", "state": "closed", "labels": []},
            {"number": 3, "title": "Another open", "state": "open", "labels": []}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 50).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 2);
        assert_eq!(changeset.updated[0].reference.id, "1");
        assert_eq!(changeset.updated[1].reference.id, "3");
        assert_eq!(changeset.closed, vec![IssueRef { source: source(), id: "2".into() }]);
        assert!(!changeset.has_more);
    }

    #[tokio::test]
    async fn changed_since_filters_pull_requests() {
        let body = r#"[
            {"number": 1, "title": "Issue", "state": "open", "labels": []},
            {"number": 2, "title": "PR", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: false,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 50).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 1);
        assert_eq!(changeset.updated[0].reference.id, "1");
        assert!(changeset.closed.is_empty());
    }

    #[tokio::test]
    async fn changed_since_escalates_on_pr_only_page_with_more_raw_results() {
        let body = r#"[
            {"number": 10, "title": "PR A", "state": "open", "labels": [], "pull_request": {"url": "..."}},
            {"number": 11, "title": "PR B", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: true,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 2).await.expect("changed-since query should succeed");

        assert!(changeset.updated.is_empty());
        assert!(changeset.closed.is_empty());
        assert!(changeset.has_more, "later raw pages may contain issue changes");
    }

    #[tokio::test]
    async fn changed_since_escalates_on_mixed_pr_issue_page() {
        // Page has both PRs and issues with has_next_page — should escalate
        // because remaining pages may contain more issues.
        let body = r#"[
            {"number": 1, "title": "Issue", "state": "open", "labels": []},
            {"number": 2, "title": "PR", "state": "open", "labels": [], "pull_request": {"url": "..."}}
        ]"#;
        let provider = mock_provider(vec![Ok(GhApiResponse {
            status: 200,
            etag: None,
            body: body.to_string(),
            has_next_page: true,
            total_count: None,
        })]);

        let changeset =
            provider.list_changed_since(&source(), "2026-03-09T00:00:00Z", 2).await.expect("changed-since query should succeed");

        assert_eq!(changeset.updated.len(), 1);
        assert!(changeset.has_more, "should escalate when page has issues and more pages exist");
    }
}
