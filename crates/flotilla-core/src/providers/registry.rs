use crate::providers::ai_utility::AiUtility;
use crate::providers::code_review::CodeReview;
use crate::providers::coding_agent::CodingAgent;
use crate::providers::issue_tracker::IssueTracker;
use crate::providers::vcs::{CheckoutManager, Vcs};
use crate::providers::workspace::WorkspaceManager;
use indexmap::IndexMap;
use std::sync::Arc;

pub struct ProviderRegistry {
    pub vcs: IndexMap<String, Arc<dyn Vcs>>,
    pub checkout_managers: IndexMap<String, Arc<dyn CheckoutManager>>,
    pub code_review: IndexMap<String, Arc<dyn CodeReview>>,
    pub issue_trackers: IndexMap<String, Arc<dyn IssueTracker>>,
    pub coding_agents: IndexMap<String, Arc<dyn CodingAgent>>,
    pub ai_utilities: IndexMap<String, Arc<dyn AiUtility>>,
    pub workspace_manager: Option<(String, Arc<dyn WorkspaceManager>)>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            vcs: IndexMap::new(),
            checkout_managers: IndexMap::new(),
            code_review: IndexMap::new(),
            issue_trackers: IndexMap::new(),
            coding_agents: IndexMap::new(),
            ai_utilities: IndexMap::new(),
            workspace_manager: None,
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
