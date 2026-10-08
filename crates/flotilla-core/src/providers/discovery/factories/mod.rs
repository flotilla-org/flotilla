pub mod claude;
pub mod cleat;
pub mod codex;
pub mod cursor;
pub mod docker;
pub mod git;
pub mod github;
pub mod host_direct;
pub mod passthrough;

use super::FactoryRegistry;

fn terminal_pool_factories() -> Vec<Box<super::TerminalPoolFactory>> {
    vec![Box::new(cleat::CleatTerminalPoolFactory), Box::new(passthrough::PassthroughTerminalPoolFactory)]
}

fn vcs_factories() -> Vec<Box<super::VcsFactory>> {
    vec![Box::new(git::GitVcsFactory)]
}

impl FactoryRegistry {
    pub fn default_all() -> Self {
        Self {
            vcs: vcs_factories(),
            change_requests: vec![Box::new(github::GitHubChangeRequestFactory), Box::new(github::ForgejoChangeRequestFactory)],
            issue_trackers: vec![Box::new(github::GitHubIssueProviderFactory), Box::new(github::ForgejoIssueProviderFactory)],
            cloud_agents: vec![
                Box::new(claude::ClaudeCodingAgentFactory),
                Box::new(cursor::CursorCodingAgentFactory),
                Box::new(codex::CodexCodingAgentFactory),
            ],
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
