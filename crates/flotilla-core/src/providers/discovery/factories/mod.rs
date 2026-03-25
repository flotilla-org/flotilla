pub mod claude;
pub mod cleat;
pub mod cmux;
pub mod codex;
pub mod cursor;
pub mod docker;
pub mod git;
pub mod github;
pub mod passthrough;
pub mod shpool;
pub mod tmux;
pub mod zellij;

use super::FactoryRegistry;

fn workspace_factories() -> Vec<Box<super::WorkspaceManagerFactory>> {
    vec![
        Box::new(cmux::CmuxInsideFactory),
        Box::new(zellij::ZellijWorkspaceManagerFactory),
        Box::new(tmux::TmuxWorkspaceManagerFactory),
        Box::new(cmux::CmuxBinaryFallbackFactory),
    ]
}

fn terminal_pool_factories() -> Vec<Box<super::TerminalPoolFactory>> {
    vec![
        Box::new(cleat::CleatTerminalPoolFactory),
        Box::new(shpool::ShpoolTerminalPoolFactory),
        Box::new(passthrough::PassthroughTerminalPoolFactory),
    ]
}

fn checkout_manager_factories() -> Vec<Box<super::CheckoutManagerFactory>> {
    vec![Box::new(git::WtCheckoutManagerFactory), Box::new(git::GitCheckoutManagerFactory)]
}

impl FactoryRegistry {
    pub fn default_all() -> Self {
        Self {
            vcs: vec![Box::new(git::GitVcsFactory)],
            checkout_managers: checkout_manager_factories(),
            change_requests: vec![Box::new(github::GitHubChangeRequestFactory)],
            issue_trackers: vec![Box::new(github::GitHubIssueTrackerFactory)],
            cloud_agents: vec![
                Box::new(claude::ClaudeCodingAgentFactory),
                Box::new(cursor::CursorCodingAgentFactory),
                Box::new(codex::CodexCodingAgentFactory),
            ],
            ai_utilities: vec![Box::new(claude::ClaudeApiAiUtilityFactory), Box::new(claude::ClaudeCliAiUtilityFactory)],
            // Priority: inside-cmux > inside-zellij > inside-tmux > cmux-binary-fallback
            workspace_managers: workspace_factories(),
            terminal_pools: terminal_pool_factories(),
            environment_providers: vec![Box::new(docker::DockerEnvironmentFactory)],
        }
    }

    pub fn for_follower() -> Self {
        Self {
            vcs: vec![Box::new(git::GitVcsFactory)],
            checkout_managers: checkout_manager_factories(),
            change_requests: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: workspace_factories(),
            terminal_pools: terminal_pool_factories(),
            environment_providers: vec![],
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
        assert!(!reg.checkout_managers.is_empty());
        assert!(!reg.change_requests.is_empty());
        assert!(!reg.issue_trackers.is_empty());
        assert!(!reg.cloud_agents.is_empty());
        assert!(!reg.ai_utilities.is_empty());
        assert!(!reg.workspace_managers.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }

    #[test]
    fn for_follower_omits_external_providers() {
        let reg = FactoryRegistry::for_follower();
        assert!(!reg.vcs.is_empty());
        assert!(!reg.checkout_managers.is_empty());
        assert!(reg.change_requests.is_empty());
        assert!(reg.issue_trackers.is_empty());
        assert!(reg.cloud_agents.is_empty());
        assert!(reg.ai_utilities.is_empty());
        assert!(!reg.workspace_managers.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }
}
