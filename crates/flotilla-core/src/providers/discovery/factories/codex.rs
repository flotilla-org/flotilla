//! Cloud agent factory for Codex-based provider.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        coding_agent::{codex::CodexCodingAgent, CloudAgentService},
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        CommandRunner, ReqwestHttpClient,
    },
};

// ---------------------------------------------------------------------------
// CodexCodingAgentFactory
// ---------------------------------------------------------------------------

pub struct CodexCodingAgentFactory;

#[async_trait]
impl Factory for CodexCodingAgentFactory {
    type Output = dyn CloudAgentService;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::CloudAgent, "codex", "Codex", "S", "Sessions", "session")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>> {
        if env.has_auth("codex") {
            let http = Arc::new(ReqwestHttpClient::new());
            Ok(Arc::new(CodexCodingAgent::new("codex".into(), http)))
        } else {
            Err(vec![UnmetRequirement::MissingAuth("codex".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CodexCodingAgentFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    fn bag_with_codex_auth() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::auth_file("codex", "/home/user/.codex/auth.json"))
    }

    #[tokio::test]
    async fn codex_factory_succeeds_with_auth() {
        let bag = bag_with_codex_auth();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CodexCodingAgentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn codex_factory_fails_without_auth() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CodexCodingAgentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without codex auth");
        assert!(unmet.contains(&UnmetRequirement::MissingAuth("codex".into())));
    }

    #[tokio::test]
    async fn codex_factory_descriptor() {
        let desc = CodexCodingAgentFactory.descriptor();
        assert_eq!(desc.backend, "codex");
        assert_eq!(desc.implementation, "codex");
        assert_eq!(desc.display_name, "Codex");
        assert_eq!(desc.abbreviation, "S");
        assert_eq!(desc.section_label, "Sessions");
        assert_eq!(desc.item_noun, "session");
    }
}
