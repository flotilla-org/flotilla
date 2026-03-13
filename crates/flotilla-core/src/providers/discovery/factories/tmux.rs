//! Workspace manager factory for tmux.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::discovery::{EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement};
use crate::providers::workspace::tmux::TmuxWorkspaceManager;
use crate::providers::workspace::WorkspaceManager;
use crate::providers::CommandRunner;

pub struct TmuxWorkspaceManagerFactory;

#[async_trait]
impl Factory for TmuxWorkspaceManagerFactory {
    type Output = dyn WorkspaceManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("tmux", "tmux Workspaces", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        if env.find_env_var("TMUX").is_some() {
            Ok(Arc::new(TmuxWorkspaceManager::new(runner)))
        } else {
            Err(vec![UnmetRequirement::MissingEnvVar("TMUX".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{
        EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement,
    };

    use super::TmuxWorkspaceManagerFactory;

    #[tokio::test]
    async fn tmux_factory_succeeds_with_env_var() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var(
            "TMUX",
            "/tmp/tmux-1001/default,12345,0",
        ));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = TmuxWorkspaceManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn tmux_factory_fails_without_env_var() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = TmuxWorkspaceManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without TMUX env var");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("TMUX".into())));
    }

    #[tokio::test]
    async fn tmux_factory_descriptor() {
        let desc = TmuxWorkspaceManagerFactory.descriptor();
        assert_eq!(desc.name, "tmux");
        assert_eq!(desc.display_name, "tmux Workspaces");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
