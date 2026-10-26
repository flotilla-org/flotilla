//! Dummy environment factory (#3206). The composition root installs it only
//! when the daemon's `[dummy_fleet]` settings enable the dummy fleet.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::environment::dummy::{
    DummyEnvironmentBehaviour, DummyEnvironmentCleanup, DummyEnvironmentProvider, DUMMY_ENVIRONMENT_INSTANCE, DUMMY_FLEET_ENV,
};
use crate::environment::EnvironmentProvider;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;

#[derive(Clone)]
pub struct DummyEnvironmentFactory {
    provider: Arc<DummyEnvironmentProvider>,
}

impl DummyEnvironmentFactory {
    /// Clones share the external backing inventory across daemon restarts.
    pub fn new(behaviour: DummyEnvironmentBehaviour, cleanup: DummyEnvironmentCleanup) -> Self {
        Self { provider: Arc::new(DummyEnvironmentProvider::new(behaviour).with_cleanup(cleanup)) }
    }
}

#[async_trait]
impl Factory for DummyEnvironmentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn EnvironmentProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, DUMMY_ENVIRONMENT_INSTANCE)
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        _runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        // Dummy environments host crews, never further environments.
        if env.find_env_var(DUMMY_FLEET_ENV).is_some() {
            return Err(vec![UnmetRequirement::MissingConfig("dummy environments do not nest".into())]);
        }
        Ok(Arc::clone(&self.provider) as Arc<dyn EnvironmentProvider>)
    }
}
