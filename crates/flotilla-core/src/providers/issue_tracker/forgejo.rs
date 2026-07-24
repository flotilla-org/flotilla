use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    AssociationKey, Issue, IssueChangeset, IssueRef, IssueSource, IssueState,
};

use crate::providers::{http_execute, run, CommandRunner, HttpClient};

const MAX_FORGEJO_LIMIT: usize = 50;

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoAuth {
    pub token: String,
    pub token_path: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ForgejoIssueProviderConfig {
    pub service_url: String,
    pub api_base_url: String,
    pub auth: ForgejoAuth,
}

impl ForgejoIssueProviderConfig {
    pub fn new(service_url: String, api_base_url: Option<String>, auth: ForgejoAuth) -> Self {
        let service_url = service_url.trim_end_matches('/').to_string();
        let api_base_url = api_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{service_url}/api/v1"));
        Self { service_url, api_base_url, auth }
    }
}

pub struct ForgejoIssueProvider {
    http: Arc<dyn HttpClient>,
    runner: Arc<dyn CommandRunner>,
    client: reqwest::Client,
    config: ForgejoIssueProviderConfig,
}

impl ForgejoIssueProvider {
    pub fn new(http: Arc<dyn HttpClient>, runner: Arc<dyn CommandRunner>, config: ForgejoIssueProviderConfig) -> Self {
        let client = crate::tls::client_builder().build().expect("build Forgejo request client");
        Self { http, runner, client, config }
    }

    fn request(&self, path: &str, query: &[(&str, String)]) -> Result<reqwest::Request, String> {
        let mut url = format!("{}/{}", self.config.api_base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        if !query.is_empty() {
            let query = query
                .iter()
                .map(|(name, value)| format!("{}={}", urlencoding::encode(name), urlencoding::encode(value)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&query);
        }
        self.client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.config.auth.token))
            .build()
            .map_err(|error| error.to_string())
    }

    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<(serde_json::Value, bool), String> {
        let response = http_execute!(self.http, self.request(path, query)?)?;
        let status = response.status();
        let has_more = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|link| link.contains("rel=\"next\""));
        let body = String::from_utf8_lossy(response.body()).to_string();
        if !status.is_success() {
            return Err(format!("Forgejo HTTP {status}: {body}"));
        }
        serde_json::from_str(&body).map(|value| (value, has_more)).map_err(|error| error.to_string())
    }

    fn html_url(&self, reference: &IssueRef) -> String {
        format!("{}/{}/issues/{}", self.config.service_url.trim_end_matches('/'), reference.source.scope, reference.id)
    }
}

fn normalized_service(value: &str) -> &str {
    value.trim_end_matches('/')
}

pub fn is_forgejo_source(source: &IssueSource, service_url: &str) -> bool {
    let service = normalized_service(&source.service);
    service == "forgejo" || service == normalized_service(service_url)
}

fn clamp_limit(count: usize) -> usize {
    if count > MAX_FORGEJO_LIMIT {
        tracing::warn!(requested = %count, max = MAX_FORGEJO_LIMIT, "Forgejo API page size capped");
        MAX_FORGEJO_LIMIT
    } else {
        count
    }
}

fn parse_issue(source: &IssueSource, value: &serde_json::Value, fetched_at: DateTime<Utc>) -> Option<Issue> {
    if !value["pull_request"].is_null() {
        return None;
    }
    let number = value["number"].as_i64()?;
    let title = value["title"].as_str()?.to_string();
    let body = value["body"].as_str().map(str::to_string);
    let state = match value["state"].as_str()? {
        "open" => IssueState::Open,
        "closed" => IssueState::Closed,
        _ => return None,
    };
    let labels = value["labels"]
        .as_array()
        .map(|labels| labels.iter().filter_map(|label| label["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let as_of = value["updated_at"].as_str().and_then(|value| value.parse::<DateTime<Utc>>().ok()).unwrap_or(fetched_at);
    let id = number.to_string();
    Some(
        Issue::builder()
            .reference(IssueRef { source: source.clone(), id: id.clone() })
            .title(title)
            .maybe_body(body)
            .state(state)
            .labels(labels)
            .as_of(as_of)
            .observed_at(fetched_at)
            .association_keys(vec![AssociationKey::IssueRef("forgejo".into(), id)])
            .provider_name("forgejo".into())
            .provider_display_name("Forgejo".into())
            .build(),
    )
}

#[cfg(target_os = "macos")]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("open", vec![url.to_string()])
}

#[cfg(target_os = "windows")]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("cmd", vec!["/C".into(), "start".into(), String::new(), url.to_string()])
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn browser_open_command(url: &str) -> (&'static str, Vec<String>) {
    ("xdg-open", vec![url.to_string()])
}

#[async_trait]
impl super::IssueProvider for ForgejoIssueProvider {
    fn supports(&self, source: &IssueSource) -> bool {
        is_forgejo_source(source, &self.config.service_url)
    }

    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        let limit = clamp_limit(count);
        let mut query = vec![
            ("state", "open".to_string()),
            ("type", "issues".to_string()),
            ("sort", "recentupdate".to_string()),
            ("page", page.to_string()),
            ("limit", limit.to_string()),
        ];
        if let Some(label) = &params.label {
            query.push(("labels", label.clone()));
        }
        if let Some(search) = &params.search {
            query.push(("q", search.clone()));
        }
        let (value, has_more) = self.get_json(&format!("repos/{}/issues", source.scope), &query).await?;
        let fetched_at = Utc::now();
        let raw_items = value.as_array().ok_or("Forgejo issue list response was not an array")?;
        let items = raw_items.iter().filter_map(|value| parse_issue(source, value, fetched_at)).collect();
        Ok(IssueResultPage { items, total: None, has_more })
    }

    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        let (value, _) = self.get_json(&format!("repos/{}/issues/{}", reference.source.scope, reference.id), &[]).await?;
        parse_issue(&reference.source, &value, Utc::now()).ok_or_else(|| format!("failed to parse Forgejo issue {}", reference.id))
    }

    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String> {
        let limit = clamp_limit(count);
        let query = vec![
            ("state", "all".to_string()),
            ("type", "issues".to_string()),
            ("since", since.to_string()),
            ("sort", "recentupdate".to_string()),
            ("limit", limit.to_string()),
        ];
        let (value, has_more) = self.get_json(&format!("repos/{}/issues", source.scope), &query).await?;
        let fetched_at = Utc::now();
        let raw_items = value.as_array().ok_or("Forgejo changed-since response was not an array")?;
        let mut updated = Vec::new();
        let mut closed = Vec::new();
        for value in raw_items {
            match value["state"].as_str() {
                Some("open") => {
                    if let Some(issue) = parse_issue(source, value, fetched_at) {
                        updated.push(issue);
                    }
                }
                Some("closed") => {
                    if let Some(number) = value["number"].as_i64() {
                        closed.push(IssueRef { source: source.clone(), id: number.to_string() });
                    }
                }
                _ => {}
            }
        }
        Ok(IssueChangeset { updated, closed, has_more })
    }

    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        let url = self.html_url(reference);
        let (cmd, args) = browser_open_command(&url);
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        run!(self.runner, cmd, &args, Path::new("/"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::providers::{
        issue_tracker::{tests::assert_provider_contract, IssueProvider},
        replay::{self, Masks},
        testing::MockRunner,
        ChannelLabel,
    };

    type RecordedRequest = (String, String, HashMap<String, String>);

    struct MockHttp {
        responses: Mutex<Vec<http::Response<bytes::Bytes>>>,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl MockHttp {
        fn new(responses: Vec<http::Response<bytes::Bytes>>) -> Self {
            Self { responses: Mutex::new(responses), requests: Mutex::new(Vec::new()) }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("request lock poisoned").clone()
        }
    }

    #[async_trait]
    impl HttpClient for MockHttp {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            let headers =
                request.headers().iter().map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_string())).collect();
            self.requests.lock().expect("request lock poisoned").push((request.method().to_string(), request.url().to_string(), headers));
            Ok(self.responses.lock().expect("response lock poisoned").remove(0))
        }
    }

    const LAB_SERVICE_URL: &str = "https://forgejo.lab.flotilla.work";

    fn source() -> IssueSource {
        IssueSource { service: LAB_SERVICE_URL.into(), scope: "fork-issues/zellij".into() }
    }

    fn contract_source() -> IssueSource {
        source()
    }

    fn auth() -> ForgejoAuth {
        ForgejoAuth { token: "test-token".into(), token_path: PathBuf::from("/tmp/lab-forgejo-test-token") }
    }

    fn response(body: &str, has_next: bool) -> http::Response<bytes::Bytes> {
        let mut builder = http::Response::builder().status(200);
        if has_next {
            builder =
                builder.header("link", "<https://forgejo.lab.flotilla.work/api/v1/repos/fork-issues/zellij/issues?page=2>; rel=\"next\"");
        }
        builder.body(bytes::Bytes::from(body.to_string())).expect("response")
    }

    fn provider(http: Arc<dyn HttpClient>) -> ForgejoIssueProvider {
        ForgejoIssueProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth()),
        )
    }

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("issue_tracker", name)
    }

    fn replay_auth() -> ForgejoAuth {
        if !replay::is_live() {
            return ForgejoAuth { token: "fixture-token".into(), token_path: PathBuf::from("fixture-token") };
        }
        let token_path = std::env::var_os("LAB_FORGEJO_TOKEN_PATH")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".config/lab-forgejo-coder-token")))
            .expect("set LAB_FORGEJO_TOKEN_PATH for live Forgejo recording");
        let token = std::fs::read_to_string(&token_path)
            .unwrap_or_else(|error| panic!("read Forgejo token {}: {error}", token_path.display()))
            .trim()
            .to_string();
        ForgejoAuth { token, token_path }
    }

    #[tokio::test]
    async fn record_replay_satisfies_provider_contract() {
        let auth = replay_auth();
        let mut masks = Masks::new();
        masks.add(&auth.token, "<LAB_FORGEJO_TOKEN>");
        let session = replay::test_session(&fixture("forgejo_issues.yaml"), masks);
        let http = replay::test_http_client(&session);
        let provider = ForgejoIssueProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth),
        );

        assert_provider_contract(&provider, &contract_source(), "9", "2026-01-01T00:00:00Z").await;

        session.finish();
    }

    #[tokio::test]
    async fn query_sends_forgejo_issue_filters() {
        let http = Arc::new(MockHttp::new(vec![response("[]", false)]));
        let provider = provider(http.clone());

        provider
            .query(&source(), &IssueQuery { search: Some("anchor".into()), label: Some("ready".into()) }, 2, 75)
            .await
            .expect("query should succeed");

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "GET");
        assert_eq!(
            requests[0].1,
            "https://forgejo.lab.flotilla.work/api/v1/repos/fork-issues/zellij/issues?state=open&type=issues&sort=recentupdate&page=2&limit=50&labels=ready&q=anchor"
        );
        assert_eq!(requests[0].2.get("authorization").map(String::as_str), Some("token test-token"));
    }

    #[tokio::test]
    async fn fetch_by_id_preserves_source_and_body() {
        let http = Arc::new(MockHttp::new(vec![response(
            r#"{"number":9,"title":"Atomic tabs","body":"Details","state":"open","labels":[{"name":"ready"}],"updated_at":"2026-07-24T10:00:00Z"}"#,
            false,
        )]));
        let provider = provider(http);

        let reference = IssueRef { source: source(), id: "9".into() };
        let issue = provider.fetch_by_id(&reference).await.expect("fetch should succeed");

        assert_eq!(issue.reference, reference);
        assert_eq!(issue.title, "Atomic tabs");
        assert_eq!(issue.body.as_deref(), Some("Details"));
        assert_eq!(issue.labels, vec!["ready"]);
        assert_eq!(issue.provider_name, "forgejo");
    }

    #[tokio::test]
    async fn changed_since_partitions_open_and_closed() {
        let http = Arc::new(MockHttp::new(vec![response(
            r#"[
                {"number":9,"title":"Open","state":"open","labels":[]},
                {"number":10,"title":"Closed","state":"closed","labels":[]}
            ]"#,
            true,
        )]));
        let provider = provider(http);

        let changes = provider.list_changed_since(&source(), "2026-07-01T00:00:00Z", 10).await.expect("changes should succeed");

        assert_eq!(changes.updated.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>(), vec!["9"]);
        assert_eq!(changes.closed, vec![IssueRef { source: source(), id: "10".into() }]);
        assert!(changes.has_more);
    }

    #[test]
    fn supports_configured_forgejo_service_and_generic_alias() {
        let provider = provider(Arc::new(MockHttp::new(vec![])));

        assert!(provider.supports(&IssueSource { service: "forgejo".into(), scope: "fork-issues/zellij".into() }));
        assert!(provider.supports(&IssueSource { service: LAB_SERVICE_URL.into(), scope: "fork-issues/zellij".into() }));
        assert!(
            !provider.supports(&IssueSource { service: "https://other-forgejo.example.test".into(), scope: "fork-issues/zellij".into() })
        );
        assert!(!provider.supports(&IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }));
    }

    #[tokio::test]
    async fn open_in_browser_uses_platform_opener_with_source_scope() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let provider = ForgejoIssueProvider::new(
            Arc::new(MockHttp::new(vec![])),
            runner.clone(),
            ForgejoIssueProviderConfig::new(LAB_SERVICE_URL.into(), None, auth()),
        );
        let reference = IssueRef { source: source(), id: "9".into() };

        provider.open_in_browser(&reference).await.expect("open in browser should succeed");

        let url = "https://forgejo.lab.flotilla.work/fork-issues/zellij/issues/9";
        let (expected_cmd, expected_args) = browser_open_command(url);
        assert_eq!(runner.calls(), vec![(expected_cmd.into(), expected_args)]);
    }
}
