//! VCS and checkout manager factories for Git-based providers.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::discovery::{
    EnvironmentBag, Factory, ProviderDescriptor, UnmetRequirement, VcsKind,
};
use crate::providers::vcs::git::GitVcs;
use crate::providers::vcs::git_worktree::GitCheckoutManager;
use crate::providers::vcs::wt::WtCheckoutManager;
use crate::providers::vcs::{CheckoutManager, Vcs};
use crate::providers::CommandRunner;

// ---------------------------------------------------------------------------
// GitVcsFactory
// ---------------------------------------------------------------------------

pub struct GitVcsFactory;

#[async_trait]
impl Factory for GitVcsFactory {
    type Output = dyn Vcs;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled("git", "Git", "", "", "")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
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
        ProviderDescriptor::labeled("wt", "wt", "CO", "Checkouts", "checkout")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        let checkouts_config = config.resolve_checkouts_config(repo_root);
        let provider = checkouts_config.provider.as_str();

        // If config explicitly names a different provider, yield gracefully.
        if provider != "auto" && provider != "wt" {
            return Err(vec![]);
        }

        if env.find_binary("wt").is_some() {
            Ok(Arc::new(WtCheckoutManager::new(runner)))
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
        ProviderDescriptor::labeled("git", "git worktrees", "WT", "Checkouts", "worktree")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        let checkouts_config = config.resolve_checkouts_config(repo_root);
        let provider = checkouts_config.provider.as_str();

        // If config explicitly names a different provider, yield gracefully.
        if provider != "auto" && provider != "git" {
            return Err(vec![]);
        }

        if env.find_binary("git").is_some() {
            Ok(Arc::new(GitCheckoutManager::new(checkouts_config, runner)))
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
    use std::path::Path;
    use std::sync::Arc;

    use crate::config::ConfigStore;
    use crate::providers::discovery::test_support::DiscoveryMockRunner;
    use crate::providers::discovery::{
        EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement, VcsKind,
    };

    use super::{GitCheckoutManagerFactory, GitVcsFactory, WtCheckoutManagerFactory};

    // ── GitVcsFactory tests ──

    #[tokio::test]
    async fn git_vcs_factory_succeeds_with_git_checkout() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::vcs_checkout(
            "/repo",
            VcsKind::Git,
            true,
        ));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitVcsFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_vcs_factory_fails_without_checkout() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitVcsFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without checkout");
        assert!(unmet.contains(&UnmetRequirement::NoVcsCheckout));
    }

    #[tokio::test]
    async fn git_vcs_factory_descriptor() {
        let desc = GitVcsFactory.descriptor();
        assert_eq!(desc.name, "git");
        assert_eq!(desc.display_name, "Git");
    }

    // ── WtCheckoutManagerFactory tests ──

    #[tokio::test]
    async fn wt_factory_succeeds_when_binary_available() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("wt", "/usr/local/bin/wt"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wt_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without wt binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("wt".into())));
    }

    #[tokio::test]
    async fn wt_factory_excluded_by_config_git() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("wt", "/usr/local/bin/wt"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path();
        // Write config that forces provider = "git"
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"git\"\n",
        )
        .expect("failed to write config");
        let config = ConfigStore::with_base(base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        // Config exclusion returns empty unmet list
        let unmet = result.err().expect("should be excluded by config");
        assert!(unmet.is_empty());
    }

    #[tokio::test]
    async fn wt_factory_allowed_by_config_auto() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("wt", "/usr/local/bin/wt"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        // Default config has provider = "auto"
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wt_factory_allowed_by_config_wt() {
        let bag =
            EnvironmentBag::new().with(EnvironmentAssertion::binary("wt", "/usr/local/bin/wt"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path();
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"wt\"\n",
        )
        .expect("failed to write config");
        let config = ConfigStore::with_base(base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = WtCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wt_factory_descriptor() {
        let desc = WtCheckoutManagerFactory.descriptor();
        assert_eq!(desc.name, "wt");
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
        let result = GitCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_checkout_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        let unmet = result.err().expect("should fail without git binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("git".into())));
    }

    #[tokio::test]
    async fn git_checkout_factory_excluded_by_config_wt() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path();
        // Write config that forces provider = "wt"
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"wt\"\n",
        )
        .expect("failed to write config");
        let config = ConfigStore::with_base(base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        // Config exclusion returns empty unmet list
        let unmet = result.err().expect("should be excluded by config");
        assert!(unmet.is_empty());
    }

    #[tokio::test]
    async fn git_checkout_factory_allowed_by_config_auto() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_checkout_factory_allowed_by_config_git() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let base = dir.path();
        std::fs::write(
            base.join("config.toml"),
            "[vcs.git.checkouts]\nprovider = \"git\"\n",
        )
        .expect("failed to write config");
        let config = ConfigStore::with_base(base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitCheckoutManagerFactory
            .probe(&bag, &config, Path::new("/repo"), runner)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn git_checkout_factory_descriptor() {
        let desc = GitCheckoutManagerFactory.descriptor();
        assert_eq!(desc.name, "git");
        assert_eq!(desc.display_name, "git worktrees");
        assert_eq!(desc.abbreviation, "WT");
        assert_eq!(desc.section_label, "Checkouts");
        assert_eq!(desc.item_noun, "worktree");
    }
}
