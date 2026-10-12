//! GitHub and Forgejo factories for issue tracker providers.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_forge_forgejo::discovery::bounded_forgejo_provider_config;
use flotilla_forge_github::github::GhApiClient;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::ReqwestHttpClient;

use crate::providers::issue_tracker::forgejo::ForgejoIssueProvider;
use crate::providers::issue_tracker::github::GitHubIssueProvider;
use crate::providers::issue_tracker::IssueProvider;

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
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        _repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueProvider>, Vec<UnmetRequirement>> {
        let settings = config.load_config_for_probe().await.map_err(|error| vec![UnmetRequirement::MissingConfig(error)])?;
        let provider_config = bounded_forgejo_provider_config(env, config, &settings).await?;
        Ok(Arc::new(ForgejoIssueProvider::new(Arc::new(ReqwestHttpClient::new()), runner, provider_config)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use flotilla_change_request::factories::forgejo::ForgejoChangeRequestFactory;
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::{IssueRef, IssueSource};
    use flotilla_resources::{ForgeKind, ForgeSpec};

    use super::{ForgejoIssueProviderFactory, GitHubIssueProviderFactory};
    use crate::config::ConfigStore;
    use crate::testkits::discovery::DiscoveryMockRunner;

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
        assert!(!unmet.contains(&UnmetRequirement::MissingRemoteHost("github.com".into())));
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

    #[tokio::test]
    async fn forgejo_origin_binds_issue_and_change_request_without_config_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_token(dir.path(), "lab-forgejo-coder-token");
        let config = ConfigStore::with_base(dir.path().join("flotilla"));
        let forge = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo.lab.flotilla.work".into()]))
            .https_url("https://forgejo.lab.flotilla.work".into())
            .git_ssh_host("forgejo.lab.flotilla.work".into())
            .build();
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::remote_host("forgejo.lab.flotilla.work", "lab", "flotilla", "origin"))
            .with(EnvironmentAssertion::origin_forge(forge))
            .with(EnvironmentAssertion::auth_file("forgejo", dir.path().join("lab-forgejo-coder-token")));
        let root = ExecutionEnvironmentPath::new("/repo");
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let issues = ForgejoIssueProviderFactory.probe(&bag, &config, &root, runner.clone()).await.expect("Forgejo issue source");
        let changes = ForgejoChangeRequestFactory.probe(&bag, &config, &root, runner).await.expect("Forgejo CR source");
        assert!(issues.supports(&IssueSource { service: "https://forgejo.lab.flotilla.work".into(), scope: "lab/flotilla".into() }));
        assert_eq!(ForgejoChangeRequestFactory.descriptor().implementation, "forgejo");
        let _ = changes;
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

        assert!(provider.supports(&IssueSource { service: "https://forgejo.example.test".into(), scope: "team/widgets".into() }));
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
            .any(|requirement| matches!(requirement, UnmetRequirement::MissingConfig(key) if key == "[issue_tracker.forgejo].service_url or Forge")));
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
            |requirement| matches!(requirement, UnmetRequirement::MissingConfig(key) if key == "[issue_tracker.forgejo].service_url or Forge")
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
