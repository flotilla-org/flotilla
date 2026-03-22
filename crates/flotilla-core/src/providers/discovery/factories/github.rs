//! GitHub factories for change request and issue tracker providers.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ConfigStore,
    providers::{
        change_request::{github::GitHubChangeRequest, ChangeRequestTracker},
        discovery::{EnvironmentBag, Factory, HostPlatform, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        github_api::GhApiClient,
        issue_tracker::{github::GitHubIssueTracker, IssueTracker},
        CommandRunner,
    },
};

fn github_repo_slug(env: &EnvironmentBag) -> Result<String, Vec<UnmetRequirement>> {
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
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn ChangeRequestTracker>, Vec<UnmetRequirement>> {
        let repo_slug = github_repo_slug(env)?;
        let api = Arc::new(GhApiClient::new(runner.clone()));
        Ok(Arc::new(GitHubChangeRequest::new("github".into(), repo_slug, api, runner)))
    }
}

// ---------------------------------------------------------------------------
// GitHubIssueTrackerFactory
// ---------------------------------------------------------------------------

pub struct GitHubIssueTrackerFactory;

#[async_trait]
impl Factory for GitHubIssueTrackerFactory {
    type Output = dyn IssueTracker;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::IssueTracker, "github", "GitHub Issues", "#", "Issues", "issue")
    }

    async fn probe(
        &self,
        env: &EnvironmentBag,
        _config: &ConfigStore,
        _repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueTracker>, Vec<UnmetRequirement>> {
        let repo_slug = github_repo_slug(env)?;
        let api = Arc::new(GhApiClient::new(runner.clone()));
        Ok(Arc::new(GitHubIssueTracker::new("github".into(), repo_slug, api, runner)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::{GitHubChangeRequestFactory, GitHubIssueTrackerFactory};
    use crate::{
        config::ConfigStore,
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
        let result = GitHubChangeRequestFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_change_request_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubChangeRequestFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
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
        let result = GitHubChangeRequestFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
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
        let result = GitHubChangeRequestFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
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

    // ── GitHubIssueTrackerFactory tests ──

    #[tokio::test]
    async fn github_issue_tracker_factory_succeeds() {
        let bag = bag_with_gh_and_github_remote();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueTrackerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_gh() {
        let bag = bag_with_github_remote_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueTrackerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without gh binary");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_remote() {
        let bag = bag_with_gh_binary_only();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueTrackerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail without remote host");
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
        assert!(!unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_missing_both() {
        let bag = EnvironmentBag::new();
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path());
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let result = GitHubIssueTrackerFactory.probe(&bag, &config, Path::new("/repo"), runner).await;
        let unmet = result.err().expect("should fail with both missing");
        assert!(unmet.contains(&UnmetRequirement::MissingBinary("gh".into())));
        assert!(unmet.contains(&UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub)));
        assert_eq!(unmet.len(), 2);
    }

    #[tokio::test]
    async fn github_issue_tracker_factory_descriptor() {
        let desc = GitHubIssueTrackerFactory.descriptor();
        assert_eq!(desc.backend, "github");
        assert_eq!(desc.implementation, "github");
        assert_eq!(desc.display_name, "GitHub Issues");
        assert_eq!(desc.abbreviation, "#");
        assert_eq!(desc.section_label, "Issues");
        assert_eq!(desc.item_noun, "issue");
    }
}
