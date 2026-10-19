//! Provider preference values stored in `config.toml` and handed to
//! provider construction through `flotilla_discovery_api::provider_config`.
use serde::{Deserialize, Serialize};

/// Per-category provider preference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderPreference {
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChangeRequestConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    /// GitHub bot whose review comments should wake a crew (GraphQL login, without `[bot]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_bot_login: Option<String>,
    /// Login of the operator who owns merge decisions on the configured forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_login: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IssueTrackerConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgejo: Option<ForgejoIssueTrackerConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ForgejoIssueTrackerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_agent: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CloudAgentConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AiUtilityConfig {
    /// Full API model ID shared by the naming API and CLI (not a CLI alias such as `haiku`).
    // ADR 0047: decode previous config; retain the default while this setting is optional.
    #[serde(default = "default_naming_model")]
    pub model: String,
    #[serde(flatten)]
    pub preference: ProviderPreference,
    pub claude: Option<ClaudeAiUtilityConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClaudeAiUtilityConfig {
    pub implementation: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TerminalPoolConfig {
    /// Cleat's atomic submission guard window; defaults to the existing 1s quiet period.
    pub controller_idle_ms: Option<u64>,
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

fn default_naming_model() -> String {
    "claude-haiku-5-5".into()
}

impl Default for AiUtilityConfig {
    fn default() -> Self {
        Self { model: default_naming_model(), preference: ProviderPreference::default(), claude: None }
    }
}
