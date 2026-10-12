//! Private test collaborators. The shared discovery testkit's fakes implement
//! the non-test build of this crate's traits, so unit tests use local doubles.
use std::path::Path;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::{ProviderConfig, ProviderConfigView, ResolvedCheckoutConfig};
use flotilla_paths::path_context::{DaemonHostPath, ExecutionEnvironmentPath};
use flotilla_protocol::ChangeRequest;

use crate::ChangeRequestTracker;

pub fn fixture_path(name: &str) -> String {
    format!("{}/src/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
}

/// Provider configuration rooted at a temporary directory, with default settings.
pub struct TestConfig(DaemonHostPath);

impl TestConfig {
    pub fn new(path: &Path) -> Self {
        Self(DaemonHostPath::new(path))
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
        ResolvedCheckoutConfig { path: String::new() }
    }

    async fn load_config_for_probe(&self) -> Result<ProviderConfig, String> {
        Ok(ProviderConfig::default())
    }
}

/// A read-only tracker over a fixed listing; exercises the trait's defaults.
pub struct ListedChangeRequests(pub Vec<(String, ChangeRequest)>);

#[async_trait]
impl ChangeRequestTracker for ListedChangeRequests {
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }

    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.0.iter().find(|(request_id, _)| request_id == id).cloned().ok_or_else(|| format!("change request {id} not found"))
    }

    async fn open_in_browser(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn close_change_request(&self, _id: &str) -> Result<(), String> {
        Err("read-only test tracker".into())
    }

    async fn merge_change_request(&self, _id: &str) -> Result<(), String> {
        Err("read-only test tracker".into())
    }

    async fn list_merged_branch_names(&self, _limit: usize) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}
