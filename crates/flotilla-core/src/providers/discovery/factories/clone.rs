//! Clone-based checkout manager factory for container/sandbox environments.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentBag, Factory, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        vcs::{clone::CloneCheckoutManager, CheckoutManager},
        CommandRunner,
    },
};

// ---------------------------------------------------------------------------
// CloneCheckoutManagerFactory
// ---------------------------------------------------------------------------

/// Factory that produces a `CloneCheckoutManager` inside flotilla-managed
/// container environments.
///
/// Activation requires:
/// 1. `FLOTILLA_ENVIRONMENT_ID` env var present in the environment bag
/// 2. `/ref/repo` exists as a valid git directory
pub struct CloneCheckoutManagerFactory;

#[async_trait]
impl Factory for CloneCheckoutManagerFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn CheckoutManager;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled(ProviderCategory::CheckoutManager, "git", "clone", "git clone", "CL", "Checkouts", "clone")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>> {
        // Require FLOTILLA_ENVIRONMENT_ID env var
        if env.find_env_var("FLOTILLA_ENVIRONMENT_ID").is_none() {
            return Err(vec![UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())]);
        }

        // Require /ref/repo to exist as a valid git dir
        if runner
            .run(
                "git",
                &["--git-dir", "/ref/repo", "rev-parse", "--git-dir"],
                std::path::Path::new("/"),
                &crate::providers::ChannelLabel::Noop,
            )
            .await
            .is_err()
        {
            return Err(vec![UnmetRequirement::MissingBinary("git (reference repo at /ref/repo)".into())]);
        }

        Ok(Arc::new(CloneCheckoutManager::new(runner, ExecutionEnvironmentPath::new("/ref/repo"))))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CloneCheckoutManagerFactory;
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, UnmetRequirement},
    };

    #[tokio::test]
    async fn factory_succeeds_with_env_var_and_ref_repo() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "env-abc-123"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("git", &["--git-dir", "/ref/repo", "rev-parse", "--git-dir"], Ok("/ref/repo".into()))
                .build(),
        );

        let result = CloneCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok(), "factory should succeed when env var is set and git ref exists");
    }

    #[tokio::test]
    async fn factory_fails_without_env_var() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().tool_exists("git", true).build());

        let result = CloneCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without env var");
        assert!(unmet.contains(&UnmetRequirement::MissingEnvVar("FLOTILLA_ENVIRONMENT_ID".into())));
    }

    #[tokio::test]
    async fn factory_fails_without_ref_repo() {
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("FLOTILLA_ENVIRONMENT_ID", "env-abc-123"));
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        // No response queued for git --git-dir /ref/repo — runner returns Err by default
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let result = CloneCheckoutManagerFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without ref repo");
        assert!(unmet.iter().any(|u| matches!(u, UnmetRequirement::MissingBinary(_))));
    }

    #[tokio::test]
    async fn factory_descriptor() {
        let desc = CloneCheckoutManagerFactory.descriptor();
        assert_eq!(desc.backend, "git");
        assert_eq!(desc.implementation, "clone");
        assert_eq!(desc.display_name, "git clone");
        assert_eq!(desc.abbreviation, "CL");
        assert_eq!(desc.section_label, "Checkouts");
        assert_eq!(desc.item_noun, "clone");
    }
}
