pub mod claude;

use async_trait::async_trait;

#[async_trait]
pub trait AiUtility: Send + Sync {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String>;
}
