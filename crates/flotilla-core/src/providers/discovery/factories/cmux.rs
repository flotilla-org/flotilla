//! Workspace manager factories for cmux.
//!
//! Two factories implement the old priority chain:
//! - `CmuxInsideFactory` — requires `CMUX_SOCKET_PATH` env var, proving we're
//!   running inside cmux. Registered before zellij/tmux so it wins when active.
//! - `CmuxBinaryFallbackFactory` — requires only the cmux binary. Registered
//!   *after* zellij/tmux so env-var-detected multiplexers take priority.

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

fn cmux_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::labeled("cmux", "cmux Workspaces", "", "", "")
}

/// Matches when running *inside* cmux (`CMUX_SOCKET_PATH` is set).
pub struct CmuxInsideFactory;

#[async_trait]
impl WorkspaceManagerFactory for CmuxInsideFactory {
    fn descriptor(&self) -> ProviderDescriptor {
        cmux_descriptor()
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        if env.find_env_var("CMUX_SOCKET_PATH").is_some() {
            Ok(Arc::new(CmuxWorkspaceManager::new(runner)))
        } else {
            Err(vec![UnmetRequirement::MissingEnvVar(
                "CMUX_SOCKET_PATH".into(),
            )])
        }
    }
}

/// Matches when the cmux binary is available but we're not necessarily inside
/// cmux. Registered after zellij/tmux so they win when their env var is set.
pub struct CmuxBinaryFallbackFactory;

#[async_trait]
impl WorkspaceManagerFactory for CmuxBinaryFallbackFactory {
    fn descriptor(&self) -> ProviderDescriptor {
        cmux_descriptor()
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>> {
        if env.find_binary("cmux").is_some() {
            Ok(Arc::new(CmuxWorkspaceManager::new(runner)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("cmux".into())])
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
        EnvironmentAssertion, EnvironmentBag, UnmetRequirement, WorkspaceManagerFactory,
    };

    use super::{CmuxBinaryFallbackFactory, CmuxInsideFactory};

    // --- CmuxInsideFactory tests ---

    #[tokio::test]
    async fn inside_factory_succeeds_with_socket_env_var() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var(
            "CMUX_SOCKET_PATH",
            "/tmp/cmux.sock",
        ));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxInsideFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn inside_factory_fails_with_only_binary() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("cmux", "/usr/local/bin/cmux"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxInsideFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inside_factory_fails_empty_bag() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxInsideFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without env var");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("CMUX_SOCKET_PATH".into())));
    }

    // --- CmuxBinaryFallbackFactory tests ---

    #[tokio::test]
    async fn fallback_factory_succeeds_with_binary() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("cmux", "/usr/local/bin/cmux"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxBinaryFallbackFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fallback_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = CmuxBinaryFallbackFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without cmux binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("cmux".into())));
    }

    // --- Descriptor tests ---

    #[tokio::test]
    async fn both_factories_share_descriptor() {
        let inside = CmuxInsideFactory.descriptor();
        let fallback = CmuxBinaryFallbackFactory.descriptor();
        assert_eq!(inside.name, "cmux");
        assert_eq!(inside.display_name, "cmux Workspaces");
        assert_eq!(inside.name, fallback.name);
        assert_eq!(inside.display_name, fallback.display_name);
    }
}
