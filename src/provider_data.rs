use crate::providers::types::*;

#[derive(Debug, Default, Clone)]
pub struct ProviderData {
    pub checkouts: Vec<Checkout>,
    pub change_requests: Vec<ChangeRequest>,
    pub issues: Vec<Issue>,
    pub sessions: Vec<CloudAgentSession>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
    pub workspaces: Vec<Workspace>,
}
