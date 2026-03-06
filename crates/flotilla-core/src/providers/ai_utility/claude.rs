use async_trait::async_trait;
use tokio::process::Command;
use tracing::info;

pub struct ClaudeAiUtility {
    claude_bin: String,
}

impl ClaudeAiUtility {
    pub fn new(claude_bin: String) -> Self {
        Self { claude_bin }
    }
}

#[async_trait]
impl super::AiUtility for ClaudeAiUtility {
    fn display_name(&self) -> &str {
        "Claude AI"
    }

    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        info!("ai: generating branch name");
        let prompt = format!(
            "Suggest a short git branch name for this context. \
             Output ONLY the branch name, nothing else. Use kebab-case: {context}"
        );

        let output = Command::new(&self.claude_bin)
            .args(["-p", &prompt])
            .output()
            .await
            .map_err(|e| e.to_string())?;

        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_matches(|c| c == '`' || c == '"' || c == '\'')
                .trim()
                .to_string();
            if branch.is_empty() {
                Err("claude returned empty output".to_string())
            } else {
                info!("ai: suggested '{branch}'");
                Ok(branch)
            }
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }
}
