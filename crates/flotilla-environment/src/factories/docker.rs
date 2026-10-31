//! Environment provider factory for Docker.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;

use crate::environment::docker::DockerEnvironmentProvider;
use crate::environment::docker_vessels::DockerVessels;
use crate::environment::EnvironmentProvider;
#[cfg(unix)]
use crate::environment::{agent::client::AgentLink, docker::DockerAgentSettings};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;

#[derive(Default)]
pub struct DockerEnvironmentFactory {
    #[cfg(unix)]
    link: Option<Arc<dyn AgentLink>>,
}

#[cfg(unix)]
impl DockerEnvironmentFactory {
    /// Providers whose vessels' agents are reached through `link`.
    pub fn with_agent_link(link: Arc<dyn AgentLink>) -> Self {
        Self { link: Some(link) }
    }
}

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
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn EnvironmentProvider>, Vec<UnmetRequirement>> {
        let vessels = config
            .load_docker_vessels_for_probe()
            .await
            .and_then(|table| table.map(DockerVessels::parse).transpose())
            .map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?;
        let build = || {
            let provider = DockerEnvironmentProvider::new(runner.clone()).with_vessel_slice(vessels.clone());
            #[cfg(unix)]
            let provider = provider.with_agent_settings(DockerAgentSettings {
                runtime_root: {
                    use sha2::{Digest, Sha256};
                    Path::new("/tmp").join(&format!("fd-{:x}", Sha256::digest(config.state_dir().to_string().as_bytes()))[..23])
                },
                executable: env
                    .find_binary("drydock-agent")
                    .map(|binary| binary.as_path().to_path_buf())
                    .or_else(|| {
                        env.find_binary("flotilla").and_then(|binary| binary.as_path().parent().map(|parent| parent.join("drydock-agent")))
                    })
                    .unwrap_or_else(|| Path::new("/usr/local/bin/drydock-agent").to_path_buf()),
            });
            #[cfg(unix)]
            let provider = match &self.link {
                Some(link) => provider.with_agent_link(link.clone()),
                None => provider,
            };
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
        let result = DockerEnvironmentFactory::default().probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn docker_factory_succeeds_via_fallback_run() {
        let bag = EnvironmentBag::new(); // no binary in bag
        let dir = tempfile::tempdir().expect("tempdir");
        let config = TestConfig::with_base(dir.path());
        // Runner returns success for `docker --version`
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("docker", &["--version"], Ok("Docker version 24.0.0".into())).build());
        let result = DockerEnvironmentFactory::default().probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
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
        let result = DockerEnvironmentFactory::default().probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without docker");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("docker".into())));
    }

    // An invalid [docker_vessels] leaves no Docker provider, so no vessel starts
    // outside the slice it names; a valid one keeps the provider available.
    #[tokio::test]
    async fn docker_factory_requires_valid_docker_vessels() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("docker", "/usr/local/bin/docker"));
        let dir = tempfile::tempdir().expect("tempdir");
        for (table, valid) in [
            ("slice = 'flotilla-crews.slice'\nmemory_ceiling_gib = 20\n", true),
            ("slice = 'flotilla-crews.slice'\n", false),
            ("slice = 'flotilla-crews'\nmemory_ceiling_gib = 20\n", false),
        ] {
            let config = TestConfig::with_base(dir.path()).with_docker_vessels(toml::from_str(table).expect("table"));
            let runner = Arc::new(DiscoveryMockRunner::builder().build());
            let result = DockerEnvironmentFactory::default().probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
            match result {
                Ok(_) => assert!(valid, "{table} should be refused"),
                Err(unmet) => {
                    assert!(!valid, "{table} should be accepted");
                    assert!(matches!(unmet.as_slice(), [UnmetRequirement::MissingConfig(_)]), "{unmet:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn docker_factory_descriptor() {
        let desc = DockerEnvironmentFactory::default().descriptor();
        assert_eq!(desc.backend, "docker");
        assert_eq!(desc.implementation, "docker");
        assert_eq!(desc.display_name, "docker");
    }
}
