use async_trait::async_trait;
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::warn;

use crate::providers::types::*;
use crate::providers::{http_execute, HttpClient};

pub struct CursorCodingAgent {
    provider_name: String,
    http: Arc<dyn HttpClient>,
    auth_warned: AtomicBool,
}

impl CursorCodingAgent {
    pub fn new(provider_name: String, http: Arc<dyn HttpClient>) -> Self {
        Self {
            provider_name,
            http,
            auth_warned: AtomicBool::new(false),
        }
    }

    fn api_key() -> Result<String, String> {
        std::env::var("CURSOR_API_KEY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "CURSOR_API_KEY is not set".to_string())
    }

    async fn fetch_agents(&self) -> Result<Vec<CursorAgent>, String> {
        let api_key = Self::api_key()?;
        let mut cursor: Option<String> = None;
        let mut all_agents = Vec::new();

        // Cursor API supports pagination; cap pages to avoid runaway loops.
        for _ in 0..20 {
            let mut url = "https://api.cursor.com/v0/agents?limit=100".to_string();
            if let Some(ref c) = cursor {
                url.push_str("&cursor=");
                url.push_str(&urlencoding::encode(c));
            }

            let request = super::REQUEST_FACTORY
                .get(&url)
                .basic_auth(&api_key, None::<&str>)
                .build()
                .map_err(|e| format!("request build error: {e}"))?;
            let resp = http_execute!(self.http, request)?;
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                return Err(format!("authentication error (HTTP {status})"));
            }
            if !resp.status().is_success() {
                let body = String::from_utf8_lossy(resp.body()).to_string();
                return Err(format!("agent list failed (HTTP {status}): {body}"));
            }

            let page: ListAgentsResponse = serde_json::from_slice(resp.body())
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
            "CREATING" | "RUNNING" => SessionStatus::Running,
            "FINISHED" | "STOPPED" | "FAILED" | "EXPIRED" => SessionStatus::Idle,
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
impl super::CloudAgentService for CursorCodingAgent {
    fn display_name(&self) -> &str {
        "Cursor Cloud Agents"
    }

    async fn list_sessions(
        &self,
        criteria: &RepoCriteria,
    ) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let agents = match self.fetch_agents().await {
            Ok(agents) => agents,
            Err(e) if e.contains("CURSOR_API_KEY is not set") || e.contains("authentication") => {
                if !self.auth_warned.swap(true, Ordering::Relaxed) {
                    warn!(
                        provider = "cursor",
                        "Cursor sessions unavailable: set CURSOR_API_KEY"
                    );
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
                        // Cursor API has no updatedAt; createdAt is the best proxy.
                        updated_at: if a.created_at.is_empty() {
                            None
                        } else {
                            Some(a.created_at)
                        },
                        correlation_keys,
                        provider_name: provider_name.clone(),
                        provider_display_name: "Cursor".into(),
                        item_noun: "Agent".into(),
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
    use crate::providers::coding_agent::CloudAgentService;

    fn cursor_agent(status: &str, repository: &str, branch: &str) -> CursorAgent {
        CursorAgent {
            id: "bc-1".to_string(),
            name: "Session".to_string(),
            status: status.to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            source: CursorSource {
                repository: repository.to_string(),
            },
            target: CursorTarget {
                branch_name: branch.to_string(),
            },
        }
    }

    #[test]
    fn repo_slug_from_cursor_repository_cases() {
        let cases = [
            ("github.com/owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo.git", Some("owner/repo")),
            ("git@github.com:owner/repo", Some("owner/repo")),
            ("owner/repo", Some("owner/repo")),
            ("http://github.com/owner/repo", Some("owner/repo")),
            ("https://github.com/owner/repo/", Some("owner/repo")),
            ("  owner/repo  ", Some("owner/repo")),
            ("", None),
            ("repo-only", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                repo_slug_from_cursor_repository(input),
                expected.map(str::to_string),
                "{input}"
            );
        }
    }

    #[test]
    fn cursor_agent_maps_status_branch_and_repo() {
        let status_cases = [
            ("CREATING", SessionStatus::Running),
            ("RUNNING", SessionStatus::Running),
            ("FINISHED", SessionStatus::Idle),
            ("STOPPED", SessionStatus::Idle),
            ("FAILED", SessionStatus::Idle),
            ("EXPIRED", SessionStatus::Idle),
            ("UNKNOWN_STATUS", SessionStatus::Idle),
        ];
        for (status, expected) in status_cases {
            assert_eq!(
                cursor_agent(status, "", "").session_status(),
                expected,
                "{status}"
            );
        }

        let agent = cursor_agent("RUNNING", "github.com/owner/repo", "feature/one");
        assert_eq!(agent.branch(), Some("feature/one"));
        assert_eq!(agent.repo_slug(), Some("owner/repo".to_string()));
    }

    #[test]
    fn cursor_agent_empty_branch_returns_none() {
        let agent = cursor_agent("RUNNING", "", "");
        assert!(agent.branch().is_none());
    }

    #[test]
    fn cursor_agent_repo_slug_delegates() {
        let agent = cursor_agent("FINISHED", "https://github.com/owner/repo.git", "");
        assert_eq!(agent.repo_slug(), Some("owner/repo".to_string()));
    }

    #[test]
    fn cursor_agent_empty_repo_returns_none() {
        let agent = cursor_agent("RUNNING", "", "");
        assert!(agent.repo_slug().is_none());
    }

    struct MockHttpClient;

    #[async_trait::async_trait]
    impl crate::providers::HttpClient for MockHttpClient {
        async fn execute(
            &self,
            _request: reqwest::Request,
            _label: &crate::providers::ChannelLabel,
        ) -> Result<http::Response<bytes::Bytes>, String> {
            Err("mock: not called".into())
        }
    }

    fn cursor_service() -> CursorCodingAgent {
        let http: Arc<dyn crate::providers::HttpClient> = Arc::new(MockHttpClient);
        CursorCodingAgent::new("cursor".into(), http)
    }

    #[tokio::test]
    async fn archive_session_returns_error() {
        let agent = cursor_service();
        let result = agent.archive_session("any-id").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not supported"));
    }

    #[tokio::test]
    async fn attach_command_formats_resume() {
        let agent = cursor_service();
        let cmd = agent.attach_command("sess-42").await.unwrap();
        assert_eq!(cmd, "agent --resume sess-42");
    }

    #[tokio::test]
    async fn display_name_returns_expected() {
        let agent = cursor_service();
        assert_eq!(agent.display_name(), "Cursor Cloud Agents");
    }
}
