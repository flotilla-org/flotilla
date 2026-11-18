//! Dummy environment factory (#3206). The composition root installs it only
//! when the daemon's `[dummy_fleet]` settings enable the dummy fleet.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::environment::dummy::{
    DummyEnvironmentBehaviour, DummyEnvironmentCleanup, DummyEnvironmentCounts, DummyEnvironmentProvider, DummyVendorChange,
    DummyVendorJournal, DUMMY_CLOUD_ENVIRONMENT_INSTANCE, DUMMY_ENVIRONMENT_INSTANCE, DUMMY_FLEET_ENV,
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
    /// Clones share the external backing inventory across daemon restarts. `journal` hears
    /// each change to it, for the next process's vendor.
    pub fn new(behaviour: DummyEnvironmentBehaviour, cleanup: DummyEnvironmentCleanup, journal: DummyVendorJournal) -> Self {
        let provider = DummyEnvironmentProvider::new(behaviour).with_cleanup(cleanup).with_journal(journal);
        Self { instance: DUMMY_ENVIRONMENT_INSTANCE, provider: Arc::new(provider) }
    }

    /// The cloud-sandbox vendor, registered as [`DUMMY_CLOUD_ENVIRONMENT_INSTANCE`].
    pub fn cloud(behaviour: DummyEnvironmentBehaviour, cleanup: DummyEnvironmentCleanup, journal: DummyVendorJournal) -> Self {
        let provider = DummyEnvironmentProvider::cloud(behaviour).with_cleanup(cleanup).with_journal(journal);
        Self { instance: DUMMY_CLOUD_ENVIRONMENT_INSTANCE, provider: Arc::new(provider) }
    }

    /// The instance this vendor is registered as.
    pub fn instance(&self) -> &'static str {
        self.instance
    }

    /// See [`DummyEnvironmentProvider::restore`].
    pub fn restore(&self, change: DummyVendorChange) {
        self.provider.restore(change);
    }

    /// See [`DummyEnvironmentProvider::snapshot`].
    pub fn snapshot(&self) -> Vec<DummyVendorChange> {
        self.provider.snapshot()
    }

    /// See [`DummyEnvironmentProvider::forward_target`].
    pub fn forward_target(&self, environment: &str) -> Option<PathBuf> {
        self.provider.forward_target(environment)
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::vanish`].
    pub fn vanish(&self, environment: &str) -> bool {
        self.provider.vanish(environment)
    }

    /// Act as the vendor: see [`DummyEnvironmentProvider::stop`].
    pub fn stop(&self, environment: &str) -> bool {
        self.provider.stop(environment)
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
