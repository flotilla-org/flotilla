pub mod claude;

use crate::providers::types::{CloudAgentSession, RepoCriteria};
use async_trait::async_trait;

#[async_trait]
pub trait CodingAgent: Send + Sync {
    #[allow(dead_code)]
    fn display_name(&self) -> &str;
    fn section_label(&self) -> &str {
        "Sessions"
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
    ) -> Result<Vec<CloudAgentSession>, String>;
    async fn archive_session(&self, session_id: &str) -> Result<(), String>;
    #[allow(dead_code)]
    async fn attach_command(&self, session_id: &str) -> Result<String, String>;
}
