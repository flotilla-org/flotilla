//! VCS and checkout manager factories for Git-based providers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        vcs::{git_worktree::GitCheckoutManager, CheckoutManager},
        CommandRunner,
    },
};

// ---------------------------------------------------------------------------
// GitCheckoutManagerFactory
// ---------------------------------------------------------------------------

pub struct GitCheckoutManagerFactory;

#[async_trait]
impl Factory for GitCheckoutManagerFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn CheckoutManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::CheckoutManager, "git", "git", "git worktrees", "WT", "Checkouts", "worktree")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        if env.find_binary("git").is_some() {
            let checkout_config = config.resolve_checkout_config(repo_root);
            Ok(Arc::new(GitCheckoutManager::new(checkout_config.path, runner)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("git".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::GitCheckoutManagerFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    // ── GitCheckoutManagerFactory tests ──

    #[tokio::test]
    async fn git_checkout_factory_succeeds_when_binary_available() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_checkout_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without git binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("git".into())));
    }

    #[tokio::test]
    async fn git_checkout_factory_descriptor() {
        let desc = GitCheckoutManagerFactory.descriptor();
        assert_eq!(desc.backend, "git");
        assert_eq!(desc.implementation, "git");
        assert_eq!(desc.display_name, "git worktrees");
        assert_eq!(desc.abbreviation, "WT");
        assert_eq!(desc.section_label, "Checkouts");
        assert_eq!(desc.item_noun, "worktree");
    }
}
