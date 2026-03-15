pub mod claude_api;
pub mod claude_cli;

use async_trait::async_trait;

/// Which Claude model to use for a request.
#[derive(Debug, Clone, Copy)]
pub enum Model {
    Haiku,
    Sonnet,
    Opus,
}

impl Model {
    /// Model alias accepted by the Claude CLI `--model` flag.
    pub fn cli_alias(&self) -> &'static str {
        match self {
            Model::Haiku => "haiku",
            Model::Sonnet => "sonnet",
            Model::Opus => "opus",
        }
    }

    /// Full model ID for the Anthropic Messages API.
    pub fn api_model_id(&self) -> &'static str {
        match self {
            Model::Haiku => "claude-haiku-4-5-20251001",
            Model::Sonnet => "claude-sonnet-4-6-20250610",
            Model::Opus => "claude-opus-4-6-20250610",
        }
    }
}

#[async_trait]
pub trait AiUtility: Send + Sync {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String>;
}
