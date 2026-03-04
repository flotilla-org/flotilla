pub mod claude;

use async_trait::async_trait;
use crate::providers::types::CloudAgentSession;

#[async_trait]
pub trait CodingAgent: Send + Sync {
    fn display_name(&self) -> &str;
    async fn list_sessions(&self) -> Result<Vec<CloudAgentSession>, String>;
    async fn archive_session(&self, session_id: &str) -> Result<(), String>;
    async fn attach_command(&self, session_id: &str) -> Result<String, String>;
}
