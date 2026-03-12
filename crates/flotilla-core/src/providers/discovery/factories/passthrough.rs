//! Terminal pool factory for passthrough (unconditional fallback).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::discovery::{
    EnvironmentBag, ProviderDescriptor, TerminalPoolFactory, UnmetRequirement,
};
use crate::providers::terminal::passthrough::PassthroughTerminalPool;
use crate::providers::terminal::TerminalPool;
use crate::providers::CommandRunner;

pub struct PassthroughTerminalPoolFactory;

#[async_trait]
impl TerminalPoolFactory for PassthroughTerminalPoolFactory {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            name: "passthrough".into(),
            display_name: "passthrough".into(),
            abbreviation: "".into(),
            section_label: "".into(),
            item_noun: "".into(),
        }
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        Ok(Arc::new(PassthroughTerminalPool))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{EnvironmentBag, TerminalPoolFactory};

    use super::PassthroughTerminalPoolFactory;

    #[tokio::test]
    async fn passthrough_factory_always_succeeds() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = PassthroughTerminalPoolFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn passthrough_factory_descriptor() {
        let desc = PassthroughTerminalPoolFactory.descriptor();
        assert_eq!(desc.name, "passthrough");
        assert_eq!(desc.display_name, "passthrough");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
