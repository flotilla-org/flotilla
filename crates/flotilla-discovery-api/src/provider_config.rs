//! Read-only configuration seam for provider construction.
use async_trait::async_trait;
use flotilla_config::{
    provider_config::{AiUtilityConfig, ChangeRequestConfig, CloudAgentConfig, IssueTrackerConfig, TerminalPoolConfig},
    ConfigStore,
};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

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

#[async_trait]
impl ProviderConfigView for ConfigStore {
    fn base_path(&self) -> &DaemonHostPath {
        self.base_path()
    }
    fn state_dir(&self) -> &DaemonHostPath {
        self.state_dir()
    }
    fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        ResolvedCheckoutConfig { path: self.resolve_checkout_path(repo_root) }
    }
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        // Share provider-runtime's bounded blocking capacity with other probes.
        let config = self
            .load_config_offloaded(|read| {
                flotilla_provider_runtime::probe::blocking("discovery config", flotilla_provider_runtime::probe::PROBE_TIMEOUT, read)
            })
            .await?;
        Ok(ProviderConfig {
            change_request: config.change_request,
            issue_tracker: config.issue_tracker,
            cloud_agent: config.cloud_agent,
            ai_utility: config.ai_utility,
            terminal_pool: config.terminal_pool,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Glue: the provider view preserves every provider field and delegates paths to the store.
    #[tokio::test]
    async fn provider_config_view_preserves_settings_and_paths() {
        let directory = tempfile::tempdir().expect("config directory");
        let store = ConfigStore::with_base(directory.path());
        std::fs::write(
            directory.path().join("config.toml"),
            r#"
[change_request]
backend = "github"
review_bot_login = "reviewer"
operator_login = "operator"
[issue_tracker]
backend = "forgejo"
[issue_tracker.forgejo]
service_url = "https://forge.example"
api_base_url = "https://forge.example/api/v1"
token_path = "token"
token_agent = "coder"
[cloud_agent]
backend = "codex"
[ai_utility]
backend = "claude"
[ai_utility.claude]
implementation = "cli"
[terminal_pool]
backend = "cleat"
[vcs.git]
checkout_path = "../worktrees/{{ branch }}"
"#,
        )
        .expect("persist settings");
        let view: &dyn ProviderConfigView = &store;
        let settings = view.load_config_for_probe().await.expect("provider settings");
        assert_eq!(settings.change_request.preference.backend.as_deref(), Some("github"));
        assert_eq!(settings.change_request.review_bot_login.as_deref(), Some("reviewer"));
        assert_eq!(settings.change_request.operator_login.as_deref(), Some("operator"));
        assert_eq!(settings.issue_tracker.preference.backend.as_deref(), Some("forgejo"));
        let forgejo = settings.issue_tracker.forgejo.expect("forgejo settings");
        assert_eq!(forgejo.service_url.as_deref(), Some("https://forge.example"));
        assert_eq!(forgejo.api_base_url.as_deref(), Some("https://forge.example/api/v1"));
        assert_eq!(forgejo.token_path.as_deref(), Some("token"));
        assert_eq!(forgejo.token_agent.as_deref(), Some("coder"));
        assert_eq!(settings.cloud_agent.preference.backend.as_deref(), Some("codex"));
        assert_eq!(settings.ai_utility.preference.backend.as_deref(), Some("claude"));
        assert_eq!(settings.ai_utility.claude.unwrap().implementation.as_deref(), Some("cli"));
        assert_eq!(settings.terminal_pool.preference.backend.as_deref(), Some("cleat"));
        assert_eq!(view.base_path(), store.base_path());
        assert_eq!(view.state_dir(), store.state_dir());
        assert_eq!(view.resolve_checkout_config(&ExecutionEnvironmentPath::new(directory.path())).path, "../worktrees/{{ branch }}");
    }
}
