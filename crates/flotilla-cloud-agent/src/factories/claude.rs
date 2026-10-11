//! Cloud agent factory for the Claude provider.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::claude::ClaudeCodingAgent;
use crate::CloudAgentService;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::ReqwestHttpClient;

// ---------------------------------------------------------------------------
// ClaudeCodingAgentFactory
// ---------------------------------------------------------------------------

pub struct ClaudeCodingAgentFactory;

#[async_trait]
impl Factory for ClaudeCodingAgentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::CloudAgent, "claude", "Claude", "S", "Sessions", "session")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
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

#[cfg(test)]
mod tests {
    // HTTP audit (#1512): the consumer cloud-session endpoints use the unpublished
    // ccr-byoc beta contract and keychain OAuth credentials, not the documented
    // Anthropic Messages API. No enforcing stand-in can claim published rules,
    // and live recording would capture user OAuth material. Legacy claude_*.yaml
    // files are not referenced by active tests and are not claimed as coverage.
    // Neither service-contract option applies here; this remains a coverage gap.

    use super::*;
    use crate::testing::{DiscoveryMockRunner, TestConfig};
    use flotilla_discovery_api::EnvironmentAssertion;
    fn bag_with_claude_binary() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/local/bin/claude"))
    }
    // ── ClaudeCodingAgentFactory tests ──

    #[tokio::test]
    async fn claude_coding_agent_factory_succeeds_with_binary() {
        let bag = bag_with_claude_binary();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn claude_coding_agent_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ClaudeCodingAgentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without claude binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("claude".into())));
    }

    #[tokio::test]
    async fn claude_coding_agent_factory_descriptor() {
        let desc = ClaudeCodingAgentFactory.descriptor();
        assert_eq!(desc.backend, "claude");
        assert_eq!(desc.implementation, "claude");
        assert_eq!(desc.display_name, "Claude");
        assert_eq!(desc.abbreviation, "S");
        assert_eq!(desc.section_label, "Sessions");
        assert_eq!(desc.item_noun, "session");
    }
}
