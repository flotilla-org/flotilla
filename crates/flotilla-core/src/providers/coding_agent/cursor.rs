use async_trait::async_trait;
use reqwest;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

use crate::providers::types::*;

/// Guard so the "sessions unavailable" warning is emitted only once per process.
static AUTH_WARNED: AtomicBool = AtomicBool::new(false);

pub struct CursorCodingAgent {
    provider_name: String,
}

impl CursorCodingAgent {
    pub fn new(provider_name: String) -> Self {
        Self { provider_name }
    }

    fn api_key() -> Result<String, String> {
        std::env::var("CURSOR_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "CURSOR_API_KEY is not set".to_string())
    }

    async fn fetch_agents() -> Result<Vec<CursorAgent>, String> {
        let api_key = Self::api_key()?;
        let client = reqwest::Client::new();
        let mut cursor: Option<String> = None;
        let mut all_agents = Vec::new();

        // Cursor API supports pagination; cap pages to avoid runaway loops.
        for _ in 0..20 {
            let mut url = "https://api.cursor.com/v0/agents?limit=100".to_string();
            if let Some(ref c) = cursor {
                url.push_str("&cursor=");
                url.push_str(&urlencoding::encode(c));
            }

            let resp = client
                .get(url)
                .bearer_auth(&api_key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            if status == 401 || status == 403 {
                return Err(format!("authentication error (HTTP {status})"));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("agent list failed (HTTP {status}): {body}"));
            }

            let page: ListAgentsResponse = resp
                .json()
                .await
                .map_err(|e| format!("agent list parse error: {e}"))?;
            all_agents.extend(page.agents);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(all_agents)
    }
}

#[derive(Debug, Deserialize)]
struct ListAgentsResponse {
    #[serde(default)]
    agents: Vec<CursorAgent>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CursorAgent {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default)]
    source: CursorSource,
    #[serde(default)]
    target: CursorTarget,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CursorSource {
    #[serde(default)]
    repository: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CursorTarget {
    #[serde(default, rename = "branchName")]
    branch_name: String,
}

impl CursorAgent {
    fn session_status(&self) -> SessionStatus {
        match self.status.as_str() {
            "RUNNING" => SessionStatus::Running,
            "ARCHIVED" => SessionStatus::Archived,
            _ => SessionStatus::Idle,
        }
    }

    fn branch(&self) -> Option<&str> {
        if self.target.branch_name.is_empty() {
            None
        } else {
            Some(self.target.branch_name.as_str())
        }
    }

    fn repo_slug(&self) -> Option<String> {
        repo_slug_from_cursor_repository(&self.source.repository)
    }
}

fn repo_slug_from_cursor_repository(repository: &str) -> Option<String> {
    let s = repository.trim().trim_end_matches(".git").trim_matches('/');
    if s.is_empty() {
        return None;
    }

    if let Some(rest) = s.strip_prefix("git@") {
        if let Some((_, path)) = rest.split_once(':') {
            if path.contains('/') {
                return Some(path.to_string());
            }
        }
    }

    if let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    {
        if let Some((_, path)) = rest.split_once('/') {
            if path.contains('/') {
                return Some(path.trim_matches('/').to_string());
            }
        }
    }

    // `github.com/owner/repo` -> `owner/repo`
    if let Some((host, path)) = s.split_once('/') {
        if host.contains('.') && path.contains('/') {
            return Some(path.to_string());
        }
    }

    // Already in owner/repo shape.
    if s.contains('/') {
        return Some(s.to_string());
    }

    None
}

#[async_trait]
impl super::CodingAgent for CursorCodingAgent {
    fn display_name(&self) -> &str {
        "Cursor Agents"
    }

    async fn list_sessions(
        &self,
        criteria: &RepoCriteria,
    ) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let agents = match Self::fetch_agents().await {
            Ok(agents) => agents,
            Err(e) if e.contains("CURSOR_API_KEY is not set") || e.contains("authentication") => {
                if !AUTH_WARNED.swap(true, Ordering::Relaxed) {
                    warn!("Cursor sessions unavailable: set CURSOR_API_KEY");
                }
                return Ok(vec![]);
            }
            Err(e) => return Err(e),
        };

        let Some(ref slug) = criteria.repo_slug else {
            return Ok(vec![]);
        };

        let provider_name = &self.provider_name;
        Ok(agents
            .into_iter()
            .filter(|a| a.repo_slug().is_some_and(|r| r == *slug))
            .map(|a| {
                let mut correlation_keys = vec![CorrelationKey::SessionRef(
                    provider_name.clone(),
                    a.id.clone(),
                )];
                if let Some(branch) = a.branch() {
                    correlation_keys.push(CorrelationKey::Branch(branch.to_string()));
                }
                let title = if a.name.is_empty() {
                    a.id.clone()
                } else {
                    a.name.clone()
                };

                (
                    a.id.clone(),
                    CloudAgentSession {
                        title,
                        status: a.session_status(),
                        model: None,
                        updated_at: if a.created_at.is_empty() {
                            None
                        } else {
                            Some(a.created_at)
                        },
                        correlation_keys,
                    },
                )
            })
            .collect())
    }

    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        Err("archiving Cursor sessions is not supported".to_string())
    }

    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("agent --resume {session_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_slug_from_cursor_repository_cases() {
        let cases = [
            ("github.com/owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo", Some("owner/repo")),
            ("owner/repo", Some("owner/repo")),
            ("", None),
            ("repo-only", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                repo_slug_from_cursor_repository(input),
                expected.map(str::to_string)
            );
        }
    }

    #[test]
    fn cursor_agent_maps_status_and_branch() {
        let agent = CursorAgent {
            id: "bc-1".to_string(),
            name: "Session".to_string(),
            status: "RUNNING".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            source: CursorSource {
                repository: "github.com/owner/repo".to_string(),
            },
            target: CursorTarget {
                branch_name: "feature/one".to_string(),
            },
        };
        assert_eq!(agent.session_status(), SessionStatus::Running);
        assert_eq!(agent.branch(), Some("feature/one"));
        assert_eq!(agent.repo_slug(), Some("owner/repo".to_string()));
    }
}
