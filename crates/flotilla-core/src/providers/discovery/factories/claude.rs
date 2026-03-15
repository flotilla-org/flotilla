//! Cloud agent and AI utility factories for Claude-based providers.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        ai_utility::{claude_api::ClaudeApiAiUtility, claude_cli::ClaudeCliAiUtility, AiUtility},
        coding_agent::{claude::ClaudeCodingAgent, CloudAgentService},
        discovery::{EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement},
        CommandRunner, ReqwestHttpClient,
    },
};

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
            Ok(Arc::new(ClaudeCodingAgent::new("claude".into(), runner, http)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("claude".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeApiAiUtilityFactory — preferred, uses ANTHROPIC_API_KEY
// ---------------------------------------------------------------------------

pub struct ClaudeApiAiUtilityFactory;

#[async_trait]
impl Factory for ClaudeApiAiUtilityFactory {
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("claude-api", "Claude API", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        if let Some(api_key) = env.find_env_var("ANTHROPIC_API_KEY") {
            let http = Arc::new(ReqwestHttpClient::new());
            Ok(Arc::new(ClaudeApiAiUtility::new(api_key.to_string(), http)))
        } else {
            Err(vec![UnmetRequirement::MissingEnvVar("ANTHROPIC_API_KEY".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// ClaudeCliAiUtilityFactory — fallback, shells out to `claude` CLI
// ---------------------------------------------------------------------------

pub struct ClaudeCliAiUtilityFactory;

#[async_trait]
impl Factory for ClaudeCliAiUtilityFactory {
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("claude-cli", "Claude CLI", "", "", "")
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
            Ok(Arc::new(ClaudeCliAiUtility::new(claude_bin, runner)))
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
    use std::{path::Path, sync::Arc};

    use super::{ClaudeApiAiUtilityFactory, ClaudeCliAiUtilityFactory, ClaudeCodingAgentFactory};
    use crate::{
        config::ConfigStore,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    fn bag_with_claude_binary() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
    }

    fn bag_with_api_key() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::env_var("ANTHROPIC_API_KEY", "sk-ant-test-key"))
    }

    // ── ClaudeCodingAgentFactory tests ──

    #[tokio::test]
    async fn claude_coding_agent_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_coding_agent_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
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

    // ── ClaudeApiAiUtilityFactory tests ──

    #[tokio::test]
    async fn claude_api_ai_utility_factory_succeeds_with_api_key() {
        let bag = bag_with_api_key();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeApiAiUtilityFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_api_ai_utility_factory_fails_without_api_key() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeApiAiUtilityFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without API key");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("ANTHROPIC_API_KEY".into())));
    }

    #[tokio::test]
    async fn claude_api_ai_utility_factory_descriptor() {
        let desc = ClaudeApiAiUtilityFactory.descriptor();
        assert_eq!(desc.name, "claude-api");
        assert_eq!(desc.display_name, "Claude API");
    }

    // ── ClaudeCliAiUtilityFactory tests ──

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCliAiUtilityFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCliAiUtilityFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without claude binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("claude".into())));
    }

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_descriptor() {
        let desc = ClaudeCliAiUtilityFactory.descriptor();
        assert_eq!(desc.name, "claude-cli");
        assert_eq!(desc.display_name, "Claude CLI");
    }
}
