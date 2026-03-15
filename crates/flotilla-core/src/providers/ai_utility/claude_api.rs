use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use super::Model;
use crate::providers::{http_execute, HttpClient};

static REQUEST_FACTORY: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(reqwest::Client::new);

const API_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const SYSTEM_PROMPT: &str = "You are a concise assistant. Output only what is asked, with no explanation or formatting.";

pub struct ClaudeApiAiUtility {
    api_key: String,
    http: Arc<dyn HttpClient>,
}

impl ClaudeApiAiUtility {
    pub fn new(api_key: String, http: Arc<dyn HttpClient>) -> Self {
        Self { api_key, http }
    }

    /// Run a one-shot prompt against the Anthropic Messages API.
    async fn prompt(&self, model: Model, prompt: &str) -> Result<String, String> {
        let body = serde_json::json!({
            "model": model.api_model_id(),
            "max_tokens": 256,
            "system": SYSTEM_PROMPT,
            "messages": [{ "role": "user", "content": prompt }],
        });

        let request = REQUEST_FACTORY
            .post(format!("{API_BASE}/v1/messages"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .build()
            .map_err(|e| e.to_string())?;

        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        let body_bytes = resp.into_body();
        let body_str = std::str::from_utf8(&body_bytes).map_err(|e| e.to_string())?;

        if status != 200 {
            return Err(format!("Anthropic API error (HTTP {status}): {body_str}"));
        }

        let parsed: MessagesResponse = serde_json::from_str(body_str).map_err(|e| format!("failed to parse API response: {e}"))?;

        parsed
            .content
            .into_iter()
            .map(|ContentBlock::Text { text }| text)
            .next()
            .ok_or_else(|| "API response contained no text".to_string())
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[async_trait]
impl super::AiUtility for ClaudeApiAiUtility {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        info!("ai: generating branch name via API");
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
