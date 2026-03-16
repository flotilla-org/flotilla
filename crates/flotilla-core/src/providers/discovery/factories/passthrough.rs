//! Terminal pool factory for passthrough (unconditional fallback).

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{passthrough::PassthroughTerminalPool, TerminalPool},
        CommandRunner,
    },
};

pub struct PassthroughTerminalPoolFactory;

#[async_trait]
impl Factory for PassthroughTerminalPoolFactory {
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough")
    }

    async fn probe_with_services(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
        _attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        Ok(Arc::new(PassthroughTerminalPool))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::PassthroughTerminalPoolFactory;
    use crate::{
        config::ConfigStore,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentBag, Factory},
    };

    #[tokio::test]
    async fn passthrough_factory_always_succeeds() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = PassthroughTerminalPoolFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
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
