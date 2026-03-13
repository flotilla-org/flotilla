//! Terminal pool factory for shpool.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{flotilla_config_dir, ConfigStore};
use crate::providers::discovery::{
    EnvironmentBag, ProviderDescriptor, TerminalPoolFactory, UnmetRequirement,
};
use crate::providers::terminal::shpool::ShpoolTerminalPool;
use crate::providers::terminal::TerminalPool;
use crate::providers::CommandRunner;

pub struct ShpoolTerminalPoolFactory;

#[async_trait]
impl TerminalPoolFactory for ShpoolTerminalPoolFactory {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named("shpool")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if env.find_binary("shpool").is_some() {
            let socket_path = flotilla_config_dir().join("shpool/shpool.socket");
            let pool = ShpoolTerminalPool::create(runner, socket_path).await;
            Ok(Arc::new(pool))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("shpool".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{
        EnvironmentAssertion, EnvironmentBag, TerminalPoolFactory, UnmetRequirement,
    };

    use super::ShpoolTerminalPoolFactory;

    #[tokio::test]
    async fn shpool_factory_succeeds_with_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary(
            "shpool",
            "/usr/local/bin/shpool",
        ));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shpool_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ShpoolTerminalPoolFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without shpool binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("shpool".into())));
    }

    #[tokio::test]
    async fn shpool_factory_descriptor() {
        let desc = ShpoolTerminalPoolFactory.descriptor();
        assert_eq!(desc.name, "shpool");
        assert_eq!(desc.display_name, "shpool");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
