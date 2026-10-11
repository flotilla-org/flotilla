//! AI utility factories for Claude-based providers.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::claude_api::ClaudeApiAiUtility;
use crate::claude_cli::ClaudeCliAiUtility;
use crate::AiUtility;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::ReqwestHttpClient;

// ---------------------------------------------------------------------------
// ClaudeApiAiUtilityFactory — preferred, uses ANTHROPIC_API_KEY
// ---------------------------------------------------------------------------

pub struct ClaudeApiAiUtilityFactory;

#[async_trait]
impl Factory for ClaudeApiAiUtilityFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::AiUtility, "claude", "api", "Claude API", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
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
    type Descriptor = ProviderDescriptor;
    type Output = dyn AiUtility;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::AiUtility, "claude", "cli", "Claude CLI", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        if let Some(path) = env.find_binary("claude") {
            let claude_bin = path.as_path().to_string_lossy().to_string();
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
    use std::sync::Arc;

    use super::{ClaudeApiAiUtilityFactory, ClaudeCliAiUtilityFactory};
    use crate::testing::TestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    fn bag_with_claude_binary() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
    }

    fn bag_with_api_key() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::env_var("ANTHROPIC_API_KEY", "sk-ant-test-key"))
    }

    // ── ClaudeApiAiUtilityFactory tests ──

    #[tokio::test]
    async fn claude_api_ai_utility_factory_succeeds_with_api_key() {
        let bag = bag_with_api_key();
        let config = TestConfig::new(std::path::Path::new("/config"));
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeApiAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_api_ai_utility_factory_fails_without_api_key() {
        let bag = EnvironmentBag::new();
        let config = TestConfig::new(std::path::Path::new("/config"));
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeApiAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without API key");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("ANTHROPIC_API_KEY".into())));
    }

    #[tokio::test]
    async fn claude_api_ai_utility_factory_descriptor() {
        let desc = ClaudeApiAiUtilityFactory.descriptor();
        assert_eq!(desc.backend, "claude");
        assert_eq!(desc.implementation, "api");
        assert_eq!(desc.display_name, "Claude API");
    }

    // ── ClaudeCliAiUtilityFactory tests ──

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let config = TestConfig::new(std::path::Path::new("/config"));
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCliAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let config = TestConfig::new(std::path::Path::new("/config"));
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCliAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without claude binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("claude".into())));
    }

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_descriptor() {
        let desc = ClaudeCliAiUtilityFactory.descriptor();
        assert_eq!(desc.backend, "claude");
        assert_eq!(desc.implementation, "cli");
        assert_eq!(desc.display_name, "Claude CLI");
    }
}
