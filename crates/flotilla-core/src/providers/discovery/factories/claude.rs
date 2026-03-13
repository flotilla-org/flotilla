//! Cloud agent and AI utility factories for Claude-based providers.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::ai_utility::claude::ClaudeAiUtility;
use crate::providers::ai_utility::AiUtility;
use crate::providers::coding_agent::claude::ClaudeCodingAgent;
use crate::providers::coding_agent::CloudAgentService;
use crate::providers::discovery::{EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement};
use crate::providers::{CommandRunner, ReqwestHttpClient};

// ---------------------------------------------------------------------------
// ClaudeCodingAgentFactory
// ---------------------------------------------------------------------------

pub struct ClaudeCodingAgentFactory;

#[async_trait]
impl Factory for ClaudeCodingAgentFactory {
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("claude", "Claude", "S", "Sessions", "session")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>> {
        if env.find_binary("claude").is_some() {
            let http = Arc::new(ReqwestHttpClient::new());
            Ok(Arc::new(ClaudeCodingAgent::new(
                "claude".into(),
                runner,
                http,
            )))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("claude".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeAiUtilityFactory
// ---------------------------------------------------------------------------

pub struct ClaudeAiUtilityFactory;

#[async_trait]
impl Factory for ClaudeAiUtilityFactory {
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("claude", "Claude", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        if let Some(path) = env.find_binary("claude") {
            let claude_bin = path.to_string_lossy().to_string();
            Ok(Arc::new(ClaudeAiUtility::new(claude_bin, runner)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("claude".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{
        EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement,
    };

    use super::{ClaudeAiUtilityFactory, ClaudeCodingAgentFactory};

    fn bag_with_claude_binary() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary(
            "claude",
            "/usr/local/bin/claude",
        ))
    }

    // ── ClaudeCodingAgentFactory tests ──

    #[tokio::test]
    async fn claude_coding_agent_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_coding_agent_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without claude binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("claude".into())));
    }

    #[tokio::test]
    async fn claude_coding_agent_factory_descriptor() {
        let desc = ClaudeCodingAgentFactory.descriptor();
        assert_eq!(desc.name, "claude");
        assert_eq!(desc.display_name, "Claude");
        assert_eq!(desc.abbreviation, "S");
        assert_eq!(desc.section_label, "Sessions");
        assert_eq!(desc.item_noun, "session");
    }

    // ── ClaudeAiUtilityFactory tests ──

    #[tokio::test]
    async fn claude_ai_utility_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeAiUtilityFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_ai_utility_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeAiUtilityFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without claude binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("claude".into())));
    }

    #[tokio::test]
    async fn claude_ai_utility_factory_descriptor() {
        let desc = ClaudeAiUtilityFactory.descriptor();
        assert_eq!(desc.name, "claude");
        assert_eq!(desc.display_name, "Claude");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
