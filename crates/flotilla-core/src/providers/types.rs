use std::collections::HashMap;
use std::path::PathBuf;

/// Criteria passed to coding agents so they can filter results to a specific repo.
#[derive(Debug, Clone, Default)]
pub struct RepoCriteria {
    /// "owner/repo" from git remote (e.g. "changedirection/reticulate")
    pub repo_slug: Option<String>,
}

/// Identity keys — safe for union-find grouping. Items sharing a
/// CorrelationKey are the same work unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CorrelationKey {
    Branch(String),
    CheckoutPath(PathBuf),
    ChangeRequestRef(String, String), // (provider_name, CR id)
    SessionRef(String, String),       // (provider_name, session_id)
}

/// Association keys — "related to" links that do NOT merge work units.
/// Two PRs referencing the same issue are separate work streams.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AssociationKey {
    IssueRef(String, String), // (provider_name, issue_id)
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub is_trunk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub branch: String,
    pub path: PathBuf,
    pub is_trunk: bool,
    pub trunk_ahead_behind: Option<AheadBehind>,
    pub remote_ahead_behind: Option<AheadBehind>,
    pub working_tree: Option<WorkingTreeStatus>,
    pub last_commit: Option<CommitInfo>,
    pub correlation_keys: Vec<CorrelationKey>,
    pub association_keys: Vec<AssociationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub short_sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingTreeStatus {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequest {
    pub id: String,
    pub title: String,
    #[allow(dead_code)]
    pub branch: String,
    pub status: ChangeRequestStatus,
    #[allow(dead_code)]
    pub body: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
    pub association_keys: Vec<AssociationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequestStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub labels: Vec<String>,
    #[allow(dead_code)]
    pub association_keys: Vec<AssociationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAgentSession {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub updated_at: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Idle,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub ws_ref: String,
    pub name: String,
    #[allow(dead_code)]
    pub directories: Vec<PathBuf>,
    pub correlation_keys: Vec<CorrelationKey>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub name: String,
    pub working_directory: PathBuf,
    pub template_vars: HashMap<String, String>,
    pub template_yaml: Option<String>,
}
