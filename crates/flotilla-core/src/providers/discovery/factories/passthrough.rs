//! Terminal pool factory for passthrough (unconditional fallback).

use crate::provider_config::ProviderConfigView;

use std::sync::Arc;

use async_trait::async_trait;

use crate::discovery_api::EnvironmentBag;
use crate::providers::discovery::Factory;
use crate::providers::discovery::ProviderCategory;
use crate::providers::discovery::ProviderDescriptor;
use crate::providers::discovery::UnmetRequirement;
use crate::providers::terminal::passthrough::PassthroughTerminalPool;
use crate::providers::terminal::TerminalPool;
use crate::providers::CommandRunner;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

pub struct PassthroughTerminalPoolFactory;

#[async_trait]
impl Factory for PassthroughTerminalPoolFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        Ok(Arc::new(PassthroughTerminalPool))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::PassthroughTerminalPoolFactory;
    use crate::config::ConfigStore;
    use crate::discovery_api::EnvironmentBag;
    use crate::providers::discovery::Factory;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    #[tokio::test]
    async fn passthrough_factory_always_succeeds() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = PassthroughTerminalPoolFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn passthrough_factory_descriptor() {
        let desc = PassthroughTerminalPoolFactory.descriptor();
        assert_eq!(desc.backend, "passthrough");
        assert_eq!(desc.implementation, "passthrough");
        assert_eq!(desc.display_name, "passthrough");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
