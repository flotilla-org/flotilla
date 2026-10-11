//! Read-only configuration seam for provider construction.
use async_trait::async_trait;
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AiUtilityConfig {
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
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

/// Resolved checkout configuration from host defaults.
pub struct ResolvedCheckoutConfig {
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub change_request: ChangeRequestConfig,
    pub issue_tracker: IssueTrackerConfig,
    pub cloud_agent: CloudAgentConfig,
    pub ai_utility: AiUtilityConfig,
    pub terminal_pool: TerminalPoolConfig,
}

#[async_trait]
pub trait ProviderConfigView: Send + Sync {
    fn base_path(&self) -> &DaemonHostPath;
    fn state_dir(&self) -> &DaemonHostPath;
    fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig;
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String>;
}

/// Forward the view through shared ownership so discovery callers can keep
/// passing their existing `Arc<ConfigStore>` without depending on the concrete
/// store or changing how the composition root shares it.
#[async_trait]
impl<T: ProviderConfigView + ?Sized> ProviderConfigView for std::sync::Arc<T> {
    fn base_path(&self) -> &DaemonHostPath {
        (**self).base_path()
    }
    fn state_dir(&self) -> &DaemonHostPath {
        (**self).state_dir()
    }
    fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        (**self).resolve_checkout_config(repo_root)
    }
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        (**self).load_config_for_probe().await
    }
}
