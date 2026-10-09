//! Checkout-scoped Git provider factory.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        vcs::{clone::ReferenceCloneStrategy, detection::reference_clone_available, git_worktree::GitWorktreeStrategy},
        CommandRunner,
    },
    vcs::{FlotillaVcs, GitCheckoutStrategy, Vcs},
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;

pub struct GitVcsFactory;

#[async_trait]
impl Factory for GitVcsFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn Vcs;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::Vcs, "git", "git-cli", "git CLI", "GI", "Checkouts", "checkout")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>> {
        if env.find_binary("git").is_none() {
            return Err(vec![UnmetRequirement::MissingBinary("git".into())]);
        }

        let reference_available = env.find_env_var("FLOTILLA_ENVIRONMENT_ID").is_some() && reference_clone_available(&*runner).await;
        let strategy = if reference_available {
            GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(
                Arc::clone(&runner),
                ExecutionEnvironmentPath::new("/ref/repo"),
            ))
        } else {
            let checkout_config = config.resolve_checkout_config(repo_root);
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(checkout_config.path, Arc::clone(&runner))))
        };
        // Despite its name, repo_root can be an inspection subdirectory: the
        // CheckoutVcsResolver constructs this provider before resolving the top level.
        // Preserve Git discovery, including bare repositories; target probes use
        // GitCliBackend::checkout_root separately.
        Ok(Arc::new(FlotillaVcs::new(repo_root.clone(), runner, strategy)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::GitVcsFactory;
    use crate::config::ConfigStore;
    use crate::providers::discovery::EnvironmentAssertion;
    use crate::providers::discovery::EnvironmentBag;
    use crate::providers::discovery::Factory;
    use crate::providers::discovery::UnmetRequirement;
    use crate::testkits::discovery::DiscoveryMockRunner;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;

    #[tokio::test]
    async fn git_factory_succeeds_when_binary_available() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        assert!(GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await.is_ok());
    }

    #[tokio::test]
    async fn environment_reference_selects_clone_strategy() {
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
            .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "env-1"));
        let dir = tempfile::tempdir().expect("config dir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("git", &["--git-dir", "/ref/repo", "rev-parse", "--git-dir"], Ok("/ref/repo".into()))
                .on_run("ls", &["-1", "/workspace"], Ok(String::new()))
                .build(),
        );
        let vcs = GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await.expect("git provider");
        assert!(vcs.list_checkouts().await.expect("clone checkout enumeration").is_empty());
    }

    #[tokio::test]
    async fn missing_reference_selects_worktree_strategy() {
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
            .with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "env-1"));
        let dir = tempfile::tempdir().expect("config dir");
        let config = ConfigStore::with_base(dir.path());
        let runner =
            Arc::new(DiscoveryMockRunner::builder().on_run("git", &["worktree", "list", "--porcelain"], Ok(String::new())).build());
        let vcs = GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await.expect("git provider");
        assert!(vcs.list_checkouts().await.expect("worktree checkout enumeration").is_empty());
    }

    #[tokio::test]
    async fn git_factory_fails_without_binary() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let unmet = GitVcsFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await.err().expect("missing git");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("git".into())));
    }
}
