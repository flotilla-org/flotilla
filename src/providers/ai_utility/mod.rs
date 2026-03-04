pub mod claude;

use async_trait::async_trait;

#[async_trait]
pub trait AiUtility: Send + Sync {
    fn display_name(&self) -> &str;
    async fn generate_branch_name(&self, context: &str) -> Result<String, String>;
}
