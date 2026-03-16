//! Terminal pool factory for shpool.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    attachable::AttachableStore,
    config::{flotilla_config_dir, ConfigStore},
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{shpool::ShpoolTerminalPool, TerminalPool},
        CommandRunner,
    },
};

pub struct ShpoolTerminalPoolFactory;

#[async_trait]
impl Factory for ShpoolTerminalPoolFactory {
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "shpool")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if env.find_binary("shpool").is_some() {
            let socket_path = flotilla_config_dir().join("shpool/shpool.socket");
            let store = Arc::new(std::sync::Mutex::new(AttachableStore::with_base(config.base_path())));
            let pool = ShpoolTerminalPool::create(runner, socket_path, store).await;
            Ok(Arc::new(pool))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("shpool".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::ShpoolTerminalPoolFactory;
    use crate::{
        config::ConfigStore,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn shpool_factory_succeeds_with_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("shpool", "/usr/local/bin/shpool"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shpool_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without shpool binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("shpool".into())));
    }

    #[tokio::test]
    async fn shpool_factory_descriptor() {
        let desc = ShpoolTerminalPoolFactory.descriptor();
        assert_eq!(desc.backend, "shpool");
        assert_eq!(desc.implementation, "shpool");
        assert_eq!(desc.display_name, "shpool");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
