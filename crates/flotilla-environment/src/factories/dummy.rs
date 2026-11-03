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
    DummyEnvironmentBehaviour, DummyEnvironmentCleanup, DummyEnvironmentCounts, DummyEnvironmentProvider, DUMMY_CLOUD_ENVIRONMENT_INSTANCE,
    DUMMY_ENVIRONMENT_INSTANCE, DUMMY_FLEET_ENV,
};
use crate::environment::EnvironmentProvider;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;

#[derive(Clone)]
pub struct DummyEnvironmentFactory {
    instance: &'static str,
    provider: Arc<DummyEnvironmentProvider>,
}

impl DummyEnvironmentFactory {
    /// Clones share the external backing inventory across daemon restarts.
    pub fn new(behaviour: DummyEnvironmentBehaviour, cleanup: DummyEnvironmentCleanup) -> Self {
        Self { instance: DUMMY_ENVIRONMENT_INSTANCE, provider: Arc::new(DummyEnvironmentProvider::new(behaviour).with_cleanup(cleanup)) }
    }

    /// The cloud-sandbox vendor, registered as [`DUMMY_CLOUD_ENVIRONMENT_INSTANCE`].
    pub fn cloud(behaviour: DummyEnvironmentBehaviour, cleanup: DummyEnvironmentCleanup) -> Self {
        let provider = DummyEnvironmentProvider::cloud(behaviour, DUMMY_CLOUD_ENVIRONMENT_INSTANCE).with_cleanup(cleanup);
        Self { instance: DUMMY_CLOUD_ENVIRONMENT_INSTANCE, provider: Arc::new(provider) }
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::vanish`].
    pub fn vanish(&self, environment: &str) -> bool {
        self.provider.vanish(environment)
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::stop`].
    pub fn stop(&self, environment: &str) -> bool {
        self.provider.stop(environment)
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::resume`].
    pub fn resume(&self, environment: &str) -> bool {
        self.provider.resume(environment)
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::refuse_provisioning`].
    pub fn refuse_provisioning(&self, refuse: bool) {
        self.provider.refuse_provisioning(refuse);
    }

    pub fn counts(&self) -> DummyEnvironmentCounts {
        self.provider.counts()
    }
}

#[async_trait]
impl Factory for DummyEnvironmentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn EnvironmentProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, self.instance)
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
