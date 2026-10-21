use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_forge_forgejo::{ForgejoClient, ForgejoIssueProviderConfig};
use flotilla_protocol::ChangeRequest;
use flotilla_protocol::ChangeRequestStatus;
use flotilla_provider_runtime::http_execute;
use flotilla_provider_runtime::run;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::HttpClient;
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    ChangeRequestReviewObservation, ChangeRequestStatus as ObservedStatus, Observation, ObservedChangeRequestState, ObservedReviewDecision,
};

use flotilla_change_request::ChangeRequestAdmission;
use flotilla_change_request::ChangeRequestTracker;

pub struct ForgejoChangeRequestProvider {
    runner: Arc<dyn CommandRunner>,
    forge: ForgejoClient,
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
        Self { forge: ForgejoClient::new(http, config), runner, repo_slug, operator_login: None }
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

    async fn read_review_decision(&self, number: u64) -> Result<Option<ObservedReviewDecision>, ObservationError> {
        const PAGE_SIZE: usize = 50;
        const MAX_PAGES: usize = 100;
        let mut reviews = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self.forge.repository_request(
                &self.repo_slug,
                reqwest::Method::GET,
                &format!("pulls/{number}/reviews"),
                &[("limit", PAGE_SIZE.to_string()), ("page", page.to_string())],
                None,
            )?;
            let response = http_execute!(self.forge.http, request)?;
            crate::observation::check_response(&self.forge.config.service_url, response.status(), response.headers(), response.body())?;
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
        Err(format!("Forgejo review page limit ({MAX_PAGES}) reached for pull request {number}").into())
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
        Some((
            number.to_string(),
            ChangeRequest {
                title,
                branch,
                status,
                body: value["body"].as_str().map(str::to_string),
                provider_name: "forgejo".into(),
                provider_display_name: "Forgejo".into(),
            },
        ))
    }

    async fn list(&self, state: &str, limit: usize) -> Result<Vec<serde_json::Value>, String> {
        let mut items = Vec::new();
        let page_size = limit.min(50);
        while items.len() < limit {
            let page = items.len() / page_size + 1;
            let value = self
                .forge
                .execute(
                    &self.repo_slug,
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
        _crew_logins: &flotilla_change_request::CrewGithubLoginsByRequest,
    ) -> Result<flotilla_change_request::BoundObservations, ObservationError> {
        let mut statuses = std::collections::HashMap::new();
        for number in numbers {
            let result = match crate::observation::execute_pull(&self.forge, &self.repo_slug, *number).await {
                Ok(value) => {
                    let mut status = self.observed_status(&value);
                    let observed_at = Utc::now();
                    status.review_decision = match self.read_review_decision(*number).await {
                        Ok(value) => Observation { value, observed_at },
                        Err(error @ ObservationError::RateLimited { .. }) => {
                            statuses.insert(*number, Err(error));
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(number, %error, "Forgejo review decision unavailable");
                            Observation::unknown(observed_at)
                        }
                    };
                    Ok(status)
                }
                Err(error) => Err(error),
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
            let value = crate::observation::execute(
                &self.forge,
                &self.repo_slug,
                "pulls",
                &[("state", "all".into()), ("limit", "50".into()), ("page", page.to_string())],
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
        let value = self.forge.execute(&self.repo_slug, reqwest::Method::GET, &format!("pulls/{id}"), &[], None).await?;
        self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))
    }

    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        let value = crate::observation::execute(&self.forge, &self.repo_slug, &format!("pulls/{id}"), &[]).await?;
        let (id, change_request) = self.parse(&value).ok_or_else(|| format!("malformed Forgejo pull request {id}"))?;
        Ok(ChangeRequestAdmission { id, change_request, base_ref: value["base"]["ref"].as_str().map(str::to_string) })
    }

    async fn update_body(&self, id: &str, body: &str) -> Result<(), String> {
        self.forge
            .execute(&self.repo_slug, reqwest::Method::PATCH, &format!("pulls/{id}"), &[], Some(serde_json::json!({"body": body})))
            .await?;
        Ok(())
    }

    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        let url = self.forge.config.change_request_url(&self.repo_slug, id);
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
        self.forge
            .execute(&self.repo_slug, reqwest::Method::PATCH, &format!("pulls/{id}"), &[], Some(serde_json::json!({"state":"closed"})))
            .await?;
        Ok(())
    }

    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        self.forge
            .execute(&self.repo_slug, reqwest::Method::POST, &format!("pulls/{id}/merge"), &[], Some(serde_json::json!({"Do":"squash"})))
            .await?;
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

    use flotilla_provider_runtime::ChannelLabel;

    use super::*;
    use flotilla_fake_forge::{
        recorded::{ManualClock, RecordedWorld, RECORDED_ORIGIN, RECORDED_TOKEN},
        FakeForge, ForgeSettings, InProcessClient, ItemEdit, ItemState, NewIssue, NewPull, NewReview, RepoRef, ReviewState,
    };
    use flotilla_forge_forgejo::ForgejoAuth;
    use flotilla_replay_testkit as replay;
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_replay_testkit::Masks;

    use crate::change_request::testing::fixture_path;
    use flotilla_change_request::branch_lookup_observer::BranchLookupObserver;

    // HTTP boundary fault injection: every request must first pass the strict fake.
    struct ThrottledForge {
        inner: InProcessClient,
        header: &'static str,
        value: &'static str,
        reviews_only: bool,
    }

    #[async_trait]
    impl HttpClient for ThrottledForge {
        async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            let throttle = !self.reviews_only || request.url().path().ends_with("/reviews");
            let response = self.inner.execute(request, label).await?;
            assert!(response.status().is_success(), "request must satisfy fake Forgejo: {response:?}");
            if !throttle {
                return Ok(response);
            }
            Ok(http::Response::builder()
                .status(429)
                .header(self.header, self.value)
                .body(bytes::Bytes::from_static(b"rate limit exceeded"))
                .expect("rate-limit response"))
        }
    }

    // #3480: observation paths preserve the server's retry deadline as a typed error.
    // A finite HTTP scenario covers reset, relative retry, missing/invalid metadata;
    // no concurrency or population boundary affects response classification.
    #[tokio::test]
    async fn fake_forge_rate_limits_preserve_typed_retry_deadlines() {
        for (header, value, expected) in [
            ("x-ratelimit-reset", "1893456000", Some(1893456000)),
            ("retry-after", "60", None),
            ("x-ratelimit-reset", "invalid", None),
            ("unrelated", "0", None),
        ] {
            let (fake, _clock, mut provider) = empty_fake();
            let repo = RepoRef::new("lab", "team", "repo");
            fake.create_pull(
                &repo,
                NewPull { title: "Work".into(), head: "work".into(), base: "main".into(), body: String::new(), user: None, draft: false },
            )
            .expect("pull");
            provider.forge.http =
                Arc::new(ThrottledForge { inner: InProcessClient::new(fake.clone()), header, value, reviews_only: false });
            let before = Utc::now();
            let lookup = provider.find_change_request_by_branch("work").await.expect_err("throttled lookup");
            let admission = provider.get_change_request_for_admission("1").await.expect_err("throttled admission");
            let mut observed = provider.observe_bound(&[1], &Default::default()).await.expect("per-request outcomes");
            let bound = observed.remove(&1).expect("bound outcome").expect_err("throttled observation");
            for error in [lookup, admission, bound] {
                let ObservationError::RateLimited { retry_at, .. } = error else { panic!("lost typed rate-limit error: {error:?}") };
                if let Some(expected) = expected {
                    assert_eq!(retry_at.map(|date| date.timestamp()), Some(expected));
                } else if header == "retry-after" {
                    let retry_at = retry_at.expect("relative retry deadline");
                    assert!(retry_at >= before + chrono::Duration::seconds(60));
                    assert!(retry_at <= Utc::now() + chrono::Duration::seconds(60));
                } else {
                    assert_eq!(retry_at, None);
                }
            }
            provider.forge.http = Arc::new(ThrottledForge { inner: InProcessClient::new(fake.clone()), header, value, reviews_only: true });
            let mut observed = provider.observe_bound(&[1], &Default::default()).await.expect("per-request outcomes");
            assert!(matches!(observed.remove(&1), Some(Err(ObservationError::RateLimited { .. }))), "review throttling must remain typed");
            assert_no_refusals(&fake);
        }
    }

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
                ForgejoAuth { token: "test".into(), token_path: PathBuf::from("test") },
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

    // HTTP boundary: check the pull and pull-list routes while varying
    // repository access after a hidden/missing pull's 404.
    #[tokio::test]
    async fn pull_absence_requires_repository_pull_access() {
        for repository_status in [200, 401, 403, 404, 429, 500] {
            let http = Arc::new(MockHttp {
                responses: Mutex::new(VecDeque::from([
                    error_response(404),
                    if repository_status == 200 { response(&[]) } else { error_response(repository_status) },
                ])),
                urls: Mutex::new(Vec::new()),
            });
            let provider = provider(http.clone());
            let outcomes = provider.observe_bound(&[70], &Default::default()).await.unwrap();
            assert_eq!(matches!(outcomes[&70], Err(ObservationError::NotFound(_))), repository_status == 200);
            assert_eq!(
                *http.urls.lock().unwrap(),
                vec![
                    "https://forgejo.example/api/v1/repos/team/repo/pulls/70",
                    "https://forgejo.example/api/v1/repos/team/repo/pulls?state=all&limit=1&page=1",
                ]
            );
            assert!(http.responses.lock().unwrap().is_empty());
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

    fn auth() -> ForgejoAuth {
        if !replay::is_live() {
            return ForgejoAuth { token: "fixture-token".into(), token_path: PathBuf::from("fixture-token") };
        }
        let token_path =
            std::env::var_os("FORGEJO_TOKEN_FILE").map(PathBuf::from).expect("FORGEJO_TOKEN_FILE is required for live recording");
        let token = std::fs::read_to_string(&token_path).expect("read Forgejo token").trim().to_string();
        ForgejoAuth { token, token_path }
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
        assert!(provider(http).read_review_decision(7).await.expect_err("malformed reviews").to_string().contains("not an array"));
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
        assert!(provider.read_review_decision(7).await.expect_err("page limit").to_string().contains("page limit"));
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
    fn replay_provider(fixture: &str, repo_slug: &str) -> (replay::Session, ForgejoChangeRequestProvider) {
        let auth = auth();
        let mut masks = Masks::new();
        masks.add(&auth.token, RECORDED_TOKEN);
        let session = replay::test_session(&fixture_path(fixture), masks);
        let provider = ForgejoChangeRequestProvider::new(
            replay::test_http_client(&session),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(RECORDED_ORIGIN.into(), None, auth),
            repo_slug.into(),
        );
        (session, provider)
    }

    const FAKE_URL: &str = "https://fake-forge.test:3443";
    const FAKE_TOKEN: &str = "fake-forge-token";

    /// The real provider, configured as a rehearsal Forge record points it at
    /// the fake's `lab` mount, reaching it through the in-process HTTP seam.
    fn fake_provider(fake: &Arc<FakeForge>, token: &str, repo_slug: &str) -> ForgejoChangeRequestProvider {
        ForgejoChangeRequestProvider::new(
            Arc::new(InProcessClient::new(Arc::clone(fake))),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(
                fake.service_url("lab"),
                None,
                ForgejoAuth { token: token.into(), token_path: PathBuf::from("fake-forge-token") },
            ),
            repo_slug.into(),
        )
    }

    fn recorded_world_provider(world: RecordedWorld) -> (Arc<FakeForge>, ForgejoChangeRequestProvider) {
        let fake = world.seed(FAKE_URL, "lab", FAKE_TOKEN);
        let provider = fake_provider(&fake, FAKE_TOKEN, world.repo_slug());
        (fake, provider)
    }

    /// The provider stayed within the fake's contract: nothing was refused.
    fn assert_no_refusals(fake: &FakeForge) {
        let (refusals, count) = fake.refusals();
        assert_eq!(count, 0, "the fake refused provider requests: {refusals:?}");
    }

    // Contract (recorded world: lab/flotilla has no open pull requests): the
    // open listing holds only open or draft requests, here none.
    async fn contract_lists_open_pull_requests(provider: &ForgejoChangeRequestProvider) {
        let requests = provider.list_change_requests(10).await.expect("list pull requests");
        assert!(requests.iter().all(|(_, request)| matches!(request.status, ChangeRequestStatus::Open | ChangeRequestStatus::Draft)));
        assert!(requests.is_empty(), "the recorded repository has no open pull requests: {requests:?}");
    }

    #[tokio::test]
    async fn record_replay_lists_forgejo_pull_requests() {
        let (session, provider) = replay_provider("forgejo_pulls.yaml", "lab/flotilla");
        contract_lists_open_pull_requests(&provider).await;
        session.finish();
    }

    #[tokio::test]
    async fn fake_forge_lists_forgejo_pull_requests() {
        let (fake, provider) = recorded_world_provider(RecordedWorld::LabFlotillaWithoutOpenPulls);
        contract_lists_open_pull_requests(&provider).await;
        assert_no_refusals(&fake);
    }

    // Contract (recorded world: robert/ghostty-ops before any pull request):
    // a branch lookup scans to the empty page and proves absence.
    async fn contract_queries_ghostty_governor_pull_request(provider: &ForgejoChangeRequestProvider) {
        let found = provider.find_change_request_by_branch("governor").await.expect("query Forgejo pull requests");
        assert!(found.is_none(), "the recorded repository has no pull requests: {found:?}");
    }

    #[tokio::test]
    async fn record_replay_queries_ghostty_governor_pull_request() {
        let (session, provider) = replay_provider("forgejo_ghostty_governor.yaml", "robert/ghostty-ops");
        contract_queries_ghostty_governor_pull_request(&provider).await;
        session.finish();
    }

    #[tokio::test]
    async fn fake_forge_queries_ghostty_governor_pull_request() {
        let (fake, provider) = recorded_world_provider(RecordedWorld::GhosttyOpsBeforePulls);
        contract_queries_ghostty_governor_pull_request(&provider).await;
        assert_no_refusals(&fake);
    }

    // HTTP boundary: an observer reuses only proven absence, then checks again
    // at the freshness boundary so a newly created fork-head PR can be found.
    #[tokio::test(start_paused = true)]
    async fn observer_repeated_absent_scans_are_bounded_and_expire() {
        let other = serde_json::json!([{"number": 1, "title": "Other", "head": {"ref": "other"}, "state": "open"}]);
        let found = serde_json::json!([{"number": 7, "title": "Fork PR", "head": {"ref": "feature/wanted", "repo": {"full_name": "fork/repo"}}, "state": "closed", "merged": true}]);
        let http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([
                json_response(&other),
                json_response(&other),
                json_response(&serde_json::json!([])),
                json_response(&found),
                json_response(&found),
            ])),
            urls: Mutex::new(Vec::new()),
        });
        let provider: Arc<dyn ChangeRequestTracker> = Arc::new(provider(http.clone()));
        let observer = BranchLookupObserver::default();
        for _ in 0..4 {
            assert!(observer.find("team/repo", &provider, "feature/wanted").await.expect("proven absence").is_none());
            assert_eq!(http.urls.lock().expect("urls").len(), 3, "repeat scans reuse the complete scan");
        }
        tokio::time::advance(tokio::time::Duration::from_secs(299)).await;
        assert!(observer.find("team/repo", &provider, "feature/wanted").await.expect("fresh absence").is_none());
        assert_eq!(http.urls.lock().expect("urls").len(), 3);
        tokio::time::advance(tokio::time::Duration::from_secs(1)).await;
        for expected_calls in [4, 5] {
            let (id, request) = observer.find("team/repo", &provider, "feature/wanted").await.expect("fresh lookup").expect("fork head");
            assert_eq!(id, "7");
            assert_eq!(request.status, ChangeRequestStatus::Merged);
            assert_eq!(http.urls.lock().expect("urls").len(), expected_calls, "positive observations are read fresh");
        }
    }

    // #2585: absence observations are isolated by repository and exact branch.
    // Generate repeated/interleaved keys, including empty, slash, percent and
    // Unicode names. The network boundary returns complete empty listings.
    #[hegel::test]
    fn observer_absence_keys_do_not_leak(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let keys = (0..steps)
            .map(|_| {
                (tc.draw(gs::integers::<usize>().min_value(0).max_value(1)), tc.draw(gs::integers::<usize>().min_value(0).max_value(4)))
            })
            .collect::<Vec<_>>();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let http = Arc::new(MockHttp {
                responses: Mutex::new((0..steps).map(|_| json_response(&serde_json::json!([]))).collect()),
                urls: Mutex::new(Vec::new()),
            });
            let provider: Arc<dyn ChangeRequestTracker> = Arc::new(provider(http.clone()));
            let observer = BranchLookupObserver::default();
            let mut seen = std::collections::HashSet::new();
            for (repository, branch) in keys {
                let repo = ["team/repo", "team/other"][repository];
                let name = ["", "feature/wanted", "feature%2Fwanted", "wanted", "修正"][branch];
                assert!(observer.find(repo, &provider, name).await.expect("absence").is_none());
                seen.insert((repository, branch));
                assert_eq!(http.urls.lock().expect("urls").len(), seen.len(), "each distinct key scans once");
            }
        });
    }

    // #2585: bounded observer memory evicts the earliest-expiring observation,
    // causing a fresh scan instead of making eviction look like a permanent miss.
    #[tokio::test(start_paused = true)]
    async fn observer_absence_capacity_evicts_oldest() {
        let http = Arc::new(MockHttp {
            responses: Mutex::new((0..1027).map(|_| json_response(&serde_json::json!([]))).collect()),
            urls: Mutex::new(Vec::new()),
        });
        let provider: Arc<dyn ChangeRequestTracker> = Arc::new(provider(http.clone()));
        let observer = BranchLookupObserver::default();
        for index in 0..1024 {
            assert!(observer.find("team/repo", &provider, &format!("branch-{index}")).await.expect("absence").is_none());
            tokio::time::advance(tokio::time::Duration::from_millis(1)).await;
        }
        assert!(observer.find("team/repo", &provider, "branch-0").await.expect("at capacity").is_none());
        assert_eq!(http.urls.lock().expect("urls").len(), 1024);
        // Replacing the provider for an existing key must not evict another key.
        let replacement: Arc<dyn ChangeRequestTracker> = Arc::new(self::provider(http.clone()));
        assert!(observer.find("team/repo", &replacement, "branch-1023").await.expect("replace existing key").is_none());
        assert!(observer.find("team/repo", &provider, "branch-0").await.expect("unrelated key survives").is_none());
        assert_eq!(http.urls.lock().expect("urls").len(), 1025);
        assert!(observer.find("team/repo", &provider, "branch-1024").await.expect("overflow").is_none());
        assert!(observer.find("team/repo", &provider, "branch-0").await.expect("evicted branch").is_none());
        assert_eq!(http.urls.lock().expect("urls").len(), 1027);
    }

    // HTTP boundary: a bounded failure is never absence, and replacement
    // providers (changed forge/credentials) do not inherit a previous absence.
    #[tokio::test]
    async fn observer_does_not_cache_failures_or_cross_provider_identity() {
        let other = serde_json::json!([{"number": 1, "title": "Other", "head": {"ref": "other"}, "state": "open"}]);
        let http = Arc::new(MockHttp {
            responses: Mutex::new(
                (0..100).map(|_| json_response(&other)).chain([error_response(403), json_response(&serde_json::json!([]))]).collect(),
            ),
            urls: Mutex::new(Vec::new()),
        });
        let first: Arc<dyn ChangeRequestTracker> = Arc::new(provider(http.clone()));
        let observer = BranchLookupObserver::default();
        assert!(observer
            .find("team/repo", &first, "wanted")
            .await
            .expect_err("budget is not absence")
            .to_string()
            .contains("exceeded 100 pages"));
        assert!(observer.find("team/repo", &first, "wanted").await.expect_err("HTTP failure is not absence").to_string().contains("403"));
        assert!(observer.find("team/repo", &first, "wanted").await.expect("retry complete scan").is_none());
        assert_eq!(http.urls.lock().expect("urls").len(), 102);
        let replacement_http = Arc::new(MockHttp {
            responses: Mutex::new(VecDeque::from([json_response(&serde_json::json!([]))])),
            urls: Mutex::new(Vec::new()),
        });
        let replacement: Arc<dyn ChangeRequestTracker> = Arc::new(provider(replacement_http.clone()));
        assert!(observer.find("team/repo", &replacement, "wanted").await.expect("new provider").is_none());
        assert_eq!(replacement_http.urls.lock().expect("urls").len(), 1);
    }

    // Contract (recorded world: robert/ghostty-ops with pull request 1 merged
    // and its branch deleted, pull request 2 merged from drop-retired-stance):
    // head filtering supports an exact branch name, but cannot reproduce
    // lookup by a deleted head's synthetic head.ref, which only an unfiltered
    // scan finds. A repeated absent lookup reuses the proven absence.
    async fn contract_lab_head_filter_capability(provider: ForgejoChangeRequestProvider) {
        for (head, expected) in [
            ("drop-retired-stance", &["2"][..]),
            ("robert:drop-retired-stance", &[]),
            ("refs/pull/1/head", &[]),
            ("flotilla-2585-definitely-absent", &[]),
        ] {
            let result = provider
                .forge
                .execute(
                    &provider.repo_slug,
                    reqwest::Method::GET,
                    "pulls",
                    &[("state", "all".into()), ("limit", "50".into()), ("page", "1".into()), ("head", head.into())],
                    None,
                )
                .await
                .expect("head query");
            let numbers: Vec<String> =
                result.as_array().expect("pull list").iter().filter_map(|pull| pull["number"].as_i64()).map(|n| n.to_string()).collect();
            assert_eq!(numbers, expected, "head filter {head}");
        }
        let (id, request) =
            provider.find_change_request_by_branch("refs/pull/1/head").await.expect("unfiltered lookup").expect("deleted head PR");
        assert_eq!(id, "1");
        assert_eq!(request.branch, "refs/pull/1/head");
        assert_eq!(request.status, ChangeRequestStatus::Merged);
        let provider: Arc<dyn ChangeRequestTracker> = Arc::new(provider);
        let observer = BranchLookupObserver::default();
        for _ in 0..3 {
            assert!(observer.find("robert/ghostty-ops", &provider, "flotilla-2585-definitely-absent").await.expect("absence").is_none());
        }
    }

    // Live-recorded evidence for the head-filter contract.
    #[tokio::test]
    async fn record_replay_lab_head_filter_capability() {
        let (session, provider) = replay_provider("forgejo_head_filter_capability.yaml", "robert/ghostty-ops");
        contract_lab_head_filter_capability(provider).await;
        session.finish();
    }

    #[tokio::test]
    async fn fake_forge_lab_head_filter_capability() {
        let (fake, provider) = recorded_world_provider(RecordedWorld::GhosttyOpsWithMergedPulls);
        contract_lab_head_filter_capability(provider).await;
        assert_no_refusals(&fake);
    }

    #[test]
    fn parses_forgejo_states_and_branch() {
        let config = ForgejoIssueProviderConfig::new("https://forgejo.example.test".into(), None, auth());
        let provider = ForgejoChangeRequestProvider::new(
            Arc::new(flotilla_provider_runtime::ReqwestHttpClient::new()),
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

    const OPERATOR: &str = "operator";

    /// A fresh fake with an empty `lab` forge whose token belongs to
    /// `flotilla-bot`, and the real provider pointed at `team/repo` on it.
    fn empty_fake() -> (Arc<FakeForge>, Arc<ManualClock>, ForgejoChangeRequestProvider) {
        let clock = Arc::new(ManualClock::new("2026-10-16T12:00:00Z".parse().expect("time")));
        let fake = Arc::new(FakeForge::new(FAKE_URL, clock.clone()));
        fake.add_forge("lab", ForgeSettings { token: FAKE_TOKEN.into(), login: "flotilla-bot".into() }).expect("lab forge");
        let provider = fake_provider(&fake, FAKE_TOKEN, "team/repo");
        (fake, clock, provider)
    }

    fn team_repo() -> RepoRef {
        RepoRef::new("lab", "team", "repo")
    }

    async fn open_through_api(provider: &ForgejoChangeRequestProvider, head: &str, title: &str, body: &str) -> String {
        let created = provider
            .forge
            .execute(
                &provider.repo_slug,
                reqwest::Method::POST,
                "pulls",
                &[],
                Some(serde_json::json!({"head": head, "base": "main", "title": title, "body": body})),
            )
            .await
            .expect("open pull request through the fake's API");
        created["number"].as_u64().expect("created number").to_string()
    }

    fn request(title: &str, branch: &str, status: ChangeRequestStatus, body: &str) -> ChangeRequest {
        ChangeRequest {
            title: title.into(),
            branch: branch.into(),
            status,
            body: Some(body.into()),
            provider_name: "forgejo".into(),
            provider_display_name: "Forgejo".into(),
        }
    }

    async fn observe(provider: &ForgejoChangeRequestProvider, number: u64) -> ObservedStatus {
        let mut observed = provider.observe_bound(&[number], &Default::default()).await.expect("observe bound");
        observed.remove(&number).expect("observed number").expect("observed status")
    }

    // Behaviour (#3374): a crew's whole change-request lifecycle runs through
    // the real Forgejo provider against the fake: a pull request opened
    // through the API takes the next shared number, is listed, found by
    // branch, read for admission with its base, has its body edited and
    // issues linked, is observed with author, owner review request, draft
    // state, head commit and the review decision derived from reviews added
    // by scenario drivers, and then closed or merged. Merged branch names
    // follow the recorded head.ref rule: a deleted head reads as its pull ref.
    // Writes cannot run on read-only recordings, so this runs on the fake only.
    #[tokio::test]
    async fn fake_forge_change_request_lifecycle() {
        let (fake, clock, provider) = empty_fake();
        let provider = provider.with_operator_login(OPERATOR.into());
        let repo = team_repo();
        fake.create_issue(&repo, NewIssue { title: "Tracked work".into(), ..NewIssue::default() }).expect("issue 1");

        let id = open_through_api(&provider, "feature/one", "First change", "Notes").await;
        assert_eq!(id, "2", "pull requests share the issue number sequence");
        let first = request("First change", "feature/one", ChangeRequestStatus::Open, "Notes");
        assert_eq!(provider.list_change_requests(50).await.expect("list"), vec![(id.clone(), first.clone())]);
        assert_eq!(provider.find_change_request_by_branch("feature/one").await.expect("find"), Some((id.clone(), first.clone())));
        assert_eq!(provider.find_change_request_by_branch("feature/absent").await.expect("absence"), None);
        assert_eq!(provider.get_change_request(&id).await.expect("get"), (id.clone(), first.clone()));
        let admission = provider.get_change_request_for_admission(&id).await.expect("admission");
        assert_eq!((admission.id.as_str(), admission.base_ref.as_deref()), (id.as_str(), Some("main")));
        assert_eq!(admission.change_request, first);

        provider.update_body(&id, "Rewritten").await.expect("update body");
        provider.link_issues(&id, &["1".into(), "1".into()]).await.expect("link issues");
        provider.link_issues(&id, &["1".into()]).await.expect("link again");
        assert_eq!(provider.get_change_request(&id).await.expect("get").1.body.as_deref(), Some("Rewritten\n\nFixes #1"));
        assert_eq!(fake.item(&repo, 2).expect("model").body, "Rewritten\n\nFixes #1");

        // Observation: author, owner review request, head commit and decision.
        let status = observe(&provider, 2).await;
        assert_eq!(status.author.value.as_deref(), Some("flotilla-bot"));
        assert_eq!(status.title.value.as_deref(), Some("First change"));
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Open));
        assert_eq!(status.review_requested_from_owner.value, Some(false));
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::None));
        assert_eq!(status.head_sha.value, fake.item(&repo, 2).expect("model").pull.map(|pull| pull.head_sha));
        fake.request_review(&repo, 2, OPERATOR).expect("request review");
        let approval = fake
            .add_review(&repo, 2, NewReview { user: "alice".into(), state: ReviewState::Approved, body: String::new() })
            .expect("approve");
        let status = observe(&provider, 2).await;
        assert_eq!(status.review_requested_from_owner.value, Some(true));
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::Approved));
        let changes = fake
            .add_review(&repo, 2, NewReview { user: "bob".into(), state: ReviewState::RequestChanges, body: "Please fix".into() })
            .expect("request changes");
        assert_eq!(observe(&provider, 2).await.review_decision.value, Some(ObservedReviewDecision::ChangesRequested));
        // A new head makes both reviews stale: the stale approval stops
        // counting, the stale change request still blocks.
        clock.set("2026-10-16T13:00:00Z".parse().expect("time"));
        let pushed = fake.push_head(&repo, 2).expect("push");
        let status = observe(&provider, 2).await;
        assert_eq!(status.review_decision.value, Some(ObservedReviewDecision::ChangesRequested));
        assert_eq!(status.head_sha.value, pushed.pull.map(|pull| pull.head_sha));
        fake.dismiss_review(&repo, 2, changes.id).expect("dismiss");
        assert_ne!(approval.id, changes.id);
        assert_eq!(observe(&provider, 2).await.review_decision.value, Some(ObservedReviewDecision::None));
        fake.set_draft(&repo, 2, true).expect("draft");
        assert_eq!(observe(&provider, 2).await.state.value, Some(ObservedChangeRequestState::Draft));
        assert_eq!(
            provider.list_change_requests(50).await.expect("list").first().map(|(_, request)| request.status.clone()),
            Some(ChangeRequestStatus::Draft)
        );
        fake.set_draft(&repo, 2, false).expect("ready");
        let missing = provider.observe_bound(&[2, 99], &Default::default()).await.expect("observe bound");
        assert!(missing[&2].is_ok());
        assert!(matches!(missing[&99], Err(ObservationError::NotFound(_))));

        // Merge one request, close the other.
        let second = open_through_api(&provider, "feature/two", "Second change", "").await;
        assert_eq!(second, "3");
        provider.merge_change_request(&second).await.expect("merge");
        assert_eq!(provider.get_change_request(&second).await.expect("get").1.status, ChangeRequestStatus::Merged);
        assert_eq!(observe(&provider, 3).await.state.value, Some(ObservedChangeRequestState::Merged));
        assert!(provider.merge_change_request(&second).await.expect_err("merged twice").contains("405"));
        provider.close_change_request(&id).await.expect("close");
        assert_eq!(provider.get_change_request(&id).await.expect("get").1.status, ChangeRequestStatus::Closed);
        assert!(provider.merge_change_request(&id).await.expect_err("closed is not mergeable").contains("405"));
        assert_eq!(fake.item(&repo, 2).expect("model").state, ItemState::Closed);

        assert_eq!(provider.list_change_requests(50).await.expect("list"), vec![]);
        assert_eq!(provider.list_merged_branch_names(50).await.expect("merged"), vec!["feature/two".to_string()]);
        fake.delete_head(&repo, 3).expect("delete head");
        assert_eq!(provider.list_merged_branch_names(50).await.expect("merged"), vec!["refs/pull/3/head".to_string()]);
        assert_eq!(provider.find_change_request_by_branch("feature/two").await.expect("find"), None);
        let (found, merged) = provider.find_change_request_by_branch("refs/pull/3/head").await.expect("find").expect("pull ref");
        assert_eq!((found.as_str(), merged.status), ("3", ChangeRequestStatus::Merged));
        assert_eq!(
            provider.find_change_request_by_branch("feature/one").await.expect("find").map(|(id, request)| (id, request.status)),
            Some(("2".into(), ChangeRequestStatus::Closed))
        );
        assert_no_refusals(&fake);
    }

    #[derive(Debug, Clone, Copy)]
    enum Fate {
        Open,
        Draft,
        Closed,
        Merged,
        MergedHeadDeleted,
    }

    const FATES: [Fate; 5] = [Fate::Open, Fate::Draft, Fate::Closed, Fate::Merged, Fate::MergedHeadDeleted];

    // Behaviour (#3374): over any pull-request population, the provider's
    // listings through the fake agree with the forge's state: open listing is
    // the open and draft requests newest first, truncated to the limit;
    // a branch lookup finds the newest request whose head.ref is the branch
    // (a deleted head reads as its pull ref); merged branch names are the
    // merged requests among the newest closed ones. The generator draws 0..=60
    // pull requests, crossing the 50-item page boundary, over six branch names
    // so branches repeat (an open duplicate is refused by the forge and
    // skipped), each with every fate, and a limit of 1..=70 on either side of
    // the page size.
    #[hegel::test]
    fn fake_forge_listings_agree_with_generated_pull_populations(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(60));
        let pulls: Vec<(usize, Fate)> = (0..count)
            .map(|_| {
                (
                    tc.draw(gs::integers::<usize>().min_value(0).max_value(5)),
                    FATES[tc.draw(gs::integers::<usize>().min_value(0).max_value(4))],
                )
            })
            .collect();
        let limit = tc.draw(gs::integers::<usize>().min_value(1).max_value(70));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let (fake, _clock, provider) = empty_fake();
            let repo = team_repo();
            // Expected state: (number, head.ref, status), oldest first.
            let mut expected: Vec<(u64, String, ChangeRequestStatus)> = Vec::new();
            for (branch, fate) in pulls {
                let head = format!("branch-{branch}");
                let new =
                    NewPull { head: head.clone(), base: "main".into(), title: head.clone(), body: String::new(), user: None, draft: false };
                let Ok(pull) = fake.create_pull(&repo, new) else {
                    continue;
                };
                let number = pull.number;
                let status = match fate {
                    Fate::Open => ChangeRequestStatus::Open,
                    Fate::Draft => {
                        fake.set_draft(&repo, number, true).expect("draft");
                        ChangeRequestStatus::Draft
                    }
                    Fate::Closed => {
                        fake.edit(&repo, number, ItemEdit { state: Some(ItemState::Closed), ..ItemEdit::default() }).expect("close");
                        ChangeRequestStatus::Closed
                    }
                    Fate::Merged | Fate::MergedHeadDeleted => {
                        fake.merge(&repo, number, "squash", None).expect("merge");
                        ChangeRequestStatus::Merged
                    }
                };
                let head_ref = if matches!(fate, Fate::MergedHeadDeleted) {
                    fake.delete_head(&repo, number).expect("delete head");
                    format!("refs/pull/{number}/head")
                } else {
                    head
                };
                expected.push((number, head_ref, status));
            }
            let newest_first: Vec<_> = expected.iter().rev().collect();

            let listed: Vec<(String, String, ChangeRequestStatus)> = provider
                .list_change_requests(limit)
                .await
                .expect("list")
                .into_iter()
                .map(|(id, request)| (id, request.branch, request.status))
                .collect();
            let open: Vec<_> = newest_first
                .iter()
                .filter(|(_, _, status)| matches!(status, ChangeRequestStatus::Open | ChangeRequestStatus::Draft))
                .take(limit)
                .map(|(number, head, status)| (number.to_string(), head.clone(), status.clone()))
                .collect();
            assert_eq!(listed, open, "open listing");

            for branch in (0..6).map(|branch| format!("branch-{branch}")) {
                let found =
                    provider.find_change_request_by_branch(&branch).await.expect("lookup").map(|(id, request)| (id, request.status));
                let newest = newest_first
                    .iter()
                    .find(|(_, head, _)| *head == branch)
                    .map(|(number, _, status)| (number.to_string(), status.clone()));
                assert_eq!(found, newest, "lookup of {branch}");
            }

            let merged = provider.list_merged_branch_names(limit).await.expect("merged branches");
            let expected_merged: Vec<String> = newest_first
                .iter()
                .filter(|(_, _, status)| matches!(status, ChangeRequestStatus::Closed | ChangeRequestStatus::Merged))
                .take(limit)
                .filter(|(_, _, status)| *status == ChangeRequestStatus::Merged)
                .map(|(_, head, _)| head.clone())
                .collect();
            assert_eq!(merged, expected_merged, "merged branch names");
            assert_no_refusals(&fake);
        });
    }
}

#[cfg(test)]
#[path = "change_request_testing.rs"]
pub(crate) mod testing;
