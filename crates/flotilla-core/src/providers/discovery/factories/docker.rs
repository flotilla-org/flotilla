//! Environment provider factory for Docker.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        container::{docker::DockerImageStore, ImageStore},
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        environment::{docker::DockerEnvironmentProvider, EnvironmentProvider},
        CommandRunner,
    },
};

pub struct DockerEnvironmentFactory;

#[async_trait]
impl Factory for DockerEnvironmentFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn EnvironmentProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, "docker")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        if env.find_binary("docker").is_none() {
            return Err(vec![UnmetRequirement::MissingBinary("docker".into())]);
        }
        Ok(Arc::new(DockerEnvironmentProvider::new(runner)))
    }
}

/// Images share Docker detection but remain a separate host capability.
pub struct DockerImageStoreFactory;
impl DockerImageStoreFactory {
    /// Shared construction path for composition roots that initialize synchronously.
    pub fn create(env: &EnvironmentBag, runner: Arc<dyn CommandRunner>) -> Result<Arc<dyn ImageStore>, Vec<UnmetRequirement>> {
        if env.find_binary("docker").is_none() {
            return Err(vec![UnmetRequirement::MissingBinary("docker".into())]);
        }
        Ok(Arc::new(DockerImageStore::new(runner)))
    }
}
#[async_trait]
impl Factory for DockerImageStoreFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn ImageStore;
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::ImageStore, "docker")
    }
    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>> {
        Self::create(env, runner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::DockerEnvironmentFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn docker_factory_succeeds_with_binary_in_bag() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("docker", "/usr/local/bin/docker"));
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn docker_factory_does_not_probe_an_undeclared_binary() {
        let bag = EnvironmentBag::new(); // no binary in bag
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        // Runner returns success for `docker --version`
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("docker", &["--version"], Ok("Docker version 24.0.0".into())).build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn docker_factory_fails_without_docker() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        // Runner returns error for `docker --version` — docker not available
        let runner =
            Arc::new(DiscoveryMockRunner::builder().on_run("docker", &["--version"], Err("docker: command not found".into())).build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without docker");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("docker".into())));
    }

    #[tokio::test]
    async fn docker_factory_descriptor() {
        let desc = DockerEnvironmentFactory.descriptor();
        assert_eq!(desc.backend, "docker");
        assert_eq!(desc.implementation, "docker");
        assert_eq!(desc.display_name, "docker");
    }
}
