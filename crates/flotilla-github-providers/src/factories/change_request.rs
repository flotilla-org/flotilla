//! Change-request factory for GitHub pull requests.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_forge_github::discovery::github_repo_slug;
use flotilla_forge_github::github::GhApiClient;
use flotilla_forge_github::github::GH_ACCOUNT_STATE;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::execution_context::AccountState;
use flotilla_provider_runtime::CommandRunner;

use crate::change_request::GitHubChangeRequest;
use flotilla_change_request::ChangeRequestTracker;

// ---------------------------------------------------------------------------
// GitHubChangeRequestFactory
// ---------------------------------------------------------------------------

pub struct GitHubChangeRequestFactory;

#[async_trait]
impl Factory for GitHubChangeRequestFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn ChangeRequestTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(
            ProviderCategory::ChangeRequest,
            "github",
            "GitHub Pull Requests",
            "PR",
            "Pull Requests",
            "pull request",
        )
    }

    fn account_state(&self) -> Option<AccountState> {
        Some(GH_ACCOUNT_STATE)
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        let repo_slug = github_repo_slug(env)?;
        let api = Arc::new(GhApiClient::new(runner.clone()).with_persistence(config.state_dir().as_path().join("forge-cache/github/rest")));
        let mut provider = GitHubChangeRequest::new("github".into(), repo_slug, api, runner)
            .with_poll_directory(config.state_dir().as_path().join("forge-cache/github/observations"));
        let settings = config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?.change_request;
        if let Some(login) = settings.review_bot_login {
            provider = provider.with_review_bot_login(login);
        }
        if let Some(login) = settings.operator_login {
            provider = provider.with_operator_login(login);
        }
        Ok(Arc::new(provider))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_replay_testkit::testing::MockRunner;

    use super::GitHubChangeRequestFactory;
    use crate::change_request::testing::TestConfig;

    fn bag_with_gh_and_github_remote() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")).with(EnvironmentAssertion::remote_host(
            "github.com",
            "acme",
            "widgets",
            "origin",
        ))
    }

    fn bag_with_github_remote_only() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::remote_host("github.com", "acme", "widgets", "origin"))
    }

    fn bag_with_gh_binary_only() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
    }

    #[tokio::test]
    async fn github_change_request_factory_succeeds() {
        let bag = bag_with_gh_and_github_remote();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost("github.com".into())));
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_remote() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without remote host");
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost("github.com".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_both() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail with both missing");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost("github.com".into())));
        assert_eq!(unmet.len(), 2);
    }

    #[tokio::test]
    async fn github_change_request_factory_descriptor() {
        let desc = GitHubChangeRequestFactory.descriptor();
        assert_eq!(desc.backend, "github");
        assert_eq!(desc.implementation, "github");
        assert_eq!(desc.display_name, "GitHub Pull Requests");
        assert_eq!(desc.abbreviation, "PR");
        assert_eq!(desc.section_label, "Pull Requests");
        assert_eq!(desc.item_noun, "pull request");
    }
}
