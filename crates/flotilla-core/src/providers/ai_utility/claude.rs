use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use tracing::info;

use crate::providers::{run, CommandRunner};

pub struct ClaudeAiUtility {
    claude_bin: String,
    runner: Arc<dyn CommandRunner>,
}

impl ClaudeAiUtility {
    pub fn new(claude_bin: String, runner: Arc<dyn CommandRunner>) -> Self {
        Self { claude_bin, runner }
    }
}

#[async_trait]
impl super::AiUtility for ClaudeAiUtility {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        info!("ai: generating branch name");
        let prompt = format!(
            "Suggest a short git branch name for this context. \
             Output ONLY the branch name, nothing else. Use kebab-case: {context}"
        );

        match run!(self.runner, &self.claude_bin, &["-p", &prompt], Path::new(".")) {
            Ok(output) => {
                let branch = output.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'').trim().to_string();
                if branch.is_empty() {
                    Err("claude returned empty output".to_string())
                } else {
                    info!(%branch, "ai: suggested branch name");
                    Ok(branch)
                }
            }
            Err(e) => Err(e),
        }
    }
}
