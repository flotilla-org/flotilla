pub mod claude;
pub mod cmux;
pub mod codex;
pub mod cursor;
pub mod git;
pub mod github;
pub mod passthrough;
pub mod shpool;
pub mod tmux;
pub mod zellij;

use super::FactoryRegistry;

impl FactoryRegistry {
    pub fn default_all() -> Self {
        Self {
            vcs: vec![Box::new(git::GitVcsFactory)],
            checkout_managers: vec![
                Box::new(git::WtCheckoutManagerFactory),
                Box::new(git::GitCheckoutManagerFactory),
            ],
            code_review: vec![Box::new(github::GitHubCodeReviewFactory)],
            issue_trackers: vec![Box::new(github::GitHubIssueTrackerFactory)],
            cloud_agents: vec![
                Box::new(claude::ClaudeCodingAgentFactory),
                Box::new(cursor::CursorCodingAgentFactory),
                Box::new(codex::CodexCodingAgentFactory),
            ],
            ai_utilities: vec![Box::new(claude::ClaudeAiUtilityFactory)],
            workspace_managers: vec![
                Box::new(cmux::CmuxWorkspaceManagerFactory),
                Box::new(zellij::ZellijWorkspaceManagerFactory),
                Box::new(tmux::TmuxWorkspaceManagerFactory),
            ],
            terminal_pools: vec![
                Box::new(shpool::ShpoolTerminalPoolFactory),
                Box::new(passthrough::PassthroughTerminalPoolFactory),
            ],
        }
    }

    pub fn for_follower() -> Self {
        Self {
            vcs: vec![Box::new(git::GitVcsFactory)],
            checkout_managers: vec![
                Box::new(git::WtCheckoutManagerFactory),
                Box::new(git::GitCheckoutManagerFactory),
            ],
            code_review: vec![],
            issue_trackers: vec![],
            cloud_agents: vec![],
            ai_utilities: vec![],
            workspace_managers: vec![
                Box::new(cmux::CmuxWorkspaceManagerFactory),
                Box::new(zellij::ZellijWorkspaceManagerFactory),
                Box::new(tmux::TmuxWorkspaceManagerFactory),
            ],
            terminal_pools: vec![
                Box::new(shpool::ShpoolTerminalPoolFactory),
                Box::new(passthrough::PassthroughTerminalPoolFactory),
            ],
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
        assert!(!reg.code_review.is_empty());
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
        assert!(reg.code_review.is_empty());
        assert!(reg.issue_trackers.is_empty());
        assert!(reg.cloud_agents.is_empty());
        assert!(reg.ai_utilities.is_empty());
        assert!(!reg.workspace_managers.is_empty());
        assert!(!reg.terminal_pools.is_empty());
    }
}
