//! Shared test builders for WorkItem and RepoInfo.
//!
//! Used by both unit tests (via test_support) and integration tests (via tests/support).
//! Always compiled so integration tests can access these through the public API.

use std::collections::HashMap;
use std::path::PathBuf;

use flotilla_protocol::{
    CheckoutRef, RepoInfo, RepoLabels, WorkItem, WorkItemIdentity, WorkItemKind,
};

pub fn bare_item() -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Issue,
        identity: WorkItemIdentity::Issue("1".into()),
        branch: None,
        description: String::new(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn issue_item(id: impl Into<String>) -> WorkItem {
    let id = id.into();
    WorkItem {
        kind: WorkItemKind::Issue,
        identity: WorkItemIdentity::Issue(id.clone()),
        branch: None,
        description: format!("Item {id}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn checkout_item(branch: &str, path: &str, is_main: bool) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(PathBuf::from(path)),
        branch: Some(branch.into()),
        description: format!("checkout {branch}"),
        checkout: Some(CheckoutRef {
            key: PathBuf::from(path),
            is_main_checkout: is_main,
        }),
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: is_main,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn pr_item(pr_id: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::ChangeRequest,
        identity: WorkItemIdentity::ChangeRequest(pr_id.into()),
        branch: Some("feat/pr-branch".into()),
        description: format!("PR #{pr_id}"),
        checkout: None,
        change_request_key: Some(pr_id.into()),
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn session_item(session_id: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Session,
        identity: WorkItemIdentity::Session(session_id.into()),
        branch: Some("feat/session-branch".into()),
        description: format!("session {session_id}"),
        checkout: None,
        change_request_key: None,
        session_key: Some(session_id.into()),
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn remote_branch_item(branch: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::RemoteBranch,
        identity: WorkItemIdentity::RemoteBranch(branch.into()),
        branch: Some(branch.into()),
        description: format!("remote {branch}"),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
    }
}

pub fn repo_info(
    path: impl Into<PathBuf>,
    name: impl Into<String>,
    labels: RepoLabels,
) -> RepoInfo {
    RepoInfo {
        path: path.into(),
        name: name.into(),
        labels,
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        loading: false,
    }
}
