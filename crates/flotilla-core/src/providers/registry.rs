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
    /// Build a list of provider info summaries for all registered providers.
    /// Category strings match the keys used in `compute_provider_health`.
    pub fn provider_infos(&self) -> Vec<(String, String)> {
        let mut infos = Vec::new();
        for (desc, _) in self.vcs.values() {
            infos.push(("vcs".into(), desc.display_name.clone()));
        }
        for (desc, _) in self.checkout_managers.values() {
            infos.push(("checkout_manager".into(), desc.display_name.clone()));
        }
        for (desc, _) in self.code_review.values() {
            infos.push(("code_review".into(), desc.display_name.clone()));
        }
        for (desc, _) in self.issue_trackers.values() {
            infos.push(("issue_tracker".into(), desc.display_name.clone()));
        }
        for (desc, _) in self.cloud_agents.values() {
            infos.push(("cloud_agent".into(), desc.display_name.clone()));
        }
        for (desc, _) in self.ai_utilities.values() {
            infos.push(("ai_utility".into(), desc.display_name.clone()));
        }
        if let Some((desc, _)) = &self.workspace_manager {
            infos.push(("workspace_manager".into(), desc.display_name.clone()));
        }
        if let Some((desc, _)) = &self.terminal_pool {
            infos.push(("terminal_pool".into(), desc.display_name.clone()));
        }
        infos
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_infos_from_empty_registry() {
        let registry = ProviderRegistry::new();
        let infos = registry.provider_infos();
        assert!(infos.is_empty());
    }
}
