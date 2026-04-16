//! Presentation manager factory for zellij.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        presentation::{zellij::ZellijPresentationManager, PresentationManager},
        CommandRunner,
    },
};

pub struct ZellijPresentationManagerFactory;

#[async_trait]
impl Factory for ZellijPresentationManagerFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn PresentationManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "zellij", "zellij Workspaces", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn PresentationManager>, Vec<UnmetRequirement>> {
        if env.find_env_var("ZELLIJ").is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar("ZELLIJ".into())]);
        }

        ZellijPresentationManager::check_version(&*runner).await.map_err(|e| vec![UnmetRequirement::MissingBinary(e)])?;

        let mgr = match env.find_env_var("ZELLIJ_SESSION_NAME") {
            Some(name) => ZellijPresentationManager::with_session_name(runner, name.to_string()),
            None => ZellijPresentationManager::new(runner),
        };
        Ok(Arc::new(mgr))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::ZellijPresentationManagerFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn zellij_factory_succeeds_with_env_var_and_version() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Ok("zellij 0.42.2".into())).build());
        let result = ZellijPresentationManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn zellij_factory_fails_without_env_var() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = ZellijPresentationManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without ZELLIJ env var");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("ZELLIJ".into())));
    }

    #[tokio::test]
    async fn zellij_factory_fails_with_old_version() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Ok("zellij 0.39.0".into())).build());
        let result = ZellijPresentationManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn zellij_factory_fails_when_version_check_errors() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("ZELLIJ", "0"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Err("command not found".into())).build());
        let result = ZellijPresentationManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn zellij_factory_succeeds_with_session_name_env() {
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("ZELLIJ", "0"))
            .with(EnvironmentAssertion::env_var("ZELLIJ_SESSION_NAME", "my-session"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().on_run("zellij", &["--version"], Ok("zellij 0.42.2".into())).build());
        let result = ZellijPresentationManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok(), "factory should succeed when ZELLIJ_SESSION_NAME is set");
    }

    #[tokio::test]
    async fn zellij_factory_descriptor() {
        let desc = ZellijPresentationManagerFactory.descriptor();
        assert_eq!(desc.backend, "zellij");
        assert_eq!(desc.implementation, "zellij");
        assert_eq!(desc.display_name, "zellij Workspaces");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
