//! Host adoption factory.

use crate::provider_config::ProviderConfigView;

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    discovery_api::{EnvironmentAssertion, EnvironmentBag},
    providers::{
        discovery::{Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        environment::{host_direct::HostDirectEnvironmentProvider, EnvironmentProvider},
        CommandRunner,
    },
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;

pub struct HostDirectEnvironmentFactory;

#[async_trait]
impl Factory for HostDirectEnvironmentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn EnvironmentProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "host-direct")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        // Discovery exposes only asserted variables. The injected host runner
        // retains its own full process environment; these are metadata overrides.
        let environment = env
            .assertions()
            .iter()
            .filter_map(|assertion| match assertion {
                EnvironmentAssertion::EnvVarSet { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect();
        Ok(Arc::new(HostDirectEnvironmentProvider::new(runner, environment)))
    }
}
