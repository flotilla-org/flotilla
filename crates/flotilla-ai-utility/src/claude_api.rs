use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

use flotilla_provider_runtime::http_execute;
use flotilla_provider_runtime::HttpClient;

static REQUEST_FACTORY: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(rigging_tls::client);

const API_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const SYSTEM_PROMPT: &str = "You are a concise assistant. Output only what is asked, with no explanation or formatting.";
const ANTHROPIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ClaudeApiAiUtility {
    model: String,
    api_key: String,
    http: Arc<dyn HttpClient>,
}

impl ClaudeApiAiUtility {
    pub fn new(api_key: String, http: Arc<dyn HttpClient>, model: String) -> Self {
        Self { api_key, http, model }
    }

    /// Run a one-shot prompt against the Anthropic Messages API.
    async fn prompt(&self, prompt: &str) -> Result<String, String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 256,
            "output_config": { "effort": "low" },
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
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                ContentBlock::Other => None,
            })
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
    // Adaptive thinking can return non-text blocks before the naming result.
    #[serde(other)]
    Other,
}

#[async_trait]
impl super::AiUtility for ClaudeApiAiUtility {
    async fn generate_branch_name(&self, context: &str) -> Result<String, String> {
        info!("ai: generating branch name via API");
        let prompt = format!(
            "Suggest a short git branch name for this context. \
             Output ONLY the branch name, nothing else. Use kebab-case: {context}"
        );

        let output = self.prompt(&prompt).await?;
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
        super::parse_convoy_names(&self.prompt(&prompt).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc, time::Duration};

    use async_trait::async_trait;

    use super::ClaudeApiAiUtility;
    use crate::{factories::claude::ClaudeApiAiUtilityFactory, testing::ModelConfig, AiUtility};
    use flotilla_provider_runtime::ChannelLabel;
    use flotilla_provider_runtime::HttpClient;

    struct HangingHttpClient;

    #[async_trait]
    impl HttpClient for HangingHttpClient {
        async fn execute(&self, _: reqwest::Request, _: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn convoy_name_generation_times_out_when_anthropic_does_not_respond() {
        let utility = ClaudeApiAiUtility::new("test-key".into(), Arc::new(HangingHttpClient), "claude-haiku-5-5".into());

        let result = tokio::time::timeout(Duration::from_secs(16), utility.generate_convoy_names("Issue 782"))
            .await
            .expect("Anthropic request should enforce its own shorter deadline");

        assert!(matches!(result, Err(message) if message.contains("timed out after 15s")));
    }
    // Anthropic's documented Messages contract requires key/version headers and
    // model, max_tokens and messages: https://platform.claude.com/docs/en/api/messages/create
    // Glue: both naming calls must send the configured ID and low effort, and accept adaptive thinking blocks.
    // The loop covers default and overridden IDs; the stand-in is the HTTP boundary.
    #[tokio::test]
    #[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "requires loopback listener")]
    async fn messages_request_satisfies_http_contract() {
        use axum::{body::Bytes, extract::State, routing::post, Router};
        use http::{HeaderMap, StatusCode};
        use serde_json::{json, Value};

        use flotilla_replay_testkit::http_contract::StandIn;
        async fn receive(State(model): State<String>, headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
            let valid_headers = headers.get("x-api-key").is_some_and(|v| v == "test-key")
                && headers.get("anthropic-version").is_some_and(|v| v == "2023-06-01")
                && headers.get("content-type").is_some_and(|v| v == "application/json");
            let Ok(body) = serde_json::from_slice::<Value>(&body) else {
                return (StatusCode::BAD_REQUEST, "malformed JSON request".into());
            };
            let valid_body = body["model"] == model
                && body["output_config"]["effort"] == "low"
                && body["max_tokens"].as_u64().is_some_and(|v| v > 0)
                && body["messages"].as_array().is_some_and(|messages| {
                    !messages.is_empty() && messages.iter().all(|m| m["role"] == "user" && m["content"].is_string())
                });
            if valid_headers && valid_body {
                let text = if body["messages"][0]["content"].as_str().is_some_and(|p| p.contains("convoy")) {
                    r#"{"name":"contract-convoy","branch":"contract-branch"}"#
                } else {
                    "contract-branch"
                };
                (StatusCode::OK, json!({"content":[{"type":"thinking","thinking":"internal"},{"type":"text","text":text}]}).to_string())
            } else {
                (StatusCode::BAD_REQUEST, "invalid Messages request".into())
            }
        }
        for model in ["claude-haiku-5-5", "configured-model"] {
            let server = StandIn::start(
                "https://api.anthropic.com",
                Router::new().route("/v1/messages", post(receive)).with_state(model.to_string()),
            )
            .await;
            let utility = ClaudeApiAiUtilityFactory::build("test-key".into(), server.http.clone(), &ModelConfig(model.into()))
                .await
                .expect("configured provider");
            assert_eq!(utility.generate_branch_name("HTTP contract").await.expect("accepted request"), "contract-branch");
            assert_eq!(
                utility.generate_convoy_names("HTTP contract").await.expect("accepted request"),
                crate::ConvoyNames { name: "contract-convoy".into(), branch: "contract-branch".into() }
            );
            // Malformed JSON must receive a service refusal, not panic the handler.
            let response = rigging_tls::client()
                .post(server.url.join("/v1/messages").expect("stand-in URL"))
                .header("x-api-key", "test-key")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body("{")
                .send()
                .await
                .expect("malformed request response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
    }
}
