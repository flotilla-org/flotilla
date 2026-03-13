//! Workspace manager factory for zellij.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement},
        workspace::{zellij::ZellijWorkspaceManager, WorkspaceManager},
        CommandRunner,
    },
};

pub struct ZellijWorkspaceManagerFactory;

#[async_trait]
impl Factory for ZellijWorkspaceManagerFactory {
    type Output = dyn WorkspaceManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("zellij", "zellij Workspaces", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        if env.find_env_var("ZELLIJ").is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar("ZELLIJ".into())]);
        }

        ZellijWorkspaceManager::check_version(&*runner).await.map_err(|e| vec![UnmetRequirement::MissingBinary(e)])?;

        Ok(Arc::new(ZellijWorkspaceManager::new(runner)))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::ZellijWorkspaceManagerFactory;
    use crate::{
        config::ConfigStore,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn zellij_factory_succeeds_with_env_var_and_version() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Ok("zellij 0.42.2".into())).build());
        let result = ZellijWorkspaceManagerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn zellij_factory_fails_without_env_var() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ZellijWorkspaceManagerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without ZELLIJ env var");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("ZELLIJ".into())));
    }

    #[tokio::test]
    async fn zellij_factory_fails_with_old_version() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Ok("zellij 0.39.0".into())).build());
        let result = ZellijWorkspaceManagerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn zellij_factory_fails_when_version_check_errors() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Err("command not found".into())).build());
        let result = ZellijWorkspaceManagerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn zellij_factory_descriptor() {
        let desc = ZellijWorkspaceManagerFactory.descriptor();
        assert_eq!(desc.name, "zellij");
        assert_eq!(desc.display_name, "zellij Workspaces");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
