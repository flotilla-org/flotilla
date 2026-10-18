//! Provider test configuration, replay paths and boundary collaborators.
use std::path::Path;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

// The shared contract resolves the trait through its parent module.
use flotilla_issue_tracker::IssueProvider;

#[path = "../../flotilla-issue-tracker/src/provider_contract.rs"]
mod provider_contract;
pub use provider_contract::assert_provider_contract;

pub fn fixture_path(name: &str) -> String {
    format!("{}/src/fixtures/issue_tracker/{}", env!("CARGO_MANIFEST_DIR"), name)
}

/// Provider configuration rooted at a directory, with fixed provider settings.
pub struct TestConfig {
    base: DaemonHostPath,
    settings: ProviderConfig,
}

impl TestConfig {
    pub fn new(path: &Path) -> Self {
        Self::with_settings(path, ProviderConfig::default())
    }

    pub fn with_settings(path: &Path, settings: ProviderConfig) -> Self {
        Self { base: DaemonHostPath::new(path), settings }
    }
}

#[async_trait]
impl ProviderConfigView for TestConfig {
    fn base_path(&self) -> &DaemonHostPath {
        &self.base
    }

    fn state_dir(&self) -> &DaemonHostPath {
        &self.base
    }

    fn resolve_checkout_config(&self, _: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        ResolvedCheckoutConfig { path: String::new() }
    }

    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        Ok(self.settings.clone())
    }
}
