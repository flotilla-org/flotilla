use std::collections::HashMap;
use std::path::PathBuf;

// Re-export provider data types from the protocol crate.
// These are the canonical definitions; core uses them via this re-export.
pub use flotilla_protocol::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession,
    CommitInfo, CorrelationKey, Issue, SessionStatus, WorkingTreeStatus, Workspace,
};

/// Criteria passed to coding agents so they can filter results to a specific repo.
#[derive(Debug, Clone, Default)]
pub struct RepoCriteria {
    /// "owner/repo" from git remote (e.g. "changedirection/reticulate")
    pub repo_slug: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_trunk: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub name: String,
    pub working_directory: PathBuf,
    pub template_vars: HashMap<String, String>,
    pub template_yaml: Option<String>,
}
