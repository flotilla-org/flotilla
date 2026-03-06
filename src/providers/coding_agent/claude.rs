use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use reqwest;

use tracing::{info, debug, warn};
use crate::providers::types::*;

pub struct ClaudeCodingAgent {
    provider_name: String,
    sessions_cache: Mutex<SessionsCache>,
}

impl ClaudeCodingAgent {
    pub fn new(provider_name: String) -> Self {
        Self {
            provider_name,
            sessions_cache: Mutex::new(SessionsCache {
                sessions: Vec::new(),
                fetched_at: None,
                known_ids: std::collections::HashSet::new(),
            }),
        }
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

static AUTH_CACHE: LazyLock<Mutex<AuthCache>> = LazyLock::new(|| {
    Mutex::new(AuthCache {
        token: None,
    })
});

/// Guard so the "sessions unavailable" warning is emitted only once per process.
static AUTH_WARNED: AtomicBool = AtomicBool::new(false);

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
        self.session_context
            .outcomes
            .first()
            .and_then(|o| o.git_info.as_ref())
            .and_then(|gi| gi.branches.first())
            .map(|s| s.as_str())
    }

    fn repo_slug(&self) -> Option<&str> {
        self.session_context
            .outcomes
            .first()
            .and_then(|o| o.git_info.as_ref())
            .and_then(|gi| gi.repo.as_deref())
    }
}

// ---------- sessions cache ----------

struct SessionsCache {
    sessions: Vec<WebSession>,
    fetched_at: Option<Instant>,
    known_ids: std::collections::HashSet<String>,
}

const SESSIONS_CACHE_TTL_SECS: u64 = 30;

// ---------- auth helpers ----------

async fn read_oauth_token_from_keychain() -> Result<OAuthToken, String> {
    let output = tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .stdin(Stdio::null())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("No Claude Code credentials in keychain".to_string());
    }

    let json = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let creds: OAuthCredentials = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    Ok(creds.claude_ai_oauth)
}

async fn get_oauth_token() -> Result<OAuthToken, String> {
    {
        let cache = AUTH_CACHE.lock().unwrap();
        if let Some(ref token) = cache.token {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            if token.expires_at > now + 60 {
                return Ok(token.clone());
            }
        }
    }
    // Token missing or expiring soon — re-read from keychain
    let token = read_oauth_token_from_keychain().await?;
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.token = Some(token.clone());
    Ok(token)
}

fn invalidate_auth_cache() {
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.token = None;
}

impl ClaudeCodingAgent {
    /// Fetch all non-archived sessions from the API, sorted by updated_at descending.
    /// Returns empty list on auth errors (insufficient scopes, expired token) to
    /// degrade gracefully instead of spamming errors on every refresh cycle.
    async fn fetch_sessions() -> Result<Vec<WebSession>, String> {
        match Self::fetch_sessions_inner().await {
            Ok(sessions) => Ok(sessions),
            Err(e) if e.contains("authentication") || e.contains("missing field `data`") => {
                debug!("session fetch failed, clearing auth cache and retrying: {e}");
                invalidate_auth_cache();
                match Self::fetch_sessions_inner().await {
                    Ok(sessions) => Ok(sessions),
                    Err(e) if e.contains("authentication") => {
                        if !AUTH_WARNED.swap(true, Ordering::Relaxed) {
                            warn!("Claude sessions unavailable: insufficient OAuth scopes");
                        }
                        Ok(vec![])
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Fetch sessions from the API. The x-organization-uuid header was removed because
    /// the OAuth token's scopes no longer include user:profile (needed to fetch the org
    /// UUID from /api/oauth/profile), and the sessions API works without it.
    async fn fetch_sessions_inner() -> Result<Vec<WebSession>, String> {
        let token = get_oauth_token().await?;
        let access_token = token.access_token;

        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.anthropic.com/v1/sessions")
            .bearer_auth(&access_token)
            .header("anthropic-beta", "ccr-byoc-2025-07-29")
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("session fetch failed (HTTP {status}): {body}"));
        }

        let resp: SessionsResponse = resp
            .json()
            .await
            .map_err(|e| format!("session parse error: {e}"))?;

        let mut sessions: Vec<WebSession> = resp
            .data
            .into_iter()
            .filter(|s| s.session_status != "archived")
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }
}

// ---------- trait implementation ----------

#[async_trait]
impl super::CodingAgent for ClaudeCodingAgent {
    fn display_name(&self) -> &str {
        "Claude Sessions"
    }

    async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<CloudAgentSession>, String> {
        // Check instance cache
        let cached = {
            let cache = self.sessions_cache.lock().unwrap();
            if let Some(fetched_at) = cache.fetched_at {
                if fetched_at.elapsed().as_secs() < SESSIONS_CACHE_TTL_SECS {
                    Some(cache.sessions.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };

        let sessions = if let Some(sessions) = cached {
            debug!("Claude sessions: cache hit");
            sessions
        } else {
            let fetched = Self::fetch_sessions().await?;
            debug!("Claude sessions: fetched {} from API", fetched.len());

            // Diff against known IDs and log additions/removals at INFO
            let mut cache = self.sessions_cache.lock().unwrap();
            let new_ids: std::collections::HashSet<String> =
                fetched.iter().map(|s| s.id.clone()).collect();
            if !cache.known_ids.is_empty() {
                for s in &fetched {
                    if !cache.known_ids.contains(&s.id) {
                        info!("session appeared: {} ({})", s.title, s.id);
                    }
                }
                for old_id in &cache.known_ids {
                    if !new_ids.contains(old_id) {
                        info!("session gone: {}", old_id);
                    }
                }
            }
            cache.known_ids = new_ids;
            cache.sessions = fetched.clone();
            cache.fetched_at = Some(Instant::now());
            fetched
        };

        // No remote slug means no cloud sessions can match this repo
        let Some(ref slug) = criteria.repo_slug else {
            return Ok(vec![]);
        };

        // Sessions with no repo info still match (backward compat with older sessions)
        let filtered: Vec<WebSession> = sessions
            .into_iter()
            .filter(|s| s.repo_slug().is_none_or(|r| r == slug))
            .collect();

        let provider_name = &self.provider_name;
        Ok(filtered
            .into_iter()
            .map(|s| {
                let status = match s.session_status.as_str() {
                    "running" => SessionStatus::Running,
                    "archived" => SessionStatus::Archived,
                    _ => SessionStatus::Idle,
                };

                let model = if s.session_context.model.is_empty() {
                    None
                } else {
                    Some(s.session_context.model.clone())
                };

                let mut correlation_keys = vec![CorrelationKey::SessionRef(
                    provider_name.clone(),
                    s.id.clone(),
                )];

                // Add branch correlation key if available
                if let Some(branch) = s.branch() {
                    let clean = branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string();
                    correlation_keys.push(CorrelationKey::Branch(clean));
                }

                CloudAgentSession {
                    id: s.id,
                    title: s.title,
                    status,
                    model,
                    updated_at: Some(s.updated_at.clone()),
                    correlation_keys,
                }
            })
            .collect())
    }

    async fn archive_session(&self, session_id: &str) -> Result<(), String> {
        info!("archiving session {session_id}");
        let token = get_oauth_token().await?;
        let access_token = token.access_token;

        let url = format!("https://api.anthropic.com/v1/sessions/{session_id}");
        let client = reqwest::Client::new();
        let resp = client
            .patch(&url)
            .bearer_auth(&access_token)
            .header("anthropic-beta", "ccr-byoc-2025-07-29")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({"session_status": "archived"}))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!("archive session failed (HTTP {status}): {body}"))
        }
    }

    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("claude --teleport {session_id}"))
    }
}
