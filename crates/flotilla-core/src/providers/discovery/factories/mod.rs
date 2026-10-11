use flotilla_cloud_agent::factories::{claude::ClaudeCodingAgentFactory, codex::CodexCodingAgentFactory, cursor::CursorCodingAgentFactory};
pub mod claude;
pub mod docker;
pub mod github;
pub mod host_direct;

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
            change_requests: vec![Box::new(github::GitHubChangeRequestFactory), Box::new(github::ForgejoChangeRequestFactory)],
            issue_trackers: vec![Box::new(github::GitHubIssueProviderFactory), Box::new(github::ForgejoIssueProviderFactory)],
            cloud_agents: vec![Box::new(ClaudeCodingAgentFactory), Box::new(CursorCodingAgentFactory), Box::new(CodexCodingAgentFactory)],
            ai_utilities: vec![Box::new(claude::ClaudeApiAiUtilityFactory), Box::new(claude::ClaudeCliAiUtilityFactory)],
            terminal_pools: terminal_pool_factories(),
            environment_providers: vec![Box::new(docker::DockerEnvironmentFactory), Box::new(host_direct::HostDirectEnvironmentFactory)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let runtime = super::super::DiscoveryRuntime::for_process();
        let reg = &runtime.factories;
        assert!(!reg.vcs.is_empty());
        assert!(!reg.change_requests.is_empty());
        assert!(!reg.issue_trackers.is_empty());
        assert!(!reg.cloud_agents.is_empty());
        assert!(!reg.ai_utilities.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }
}
