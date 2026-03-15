use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use tracing::info;

use super::Model;
use crate::providers::{run, CommandRunner};

pub struct ClaudeCliAiUtility {
    claude_bin: String,
    runner: Arc<dyn CommandRunner>,
}

impl ClaudeCliAiUtility {
    pub fn new(claude_bin: String, runner: Arc<dyn CommandRunner>) -> Self {
        Self { claude_bin, runner }
    }

    /// Run a one-shot prompt against Claude CLI with minimal overhead.
    ///
    /// Tools, skills, and session persistence are all disabled so the only
    /// tokens consumed are the system prompt + user prompt + response.
    async fn prompt(&self, model: Model, prompt: &str) -> Result<String, String> {
        let model_str = model.cli_alias();
        let args: Vec<&str> = vec![
            "--model",
            model_str,
            "--system-prompt",
            "You are a concise assistant. Output only what is asked, with no explanation or formatting.",
            "--tools",
            "",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--effort",
            "low",
            "-p",
            prompt,
        ];

        run!(self.runner, &self.claude_bin, args.as_slice(), Path::new("."))
    }
}

#[async_trait]
impl super::AiUtility for ClaudeCliAiUtility {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        info!("ai: generating branch name via CLI");
        let prompt = format!(
            "Suggest a short git branch name for this context. \
             Output ONLY the branch name, nothing else. Use kebab-case: {context}"
        );

        let output = self.prompt(Model::Haiku, &prompt).await?;
        let branch = output.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'').trim().to_string();
        if branch.is_empty() {
            Err("claude returned empty output".to_string())
        } else {
            info!(%branch, "ai: suggested branch name");
            Ok(branch)
        }
    }
}
