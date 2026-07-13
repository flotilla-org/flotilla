use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};

use async_trait::async_trait;
use reqwest;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::providers::{http_execute, run, scan_cache::SharedScan, types::*, CommandRunner, HttpClient};

pub struct ClaudeCodingAgent {
    provider_name: String,
    runner: Arc<dyn CommandRunner>,
    http: Arc<dyn HttpClient>,
    sessions: SharedScan<Vec<WebSession>>,
    known_session_ids: Mutex<std::collections::HashSet<String>>,
    auth_warned: AtomicBool,
}

impl ClaudeCodingAgent {
    pub fn new(provider_name: String, runner: Arc<dyn CommandRunner>, http: Arc<dyn HttpClient>) -> Self {
        Self {
            provider_name,
            runner,
            http,
            sessions: SharedScan::new(std::time::Duration::from_secs(SESSIONS_CACHE_TTL_SECS)),
            known_session_ids: Mutex::new(std::collections::HashSet::new()),
            auth_warned: AtomicBool::new(false),
        }
    }

    fn log_session_changes(&self, fetched: &[WebSession]) {
        let mut known_ids = self.known_session_ids.lock().expect("Claude known session IDs lock poisoned");
        let new_ids: std::collections::HashSet<String> = fetched.iter().map(|session| session.id.clone()).collect();
        if !known_ids.is_empty() {
            for session in fetched {
                if !known_ids.contains(&session.id) {
                    info!(provider = "claude", title = %session.title, id = %session.id, "session appeared");
                }
            }
            for old_id in &*known_ids {
                if !new_ids.contains(old_id) {
                    info!(provider = "claude", id = %old_id, "session gone");
                }
            }
        }
        *known_ids = new_ids;
    }
}

// ---------- internal auth types ----------

#[derive(Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OAuthToken,
}

#[derive(Deserialize, Clone)]
struct OAuthToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
}

struct AuthCache {
    token: Option<OAuthToken>,
}

static AUTH_CACHE: LazyLock<Mutex<AuthCache>> = LazyLock::new(|| Mutex::new(AuthCache { token: None }));

// ---------- API deserialization types ----------

#[derive(Deserialize)]
struct SessionsResponse {
    data: Vec<WebSession>,
}

#[derive(Debug, Clone, Deserialize)]
struct WebSession {
    id: String,
    title: String,
    session_status: String,
    #[serde(default)]
    #[allow(dead_code)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    session_context: SessionContext,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SessionContext {
    #[serde(default)]
    model: String,
    #[serde(default)]
    outcomes: Vec<SessionOutcome>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionOutcome {
    #[serde(default)]
    git_info: Option<SessionGitInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionGitInfo {
    #[serde(default)]
    branches: Vec<String>,
    /// "owner/repo" slug (e.g. "changedirection/reticulate")
    #[serde(default)]
    repo: Option<String>,
}

impl WebSession {
    fn branch(&self) -> Option<&str> {
        self.session_context.outcomes.first().and_then(|o| o.git_info.as_ref()).and_then(|gi| gi.branches.first()).map(|s| s.as_str())
    }

    fn repo_slug(&self) -> Option<&str> {
        self.session_context.outcomes.first().and_then(|o| o.git_info.as_ref()).and_then(|gi| gi.repo.as_deref())
    }
}

const SESSIONS_CACHE_TTL_SECS: u64 = 60;
const CLAUDE_API_BASE_URL: &str = "https://api.anthropic.com";

fn sessions_url_for(base_url: &str) -> String {
    format!("{}/v1/sessions", base_url.trim_end_matches('/'))
}

fn session_url_for(base_url: &str, session_id: &str) -> String {
    format!("{}/v1/sessions/{session_id}", base_url.trim_end_matches('/'))
}

// ---------- auth helpers ----------

async fn read_oauth_token_from_keychain(runner: &dyn CommandRunner) -> Result<OAuthToken, String> {
    let output = run!(runner, "security", &["find-generic-password", "-s", "Claude Code-credentials", "-w",], Path::new("."),)
        .map_err(|_| "No Claude Code credentials in keychain".to_string())?;
    let json = output.trim();
    let creds: OAuthCredentials = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(creds.claude_ai_oauth)
}

async fn get_oauth_token(runner: &dyn CommandRunner) -> Result<OAuthToken, String> {
    {
        let cache = AUTH_CACHE.lock().unwrap();
        if let Some(ref token) = cache.token {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
            if token.expires_at > now + 60 {
                return Ok(token.clone());
            }
        }
    }
    // Token missing or expiring soon — re-read from keychain
    let token = read_oauth_token_from_keychain(runner).await?;
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.token = Some(token.clone());
    Ok(token)
}

fn invalidate_auth_cache() {
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.token = None;
}

impl ClaudeCodingAgent {
    fn build_request(
        method: &str,
        url: &str,
        access_token: &str,
        json_body: Option<serde_json::Value>,
    ) -> Result<reqwest::Request, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| format!("invalid HTTP method: {e}"))?;
        let mut builder = super::REQUEST_FACTORY
            .request(method, url)
            .header("authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", "ccr-byoc-2025-07-29")
            .header("anthropic-version", "2023-06-01");
        if let Some(body) = json_body {
            builder = builder.json(&body);
        }
        builder.build().map_err(|e| e.to_string())
    }

    async fn fetch_sessions(&self, base_url: &str) -> Result<Vec<WebSession>, String> {
        match self.fetch_sessions_inner(base_url).await {
            Ok(sessions) => Ok(sessions),
            Err(e) if e.contains("authentication") || e.contains("missing field `data`") => {
                debug!(provider = "claude", err = %e, "session fetch failed, clearing auth cache and retrying");
                invalidate_auth_cache();
                match self.fetch_sessions_inner(base_url).await {
                    Ok(sessions) => Ok(sessions),
                    Err(e) if e.contains("authentication") => {
                        if !self.auth_warned.swap(true, Ordering::Relaxed) {
                            warn!(provider = "claude", "Claude sessions unavailable: insufficient OAuth scopes");
                        }
                        debug!(provider = "claude", err = %e, "Claude auth error detail");
                        Ok(vec![])
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_sessions_inner(&self, base_url: &str) -> Result<Vec<WebSession>, String> {
        let token = get_oauth_token(&*self.runner).await?;
        let url = sessions_url_for(base_url);
        let request = Self::build_request("GET", &url, &token.access_token, None)?;
        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        let body = std::str::from_utf8(resp.body()).map_err(|e| e.to_string())?;

        // Both 401 and 403 are treated as auth errors so the caller's retry
        // logic (which matches on "authentication") can invalidate the cached
        // token and try again with fresh credentials.
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !(200..300).contains(&status) {
            return Err(format!("session fetch failed (HTTP {status}): {body}"));
        }

        let parsed: SessionsResponse = serde_json::from_str(body).map_err(|e| format!("session parse error: {e}"))?;

        let mut sessions: Vec<WebSession> = parsed.data.into_iter().collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    async fn archive_session_inner(&self, session_id: &str, base_url: &str) -> Result<(), String> {
        info!(provider = "claude", %session_id, "archiving session");
        let token = get_oauth_token(&*self.runner).await?;
        let url = session_url_for(base_url, session_id);
        let request = Self::build_request("PATCH", &url, &token.access_token, Some(serde_json::json!({"session_status": "archived"})))?;
        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let body = std::str::from_utf8(resp.body()).unwrap_or("<binary>");
            Err(format!("archive session failed (HTTP {status}): {body}"))
        }
    }
}

// ---------- trait implementation ----------

#[async_trait]
impl super::CloudAgentService for ClaudeCodingAgent {
    async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let sessions = self
            .sessions
            .get_or_scan(|| async {
                let fetched = self.fetch_sessions(CLAUDE_API_BASE_URL).await?;
                debug!(provider = "claude", count = fetched.len(), "Claude sessions: fetched from API");
                self.log_session_changes(&fetched);
                Ok(fetched)
            })
            .await?;

        // No remote slug means no cloud sessions can match this repo
        let Some(ref slug) = criteria.repo_slug else {
            return Ok(vec![]);
        };

        // Sessions with no repo info still match (backward compat with older sessions)
        let filtered: Vec<WebSession> = sessions.into_iter().filter(|s| s.repo_slug().is_none_or(|r| r == slug)).collect();

        let provider_name = &self.provider_name;
        Ok(filtered
            .into_iter()
            .map(|s| {
                let status = match s.session_status.as_str() {
                    "running" => SessionStatus::Running,
                    "archived" => SessionStatus::Archived,
                    _ => SessionStatus::Idle,
                };

                let model = if s.session_context.model.is_empty() { None } else { Some(s.session_context.model.clone()) };

                let id = s.id.clone();
                let mut correlation_keys = vec![CorrelationKey::SessionRef(provider_name.clone(), id.clone())];

                // Add branch correlation key if available
                if let Some(branch) = s.branch() {
                    let clean = branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string();
                    correlation_keys.push(CorrelationKey::Branch(clean));
                }

                (id, CloudAgentSession {
                    title: s.title,
                    status,
                    model,
                    updated_at: Some(s.updated_at.clone()),
                    correlation_keys,
                    provider_name: provider_name.clone(),
                    provider_display_name: "Claude".into(),
                    item_noun: "Agent".into(),
                })
            })
            .collect())
    }

    async fn archive_session(&self, session_id: &str) -> Result<(), String> {
        let result = self.archive_session_inner(session_id, CLAUDE_API_BASE_URL).await;
        if result.is_ok() {
            self.sessions.invalidate();
        }
        result
    }

    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("claude --teleport {session_id}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::providers::{coding_agent::CloudAgentService, replay, testing::MockRunner};

    static TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("coding_agent", name)
    }

    fn now_epoch_secs() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn token_json(access_token: &str, expires_at: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "expiresAt": expires_at
            }
        })
        .to_string()
    }

    fn reset_auth_state() {
        invalidate_auth_cache();
    }

    fn mock_runner(responses: Vec<Result<String, String>>) -> Arc<dyn CommandRunner> {
        Arc::new(MockRunner::new(responses))
    }

    fn make_agent(runner: Arc<dyn CommandRunner>, http: Arc<dyn crate::providers::HttpClient>) -> ClaudeCodingAgent {
        ClaudeCodingAgent::new("claude".into(), runner, http)
    }

    struct CountingSessionsHttp {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl crate::providers::HttpClient for CountingSessionsHttp {
        async fn execute(
            &self,
            _request: reqwest::Request,
            _label: &crate::providers::ChannelLabel,
        ) -> Result<http::Response<bytes::Bytes>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            Ok(http::Response::builder()
                .status(200)
                .body(bytes::Bytes::from_static(
                    br#"{"data":[{"id":"one","title":"One","session_status":"running","updated_at":"2026-03-01T00:00:00Z","session_context":{"outcomes":[]}}]}"#,
                ))
                .expect("session response"))
        }
    }

    #[tokio::test]
    async fn concurrent_repo_projections_share_one_account_wide_session_fetch() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();
        let runner = mock_runner(vec![Ok(token_json("shared", now_epoch_secs() + 3600))]);
        let http = Arc::new(CountingSessionsHttp { calls: AtomicUsize::new(0) });
        let agent = make_agent(runner, http.clone());

        let repo_a = RepoCriteria { repo_slug: Some("owner/a".into()) };
        let repo_b = RepoCriteria { repo_slug: Some("owner/b".into()) };
        let (first, second) = tokio::join!(agent.list_sessions(&repo_a), agent.list_sessions(&repo_b));

        first.expect("first repo projection");
        second.expect("second repo projection");
        assert_eq!(http.calls.load(Ordering::SeqCst), 1, "account-wide fetch should be shared before filtering by repo");
    }

    #[tokio::test]
    async fn oauth_token_is_cached_until_near_expiry() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();
        let runner = MockRunner::new(vec![Ok(token_json("token-1", now_epoch_secs() + 3600))]);
        let token1 = get_oauth_token(&runner).await.expect("first token");
        let token2 = get_oauth_token(&runner).await.expect("cached token");
        assert_eq!(token1.access_token, "token-1");
        assert_eq!(token2.access_token, "token-1");
    }

    #[tokio::test]
    async fn oauth_token_refreshes_when_expiring_soon() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();
        let runner =
            MockRunner::new(vec![Ok(token_json("old-token", now_epoch_secs() + 10)), Ok(token_json("new-token", now_epoch_secs() + 3600))]);
        let first = get_oauth_token(&runner).await.expect("first token");
        let second = get_oauth_token(&runner).await.expect("refreshed token");
        assert_eq!(first.access_token, "old-token");
        assert_eq!(second.access_token, "new-token");
    }

    #[tokio::test]
    async fn fetch_sessions_inner_includes_archived_and_sorts() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![Ok(token_json("abc123", now_epoch_secs() + 3600))]);
        let session = replay::test_session(&fixture("claude_sessions.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions_inner("https://api.test").await.expect("fetch sessions");
        session.finish();

        assert_eq!(sessions.len(), 3, "all sessions including archived should be returned");
        assert_eq!(sessions[0].id, "skip", "archived session with newest timestamp first");
        assert_eq!(sessions[1].id, "new");
        assert_eq!(sessions[2].id, "old");
    }

    #[tokio::test]
    async fn fetch_sessions_retries_after_auth_error() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner =
            mock_runner(vec![Ok(token_json("expired", now_epoch_secs() + 3600)), Ok(token_json("fresh", now_epoch_secs() + 3600))]);
        let session = replay::test_session(&fixture("claude_auth_retry.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions("https://api.test").await.expect("retry should succeed");
        session.finish();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }

    #[tokio::test]
    async fn fetch_sessions_returns_empty_after_second_auth_error() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![Ok(token_json("bad-1", now_epoch_secs() + 3600)), Ok(token_json("bad-2", now_epoch_secs() + 3600))]);
        let session = replay::test_session(&fixture("claude_auth_failure.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions("https://api.test").await.expect("auth failures should degrade gracefully");
        session.finish();

        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_uses_cache_and_maps_fields() {
        // This test pre-populates the cache, so no HTTP calls are made.
        // Use an empty replay session to make that explicit.
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let empty_fixture = dir.path().join("empty.yaml");
        std::fs::write(&empty_fixture, "interactions: []\n").unwrap();
        let session = replay::test_session(empty_fixture.to_str().unwrap(), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);
        agent.sessions.seed(vec![
            WebSession {
                id: "one".into(),
                title: "One".into(),
                session_status: "running".into(),
                created_at: String::new(),
                updated_at: "2026-03-05T00:00:00Z".into(),
                session_context: SessionContext {
                    model: "sonnet".into(),
                    outcomes: vec![SessionOutcome {
                        git_info: Some(SessionGitInfo { branches: vec!["refs/heads/feat-a".into()], repo: Some("owner/repo".into()) }),
                    }],
                },
            },
            WebSession {
                id: "two".into(),
                title: "Two".into(),
                session_status: "something-else".into(),
                created_at: String::new(),
                updated_at: "2026-03-04T00:00:00Z".into(),
                session_context: SessionContext { model: String::new(), outcomes: vec![SessionOutcome { git_info: None }] },
            },
            WebSession {
                id: "skip".into(),
                title: "Skip".into(),
                session_status: "running".into(),
                created_at: String::new(),
                updated_at: "2026-03-03T00:00:00Z".into(),
                session_context: SessionContext {
                    model: "opus".into(),
                    outcomes: vec![SessionOutcome {
                        git_info: Some(SessionGitInfo { branches: vec!["refs/heads/feat-b".into()], repo: Some("other/repo".into()) }),
                    }],
                },
            },
        ]);

        let sessions = agent.list_sessions(&RepoCriteria { repo_slug: Some("owner/repo".into()) }).await.expect("list sessions");

        assert_eq!(sessions.len(), 2);
        let one = sessions.iter().find(|(id, _)| id == "one").expect("one session");
        assert_eq!(one.1.status, SessionStatus::Running);
        assert_eq!(one.1.model.as_deref(), Some("sonnet"));
        assert!(one.1.correlation_keys.contains(&CorrelationKey::Branch("feat-a".into())));

        let two = sessions.iter().find(|(id, _)| id == "two").expect("two session");
        assert_eq!(two.1.status, SessionStatus::Idle);
        assert!(two.1.model.is_none());
    }

    #[tokio::test]
    async fn archive_session_succeeds() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![Ok(token_json("archive-token", now_epoch_secs() + 3600))]);
        let session = replay::test_session(&fixture("claude_archive_ok.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        agent.archive_session_inner("s-ok", "https://api.test").await.expect("archive should succeed");
        session.finish();
    }

    #[tokio::test]
    async fn archive_session_returns_error_on_failure() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![Ok(token_json("archive-token", now_epoch_secs() + 3600))]);
        let session = replay::test_session(&fixture("claude_archive_fail.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let err = agent.archive_session_inner("s-fail", "https://api.test").await.expect_err("archive should fail");
        session.finish();

        assert!(err.contains("HTTP 500"));
        assert!(err.contains("boom"));
    }

    #[tokio::test]
    async fn attach_command_formats_teleport_command() {
        // No TEST_LOCK needed: this test is pure string formatting,
        // it doesn't touch the global AUTH_CACHE.
        let runner = mock_runner(vec![]);
        let dir = tempfile::tempdir().unwrap();
        let empty_fixture = dir.path().join("empty.yaml");
        std::fs::write(&empty_fixture, "interactions: []\n").unwrap();
        let session = replay::test_session(empty_fixture.to_str().unwrap(), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);
        let cmd = agent.attach_command("abc123").await.expect("attach command");
        assert_eq!(cmd, "claude --teleport abc123");
    }
}
