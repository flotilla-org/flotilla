use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CorrelationKey {
    Branch(String),
    CheckoutPath(PathBuf),
    IssueRef(String, String),          // (provider_name, issue_id)
    ChangeRequestRef(String, String),  // (provider_name, CR id)
    SessionRef(String, String),        // (provider_name, session_id)
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_trunk: bool,
}

#[derive(Debug, Clone)]
pub struct Checkout {
    pub branch: String,
    pub path: PathBuf,
    pub is_trunk: bool,
    pub trunk_ahead_behind: Option<AheadBehind>,
    pub remote_ahead_behind: Option<AheadBehind>,
    pub working_tree: Option<WorkingTreeStatus>,
    pub last_commit: Option<CommitInfo>,
    pub correlation_keys: Vec<CorrelationKey>,
}

#[derive(Debug, Clone)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub short_sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorkingTreeStatus {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

#[derive(Debug, Clone)]
pub struct ChangeRequest {
    pub id: String,
    pub title: String,
    #[allow(dead_code)]
    pub branch: String,
    pub status: ChangeRequestStatus,
    #[allow(dead_code)]
    pub body: Option<String>,
    pub correlation_keys: Vec<CorrelationKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequestStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub labels: Vec<String>,
    #[allow(dead_code)]
    pub correlation_keys: Vec<CorrelationKey>,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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
