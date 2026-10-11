use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

// AI factories use environment assertions and must not read configuration.
pub struct TestConfig;

#[async_trait::async_trait]
impl ProviderConfigView for TestConfig {
    fn base_path(&self) -> &DaemonHostPath {
        panic!("AI utility factories must not read config paths")
    }

    fn state_dir(&self) -> &DaemonHostPath {
        panic!("AI utility factories must not read config paths")
    }

    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        panic!("AI utility factories must not resolve checkout config")
    }

    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        panic!("AI utility factories must not load config")
    }
}
