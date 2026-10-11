//! Cloud session providers and their discovery contracts.
pub mod detectors;
pub mod factories;

pub mod claude;
pub mod codex;
pub mod cursor;

use std::sync::LazyLock;

use async_trait::async_trait;

use flotilla_protocol::CloudAgentSession;

/// Shared reqwest client used only as a request factory (building
/// `reqwest::Request` objects). Actual execution goes through the
/// injected `HttpClient` trait, which has its own client. This is
/// necessary because reqwest only exposes `RequestBuilder` via `Client`.
static REQUEST_FACTORY: LazyLock<reqwest::Client> = LazyLock::new(flotilla_tls::client);

#[async_trait]
pub trait CloudAgentService: Send + Sync {
    async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String>;
    async fn archive_session(&self, session_id: &str) -> Result<(), String>;
    #[allow(dead_code)]
    async fn attach_command(&self, session_id: &str) -> Result<String, String>;
}

/// Criteria passed to coding agents so they can filter results to a specific repo.
#[derive(Debug, Clone, Default)]
pub struct RepoCriteria {
    /// "owner/repo" from git remote (e.g. "changedirection/reticulate")
    pub repo_slug: Option<String>,
}

#[cfg(test)]
mod testing;
