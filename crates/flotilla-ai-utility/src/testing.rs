use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

// Injected host configuration without filesystem access.
pub struct TestConfig;

pub struct ModelConfig(pub String);

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
        Ok(ProviderConfig::default())
    }
}

#[async_trait::async_trait]
impl ProviderConfigView for ModelConfig {
    fn base_path(&self) -> &DaemonHostPath {
        panic!("no filesystem config")
    }
    fn state_dir(&self) -> &DaemonHostPath {
        panic!("no filesystem config")
    }
    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        panic!("no checkout config")
    }
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        let mut config = ProviderConfig::default();
        config.ai_utility.model = self.0.clone();
        Ok(config)
    }
}
