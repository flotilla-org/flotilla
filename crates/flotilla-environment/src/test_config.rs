use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
pub(crate) struct TestConfig(DaemonHostPath);
impl TestConfig {
    pub(crate) fn with_base(path: &std::path::Path) -> Self {
        Self(DaemonHostPath::new(path))
    }
}
#[async_trait::async_trait]
impl ProviderConfigView for TestConfig {
    fn base_path(&self) -> &DaemonHostPath {
        &self.0
    }
    fn state_dir(&self) -> &DaemonHostPath {
        &self.0
    }
    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        ResolvedCheckoutConfig { path: "{{ repo_path }}".into() }
    }
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        Ok(ProviderConfig::default())
    }
}
