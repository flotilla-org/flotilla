//! VCS and checkout manager factories for Git-based providers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement, VcsKind},
        vcs::{git::GitVcs, git_worktree::GitCheckoutManager, wt::WtCheckoutManager, CheckoutManager, Vcs},
        CommandRunner,
    },
};

// ---------------------------------------------------------------------------
// GitVcsFactory
// ---------------------------------------------------------------------------

pub struct GitVcsFactory;

#[async_trait]
impl Factory for GitVcsFactory {
    type Output = dyn Vcs;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::Vcs, "git", "Git", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>> {
        if env.find_vcs_checkout(VcsKind::Git).is_some() {
            Ok(Arc::new(GitVcs::new(runner)))
        } else {
            Err(vec![UnmetRequirement::NoVcsCheckout])
        }
    }
}

// ---------------------------------------------------------------------------
// WtCheckoutManagerFactory
// ---------------------------------------------------------------------------

pub struct WtCheckoutManagerFactory;

#[async_trait]
impl Factory for WtCheckoutManagerFactory {
    type Output = dyn CheckoutManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::CheckoutManager, "git", "wt", "wt", "CO", "Checkouts", "checkout")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        if env.find_binary("wt").is_some() {
            let host_name = env.host_name().cloned().unwrap_or_else(flotilla_protocol::HostName::local);
            Ok(Arc::new(WtCheckoutManager::new(runner, host_name)))
        } else {
            Err(vec![UnmetRequirement::MissingBinary("wt".into())])
        }
    }
}

// ---------------------------------------------------------------------------
// GitCheckoutManagerFactory
// ---------------------------------------------------------------------------

pub struct GitCheckoutManagerFactory;

#[async_trait]
impl Factory for GitCheckoutManagerFactory {
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
            let host_name = env.host_name().cloned().unwrap_or_else(flotilla_protocol::HostName::local);
            Ok(Arc::new(GitCheckoutManager::new(checkout_config.path, runner, host_name)))
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

    use super::{GitCheckoutManagerFactory, GitVcsFactory, WtCheckoutManagerFactory};
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{
            test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement, VcsKind,
        },
    };

    // ── GitVcsFactory tests ──

    #[tokio::test]
    async fn git_vcs_factory_succeeds_with_git_checkout() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::vcs_checkout("/repo", VcsKind::Git, true));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_vcs_factory_fails_without_checkout() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without checkout");
        assert!(unmet.contains(&UnmetRequirement::NoVcsCheckout));
    }

    #[tokio::test]
    async fn git_vcs_factory_descriptor() {
        let desc = GitVcsFactory.descriptor();
        assert_eq!(desc.backend, "git");
        assert_eq!(desc.implementation, "git");
        assert_eq!(desc.display_name, "Git");
    }

    // ── WtCheckoutManagerFactory tests ──

    #[tokio::test]
    async fn wt_factory_succeeds_when_binary_available() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("wt", "/usr/local/bin/wt"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wt_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without wt binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("wt".into())));
    }

    #[tokio::test]
    async fn wt_factory_descriptor() {
        let desc = WtCheckoutManagerFactory.descriptor();
        assert_eq!(desc.backend, "git");
        assert_eq!(desc.implementation, "wt");
        assert_eq!(desc.display_name, "wt");
        assert_eq!(desc.abbreviation, "CO");
        assert_eq!(desc.section_label, "Checkouts");
        assert_eq!(desc.item_noun, "checkout");
    }

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
