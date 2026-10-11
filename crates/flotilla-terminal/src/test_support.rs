//! Config values are unused by terminal factories; this view keeps their tests independent of core.
use async_trait::async_trait;
use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

pub struct TestConfig(DaemonHostPath);
impl TestConfig {
    pub fn with_base(path: impl AsRef<std::path::Path>) -> Self {
        Self(DaemonHostPath::new(path.as_ref()))
    }
}
#[async_trait]
impl ProviderConfigView for TestConfig {
    fn base_path(&self) -> &DaemonHostPath {
        &self.0
    }
    fn state_dir(&self) -> &DaemonHostPath {
        &self.0
    }
    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        panic!("terminal factories do not read checkout config")
    }
    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        panic!("terminal factories do not read provider config")
    }
}
