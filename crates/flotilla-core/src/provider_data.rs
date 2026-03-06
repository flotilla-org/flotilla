use std::path::PathBuf;
use indexmap::IndexMap;
use crate::providers::types::*;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProviderData {
    pub checkouts: IndexMap<PathBuf, Checkout>,
    pub change_requests: IndexMap<String, ChangeRequest>,
    pub issues: IndexMap<String, Issue>,
    pub sessions: IndexMap<String, CloudAgentSession>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
    pub workspaces: IndexMap<String, Workspace>,
}
