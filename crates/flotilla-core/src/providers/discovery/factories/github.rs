//! GitHub factories for change request and issue tracker providers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        change_request::{github::GitHubChangeRequest, ChangeRequestTracker},
        discovery::{EnvironmentBag, Factory, HostPlatform, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        github_api::GhApiClient,
        issue_tracker::{github::GitHubIssueProvider, IssueProvider},
        CommandRunner,
    },
};

pub(super) fn github_repo_slug(env: &EnvironmentBag) -> Result<String, Vec<UnmetRequirement>> {
    let mut unmet = vec![];
    if env.find_binary("gh").is_none() {
        unmet.push(UnmetRequirement::MissingBinary("gh".into()));
    }
    let remote = env.find_remote_host(HostPlatform::GitHub);
    if remote.is_none() {
        unmet.push(UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub));
    }
    if !unmet.is_empty() {
        return Err(unmet);
    }
    let (owner, repo, _remote_name) = remote.expect("checked above");
    Ok(format!("{owner}/{repo}"))
}

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

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        let repo_slug = github_repo_slug(env)?;
        let api = Arc::new(GhApiClient::new(runner.clone()));
        Ok(Arc::new(GitHubChangeRequest::new("github".into(), repo_slug, api, runner)))
    }
}

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
        config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueProvider>, Vec<UnmetRequirement>> {
        if env.find_binary("gh").is_none() {
            return Err(vec![UnmetRequirement::MissingBinary("gh".into())]);
        }
        let api = Arc::new(GhApiClient::new(runner.clone()));
        Ok(Arc::new(GitHubIssueProvider::new(api, runner, config.base_path().as_path())))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_protocol::{IssueRef, IssueSource};

    use super::{GitHubChangeRequestFactory, GitHubIssueProviderFactory};
    use crate::{
        config::ConfigStore,
        path_context::ExecutionEnvironmentPath,
        providers::discovery::{
            test_support::DiscoveryMockRunner, EnvironmentAssertion, EnvironmentBag, Factory, HostPlatform, UnmetRequirement,
        },
    };

    fn bag_with_gh_and_github_remote() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")).with(EnvironmentAssertion::remote_host(
            HostPlatform::GitHub,
            "acme",
            "widgets",
            "origin",
        ))
    }

    fn bag_with_github_remote_only() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::remote_host(HostPlatform::GitHub, "acme", "widgets", "origin"))
    }

    fn bag_with_gh_binary_only() -> EnvironmentBag {
        EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
    }

    // ── GitHubChangeRequestFactory tests ──

    #[tokio::test]
    async fn github_change_request_factory_succeeds() {
        let bag = bag_with_gh_and_github_remote();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_remote() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without remote host");
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
        assert!(!unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_both() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubChangeRequestFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail with both missing");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
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

    // ── GitHubIssueProviderFactory tests ──

    #[tokio::test]
    async fn github_issue_tracker_factory_succeeds() {
        let bag = bag_with_gh_and_github_remote();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
    }

    #[tokio::test]
    async fn github_issue_provider_factory_does_not_require_a_repository_remote() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueProviderFactory.probe(&bag, &config, &ExecutionEnvironmentPath::new("/repo"), runner).await;
        assert!(result.is_ok(), "a source-addressed provider should be available without repository detector state");
    }

    #[tokio::test]
    async fn github_issue_provider_uses_host_config_root_not_probe_checkout() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run(
                    "gh",
                    &["api", "--include", "repos/owner/repo/issues/42"],
                    Ok("HTTP/2.0 200 OK\n\n{\"number\":42,\"title\":\"Issue\",\"body\":null,\"state\":\"open\",\"labels\":[]}".into()),
                )
                .build(),
        );
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
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
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
