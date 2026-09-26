//! Checkout-scoped Git provider factory.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        vcs::{clone::ReferenceCloneStrategy, git_worktree::GitWorktreeStrategy},
        ChannelLabel, CommandRunner,
    },
    vcs::{FlotillaVcs, GitCheckoutStrategy, Vcs},
};

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

        let reference_available = env.find_env_var("FLOTILLA_ENVIRONMENT_ID").is_some()
            && runner
                .run("git", &["--git-dir", "/ref/repo", "rev-parse", "--git-dir"], Path::new("/"), &ChannelLabel::Default)
                .await
                .is_ok();
        let strategy = if reference_available {
            GitCheckoutStrategy::ReferenceClone(ReferenceCloneStrategy::new(
                Arc::clone(&runner),
                ExecutionEnvironmentPath::new("/ref/repo"),
            ))
        } else {
            let checkout_config = config.resolve_checkout_config(repo_root);
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(checkout_config.path, Arc::clone(&runner))))
        };
        Ok(Arc::new(FlotillaVcs::new(repo_root.clone(), runner, strategy)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::GitVcsFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

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
