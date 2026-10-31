//! Issue-provider factory for Forgejo issues.

use std::sync::Arc;

use async_trait::async_trait;
use flotilla_discovery_api::provider_config::ProviderConfigView;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::Factory;
use flotilla_discovery_api::ProviderCategory;
use flotilla_discovery_api::ProviderDescriptor;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_forge_forgejo::discovery::bounded_forgejo_provider_config;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::{HttpClient, ReqwestHttpClient};

use crate::issue_tracker::ForgejoIssueProvider;
use flotilla_issue_tracker::IssueProvider;

// ---------------------------------------------------------------------------
// ForgejoIssueProviderFactory
// ---------------------------------------------------------------------------

pub struct ForgejoIssueProviderFactory {
    http: Arc<dyn HttpClient>,
}

impl ForgejoIssueProviderFactory {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self { http }
    }
}

impl Default for ForgejoIssueProviderFactory {
    fn default() -> Self {
        Self::new(Arc::new(ReqwestHttpClient::new()))
    }
}

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
        let (provider_config, http) = bounded_forgejo_provider_config(env, config, &settings, Arc::clone(&self.http)).await?;
        Ok(Arc::new(ForgejoIssueProvider::new(http, runner, provider_config)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_config::provider_config::{ForgejoIssueTrackerConfig, IssueTrackerConfig};
    use flotilla_discovery_api::provider_config::ProviderConfig;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::Factory;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::IssueSource;
    use flotilla_replay_testkit::testing::MockRunner;

    use super::ForgejoIssueProviderFactory;
    use crate::issue_tracker::testing::TestConfig;

    fn write_token(base: &std::path::Path, name: &str) {
        std::fs::create_dir_all(base).expect("create config parent");
        std::fs::write(base.join(name), "test-token\n").expect("write token");
    }

    /// Settings equivalent to an `[issue_tracker.forgejo]` section; core's
    /// config tests cover decoding that section from `config.toml`.
    fn forgejo_settings(forgejo: ForgejoIssueTrackerConfig) -> ProviderConfig {
        ProviderConfig { issue_tracker: IssueTrackerConfig { forgejo: Some(forgejo), ..Default::default() }, ..Default::default() }
    }

    fn example_service(token_agent: Option<&str>) -> ProviderConfig {
        forgejo_settings(ForgejoIssueTrackerConfig {
            service_url: Some("https://forgejo.example.test".into()),
            token_agent: token_agent.map(Into::into),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_uses_single_lab_token_file() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_parent = dir.path();
        write_token(config_parent, "lab-forgejo-coder-token");
        let config = TestConfig::with_settings(&config_parent.join("flotilla"), example_service(None));
        let runner = Arc::new(MockRunner::new(vec![]));

        let provider = ForgejoIssueProviderFactory::default()
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .expect("Forgejo issue provider should be available");

        assert!(provider.supports(&IssueSource { service: "https://forgejo.example.test".into(), scope: "fork-issues/zellij".into() }));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_honors_configured_token_agent() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config_parent = dir.path();
        write_token(config_parent, "lab-forgejo-coder-token");
        write_token(config_parent, "lab-forgejo-planner-token");
        let config = TestConfig::with_settings(&config_parent.join("flotilla"), example_service(Some("planner")));
        let runner = Arc::new(MockRunner::new(vec![]));

        let provider = ForgejoIssueProviderFactory::default()
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .expect("Forgejo issue provider should use configured token agent");

        assert!(provider.supports(&IssueSource { service: "https://forgejo.example.test".into(), scope: "team/widgets".into() }));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_requires_configuration() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let config = TestConfig::new(&dir.path().join("flotilla"));
        let runner = Arc::new(MockRunner::new(vec![]));

        let unmet = ForgejoIssueProviderFactory::default()
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
        let settings = forgejo_settings(ForgejoIssueTrackerConfig { token_agent: Some("coder".into()), ..Default::default() });
        let config = TestConfig::with_settings(&dir.path().join("flotilla"), settings);
        let runner = Arc::new(MockRunner::new(vec![]));

        let unmet = ForgejoIssueProviderFactory::default()
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
        let config = TestConfig::with_settings(&dir.path().join("flotilla"), example_service(None));
        let runner = Arc::new(MockRunner::new(vec![]));

        let unmet = ForgejoIssueProviderFactory::default()
            .probe(&EnvironmentBag::new(), &config, &ExecutionEnvironmentPath::new("/repo"), runner)
            .await
            .err()
            .expect("should fail without Forgejo token");

        assert!(unmet.iter().any(|requirement| matches!(requirement, UnmetRequirement::MissingAuth(provider) if provider == "forgejo")));
    }

    #[tokio::test]
    async fn forgejo_issue_provider_factory_descriptor() {
        let desc = ForgejoIssueProviderFactory::default().descriptor();
        assert_eq!(desc.backend, "forgejo");
        assert_eq!(desc.implementation, "forgejo");
        assert_eq!(desc.display_name, "Forgejo Issues");
        assert_eq!(desc.abbreviation, "#");
        assert_eq!(desc.section_label, "Issues");
        assert_eq!(desc.item_noun, "issue");
    }
}
