use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use super::Model;
use crate::providers::{http_execute, HttpClient};

static REQUEST_FACTORY: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(crate::tls::client);

const API_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const SYSTEM_PROMPT: &str = "You are a concise assistant. Output only what is asked, with no explanation or formatting.";
const ANTHROPIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

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

        let resp = tokio::time::timeout(ANTHROPIC_REQUEST_TIMEOUT, async { http_execute!(self.http, request) })
            .await
            .map_err(|_| format!("Anthropic API request timed out after {}s", ANTHROPIC_REQUEST_TIMEOUT.as_secs()))??;
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

    async fn generate_convoy_names(&self, context: &str) -> Result<super::ConvoyNames, String> {
        info!("ai: generating convoy and branch names via API");
        let prompt = format!(
            "Suggest a coherent short convoy resource name and git branch name for this context. \
             Return ONLY JSON with string fields name and branch. Use lowercase kebab-case; the branch may contain one slash: {context}"
        );
        super::parse_convoy_names(&self.prompt(Model::Haiku, &prompt).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc, time::Duration};

    use async_trait::async_trait;

    use super::ClaudeApiAiUtility;
    use crate::providers::{ai_utility::AiUtility, ChannelLabel, HttpClient};

    struct HangingHttpClient;

    #[async_trait]
    impl HttpClient for HangingHttpClient {
        async fn execute(&self, _: reqwest::Request, _: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn convoy_name_generation_times_out_when_anthropic_does_not_respond() {
        let utility = ClaudeApiAiUtility::new("test-key".into(), Arc::new(HangingHttpClient));

        let result = tokio::time::timeout(Duration::from_secs(16), utility.generate_convoy_names("Issue 782"))
            .await
            .expect("Anthropic request should enforce its own shorter deadline");

        assert!(matches!(result, Err(message) if message.contains("timed out after 15s")));
    }
}
