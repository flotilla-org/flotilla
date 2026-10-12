//! Private test collaborators. The shared discovery testkit's runner and
//! environment doubles implement provider-runtime traits, so they are safe to
//! use here; its runtime builders return the non-test build of this crate and
//! are never passed into these unit tests.
use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
pub use flotilla_discovery_testkit::{DiscoveryMockRunner, TestEnvVars};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

/// Provider configuration rooted at a directory with no configured sections.
pub struct TestConfig(DaemonHostPath);

impl TestConfig {
    pub fn new(path: &std::path::Path) -> Self {
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
        ResolvedCheckoutConfig { path: String::new() }
    }

    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        Ok(ProviderConfig::default())
    }
}
