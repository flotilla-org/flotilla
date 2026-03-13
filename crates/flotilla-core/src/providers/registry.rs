use std::sync::Arc;

use indexmap::IndexMap;

use crate::providers::{
    ai_utility::AiUtility,
    code_review::CodeReview,
    coding_agent::CloudAgentService,
    discovery::ProviderDescriptor,
    issue_tracker::IssueTracker,
    terminal::TerminalPool,
    vcs::{CheckoutManager, Vcs},
    workspace::WorkspaceManager,
};

pub struct ProviderRegistry {
    pub vcs: IndexMap<String, (ProviderDescriptor, Arc<dyn Vcs>)>,
    pub checkout_managers: IndexMap<String, (ProviderDescriptor, Arc<dyn CheckoutManager>)>,
    pub code_review: IndexMap<String, (ProviderDescriptor, Arc<dyn CodeReview>)>,
    pub issue_trackers: IndexMap<String, (ProviderDescriptor, Arc<dyn IssueTracker>)>,
    pub cloud_agents: IndexMap<String, (ProviderDescriptor, Arc<dyn CloudAgentService>)>,
    pub ai_utilities: IndexMap<String, (ProviderDescriptor, Arc<dyn AiUtility>)>,
    pub workspace_manager: Option<(ProviderDescriptor, Arc<dyn WorkspaceManager>)>,
    pub terminal_pool: Option<(ProviderDescriptor, Arc<dyn TerminalPool>)>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            vcs: IndexMap::new(),
            checkout_managers: IndexMap::new(),
            code_review: IndexMap::new(),
            issue_trackers: IndexMap::new(),
            cloud_agents: IndexMap::new(),
            ai_utilities: IndexMap::new(),
            workspace_manager: None,
            terminal_pool: None,
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    /// Remove external (network-polling) providers, keeping only local ones.
    ///
    /// Local providers (kept): VCS, CheckoutManagers, WorkspaceManager, TerminalPool
    /// External providers (removed): CodeReview, IssueTracker, CloudAgents, AiUtilities
    ///
    /// Used by follower-mode daemons that receive service-level data
    /// (PRs, issues, sessions) from the leader via PeerData messages
    /// instead of polling external APIs directly.
    pub fn strip_external_providers(&mut self) {
        self.code_review.clear();
        self.issue_trackers.clear();
        self.cloud_agents.clear();
        self.ai_utilities.clear();
    }
}
