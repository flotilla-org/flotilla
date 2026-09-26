use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use super::{ChangeRequestAdmission, ChangeRequestTracker};
use crate::providers::{
    http_execute,
    issue_tracker::forgejo::ForgejoIssueProviderConfig,
    run,
    types::{ChangeRequest, ChangeRequestStatus},
    CommandRunner, HttpClient,
};

pub struct ForgejoChangeRequestProvider {
    http: Arc<dyn HttpClient>,
    runner: Arc<dyn CommandRunner>,
    client: reqwest::Client,
    config: ForgejoIssueProviderConfig,
    repo_slug: String,
}

impl ForgejoChangeRequestProvider {
    pub fn new(http: Arc<dyn HttpClient>, runner: Arc<dyn CommandRunner>, config: ForgejoIssueProviderConfig, repo_slug: String) -> Self {
        let client = crate::tls::client_builder().build().expect("build Forgejo request client");
        Self { http, runner, client, config, repo_slug }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Request, String> {
        let mut url = format!("{}/repos/{}/{}", self.config.api_base_url.trim_end_matches('/'), self.repo_slug, path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(
                &query
                    .iter()
                    .map(|(key, value)| format!("{}={}", urlencoding::encode(key), urlencoding::encode(value)))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        let mut request = self
            .client
            .request(method, url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.config.auth.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.build().map_err(|error| error.to_string())
    }

    async fn execute(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let response = http_execute!(self.http, self.request(method, path, query, body)?)?;
        if !response.status().is_success() {
            return Err(format!("Forgejo HTTP {}: {}", response.status(), String::from_utf8_lossy(response.body())));
        }
        if response.body().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(response.body()).map_err(|error| error.to_string())
    }

    fn parse(&self, value: &serde_json::Value) -> Option<(String, ChangeRequest)> {
        let number = value["number"].as_i64()?;
        let title = value["title"].as_str()?.to_string();
        let branch = value["head"]["ref"].as_str()?.to_string();
        let status = if value["merged"].as_bool().unwrap_or(false) || !value["merged_at"].is_null() {
            ChangeRequestStatus::Merged
        } else if value["state"].as_str() == Some("closed") {
            ChangeRequestStatus::Closed
        } else if value["draft"].as_bool().unwrap_or(false) {
            ChangeRequestStatus::Draft
        } else {
            ChangeRequestStatus::Open
        };
        Some((number.to_string(), ChangeRequest {
            title,
            branch,
            status,
            body: value["body"].as_str().map(str::to_string),
            provider_name: "forgejo".into(),
            provider_display_name: "Forgejo".into(),
        }))
    }

    async fn list(&self, state: &str, limit: usize) -> Result<Vec<serde_json::Value>, String> {
        let mut items = Vec::new();
        let page_size = limit.min(50);
        while items.len() < limit {
            let page = items.len() / page_size + 1;
            let value = self
                .execute(
                    reqwest::Method::GET,
                    "pulls",
                    &[("state", state.into()), ("limit", page_size.to_string()), ("page", page.to_string())],
                    None,
                )
                .await?;
            let batch = value.as_array().ok_or("Forgejo pull request list response was not an array")?;
            let batch_len = batch.len();
            items.extend(batch.iter().cloned());
            if batch_len < page_size {
                break;
            }
        }
        items.truncate(limit);
        Ok(items)
    }
}

#[async_trait]
impl ChangeRequestTracker for ForgejoChangeRequestProvider {
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(self.list("open", limit).await?.iter().filter_map(|value| self.parse(value)).collect())
    }

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, String> {
        Ok(self.list("all", 100).await?.iter().filter_map(|value| self.parse(value)).find(|(_, request)| request.branch == branch))
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let value = self.execute(reqwest::Method::GET, &format!("pulls/{id}"), &[], None).await?;
        self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, String> {
        let value = self.execute(reqwest::Method::GET, &format!("pulls/{id}"), &[], None).await?;
        let (id, change_request) = self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: value["base"]["ref"].as_str().map(str::to_string) })
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        let url = format!("{}/{}/pulls/{id}", self.config.service_url, self.repo_slug);
        #[cfg(target_os = "macos")]
        let (cmd, args): (&str, Vec<&str>) = ("open", vec![&url]);
        #[cfg(target_os = "windows")]
        let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", &url]);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![&url]);
        run!(self.runner, cmd, &args, Path::new("/"))?;
        Ok(())
    }

    async fn close_change_request(&self, id: &str) -> Result<(), String> {
        self.execute(reqwest::Method::PATCH, &format!("pulls/{id}"), &[], Some(serde_json::json!({"state":"closed"}))).await?;
        Ok(())
    }

    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        self.execute(reqwest::Method::POST, &format!("pulls/{id}/merge"), &[], Some(serde_json::json!({"Do":"squash"}))).await?;
        Ok(())
    }

    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        Ok(self
            .list("closed", limit)
            .await?
            .iter()
            .filter(|value| value["merged"].as_bool().unwrap_or(false) || !value["merged_at"].is_null())
            .filter_map(|value| value["head"]["ref"].as_str().map(str::to_string))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::providers::{
        replay::{self, Masks},
        testing::MockRunner,
        ChannelLabel,
    };

    struct MockHttp {
        responses: Mutex<VecDeque<http::Response<bytes::Bytes>>>,
        urls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HttpClient for MockHttp {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            self.urls.lock().expect("request URLs").push(request.url().to_string());
            self.responses.lock().expect("responses").pop_front().ok_or_else(|| "unexpected HTTP request".into())
        }
    }

    fn response(items: &[serde_json::Value]) -> http::Response<bytes::Bytes> {
        http::Response::builder()
            .status(200)
            .body(bytes::Bytes::from(serde_json::to_vec(items).expect("serialize items")))
            .expect("response")
    }

    fn json_response(value: &serde_json::Value) -> http::Response<bytes::Bytes> {
        http::Response::builder()
            .status(200)
            .body(bytes::Bytes::from(serde_json::to_vec(value).expect("serialize value")))
            .expect("response")
    }

    fn auth() -> crate::providers::issue_tracker::forgejo::ForgejoAuth {
        if !replay::is_live() {
            return crate::providers::issue_tracker::forgejo::ForgejoAuth {
                token: "fixture-token".into(),
                token_path: PathBuf::from("fixture-token"),
            };
        }
        let token_path =
            std::env::var_os("FORGEJO_TOKEN_FILE").map(PathBuf::from).expect("FORGEJO_TOKEN_FILE is required for live recording");
        let token = std::fs::read_to_string(&token_path).expect("read Forgejo token").trim().to_string();
        crate::providers::issue_tracker::forgejo::ForgejoAuth { token, token_path }
    }

    #[tokio::test]
    async fn record_replay_lists_forgejo_pull_requests() {
        let auth = auth();
        let mut masks = Masks::new();
        masks.add(&auth.token, "<LAB_FORGEJO_TOKEN>");
        let fixture = crate::providers::testing::fixture_path("change_request", "forgejo_pulls.yaml");
        let session = replay::test_session(&fixture, masks);
        let provider = ForgejoChangeRequestProvider::new(
            replay::test_http_client(&session),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.lab.flotilla.work".into(), None, auth),
            "lab/flotilla".into(),
        );
        let requests = provider.list_change_requests(10).await.expect("list pull requests");
        assert!(requests.iter().all(|(_, request)| matches!(request.status, ChangeRequestStatus::Open | ChangeRequestStatus::Draft)));
        session.finish();
    }

    #[tokio::test]
    async fn record_replay_queries_ghostty_governor_pull_request() {
        let auth = auth();
        let mut masks = Masks::new();
        masks.add(&auth.token, "<LAB_FORGEJO_TOKEN>");
        let fixture = crate::providers::testing::fixture_path("change_request", "forgejo_ghostty_governor.yaml");
        let session = replay::test_session(&fixture, masks);
        let provider = ForgejoChangeRequestProvider::new(
            replay::test_http_client(&session),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.lab.flotilla.work".into(), None, auth),
            "robert/ghostty-ops".into(),
        );
        provider.find_change_request_by_branch("governor").await.expect("query Forgejo pull requests");
        session.finish();
    }

    #[test]
    fn parses_forgejo_states_and_branch() {
        let config = ForgejoIssueProviderConfig::new("https://forgejo.example.test".into(), None, auth());
        let provider = ForgejoChangeRequestProvider::new(
            Arc::new(crate::providers::ReqwestHttpClient::new()),
            Arc::new(MockRunner::new(vec![])),
            config,
            "team/repo".into(),
        );
        let value = serde_json::json!({"number": 7, "title": "Change", "head": {"ref": "feature"}, "base": {"ref": "main"}, "state": "open", "draft": true, "body": "Notes", "merged": false, "merged_at": null});
        let (id, request) = provider.parse(&value).expect("parse pull request");
        assert_eq!(id, "7");
        assert_eq!(request.branch, "feature");
        assert_eq!(request.status, ChangeRequestStatus::Draft);
        let mut merged = value.clone();
        merged["state"] = "closed".into();
        merged["merged"] = true.into();
        assert_eq!(provider.parse(&merged).expect("parse merged request").1.status, ChangeRequestStatus::Merged);
    }

    #[tokio::test]
    async fn pagination_keeps_page_size_constant_and_truncates_to_requested_limit() {
        let values = (1..=100)
            .map(|number| serde_json::json!({"number": number, "title": format!("Change {number}"), "head": {"ref": format!("branch-{number}")}, "state": "open"}))
            .collect::<Vec<_>>();
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([response(&values[..50]), response(&values[50..])])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = ForgejoChangeRequestProvider::new(
            http.clone(),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.example.test".into(), None, auth()),
            "team/repo".into(),
        );
        let requests = provider.list_change_requests(70).await.expect("list change requests");
        assert_eq!(requests.len(), 70);
        assert_eq!(requests.first().expect("first").0, "1");
        assert_eq!(requests.last().expect("last").0, "70");
        let urls = http.urls.lock().expect("request URLs");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("pulls?state=open&limit=50&page=1"));
        assert!(urls[1].ends_with("pulls?state=open&limit=50&page=2"));
    }

    #[tokio::test]
    async fn admission_close_and_merge_use_forgejo_pull_endpoints() {
        let value =
            serde_json::json!({"number": 7, "title": "Change", "head": {"ref": "feature"}, "base": {"ref": "main"}, "state": "open"});
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&value),
                json_response(&value),
                http::Response::builder().status(200).body(bytes::Bytes::new()).expect("response"),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = ForgejoChangeRequestProvider::new(
            http.clone(),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.example.test".into(), None, auth()),
            "team/repo".into(),
        );
        let admission = provider.get_change_request_for_admission("7").await.expect("admission");
        assert_eq!(admission.base_ref.as_deref(), Some("main"));
        provider.close_change_request("7").await.expect("close");
        provider.merge_change_request("7").await.expect("merge");
        let urls = http.urls.lock().expect("request URLs");
        assert!(urls[0].ends_with("/repos/team/repo/pulls/7"));
        assert!(urls[1].ends_with("/repos/team/repo/pulls/7"));
        assert!(urls[2].ends_with("/repos/team/repo/pulls/7/merge"));
    }

    #[tokio::test]
    async fn reports_forgejo_http_errors() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([http::Response::builder()
                .status(401)
                .body(bytes::Bytes::from_static(b"unauthorized"))
                .expect("response")])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = ForgejoChangeRequestProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.example.test".into(), None, auth()),
            "team/repo".into(),
        );
        let error = provider.list_change_requests(1).await.expect_err("HTTP error");
        assert!(error.contains("401"));
    }
}
