//! Environment provider factory for Docker.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

#[cfg(unix)]
use crate::environment::docker::DockerAgentSettings;
use crate::environment::docker::DockerEnvironmentProvider;
use crate::environment::EnvironmentProvider;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;

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
        _config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        let build = || {
            let provider = DockerEnvironmentProvider::new(runner.clone());
            #[cfg(unix)]
            let provider = provider.with_agent_settings(DockerAgentSettings {
                runtime_root: {
                    use sha2::{Digest, Sha256};
                    Path::new("/tmp").join(&format!("fd-{:x}", Sha256::digest(_config.state_dir().to_string().as_bytes()))[..23])
                },
                executable: env
                    .find_binary("drydock-agent")
                    .map(|binary| binary.as_path().to_path_buf())
                    .or_else(|| {
                        env.find_binary("flotilla").and_then(|binary| binary.as_path().parent().map(|parent| parent.join("drydock-agent")))
                    })
                    .unwrap_or_else(|| Path::new("/usr/local/bin/drydock-agent").to_path_buf()),
            });
            Arc::new(provider) as Arc<dyn EnvironmentProvider>
        };
        if env.find_binary("docker").is_some() {
            return Ok(build());
        }
        match runner.run("docker", &["--version"], Path::new("/"), &ChannelLabel::Default).await {
            Ok(_) => Ok(build()),
            Err(_) => Err(vec![UnmetRequirement::MissingBinary("docker".into())]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::DockerEnvironmentFactory;
    use crate::test_config::TestConfig;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_discovery_testkit::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    #[tokio::test]
    async fn docker_factory_succeeds_with_binary_in_bag() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("docker", "/usr/local/bin/docker"));
        let dir = tempfile::tempdir().expect("tempdir");
        let config = TestConfig::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn docker_factory_succeeds_via_fallback_run() {
        let bag = EnvironmentBag::new(); // no binary in bag
        let dir = tempfile::tempdir().expect("tempdir");
        let config = TestConfig::with_base(dir.path());
        // Runner returns success for `docker --version`
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("docker", &["--version"], Ok("Docker version 24.0.0".into())).build());
        let result = DockerEnvironmentFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn docker_factory_fails_without_docker() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let config = TestConfig::with_base(dir.path());
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
