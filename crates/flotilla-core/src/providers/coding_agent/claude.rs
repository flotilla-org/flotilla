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
    /// "owner/repo" slug (e.g. "changedirection/reticulate")
    #[serde(default)]
    repo: Option<String>,
}

impl WebSession {
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

                (id, CloudAgentSession {
                    title: s.title,
                    status,
                    model,
                    updated_at: Some(s.updated_at.clone()),
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
