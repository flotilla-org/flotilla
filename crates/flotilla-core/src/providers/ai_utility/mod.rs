pub mod claude_api;
pub mod claude_cli;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyNames {
    pub name: String,
    pub branch: String,
}

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

    /// Generate the coupled resource and branch names in one model request.
    async fn generate_convoy_names(&self, context: &str) -> Result<ConvoyNames, String> {
        let branch = self.generate_branch_name(context).await?;
        Ok(ConvoyNames { name: branch.replace('/', "-"), branch })
    }
}

pub(super) fn parse_convoy_names(output: &str) -> Result<ConvoyNames, String> {
    let output = output.trim().trim_matches('`').trim();
    let output = output.strip_prefix("json").map(str::trim).unwrap_or(output);
    let names: ConvoyNames = serde_json::from_str(output).map_err(|error| format!("invalid convoy names response: {error}"))?;
    if names.name.trim().is_empty() || names.branch.trim().is_empty() {
        Err("claude returned an empty convoy or branch name".to_string())
    } else {
        Ok(names)
    }
}
