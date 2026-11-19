//! The dummy environment provider stands in for a host's own Docker vessels: a dummy-fleet daemon
//! keeps every other real environment provider, but not one taking the same stored input, since
//! two providers of one input leave every Docker kind on the host without a provider.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery::ProviderFactory;
use flotilla_discovery_api::{provider_config::ProviderConfigView, EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement};
use flotilla_environment::environment::{EnvironmentInput, EnvironmentProvider};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::{execution_context::AccountState, CommandRunner};

pub(crate) const STOOD_IN: &str = "the dummy fleet stands in for Docker vessels on this host";

/// Refuses a provider its factory probes when it takes Docker input, which the dummy provider
/// realises instead.
pub(crate) struct DockerStoodIn {
    inner: Box<ProviderFactory<dyn EnvironmentProvider>>,
}

impl DockerStoodIn {
    pub(crate) fn wrap(inner: Box<ProviderFactory<dyn EnvironmentProvider>>) -> Box<ProviderFactory<dyn EnvironmentProvider>> {
        Box::new(Self { inner })
    }
}

#[async_trait]
impl Factory for DockerStoodIn {
    type Descriptor = ProviderDescriptor;
    type Output = dyn EnvironmentProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        self.inner.descriptor()
    }

    fn account_state(&self) -> Option<AccountState> {
        self.inner.account_state()
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        let provider = self.inner.probe(env, config, repo_root, runner).await?;
        if provider.legacy_input() == Some(EnvironmentInput::Docker) {
            return Err(vec![UnmetRequirement::MissingConfig(STOOD_IN.into())]);
        }
        Ok(provider)
    }
}

#[cfg(test)]
mod tests {
    use flotilla_config::{dummy_fleet::DummyFleetConfig, ConfigStore};
    use flotilla_discovery::{registry::ProviderRegistry, DiscoveryRuntime};
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_environment::environment::dummy::{DummyCommandRunner, DUMMY_ENVIRONMENT_INSTANCE};
    use flotilla_protocol::EnvironmentId;

    use super::*;
    use crate::DummyFleet;

    // Behaviour: a dummy-fleet daemon on a host with Docker discovers one provider for Docker
    // kinds, the dummy one, so the host observes its Docker kinds and reports its harness.
    #[tokio::test]
    async fn the_dummy_provider_is_the_one_docker_provider_on_a_host_with_docker() {
        let mut discovery = DiscoveryRuntime::for_process();
        DummyFleet::new(DummyFleetConfig::default()).install(&mut discovery);
        let directory = tempfile::tempdir().expect("configuration");
        let config = ConfigStore::with_base(directory.path().to_path_buf());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("docker", "/usr/bin/docker"));
        let discovered = discovery
            .discover_host_scoped(
                &EnvironmentId::new("host"),
                &bag,
                &config,
                &ExecutionEnvironmentPath::new("/"),
                Arc::new(DummyCommandRunner::default()),
            )
            .await;
        let mut registry = ProviderRegistry::new();
        let mut unmet = Vec::new();
        discovered.install(&mut registry, &mut unmet);
        assert!(
            unmet.iter().any(|(_, requirement)| matches!(requirement, UnmetRequirement::MissingConfig(reason) if reason == STOOD_IN)),
            "{unmet:?}"
        );
        let (_, provider) =
            registry.environment_providers.for_legacy_input(EnvironmentInput::Docker).expect("one provider takes Docker input");
        let (_, dummy) = registry.environment_providers.select(DUMMY_ENVIRONMENT_INSTANCE).expect("the dummy instance");
        assert!(Arc::ptr_eq(provider, dummy));
    }
}
