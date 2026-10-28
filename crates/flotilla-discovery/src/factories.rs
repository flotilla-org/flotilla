use flotilla_ai_utility::factories::claude::{ClaudeApiAiUtilityFactory, ClaudeCliAiUtilityFactory};
use flotilla_cloud_agent::factories::{claude::ClaudeCodingAgentFactory, codex::CodexCodingAgentFactory, cursor::CursorCodingAgentFactory};
use flotilla_environment::factories::{docker, host_direct};
use flotilla_forgejo_providers::factories::change_request::ForgejoChangeRequestFactory;
use flotilla_forgejo_providers::factories::issue_tracker::ForgejoIssueProviderFactory;
use flotilla_github_providers::factories::change_request::GitHubChangeRequestFactory;
use flotilla_github_providers::factories::issue_tracker::GitHubIssueProviderFactory;

use super::FactoryRegistry;
use flotilla_terminal::factories::{cleat::CleatTerminalPoolFactory, passthrough::PassthroughTerminalPoolFactory};

fn terminal_pool_factories() -> Vec<Box<super::TerminalPoolFactory>> {
    vec![Box::new(CleatTerminalPoolFactory), Box::new(PassthroughTerminalPoolFactory)]
}

fn vcs_factories() -> Vec<Box<super::VcsFactory>> {
    vec![Box::new(flotilla_vcs::factory::GitVcsFactory)]
}

impl FactoryRegistry {
    pub fn default_all() -> Self {
        Self {
            vcs: vcs_factories(),
            change_requests: vec![Box::new(GitHubChangeRequestFactory), Box::new(ForgejoChangeRequestFactory::default())],
            issue_trackers: vec![Box::new(GitHubIssueProviderFactory), Box::new(ForgejoIssueProviderFactory::default())],
            cloud_agents: vec![Box::new(ClaudeCodingAgentFactory), Box::new(CursorCodingAgentFactory), Box::new(CodexCodingAgentFactory)],
            ai_utilities: vec![Box::new(ClaudeApiAiUtilityFactory), Box::new(ClaudeCliAiUtilityFactory)],
            terminal_pools: terminal_pool_factories(),
            environment_providers: vec![Box::new(docker::DockerEnvironmentFactory), Box::new(host_direct::HostDirectEnvironmentFactory)],
            agent_adapters: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag, Factory};
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::IssueSource;
    use flotilla_resources::{ForgeKind, ForgeSpec};

    use super::*;
    use crate::testing::{DiscoveryMockRunner, TestConfig};

    #[test]
    fn default_all_has_all_categories() {
        let reg = FactoryRegistry::default_all();
        assert!(!reg.vcs.is_empty());
        assert!(!reg.change_requests.is_empty());
        assert!(!reg.issue_trackers.is_empty());
        assert!(!reg.cloud_agents.is_empty());
        assert!(!reg.ai_utilities.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }

    #[test]
    fn process_runtime_constructs_external_providers() {
        let runtime = crate::DiscoveryRuntime::for_process();
        let reg = &runtime.factories;
        assert!(!reg.vcs.is_empty());
        assert!(!reg.change_requests.is_empty());
        assert!(!reg.issue_trackers.is_empty());
        assert!(!reg.cloud_agents.is_empty());
        assert!(!reg.ai_utilities.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }

    #[tokio::test]
    async fn forgejo_origin_binds_issue_and_change_request_without_config_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("lab-forgejo-coder-token"), "test-token\n").expect("write token");
        let config = TestConfig::new(&dir.path().join("flotilla"));
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
        let issues =
            ForgejoIssueProviderFactory::default().probe(&bag, &config, &root, runner.clone()).await.expect("Forgejo issue source");
        let changes = ForgejoChangeRequestFactory::default().probe(&bag, &config, &root, runner).await.expect("Forgejo CR source");
        assert!(issues.supports(&IssueSource { service: "https://forgejo.lab.flotilla.work".into(), scope: "lab/flotilla".into() }));
        assert_eq!(ForgejoChangeRequestFactory::default().descriptor().implementation, "forgejo");
        let _ = changes;
    }
}
