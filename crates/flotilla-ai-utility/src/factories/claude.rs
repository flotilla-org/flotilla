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
use flotilla_provider_runtime::ReqwestHttpClient;
use flotilla_provider_runtime::{CommandRunner, HttpClient};

async fn naming_model(config: &dyn ProviderConfigView) -> Result<String, Vec<UnmetRequirement>> {
    let model = config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?.ai_utility.model;
    if model.trim().is_empty() {
        return Err(vec![UnmetRequirement::MissingConfig("ai_utility.model must be a nonempty full API model ID".into())]);
    }
    Ok(model)
}

// ---------------------------------------------------------------------------
// ClaudeApiAiUtilityFactory — preferred, uses ANTHROPIC_API_KEY
// ---------------------------------------------------------------------------

pub struct ClaudeApiAiUtilityFactory;

impl ClaudeApiAiUtilityFactory {
    // The HTTP boundary is injected so the production config path can be exercised offline.
    pub(crate) async fn build(
        api_key: String,
        http: Arc<dyn HttpClient>,
        config: &dyn ProviderConfigView,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        let model = naming_model(config).await?;
        Ok(Arc::new(ClaudeApiAiUtility::new(api_key, http, model)))
    }
}

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
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        if let Some(api_key) = env.find_env_var("ANTHROPIC_API_KEY") {
            let http = Arc::new(ReqwestHttpClient::new());
            Self::build(api_key.to_string(), http, config).await
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
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>> {
        if let Some(path) = env.find_binary("claude") {
            let claude_bin = path.as_path().to_string_lossy().to_string();
            Ok(Arc::new(ClaudeCliAiUtility::new(claude_bin, runner, naming_model(config).await?)))
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
        let config = TestConfig;
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeApiAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_api_ai_utility_factory_fails_without_api_key() {
        let bag = EnvironmentBag::new();
        let config = TestConfig;
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
        let config = TestConfig;
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCliAiUtilityFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_cli_ai_utility_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let config = TestConfig;
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

#[cfg(test)]
mod configured_cli_tests {
    use super::ClaudeCliAiUtilityFactory;
    use crate::testing::ModelConfig;
    use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag, Factory};
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use std::sync::Arc;

    // Glue: config must reach both CLI naming calls, with explicit low effort.
    // The runner stands in for the Claude subprocess boundary.
    #[tokio::test]
    async fn configured_model_and_low_effort_reach_cli() {
        let branch_prompt =
            "Suggest a short git branch name for this context. Output ONLY the branch name, nothing else. Use kebab-case: issue";
        let convoy_prompt = "Suggest a coherent short convoy resource name and git branch name for this context. Return ONLY JSON with string fields name and branch. Use lowercase kebab-case; the branch may contain one slash: issue";
        let args = |prompt| {
            vec![
                "--model",
                "configured-model",
                "--system-prompt",
                "You are a concise assistant. Output only what is asked, with no explanation or formatting.",
                "--tools",
                "",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--effort",
                "low",
                "-p",
                prompt,
            ]
        };
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("/usr/local/bin/claude", &args(branch_prompt), Ok("branch".into()))
                .on_run("/usr/local/bin/claude", &args(convoy_prompt), Ok(r#"{"name":"convoy","branch":"branch"}"#.into()))
                .build(),
        );
        let env = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"));
        let utility = ClaudeCliAiUtilityFactory
            .probe(&env, &ModelConfig("configured-model".into()), &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .expect("provider");
        assert_eq!(utility.generate_branch_name("issue").await.expect("branch"), "branch");
        assert_eq!(utility.generate_convoy_names("issue").await.expect("names").name, "convoy");
    }
}

#[cfg(test)]
mod invalid_model_tests {
    use super::{ClaudeApiAiUtilityFactory, ClaudeCliAiUtilityFactory};
    use crate::testing::ModelConfig;
    use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement};
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use std::sync::Arc;

    // Empty or whitespace-only model config refuses probing before any remote call.
    // Admission can use the existing MissingConfig diagnostic and deterministic fallback.
    #[tokio::test]
    async fn empty_model_refuses_both_factories() {
        let env = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
            .with(EnvironmentAssertion::env_var("ANTHROPIC_API_KEY", "test-key"));
        for model in ["", " ", "\t\n"] {
            let config = ModelConfig(model.into());
            let runner = Arc::new(DiscoveryMockRunner::builder().build());
            let root = ExecutionEnvironmentPath::new("/repo");
            for result in [
                ClaudeApiAiUtilityFactory.probe(&env, &config, &root, runner.clone()).await,
                ClaudeCliAiUtilityFactory.probe(&env, &config, &root, runner.clone()).await,
            ] {
                assert_eq!(
                    result.err().expect("invalid model must refuse"),
                    vec![UnmetRequirement::MissingConfig("ai_utility.model must be a nonempty full API model ID".into())]
                );
            }
        }
    }
}
