use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};

use crate::providers::types::*;

pub struct ClaudeCodingAgent {
    provider_name: String,
}

impl ClaudeCodingAgent {
    pub fn new(provider_name: String) -> Self {
        Self { provider_name }
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
    org_uuid: Option<String>,
}

static AUTH_CACHE: LazyLock<Mutex<AuthCache>> = LazyLock::new(|| {
    Mutex::new(AuthCache {
        token: None,
        org_uuid: None,
    })
});

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
}

// ---------- auth helpers ----------

async fn read_oauth_token_from_keychain() -> Result<OAuthToken, String> {
    let output = tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
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
    cache.org_uuid = None; // new token might mean different user
    Ok(token)
}

async fn get_org_uuid(token: &str) -> Result<String, String> {
    {
        let cache = AUTH_CACHE.lock().unwrap();
        if let Some(ref uuid) = cache.org_uuid {
            return Ok(uuid.clone());
        }
    }
    let uuid = read_org_uuid(token).await?;
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.org_uuid = Some(uuid.clone());
    Ok(uuid)
}

async fn read_org_uuid(token: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "anthropic-version: 2023-06-01",
            "https://api.anthropic.com/api/oauth/profile",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v.get("organization")
        .and_then(|o| o.get("uuid"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No organization.uuid in profile".to_string())
}

// ---------- trait implementation ----------

#[async_trait]
impl super::CodingAgent for ClaudeCodingAgent {
    fn display_name(&self) -> &str {
        "Claude Sessions"
    }

    async fn list_sessions(&self) -> Result<Vec<CloudAgentSession>, String> {
        let token = get_oauth_token().await?;
        let org_uuid = get_org_uuid(&token.access_token).await?;
        let access_token = token.access_token;

        let output = tokio::process::Command::new("curl")
            .args([
                "-s",
                "-H", &format!("Authorization: Bearer {access_token}"),
                "-H", "anthropic-beta: ccr-byoc-2025-07-29",
                "-H", "anthropic-version: 2023-06-01",
                "-H", &format!("x-organization-uuid: {org_uuid}"),
                "https://api.anthropic.com/v1/sessions",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;

        let body = String::from_utf8_lossy(&output.stdout).to_string();
        let resp: SessionsResponse =
            serde_json::from_str(&body).map_err(|e| e.to_string())?;

        // Filter to non-archived sessions, sorted by updated_at descending
        let mut sessions: Vec<WebSession> = resp
            .data
            .into_iter()
            .filter(|s| s.session_status != "archived")
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(sessions
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
                    self.provider_name.clone(),
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
        let token = get_oauth_token().await?;
        let org_uuid = get_org_uuid(&token.access_token).await?;
        let access_token = token.access_token;

        let url = format!("https://api.anthropic.com/v1/sessions/{session_id}");
        let output = tokio::process::Command::new("curl")
            .args([
                "-s",
                "-w", "\n%{http_code}",
                "-X", "PATCH",
                "-H", &format!("Authorization: Bearer {access_token}"),
                "-H", "anthropic-beta: ccr-byoc-2025-07-29",
                "-H", "anthropic-version: 2023-06-01",
                "-H", &format!("x-organization-uuid: {org_uuid}"),
                "-H", "content-type: application/json",
                "-d", r#"{"session_status":"archived"}"#,
                &url,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())?;

        let body = String::from_utf8_lossy(&output.stdout).to_string();
        let status_code: u16 = body
            .lines()
            .last()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        if (200..300).contains(&status_code) {
            Ok(())
        } else {
            Err(format!(
                "archive session failed (HTTP {}): {}",
                status_code,
                body.lines().next().unwrap_or("")
            ))
        }
    }

    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("claude --teleport {session_id}"))
    }
}
