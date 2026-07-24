//! GitHub and Forgejo factories for change request and issue tracker providers.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::{ConfigStore, ForgejoIssueTrackerConfig},
    path_context::ExecutionEnvironmentPath,
    providers::{
        change_request::{github::GitHubChangeRequest, ChangeRequestTracker},
        discovery::{EnvironmentBag, Factory, HostPlatform, ProviderCategory, ProviderDescriptor, UnmetRequirement},
        github_api::GhApiClient,
        issue_tracker::{
            forgejo::{ForgejoAuth, ForgejoIssueProvider, ForgejoIssueProviderConfig},
            github::GitHubIssueProvider,
            IssueProvider,
        },
        CommandRunner, ReqwestHttpClient,
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
// ForgejoIssueProviderFactory
// ---------------------------------------------------------------------------

pub struct ForgejoIssueProviderFactory;

#[async_trait]
impl Factory for ForgejoIssueProviderFactory {
    type Descriptor = ProviderDescriptor;
    type Output = dyn IssueProvider;

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::labeled_simple(ProviderCategory::IssueProvider, "forgejo", "Forgejo Issues", "#", "Issues", "issue")
    }

    async fn probe(
        &self,
        _env: &EnvironmentBag,
        config: &ConfigStore,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueProvider>, Vec<UnmetRequirement>> {
        let Some(forgejo) = config.load_config().issue_tracker.forgejo else {
            return Err(vec![UnmetRequirement::MissingConfig("[issue_tracker.forgejo]".into())]);
        };
        let service_url = forgejo
            .service_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| vec![UnmetRequirement::MissingConfig("[issue_tracker.forgejo].service_url".into())])?;
        let auth = resolve_forgejo_auth(config, &forgejo).map_err(|error| {
            tracing::warn!(%error, "Forgejo authentication unavailable");
            vec![UnmetRequirement::MissingAuth("forgejo".into())]
        })?;
        let provider_config = ForgejoIssueProviderConfig::new(service_url.into(), forgejo.api_base_url, auth);
        Ok(Arc::new(ForgejoIssueProvider::new(Arc::new(ReqwestHttpClient::new()), runner, provider_config)))
    }
}

fn resolve_forgejo_auth(config: &ConfigStore, forgejo: &ForgejoIssueTrackerConfig) -> Result<ForgejoAuth, String> {
    let path = resolve_forgejo_token_path(config, forgejo)?;
    let token =
        std::fs::read_to_string(&path).map_err(|error| format!("forgejo token file {}: {error}", path.display()))?.trim().to_string();
    if token.is_empty() {
        return Err(format!("forgejo token file {} is empty", path.display()));
    }
    Ok(ForgejoAuth { token, token_path: path })
}

fn resolve_forgejo_token_path(config: &ConfigStore, forgejo: &ForgejoIssueTrackerConfig) -> Result<PathBuf, String> {
    let config_parent =
        config.base_path().as_path().parent().map(PathBuf::from).unwrap_or_else(|| config.base_path().as_path().to_path_buf());
    if let Some(path) = &forgejo.token_path {
        return Ok(config_path(&config_parent, path));
    }
    if let Some(agent) = &forgejo.token_agent {
        return Ok(config_parent.join(format!("lab-forgejo-{agent}-token")));
    }
    let mut candidates = std::fs::read_dir(&config_parent)
        .map_err(|error| format!("read {}: {error}", config_parent.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("lab-forgejo-") && name.ends_with("-token"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "no lab Forgejo token file found; set [issue_tracker.forgejo].token_path or create {}/lab-forgejo-<agent>-token",
            config_parent.display()
        )),
        _ => Err("multiple lab Forgejo token files found; set [issue_tracker.forgejo].token_path or token_agent".into()),
    }
}

fn config_path(config_parent: &std::path::Path, path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/.config/") {
        return config_parent.join(rest);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        config_parent.join(path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_protocol::{IssueRef, IssueSource};

    use super::{config_path, ForgejoIssueProviderFactory, GitHubChangeRequestFactory, GitHubIssueProviderFactory};
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

    // ── ForgejoIssueProviderFactory tests ──

    fn write_token(base: &std::path::Path, name: &str) {
        std::fs::create_dir_all(base).expect("create config parent");
        std::fs::write(base.join(name), "test-token\n").expect("write token");
    }

    fn write_forgejo_config(config_base: &std::path::Path, body: &str) {
        std::fs::create_dir_all(config_base).expect("create config base");
        std::fs::write(
            config_base.join("config.toml"),
            format!("[issue_tracker.forgejo]\nservice_url = \"https://forgejo.example.test\"\n{body}"),
        )
        .expect("write config");
    }

    #[test]
    fn config_path_expands_home_relative_token_paths() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let resolved = config_path(std::path::Path::new("/tmp/config-parent"), "~/lab-token");

        assert_eq!(resolved, home.join("lab-token"));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_uses_single_lab_token_file() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_parent = dir.path();
        write_token(config_parent, "lab-forgejo-coder-token");
        let config_base = config_parent.join("flotilla");
        write_forgejo_config(&config_base, "");
        let config = ConfigStore::with_base(config_base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let provider = ForgejoIssueProviderFactory
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .expect("Forgejo issue provider should be available");

        assert!(provider.supports(&IssueSource { service: "https://forgejo.example.test".into(), scope: "fork-issues/zellij".into() }));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_honors_configured_token_agent() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_parent = dir.path();
        let config_base = config_parent.join("flotilla");
        write_token(config_parent, "lab-forgejo-coder-token");
        write_token(config_parent, "lab-forgejo-planner-token");
        write_forgejo_config(&config_base, "token_agent = \"planner\"\n");
        let config = ConfigStore::with_base(config_base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let provider = ForgejoIssueProviderFactory
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .expect("Forgejo issue provider should use configured token agent");

        assert!(provider.supports(&IssueSource { service: "forgejo".into(), scope: "team/widgets".into() }));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_requires_configuration() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = ConfigStore::with_base(dir.path().join("flotilla"));
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let unmet = ForgejoIssueProviderFactory
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .err()
            .expect("should fail without Forgejo configuration");

        assert!(unmet
            .iter()
            .any(|requirement| matches!(requirement, UnmetRequirement::MissingConfig(key) if key == "[issue_tracker.forgejo]")));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_requires_service_url() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_base = dir.path().join("flotilla");
        std::fs::create_dir_all(&config_base).expect("create config base");
        std::fs::write(config_base.join("config.toml"), "[issue_tracker.forgejo]\ntoken_agent = \"coder\"\n").expect("write config");
        let config = ConfigStore::with_base(config_base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let unmet = ForgejoIssueProviderFactory
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .err()
            .expect("should fail without Forgejo service URL");

        assert!(unmet.iter().any(
            |requirement| matches!(requirement, UnmetRequirement::MissingConfig(key) if key == "[issue_tracker.forgejo].service_url")
        ));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_reports_missing_auth_without_token() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_base = dir.path().join("flotilla");
        write_forgejo_config(&config_base, "");
        let config = ConfigStore::with_base(config_base);
        let runner = Arc::new(DiscoveryMockRunner::builder().build());

        let unmet = ForgejoIssueProviderFactory
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .err()
            .expect("should fail without Forgejo token");

        assert!(unmet.iter().any(|requirement| matches!(requirement, UnmetRequirement::MissingAuth(provider) if provider == "forgejo")));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_descriptor() {
        let desc = ForgejoIssueProviderFactory.descriptor();
        assert_eq!(desc.backend, "forgejo");
        assert_eq!(desc.implementation, "forgejo");
        assert_eq!(desc.display_name, "Forgejo Issues");
        assert_eq!(desc.abbreviation, "#");
        assert_eq!(desc.section_label, "Issues");
        assert_eq!(desc.item_noun, "issue");
    }
}
