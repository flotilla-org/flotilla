use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, IssueState,
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
    let assignees = v["assignees"]
        .as_array()
        .map(|items| items.iter().filter_map(|item| item["login"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let id = number.to_string();
    let as_of = v["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).unwrap_or(fetched_at);
    let reference = IssueRef { source: source.clone(), id: id.clone() };
    Some(
        Issue::builder()
            .reference(reference)
            .title(title)
            .maybe_body(body)
            .state(state)
            .labels(labels)
            .assignees(assignees)
            .as_of(as_of)
            .observed_at(fetched_at)
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
                let label = params.label.as_ref().map(|label| format!("&labels={}", urlencoding::encode(label))).unwrap_or_default();
                let fields = params
                    .match_fields
                    .iter()
                    .map(|(field, values)| format!("&{}={}", urlencoding::encode(field), urlencoding::encode(&values.join(","))))
                    .collect::<String>();
                let endpoint = format!(
                    "repos/{}/issues?state=open&sort=updated&direction=desc&per_page={}&page={}{}{}",
                    source.scope, per_page, page, label, fields
                );
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
                let label = params.label.as_ref().map(|label| format!(" label:\"{label}\"")).unwrap_or_default();
                let fields = params
                    .match_fields
                    .iter()
                    .flat_map(|(field, values)| values.iter().map(move |value| format!(" {field}:\"{value}\"")))
                    .collect::<String>();
                let raw_query = format!("repo:{} is:issue is:open{}{} {}", source.scope, label, fields, search_term);
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

    async fn list_changed_since(&self, source: &IssueSource, since: &str, _count: usize) -> Result<IssueChangeset, String> {
        // Incremental pages include PRs. Use the REST maximum independently
        // of the demanded window size to avoid needless overflow reloads.
        let per_page = 100;
        // Keep the URI stable so the GhApi ETag cache can validate every poll.
        // Filter the sorted page locally and reload if relevant changes may
        // continue onto another page.
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page={}", source.scope, per_page);
        let response = gh_api_get_with_headers!(self.api, &endpoint, &self.host_root)?;
        // GhApi serves the cached 200 body on a 304. Reapply it against the
        // caller's cursor: a previous consumer may have failed before it
        // committed the changes and must be able to retry without another 200.
        let items: Vec<serde_json::Value> = serde_json::from_str(&response.body).map_err(|e| e.to_string())?;
        let as_of = Utc::now();
        let since = since.parse::<DateTime<Utc>>().map_err(|error| format!("invalid issue refresh cursor: {error}"))?;
        let has_more = response.has_next_page
            && items.last().is_some_and(|item| {
                item["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).is_none_or(|updated| updated >= since)
            });

        let mut updated = Vec::new();
        let mut closed = Vec::new();

        for v in &items {
            if v["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).is_some_and(|updated| updated < since) {
                continue;
            }
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

        Ok(IssueChangeset { updated, closed, has_more })
    }

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        run!(self.runner, "gh", &["issue", "view", &reference.id, "--repo", &reference.source.scope, "--web"], &self.host_root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    use super::*;
    use crate::providers::{
        github_api::{GhApi, GhApiResponse},
        github_test_support::{build_api_and_runner, repo_root_for_recording},
        issue_tracker::{tests::assert_provider_contract, IssueProvider},
        replay::{self, Masks},
        testing::MockRunner,
        ChannelLabel,
    };

    #[test]
    fn parses_issue_title_and_assignees_from_existing_rest_response() {
        let source = IssueSource { service: "github.com".into(), scope: "team/repo".into() };
        let observed_at = "2026-10-01T12:00:00Z".parse().expect("time");
        let raw =
            serde_json::json!({"number": 7, "title": "Fix refresh", "state": "open", "assignees": [{"login": "alice"}, {"login": "bob"}]});
        let issue = parse_issue(&source, &raw, observed_at).expect("parse issue");
        assert_eq!(issue.title, "Fix refresh");
        assert_eq!(issue.assignees, ["alice", "bob"]);
        assert_eq!(issue.observed_at, Some(observed_at));
    }
    struct CountingGhApi {
        inner: Arc<dyn GhApi>,
        requests: AtomicUsize,
    }

    #[async_trait]
    impl GhApi for CountingGhApi {
        async fn get(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.get_with_headers(endpoint, repo_root, label).await.map(|response| response.body)
        }
        async fn get_with_headers(&self, endpoint: &str, repo_root: &Path, label: &ChannelLabel) -> Result<GhApiResponse, String> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.inner.get_with_headers(endpoint, repo_root, label).await
        }
    }

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
    async fn recorded_live_page_comparison_reduces_two_window_refresh_from_three_requests_to_one() {
        let session = replay::test_session(&fixture("github_refresh_request_counts.yaml"), Masks::new());
        let api = Arc::new(CountingGhApi { inner: replay::test_gh_api(&session), requests: Default::default() });
        let runner = replay::test_runner(&session);
        let provider = GitHubIssueProvider::new(api.clone(), runner, Path::new("/"));
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page=100", source.scope);
        // Choose a reproducible cutoff from the recorded live page. This setup
        // request is excluded from each measured refresh path.
        let sample = gh_api_get_with_headers!(api, &endpoint, Path::new("/")).expect("live sample");
        let records: Vec<serde_json::Value> = serde_json::from_str(&sample.body).expect("records");
        let since = records[70]["updated_at"].as_str().expect("cutoff");
        assert!(records.iter().any(|record| record.get("pull_request").is_some()), "sample must include PR traffic");
        api.requests.store(0, Ordering::Relaxed);
        // Reproduce the previous 50-record endpoint and its conservative full
        // reload of both demanded windows when the cutoff extends past it.
        let endpoint = format!("repos/{}/issues?state=all&sort=updated&direction=desc&per_page=50", source.scope);
        let before = gh_api_get_with_headers!(api, &endpoint, Path::new("/")).expect("previous page");
        let before_records: Vec<serde_json::Value> = serde_json::from_str(&before.body).expect("previous records");
        assert!(before.has_next_page && before_records.last().expect("last record")["updated_at"].as_str().expect("updated") >= since);
        for _ in 0..2 {
            provider.query(&source, &IssueQuery::default(), 1, 50).await.expect("previous window reload");
        }
        assert_eq!(api.requests.load(Ordering::Relaxed), 3);
        api.requests.store(0, Ordering::Relaxed);
        let after = provider.list_changed_since(&source, since, 50).await.expect("larger incremental page");
        assert!(!after.has_more, "the maximum page contains the complete refresh interval");
        assert_eq!(api.requests.load(Ordering::Relaxed), 1);
        for record in records
            .iter()
            .filter(|record| record.get("pull_request").is_none() && record["updated_at"].as_str().is_some_and(|updated| updated >= since))
        {
            let id = record["number"].as_u64().expect("number").to_string();
            assert!(
                after.updated.iter().any(|issue| issue.reference.id == id) || after.closed.iter().any(|reference| reference.id == id),
                "missed issue {id}"
            );
        }
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
    async fn query_sends_label_filter_to_github() {
        let api = Arc::new(MockGhApi::new(vec![ok_response("[]", false)]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/neutral"));

        provider
            .query(&source(), &IssueQuery { search: None, label: Some("ready now".into()), match_fields: Default::default() }, 2, 10)
            .await
            .expect("label-filtered issue query should succeed");

        assert_eq!(api.requests(), vec![(
            "repos/owner/repo/issues?state=open&sort=updated&direction=desc&per_page=10&page=2&labels=ready%20now".into(),
            PathBuf::from("/neutral"),
        )]);
    }

    #[tokio::test]
    async fn search_query_returns_total_and_pagination() {
        let body = r#"{"total_count":5,"items":[{"number":1,"title":"Bug","state":"open","labels":[]}]}"#;
        let provider = mock_provider(vec![ok_response(body, true)]);

        let page = provider
            .query(&source(), &IssueQuery { search: Some("bug".into()), label: None, match_fields: Default::default() }, 2, 10)
            .await
            .expect("issue search should succeed");

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
    async fn incremental_poll_reuses_etag_and_304_spends_no_primary_quota() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("HTTP/2 200 OK\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n[{\"number\":1,\"title\":\"Changed\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-01T00:00:10Z\"}]".into()),
            Ok("HTTP/2 304 Not Modified\r\nETag: \"issue-window\"\r\nX-RateLimit-Remaining: 4800\r\n\r\n".into()),
        ]));
        let api = Arc::new(crate::providers::github_api::GhApiClient::new(runner.clone()));
        let provider = GitHubIssueProvider::new(api, runner.clone(), Path::new("/neutral"));

        let first = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("first poll");
        let second = provider.list_changed_since(&source(), "2026-07-01T00:00:20Z", 50).await.expect("conditional poll");

        assert_eq!(first.updated.len(), 1);
        assert!(second.updated.is_empty());
        assert!(second.closed.is_empty());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1[2], calls[1].1[2], "incremental endpoint must be stable for ETag reuse");
        assert!(calls[1].1.contains(&"If-None-Match: \"issue-window\"".to_string()));
    }

    #[tokio::test]
    async fn conditional_hit_replays_a_change_when_the_consumer_retries_its_cursor() {
        let runner = Arc::new(MockRunner::new(vec![
            Ok("HTTP/2 200 OK\r\nETag: \"issue-window\"\r\n\r\n[{\"number\":1,\"title\":\"Changed\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-01T00:00:10Z\"}]".into()),
            Ok("HTTP/2 304 Not Modified\r\nETag: \"issue-window\"\r\n\r\n".into()),
        ]));
        let api = Arc::new(crate::providers::github_api::GhApiClient::new(runner.clone()));
        let provider = GitHubIssueProvider::new(api, runner, Path::new("/neutral"));

        provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("first poll");
        let retried = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("retry after 304");

        assert_eq!(retried.updated.len(), 1, "a 304 must not discard uncommitted changes");
    }

    #[tokio::test]
    async fn pr_heavy_refresh_uses_the_largest_stable_page_without_reloading() {
        let mut items = (1..=50)
            .map(|number| {
                serde_json::json!({"number": number,
            "title": "PR", "state": "open", "pull_request": {}, "updated_at": "2026-07-01T00:00:10Z"})
            })
            .collect::<Vec<_>>();
        items.push(serde_json::json!({"number": 51, "title": "Issue", "state": "open",
            "updated_at": "2026-07-01T00:00:05Z"}));
        let api = Arc::new(MockGhApi::new(vec![ok_response(&serde_json::to_string(&items).expect("items"), false)]));
        let provider = GitHubIssueProvider::new(api.clone(), Arc::new(MockRunner::new(vec![])), Path::new("/neutral"));
        let changes = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 50).await.expect("refresh");
        assert_eq!(changes.updated[0].reference.id, "51");
        assert!(!changes.has_more, "the issue behind fifty PRs must not require a reload");
        assert_eq!(api.requests().len(), 1);
        assert!(api.requests()[0].0.ends_with("per_page=100"), "use GitHub's maximum page size independently of query window size");
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
