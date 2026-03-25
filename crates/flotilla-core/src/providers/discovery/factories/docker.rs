//! Environment provider factory for Docker.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        environment::{docker::DockerEnvironment, EnvironmentProvider},
        ChannelLabel, CommandRunner,
    },
};

pub struct DockerEnvironmentFactory;

#[async_trait]
impl Factory for DockerEnvironmentFactory {
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
        // Check EnvironmentBag first (preferred pattern)
        if env.find_binary("docker").is_some() {
            return Ok(Arc::new(DockerEnvironment::new(runner)));
        }
        // Fallback: try running docker directly
        match runner.run("docker", &["--version"], Path::new("/"), &ChannelLabel::Noop).await {
            Ok(_) => Ok(Arc::new(DockerEnvironment::new(runner))),
            Err(_) => Err(vec![UnmetRequirement::MissingBinary("docker".into())]),
        }
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
    async fn docker_factory_succeeds_via_fallback_run() {
        let bag = EnvironmentBag::new(); // no binary in bag
        let dir = tempfile::tempdir().expect("tempdir");
        let config = ConfigStore::with_base(dir.path());
        // Runner returns success for `docker --version`
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("docker", &["--version"], Ok("Docker version 24.0.0".into())).build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
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
