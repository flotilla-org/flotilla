use async_trait::async_trait;
use tokio::process::Command;

pub struct ClaudeAiUtility;

impl ClaudeAiUtility {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl super::AiUtility for ClaudeAiUtility {
    fn display_name(&self) -> &str {
        "Claude AI"
    }

    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        let prompt = format!(
            "Suggest a short git branch name for this context. \
             Output ONLY the branch name, nothing else. Use kebab-case: {context}"
        );

        let output = Command::new("claude")
            .args(["-p", &prompt])
            .output()
            .await
            .map_err(|e| e.to_string())?;

        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if branch.is_empty() {
                Err("claude returned empty output".to_string())
            } else {
                Ok(branch)
            }
        } else {
            Err(String::from_utf8_lossy(&output.stderr).to_string())
        }
    }
}
