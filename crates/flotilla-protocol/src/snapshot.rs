use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::provider_data::ProviderData;

/// Repo info for list_repos response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub name: String,
    pub provider_health: HashMap<String, bool>,
    pub loading: bool,
}

/// A complete snapshot for one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub repo: PathBuf,
    pub work_items: Vec<WorkItem>,
    pub providers: ProviderData,
    pub provider_health: HashMap<String, bool>,
    pub errors: Vec<ProviderError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub category: String,
    pub message: String,
}

/// Serializable work item — flattened from the core WorkItem enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub identity: WorkItemIdentity,
    pub branch: Option<String>,
    pub description: String,
    pub checkout: Option<CheckoutRef>,
    pub pr_key: Option<String>,
    pub session_key: Option<String>,
    pub issue_keys: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub is_main_worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemKind {
    Checkout,
    Session,
    Pr,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WorkItemIdentity {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutRef {
    pub key: PathBuf,
    pub is_main_worktree: bool,
}
