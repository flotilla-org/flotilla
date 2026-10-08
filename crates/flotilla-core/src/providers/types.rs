// Re-export provider data types from the protocol crate.
// These are the canonical definitions; core uses them via this re-export.
pub use flotilla_protocol::{
    AheadBehind, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, Issue, IssueChangeset, SessionStatus,
    WorkingTreeStatus, Workspace,
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
