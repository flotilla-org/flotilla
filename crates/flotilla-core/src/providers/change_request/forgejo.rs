use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_resources::{
    ChangeRequestReviewObservation, ChangeRequestStatus as ObservedStatus, Observation, ObservedChangeRequestState, ObservedReviewDecision,
};

use super::{ChangeRequestAdmission, ChangeRequestTracker, ObservationError};
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
    operator_login: Option<String>,
}

// None means the list cannot be interpreted; Some(ObservedReviewDecision::None)
// means a complete list with no decisive review.
fn review_decision(reviews: &[serde_json::Value]) -> Option<ObservedReviewDecision> {
    let mut latest_by_reviewer = HashMap::<String, (i64, Option<ObservedReviewDecision>)>::new();
    for review in reviews {
        let Some(state) = review["state"].as_str() else {
            tracing::warn!("Forgejo review is missing a state");
            return None;
        };
        let decision = match state {
            "APPROVED" => ObservedReviewDecision::Approved,
            "REQUEST_CHANGES" => ObservedReviewDecision::ChangesRequested,
            "REQUEST_REVIEW" => ObservedReviewDecision::Required,
            "COMMENT" | "PENDING" => continue,
            _ => {
                tracing::warn!(state, "Unknown Forgejo review state");
                return None;
            }
        };
        let reviewer = review["user"]["login"]
            .as_str()
            .map(|login| format!("user:{}", login.to_ascii_lowercase()))
            .or_else(|| review["team"]["id"].as_i64().map(|id| format!("team:{id}")));
        let Some(reviewer) = reviewer else {
            tracing::warn!(state, "Forgejo review is missing a reviewer");
            return None;
        };
        let Some(id) = review["id"].as_i64() else {
            tracing::warn!(state, "Forgejo review is missing an id");
            return None;
        };
        // A dismissed latest review suppresses an older decision. Forgejo's
        // merge check still counts stale change requests, while stale approvals
        // may be ignored under branch protection settings.
        let active = review["dismissed"] != true && !(review["stale"] == true && decision == ObservedReviewDecision::Approved);
        let decision = active.then_some(decision);
        let entry = latest_by_reviewer.entry(reviewer).or_insert((id, decision));
        if id > entry.0 {
            *entry = (id, decision);
        }
    }
    let decisions: Vec<_> = latest_by_reviewer.values().filter_map(|(_, decision)| *decision).collect();
    if decisions.contains(&ObservedReviewDecision::ChangesRequested) {
        Some(ObservedReviewDecision::ChangesRequested)
    } else if decisions.contains(&ObservedReviewDecision::Required) {
        Some(ObservedReviewDecision::Required)
    } else if decisions.contains(&ObservedReviewDecision::Approved) {
        Some(ObservedReviewDecision::Approved)
    } else {
        Some(ObservedReviewDecision::None)
    }
}

impl ForgejoChangeRequestProvider {
    pub fn new(http: Arc<dyn HttpClient>, runner: Arc<dyn CommandRunner>, config: ForgejoIssueProviderConfig, repo_slug: String) -> Self {
        let client = crate::tls::client_builder().build().expect("build Forgejo request client");
        Self { http, runner, client, config, repo_slug, operator_login: None }
    }

    pub fn with_operator_login(mut self, login: String) -> Self {
        self.operator_login = Some(login);
        self
    }

    fn observed_status(&self, value: &serde_json::Value) -> ObservedStatus {
        let observed_at = Utc::now();
        let state = if value["merged"].as_bool() == Some(true) || !value["merged_at"].is_null() {
            Some(ObservedChangeRequestState::Merged)
        } else if value["state"] == "closed" {
            Some(ObservedChangeRequestState::Closed)
        } else if value["draft"].as_bool() == Some(true) {
            Some(ObservedChangeRequestState::Draft)
        } else if value["state"] == "open" {
            Some(ObservedChangeRequestState::Open)
        } else {
            None
        };
        let requested = self.operator_login.as_deref().and_then(|operator| {
            value["requested_reviewers"].as_array().map(|reviewers| {
                reviewers.iter().any(|reviewer| reviewer["login"].as_str().is_some_and(|login| login.eq_ignore_ascii_case(operator)))
            })
        });
        ObservedStatus {
            title: Observation { value: value["title"].as_str().map(str::to_string), observed_at },
            author: Observation { value: value["user"]["login"].as_str().map(str::to_string), observed_at },
            // The review endpoint supplies this separately during bound observation.
            review_decision: Observation::unknown(observed_at),
            review_requested_from_owner: Observation { value: requested, observed_at },
            state: Observation { value: state, observed_at },
            head_sha: Observation { value: value["head"]["sha"].as_str().map(str::to_string), observed_at },
            checks: Observation::unknown(observed_at),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::unknown(observed_at) },
            mergeable: Observation::unknown(observed_at),
        }
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

    async fn read_review_decision(&self, number: u64) -> Result<Option<ObservedReviewDecision>, String> {
        const PAGE_SIZE: usize = 50;
        const MAX_PAGES: usize = 100;
        let mut reviews = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self.request(
                reqwest::Method::GET,
                &format!("pulls/{number}/reviews"),
                &[("limit", PAGE_SIZE.to_string()), ("page", page.to_string())],
                None,
            )?;
            let response = http_execute!(self.http, request)?;
            if !response.status().is_success() {
                return Err(format!("Forgejo HTTP {}: {}", response.status(), String::from_utf8_lossy(response.body())));
            }
            // Forgejo counts all returned reviews, including stale and
            // dismissed ones. If a proxy strips the header, an empty page
            // still terminates the scan safely.
            let total =
                response.headers().get("x-total-count").and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<usize>().ok());
            let batch: Vec<serde_json::Value> =
                serde_json::from_slice(response.body()).map_err(|error| format!("Forgejo review response was not an array: {error}"))?;
            if batch.is_empty() {
                return Ok(review_decision(&reviews));
            }
            reviews.extend(batch);
            if total.is_some_and(|total| reviews.len() >= total) {
                return Ok(review_decision(&reviews));
            }
        }
        Err(format!("Forgejo review page limit ({MAX_PAGES}) reached for pull request {number}"))
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
    /// The trait default converts only the presentation snapshot. Read the
    /// pull endpoint once per bound number for its author and reviewer facts,
    /// then read the review list to derive the aggregate decision.
    async fn observe_bound(
        &self,
        numbers: &[u64],
        _crew_logins: &super::CrewGithubLoginsByRequest,
    ) -> Result<super::BoundObservations, super::ObservationError> {
        let mut statuses = std::collections::HashMap::new();
        for number in numbers {
            let result = match self.execute(reqwest::Method::GET, &format!("pulls/{number}"), &[], None).await {
                Ok(value) => {
                    let mut status = self.observed_status(&value);
                    let observed_at = Utc::now();
                    status.review_decision = match self.read_review_decision(*number).await {
                        Ok(value) => Observation { value, observed_at },
                        Err(error) => {
                            tracing::warn!(number, %error, "Forgejo review decision unavailable");
                            Observation::unknown(observed_at)
                        }
                    };
                    Ok(status)
                }
                Err(error) => Err(error.into()),
            };
            statuses.insert(*number, result);
        }
        Ok(statuses)
    }
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(self.list("open", limit).await?.iter().filter_map(|value| self.parse(value)).collect())
    }

    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        // Search to an empty page instead of treating the first 100 requests
        // (or a short page from a server-side limit) as proof of absence.
        for page in 1..=100 {
            let value = self
                .execute(
                    reqwest::Method::GET,
                    "pulls",
                    &[("state", "all".into()), ("limit", "50".into()), ("page", page.to_string())],
                    None,
                )
                .await?;
            let items = value.as_array().ok_or("Forgejo pull request list response was not an array")?;
            if let Some(request) = items.iter().filter_map(|value| self.parse(value)).find(|(_, request)| request.branch == branch) {
                return Ok(Some(request));
            }
            if items.is_empty() {
                return Ok(None);
            }
        }
        Err(format!("Forgejo pull request lookup for branch {branch} exceeded 100 pages").into())
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        let value = self.execute(reqwest::Method::GET, &format!("pulls/{id}"), &[], None).await?;
        self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        let value = self.execute(reqwest::Method::GET, &format!("pulls/{id}"), &[], None).await?;
        let (id, change_request) = self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: value["base"]["ref"].as_str().map(str::to_string) })
    }

    async fn update_body(&self, id: &str, body: &str) -> Result<(), String> {
        self.execute(reqwest::Method::PATCH, &format!("pulls/{id}"), &[], Some(serde_json::json!({"body": body}))).await?;
        Ok(())
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
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };

    use super::*;
    use crate::providers::{
        replay::{self, Masks},
        testing::MockRunner,
        ChannelLabel,
    };

    // Boundary double: Forgejo HTTP requests, including the body update.
    struct LinkHttp {
        body: Mutex<String>,
        patches: AtomicUsize,
    }

    #[async_trait]
    impl HttpClient for LinkHttp {
        async fn execute(&self, request: reqwest::Request, _label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            assert_eq!(request.url().as_str(), "https://forgejo.example/api/v1/repos/team/repo/pulls/7");
            let mut body = self.body.lock().expect("body");
            if request.method() == reqwest::Method::PATCH {
                let value: serde_json::Value =
                    serde_json::from_slice(request.body().expect("PATCH body").as_bytes().expect("JSON bytes")).expect("JSON");
                *body = value["body"].as_str().expect("body field").into();
                self.patches.fetch_add(1, Ordering::SeqCst);
            } else {
                assert_eq!(request.method(), reqwest::Method::GET);
            }
            Ok(json_response(&serde_json::json!({"number": 7, "title": "PR", "head": {"ref": "feature"}, "state": "open", "body": *body})))
        }
    }

    // Glue: the native Forgejo endpoint preserves the body, deduplicates links,
    // and an empty issue list produces no write. No working copy is involved.
    #[tokio::test]
    async fn links_issues_through_forgejo_body_endpoint() {
        let http = Arc::new(LinkHttp { body: Mutex::new("Notes".into()), patches: AtomicUsize::new(0) });
        let provider = ForgejoChangeRequestProvider::new(
            http.clone(),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new("https://forgejo.example".into(), None, auth()),
            "team/repo".into(),
        );
        provider.link_issues("7", &["10".into(), "10".into()]).await.expect("link");
        provider.link_issues("7", &["10".into()]).await.expect("repeat");
        provider.link_issues("7", &[]).await.expect("empty");
        assert_eq!(*http.body.lock().expect("body"), "Notes\n\nFixes #10");
        assert_eq!(http.patches.load(Ordering::SeqCst), 1);
    }

    fn parse_review_decision(value: &serde_json::Value) -> Option<ObservedReviewDecision> {
        review_decision(value.as_array().expect("review array"))
    }

    struct MockHttp {
        responses: Mutex<VecDeque<http::Response<bytes::Bytes>>>,
        urls: Mutex<Vec<String>>,
    }

    fn provider(http: Arc<MockHttp>) -> ForgejoChangeRequestProvider {
        ForgejoChangeRequestProvider::new(
            http,
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(
                "https://forgejo.example".into(),
                None,
                crate::providers::issue_tracker::forgejo::ForgejoAuth { token: "test".into(), token_path: PathBuf::from("test") },
            ),
            "team/repo".into(),
        )
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

    fn error_response(status: u16) -> http::Response<bytes::Bytes> {
        http::Response::builder().status(status).body(bytes::Bytes::from_static(b"review read failed")).expect("error response")
    }

    fn review_response(value: &serde_json::Value, total: usize) -> http::Response<bytes::Bytes> {
        http::Response::builder()
            .status(200)
            .header("x-total-count", total.to_string())
            .body(bytes::Bytes::from(serde_json::to_vec(value).expect("serialize reviews")))
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

    #[test]
    fn parses_pull_presentation_fields_from_existing_response() {
        let mut provider = provider(Arc::new(MockHttp { responses: Mutex::new(VecDeque::new()), urls: Mutex::new(Vec::new()) }))
            .with_operator_login("owner".into());
        let raw = serde_json::json!({
            "title": "Keep metadata current", "state": "open", "draft": true,
            "user": {"login": "contributor"}, "requested_reviewers": [{"login": "owner"}],
            "head": {"sha": "abc"}
        });
        let status = provider.observed_status(&raw);
        assert_eq!(status.title.value.as_deref(), Some("Keep metadata current"));
        assert_eq!(status.author.value.as_deref(), Some("contributor"));
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Draft));
        assert_eq!(status.review_decision.value, None);
        assert_eq!(status.review_requested_from_owner.value, Some(true));
        provider.operator_login = None;
        assert_eq!(provider.observed_status(&raw).review_requested_from_owner.value, None);
    }

    #[test]
    fn review_decision_uses_latest_active_decisive_review_per_reviewer() {
        let reviews = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "REQUEST_CHANGES"},
            {"id": 2, "user": {"login": "alice"}, "state": "APPROVED"},
            {"id": 3, "user": {"login": "bob"}, "state": "APPROVED", "dismissed": true},
            {"id": 4, "user": {"login": "carol"}, "state": "REQUEST_CHANGES", "stale": true}
        ]);
        assert_eq!(parse_review_decision(&reviews), Some(ObservedReviewDecision::ChangesRequested));
        let requested = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "APPROVED"},
            {"id": 2, "user": {"login": "bob"}, "state": "REQUEST_REVIEW"}
        ]);
        assert_eq!(parse_review_decision(&requested), Some(ObservedReviewDecision::Required));
        let rerequested = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "APPROVED"},
            {"id": 2, "user": {"login": "alice"}, "state": "REQUEST_REVIEW"}
        ]);
        assert_eq!(parse_review_decision(&rerequested), Some(ObservedReviewDecision::Required));
        let changes = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "APPROVED"},
            {"id": 2, "user": {"login": "bob"}, "state": "REQUEST_CHANGES"}
        ]);
        assert_eq!(parse_review_decision(&changes), Some(ObservedReviewDecision::ChangesRequested));
        let dismissed_latest = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "REQUEST_CHANGES"},
            {"id": 2, "user": {"login": "alice"}, "state": "APPROVED", "dismissed": true}
        ]);
        assert_eq!(parse_review_decision(&dismissed_latest), Some(ObservedReviewDecision::None));
        let stale_latest = serde_json::json!([
            {"id": 1, "user": {"login": "alice"}, "state": "REQUEST_CHANGES"},
            {"id": 2, "user": {"login": "alice"}, "state": "APPROVED", "stale": true}
        ]);
        // The newer approval replaces the older request, but is not itself
        // counted because it is stale.
        assert_eq!(parse_review_decision(&stale_latest), Some(ObservedReviewDecision::None));
        assert_eq!(parse_review_decision(&serde_json::json!([])), Some(ObservedReviewDecision::None));
        assert_eq!(
            parse_review_decision(&serde_json::json!([
                {"id": 1, "user": {"login": "alice"}, "state": "PENDING"},
                {"id": 2, "user": {"login": "bob"}, "state": "COMMENT"}
            ])),
            Some(ObservedReviewDecision::None)
        );
        assert_eq!(
            parse_review_decision(&serde_json::json!([{"id": 1, "team": {"id": 42}, "state": "REQUEST_REVIEW"}])),
            Some(ObservedReviewDecision::Required)
        );
        assert_eq!(
            parse_review_decision(&serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "APPROVED", "stale": true}])),
            Some(ObservedReviewDecision::None)
        );
        assert_eq!(
            parse_review_decision(
                &serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "REQUEST_CHANGES", "dismissed": true}])
            ),
            Some(ObservedReviewDecision::None)
        );
        assert_eq!(parse_review_decision(&serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "NEW_STATE"}])), None);
    }

    #[tokio::test]
    async fn malformed_review_body_is_rejected() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([json_response(&serde_json::json!({"invalid": true}))])),
            urls: Mutex::new(Vec::new()),
        });
        assert!(provider(http).read_review_decision(7).await.expect_err("malformed reviews").contains("not an array"));
    }

    #[tokio::test]
    async fn review_read_pages_all_reviews() {
        let first_page = serde_json::Value::Array(
            (1..=50).map(|id| serde_json::json!({"id": id, "user": {"login": format!("reviewer{id}")}, "state": "COMMENT"})).collect(),
        );
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&first_page),
                review_response(&serde_json::json!([{"id": 51, "user": {"login": "alice"}, "state": "APPROVED"}]), 51),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = provider(http.clone());
        assert_eq!(provider.read_review_decision(7).await.expect("reviews"), Some(ObservedReviewDecision::Approved));
        let urls = http.urls.lock().expect("urls");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("page=1"));
        assert!(urls[1].contains("page=2"));
    }

    #[tokio::test]
    async fn review_read_continues_after_short_pages() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "APPROVED"}])),
                json_response(&serde_json::json!([{"id": 2, "user": {"login": "bob"}, "state": "REQUEST_CHANGES"}])),
                json_response(&serde_json::json!([])),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = provider(http.clone());
        assert_eq!(provider.read_review_decision(7).await.expect("reviews"), Some(ObservedReviewDecision::ChangesRequested));
        assert_eq!(http.urls.lock().expect("urls").len(), 3);
    }

    #[tokio::test]
    async fn review_read_stops_at_page_limit() {
        let page = serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "APPROVED"}]);
        let http =
            Arc::new(MockHttp { responses: Mutex::new((0..101).map(|_| json_response(&page)).collect()), urls: Mutex::new(Vec::new()) });
        let provider = provider(http.clone());
        assert!(provider.read_review_decision(7).await.expect_err("page limit").contains("page limit"));
        assert_eq!(http.urls.lock().expect("urls").len(), 100);
    }

    #[tokio::test]
    async fn bound_observation_reads_pull_and_reviews_per_number() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&serde_json::json!({
                    "number": 7, "title": "Keep metadata current", "state": "open", "draft": false,
                    "user": {"login": "contributor"}, "requested_reviewers": [{"login": "owner"}]
                })),
                json_response(&serde_json::json!([])),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = provider(http.clone()).with_operator_login("owner".into());
        let observed = provider.observe_bound(&[7], &Default::default()).await.expect("observe bound request");
        let status = observed[&7].as_ref().expect("status");
        assert_eq!(status.title.value.as_deref(), Some("Keep metadata current"));
        assert_eq!(status.review_requested_from_owner.value, Some(true));
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::None));
        assert_eq!(http.urls.lock().expect("urls").len(), 2);
    }

    #[tokio::test]
    async fn bound_observation_isolates_review_failure_per_pull() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&serde_json::json!({"number": 7, "title": "PR", "state": "open"})),
                review_response(&serde_json::json!([{"id": 1, "user": {"login": "alice"}, "state": "APPROVED"}]), 1),
                json_response(&serde_json::json!({"number": 8, "title": "Another PR", "state": "open"})),
                error_response(500),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider = provider(http.clone());
        let observed = provider.observe_bound(&[7, 8], &Default::default()).await.expect("observe bound requests");
        assert_eq!(observed[&7].as_ref().expect("status").review_decision.value, Some(ObservedReviewDecision::Approved));
        let failed_review = observed[&8].as_ref().expect("status survives review failure");
        assert_eq!(failed_review.title.value.as_deref(), Some("Another PR"));
        assert_eq!(failed_review.review_decision.value, None);
        let urls = http.urls.lock().expect("urls");
        assert_eq!(urls.len(), 4);
        assert!(urls[1].contains("pulls/7/reviews"));
        assert!(urls[3].contains("pulls/8/reviews"));
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

    // HTTP boundary: fixtures exercise late matches, short pages, proven absence
    // and budget exhaustion without contacting a Forgejo deployment.
    #[tokio::test]
    async fn branch_lookup_searches_beyond_the_old_window_and_proves_absence() {
        for found in [false, true] {
            let first = serde_json::Value::Array(
                (1..=50)
                    .map(|number| serde_json::json!({"number": number, "title": "Other", "head": {"ref": "other"}, "state": "open"}))
                    .collect(),
            );
            let last = if found {
                serde_json::json!([{"number": 101, "title": "Wanted", "head": {"ref": "wanted"}, "state": "closed", "merged": true}])
            } else {
                serde_json::json!([])
            };
            let http = Arc::new(MockHttp {
                responses: Mutex::new(VecDeque::from([json_response(&first), json_response(&first), json_response(&last)])),
                urls: Mutex::new(Vec::new()),
            });
            let result = provider(http.clone()).find_change_request_by_branch("wanted").await.expect("lookup");
            assert_eq!(result.is_some(), found);
            assert_eq!(http.urls.lock().expect("urls").len(), 3);
            if let Some((id, request)) = result {
                assert_eq!(id, "101");
                assert_eq!(request.status, ChangeRequestStatus::Merged);
            }
        }
        let item = serde_json::json!([{"number": 1, "title": "Other", "head": {"ref": "other"}, "state": "open"}]);
        let http =
            Arc::new(MockHttp { responses: Mutex::new((0..100).map(|_| json_response(&item)).collect()), urls: Mutex::new(Vec::new()) });
        assert!(provider(http.clone())
            .find_change_request_by_branch("wanted")
            .await
            .expect_err("cannot prove absence")
            .to_string()
            .contains("exceeded 100 pages"));
        assert_eq!(http.urls.lock().expect("urls").len(), 100);
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
