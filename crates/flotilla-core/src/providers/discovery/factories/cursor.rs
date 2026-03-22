//! Cloud agent factory for Cursor-based provider.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        coding_agent::{cursor::CursorCodingAgent, CloudAgentService},
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        CommandRunner, ReqwestHttpClient,
    },
};

// ---------------------------------------------------------------------------
// CursorCodingAgentFactory
// ---------------------------------------------------------------------------

pub struct CursorCodingAgentFactory;

#[async_trait]
impl Factory for CursorCodingAgentFactory {
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::CloudAgent, "cursor", "Cursor", "S", "Sessions", "session")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>> {
        let mut unmet = vec![];
        if env.find_binary("agent").is_none() {
            unmet.push(UnmetRequirement::MissingBinary("agent".into()));
        }
        if env.find_env_var("CURSOR_API_KEY").is_none() {
            unmet.push(UnmetRequirement::MissingEnvVar("CURSOR_API_KEY".into()));
        }
        if !unmet.is_empty() {
            return Err(unmet);
        }
        let http = Arc::new(ReqwestHttpClient::new());
        Ok(Arc::new(CursorCodingAgent::new("cursor".into(), http)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::CursorCodingAgentFactory;
    use crate::{
        config::ConfigStore,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    fn bag_with_agent_and_api_key() -> EnvironmentBag {
        EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("agent", "/usr/local/bin/agent"))
            .with(EnvironmentAssertion::env_var("CURSOR_API_KEY", "cursor-key-123"))
    }

    #[tokio::test]
    async fn cursor_factory_succeeds_with_binary_and_env_var() {
        let bag = bag_with_agent_and_api_key();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CursorCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cursor_factory_fails_without_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("CURSOR_API_KEY", "cursor-key-123"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CursorCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without agent binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("agent".into())));
        assert!(
            !unmet.contains(&UnmetRequirement::MissingEnvVar("CURSOR_API_KEY".into())),
            "should not report missing env var when it is present"
        );
    }

    #[tokio::test]
    async fn cursor_factory_fails_without_env_var() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("agent", "/usr/local/bin/agent"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CursorCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without CURSOR_API_KEY");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("CURSOR_API_KEY".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingBinary("agent".into())), "should not report missing binary when it is present");
    }

    #[tokio::test]
    async fn cursor_factory_fails_without_both() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CursorCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail with both missing");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("agent".into())));
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("CURSOR_API_KEY".into())));
        assert_eq!(unmet.len(), 2);
    }

    #[tokio::test]
    async fn cursor_factory_descriptor() {
        let desc = CursorCodingAgentFactory.descriptor();
        assert_eq!(desc.backend, "cursor");
        assert_eq!(desc.implementation, "cursor");
        assert_eq!(desc.display_name, "Cursor");
        assert_eq!(desc.abbreviation, "S");
        assert_eq!(desc.section_label, "Sessions");
        assert_eq!(desc.item_noun, "session");
    }
}
