use std::collections::HashMap;

// Re-export provider data types from the protocol crate.
// These are the canonical definitions; core uses them via this re-export.
pub use flotilla_protocol::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, CorrelationKey, Issue,
    IssueChangeset, IssuePage, SessionStatus, WorkingTreeStatus, Workspace,
};

use crate::path_context::ExecutionEnvironmentPath;

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
    pub working_directory: ExecutionEnvironmentPath,
    pub template_vars: HashMap<String, String>,
    pub template_yaml: Option<String>,
    /// When set, these override template commands — each entry is (role, attach_command).
    /// Used when a TerminalPool has pre-started persistent sessions.
    pub resolved_commands: Option<Vec<(String, String)>>,
}
