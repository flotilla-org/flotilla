pub mod claude;
pub mod cursor;

use crate::providers::types::{CloudAgentSession, RepoCriteria};
use async_trait::async_trait;
use std::sync::LazyLock;

/// Shared reqwest client used only as a request factory (building
/// `reqwest::Request` objects). Actual execution goes through the
/// injected `HttpClient` trait, which has its own client. This is
/// necessary because reqwest only exposes `RequestBuilder` via `Client`.
static REQUEST_FACTORY: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

#[async_trait]
pub trait CloudAgentService: Send + Sync {
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str {
        "Cloud Agents"
    }
    fn item_noun(&self) -> &str {
        "session"
    }
    fn abbreviation(&self) -> &str {
        "Ses"
    }
    async fn list_sessions(
        &self,
        criteria: &RepoCriteria,
    ) -> Result<Vec<(String, CloudAgentSession)>, String>;
    async fn archive_session(&self, session_id: &str) -> Result<(), String>;
    #[allow(dead_code)]
    async fn attach_command(&self, session_id: &str) -> Result<String, String>;
}
