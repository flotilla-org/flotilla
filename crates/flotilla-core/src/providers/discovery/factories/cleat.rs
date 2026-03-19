//! Terminal pool factory for cleat.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        terminal::{cleat::CleatTerminalPool, TerminalPool},
        CommandRunner,
    },
};

pub struct CleatTerminalPoolFactory;

#[async_trait]
impl Factory for CleatTerminalPoolFactory {
    type Output = dyn TerminalPool;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::TerminalPool, "cleat")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
        attachable_store: crate::attachable::SharedAttachableStore,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>> {
        if let Some(binary) = env.find_binary("cleat") {
            Ok(Arc::new(CleatTerminalPool::new(runner, binary.display().to_string(), attachable_store)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cleat".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::CleatTerminalPoolFactory;
    use crate::{
        config::ConfigStore,
        providers::discovery::{
            test_support::{test_attachable_store, DiscoveryMockRunner},
            EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement,
        },
    };

    #[tokio::test]
    async fn session_factory_succeeds_with_binary() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("cleat", "/usr/local/bin/cleat"));
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CleatTerminalPoolFactory.probe(&bag, &config, Path::new("/repo"), runner, test_attachable_store(&config)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CleatTerminalPoolFactory.probe(&bag, &config, Path::new("/repo"), runner, test_attachable_store(&config)).await;
        let unmet = result.err().expect("missing binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("cleat".into())));
    }
}
