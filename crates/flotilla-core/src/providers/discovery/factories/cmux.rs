//! Workspace manager factory for cmux.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::discovery::{
    EnvironmentBag, ProviderDescriptor, UnmetRequirement, WorkspaceManagerFactory,
};
use crate::providers::workspace::cmux::CmuxWorkspaceManager;
use crate::providers::workspace::WorkspaceManager;
use crate::providers::CommandRunner;

pub struct CmuxWorkspaceManagerFactory;

#[async_trait]
impl WorkspaceManagerFactory for CmuxWorkspaceManagerFactory {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            name: "cmux".into(),
            display_name: "cmux Workspaces".into(),
            abbreviation: "".into(),
            section_label: "".into(),
            item_noun: "".into(),
        }
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        if env.find_env_var("CMUX_SOCKET_PATH").is_some() || env.find_binary("cmux").is_some() {
            Ok(Arc::new(CmuxWorkspaceManager::new(runner)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cmux".into())])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{
        EnvironmentAssertion, EnvironmentBag, UnmetRequirement, WorkspaceManagerFactory,
    };

    use super::CmuxWorkspaceManagerFactory;

    #[tokio::test]
    async fn cmux_factory_succeeds_with_env_var() {
        let mut bag = EnvironmentBag::new();
        bag.push(EnvironmentAssertion::EnvVarSet {
            key: "CMUX_SOCKET_PATH".into(),
            value: "/tmp/cmux.sock".into(),
        });
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxWorkspaceManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cmux_factory_succeeds_with_binary() {
        let mut bag = EnvironmentBag::new();
        bag.push(EnvironmentAssertion::BinaryAvailable {
            name: "cmux".into(),
            path: PathBuf::from("/usr/local/bin/cmux"),
            version: None,
        });
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxWorkspaceManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cmux_factory_fails_without_env_var_or_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxWorkspaceManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without cmux");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("cmux".into())));
    }

    #[tokio::test]
    async fn cmux_factory_descriptor() {
        let desc = CmuxWorkspaceManagerFactory.descriptor();
        assert_eq!(desc.name, "cmux");
        assert_eq!(desc.display_name, "cmux Workspaces");
        assert_eq!(desc.abbreviation, "");
        assert_eq!(desc.section_label, "");
        assert_eq!(desc.item_noun, "");
    }
}
