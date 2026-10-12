//! Issue-provider factory for GitHub issues.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_forge_github::github::GhApiClient;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;

use crate::github::GitHubIssueProvider;
use crate::IssueProvider;

// ---------------------------------------------------------------------------
// GitHubIssueProviderFactory
// ---------------------------------------------------------------------------

pub struct GitHubIssueProviderFactory;

#[async_trait]
impl Factory for GitHubIssueProviderFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn IssueProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::IssueProvider, "github", "GitHub Issues", "#", "Issues", "issue")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueProvider>, Vec<UnmetRequirement>> {
        if env.find_binary("gh").is_none() {
            return Err(vec![UnmetRequirement::MissingBinary("gh".into())]);
        }
        let api = Arc::new(GhApiClient::new(runner.clone()).with_persistence(config.state_dir().as_path().join("forge-cache/github/rest")));
        Ok(Arc::new(
            GitHubIssueProvider::new(api, runner, config.base_path().as_path())
                .with_poll_directory(config.state_dir().as_path().join("forge-cache/github/boards")),
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_discovery_api::provider_config::ProviderConfigView;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::{IssueRef, IssueSource};
    use flotilla_replay_testkit::testing::MockRunner;

    use super::GitHubIssueProviderFactory;
    use crate::testing::{CwdRecordingRunner, TestConfig};

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
    async fn github_issue_tracker_factory_succeeds() {
        let bag = bag_with_gh_and_github_remote();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost("github.com".into())));
    }

    #[tokio::test]
    async fn github_issue_provider_factory_does_not_require_a_repository_remote() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok(), "a source-addressed provider should be available without repository detector state");
    }

    #[tokio::test]
    async fn github_issue_provider_uses_host_config_root_not_probe_checkout() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(CwdRecordingRunner::new(
            "gh",
            &["api", "--include", "repos/owner/repo/issues/42"],
            "HTTP/2.0 200 OK\n\n{\"number\":42,\"title\":\"Issue\",\"body\":null,\"state\":\"open\",\"labels\":[]}",
        ));
        let probe_checkout = ExecutionEnvironmentPath::new("/first-checkout");
        let provider = GitHubIssueProviderFactory
            .probe(&bag, &config, &probe_checkout, runner.clone())
            .await
            .expect("GitHub issue provider should be available");

        provider
            .fetch_by_id(&IssueRef {
                source: IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() },
                id: "42".into(),
            })
            .await
            .expect("source-addressed fetch should succeed");

        assert!(runner.saw_cwd(config.base_path().as_path()));
        assert!(!runner.saw_cwd(probe_checkout.as_path()));
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_both() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(dir.path());
        let runner = Arc::new(MockRunner::new(vec![]));
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert_eq!(unmet.len(), 1);
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_descriptor() {
        let desc = GitHubIssueProviderFactory.descriptor();
        assert_eq!(desc.backend, "github");
        assert_eq!(desc.implementation, "github");
        assert_eq!(desc.display_name, "GitHub Issues");
        assert_eq!(desc.abbreviation, "#");
        assert_eq!(desc.section_label, "Issues");
        assert_eq!(desc.item_noun, "issue");
    }
}
