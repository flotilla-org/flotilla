use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::provider_data::ProviderData;
use crate::providers::correlation::{self, CorrelatedItem, CorrelatedGroup, ItemKind as CorItemKind, ProviderItemKey};
use crate::providers::types::AssociationKey;

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub category: &'static str,
    pub message: String,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.category, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkItemKind {
    Checkout,
    Session,
    Pr,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkItemIdentity {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
}

impl WorkItem {
    pub fn identity(&self) -> Option<WorkItemIdentity> {
        match self.kind {
            WorkItemKind::Checkout => self.checkout_key.clone().map(WorkItemIdentity::Checkout),
            WorkItemKind::Pr => self.pr_key.clone().map(WorkItemIdentity::ChangeRequest),
            WorkItemKind::Session => self.session_key.clone().map(WorkItemIdentity::Session),
            WorkItemKind::Issue => self.issue_keys.first().cloned().map(WorkItemIdentity::Issue),
            WorkItemKind::RemoteBranch => self.branch.clone().map(WorkItemIdentity::RemoteBranch),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SectionHeader(pub String);

impl fmt::Display for SectionHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub enum TableEntry {
    Header(SectionHeader),
    Item(WorkItem),
}

#[derive(Debug, Clone)]
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub branch: Option<String>,
    pub description: String,
    pub checkout_key: Option<PathBuf>,
    pub is_main_worktree: bool,
    pub pr_key: Option<String>,
    pub session_key: Option<String>,
    pub issue_keys: Vec<String>,
    pub workspace_refs: Vec<String>,
    /// Index into correlation_groups for debug display.
    pub correlation_group_idx: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct TableView {
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub providers: Arc<ProviderData>,
    pub loading: bool,
    /// Set from the latest background refresh snapshot, for debug display.
    pub correlation_groups: Vec<CorrelatedGroup>,
    pub provider_health: HashMap<&'static str, bool>,
}

pub struct SectionLabels {
    pub checkouts: String,
    pub code_review: String,
    pub issues: String,
    pub sessions: String,
}

impl Default for SectionLabels {
    fn default() -> Self {
        Self {
            checkouts: "Checkouts".into(),
            code_review: "Change Requests".into(),
            issues: "Issues".into(),
            sessions: "Sessions".into(),
        }
    }
}


/// Convert a correlation group into a WorkItem.
/// Returns None for groups that contain only workspaces (no checkout, PR, or session).
fn group_to_work_item(providers: &ProviderData, group: &CorrelatedGroup, group_idx: usize) -> Option<WorkItem> {
    let mut checkout_key: Option<PathBuf> = None;
    let mut pr_key: Option<String> = None;
    let mut session_key: Option<String> = None;
    let mut workspace_refs: Vec<String> = Vec::new();
    let mut is_main_worktree = false;

    for item in &group.items {
        match (&item.kind, &item.source_key) {
            (CorItemKind::Checkout, ProviderItemKey::Checkout(path)) => {
                if checkout_key.is_none() {
                    checkout_key = Some(path.clone());
                    if let Some(co) = providers.checkouts.get(path) {
                        is_main_worktree = co.is_trunk;
                    }
                }
            }
            (CorItemKind::ChangeRequest, ProviderItemKey::ChangeRequest(id)) => {
                pr_key = Some(id.clone());
            }
            (CorItemKind::CloudSession, ProviderItemKey::Session(id)) => {
                if session_key.is_none() {
                    session_key = Some(id.clone());
                }
            }
            (CorItemKind::Workspace, ProviderItemKey::Workspace(ws_ref)) => {
                if providers.workspaces.contains_key(ws_ref.as_str()) {
                    workspace_refs.push(ws_ref.clone());
                }
            }
            _ => {}
        }
    }

    let kind = if checkout_key.is_some() {
        WorkItemKind::Checkout
    } else if pr_key.is_some() {
        WorkItemKind::Pr
    } else if session_key.is_some() {
        WorkItemKind::Session
    } else {
        return None;
    };

    let branch = group.branch().map(|s| s.to_string());

    let pr_title = pr_key.as_ref()
        .and_then(|k| providers.change_requests.get(k.as_str()))
        .map(|cr| cr.title.clone())
        .filter(|t| !t.is_empty());
    let session_title = session_key.as_ref()
        .and_then(|k| providers.sessions.get(k.as_str()))
        .map(|s| s.title.clone())
        .filter(|t| !t.is_empty());
    let description = pr_title
        .or(session_title)
        .or_else(|| branch.clone())
        .unwrap_or_default();

    Some(WorkItem {
        kind,
        branch,
        description,
        checkout_key,
        is_main_worktree,
        pr_key,
        session_key,
        issue_keys: Vec::new(),
        workspace_refs,
        correlation_group_idx: Some(group_idx),
    })
}

/// Phases 1-3: Build CorrelatedItems, run union-find, convert to WorkItems.
/// Returns (work_items, correlation_groups).
pub fn correlate(providers: &ProviderData) -> (Vec<WorkItem>, Vec<CorrelatedGroup>) {
    // Phase 1: Build CorrelatedItems from identity-keyed sources.
    let mut items: Vec<CorrelatedItem> = Vec::new();

    for (path, co) in &providers.checkouts {
        items.push(CorrelatedItem {
            provider_name: "checkout".to_string(),
            kind: CorItemKind::Checkout,
            title: co.branch.clone(),
            correlation_keys: co.correlation_keys.clone(),
            source_key: ProviderItemKey::Checkout(path.clone()),
        });
    }

    for (id, cr) in &providers.change_requests {
        items.push(CorrelatedItem {
            provider_name: "change_request".to_string(),
            kind: CorItemKind::ChangeRequest,
            title: cr.title.clone(),
            correlation_keys: cr.correlation_keys.clone(),
            source_key: ProviderItemKey::ChangeRequest(id.clone()),
        });
    }

    for (id, session) in &providers.sessions {
        items.push(CorrelatedItem {
            provider_name: "session".to_string(),
            kind: CorItemKind::CloudSession,
            title: session.title.clone(),
            correlation_keys: session.correlation_keys.clone(),
            source_key: ProviderItemKey::Session(id.clone()),
        });
    }

    for (ws_ref, ws) in &providers.workspaces {
        items.push(CorrelatedItem {
            provider_name: "workspace".to_string(),
            kind: CorItemKind::Workspace,
            title: ws.name.clone(),
            correlation_keys: ws.correlation_keys.clone(),
            source_key: ProviderItemKey::Workspace(ws_ref.clone()),
        });
    }

    // Phase 2: Run correlation engine
    let groups = correlation::correlate(items);

    // Phase 3: Convert groups to WorkItems
    let mut work_items: Vec<WorkItem> = Vec::new();
    let mut linked_issue_keys: HashSet<String> = HashSet::new();

    for (group_idx, group) in groups.iter().enumerate() {
        let mut work_item = match group_to_work_item(providers, group, group_idx) {
            Some(wi) => wi,
            None => continue,
        };

        // Post-correlation: link issues via association keys on change requests
        if let Some(ref pr_key) = work_item.pr_key {
            if let Some(cr) = providers.change_requests.get(pr_key.as_str()) {
                for key in &cr.association_keys {
                    let AssociationKey::IssueRef(_, issue_id) = key;
                    if providers.issues.contains_key(issue_id.as_str())
                        && !work_item.issue_keys.contains(issue_id)
                    {
                        work_item.issue_keys.push(issue_id.clone());
                        linked_issue_keys.insert(issue_id.clone());
                    }
                }
            }
        }

        // Also link issues via association keys on checkouts (from git config)
        if let Some(ref co_key) = work_item.checkout_key {
            if let Some(co) = providers.checkouts.get(co_key) {
                for key in &co.association_keys {
                    let AssociationKey::IssueRef(_, issue_id) = key;
                    if providers.issues.contains_key(issue_id.as_str())
                        && !work_item.issue_keys.contains(issue_id)
                    {
                        work_item.issue_keys.push(issue_id.clone());
                        linked_issue_keys.insert(issue_id.clone());
                    }
                }
            }
        }

        work_items.push(work_item);
    }

    // Add standalone issues (not linked to any PR)
    for (id, issue) in &providers.issues {
        if !linked_issue_keys.contains(id.as_str()) {
            work_items.push(WorkItem {
                kind: WorkItemKind::Issue,
                branch: None,
                description: issue.title.clone(),
                checkout_key: None,
                is_main_worktree: false,
                pr_key: None,
                session_key: None,
                issue_keys: vec![id.clone()],
                workspace_refs: Vec::new(),
                correlation_group_idx: None,
            });
        }
    }

    // Add remote-only branches
    let known_branches: HashSet<String> = work_items.iter()
        .filter_map(|wi| wi.branch.clone())
        .collect();
    let merged_set: HashSet<&str> = providers.merged_branches.iter()
        .map(|s| s.as_str())
        .collect();
    for b in &providers.remote_branches {
        if b.as_str() != "HEAD" && b.as_str() != "main" && b.as_str() != "master"
            && !known_branches.contains(b.as_str())
            && !merged_set.contains(b.as_str())
        {
            work_items.push(WorkItem {
                kind: WorkItemKind::RemoteBranch,
                branch: Some(b.clone()),
                description: b.clone(),
                checkout_key: None,
                is_main_worktree: false,
                pr_key: None,
                session_key: None,
                issue_keys: Vec::new(),
                workspace_refs: Vec::new(),
                correlation_group_idx: None,
            });
        }
    }

    (work_items, groups)
}

/// Phase 4: Sort work items into sections and build table entries.
pub fn build_table_view(work_items: &[WorkItem], providers: &ProviderData, labels: &SectionLabels) -> TableView {
    let mut checkout_items: Vec<&WorkItem> = Vec::new();
    let mut session_items: Vec<&WorkItem> = Vec::new();
    let mut pr_items: Vec<&WorkItem> = Vec::new();
    let mut remote_items: Vec<&WorkItem> = Vec::new();
    let mut issue_items: Vec<&WorkItem> = Vec::new();

    for item in work_items {
        match item.kind {
            WorkItemKind::Checkout => checkout_items.push(item),
            WorkItemKind::Session => session_items.push(item),
            WorkItemKind::Pr => pr_items.push(item),
            WorkItemKind::RemoteBranch => remote_items.push(item),
            WorkItemKind::Issue => issue_items.push(item),
        }
    }

    let mut entries: Vec<TableEntry> = Vec::new();
    let mut selectable: Vec<usize> = Vec::new();

    // Checkouts -- sorted by branch name ascending
    checkout_items.sort_by(|a, b| a.branch.cmp(&b.branch));
    if !checkout_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader(labels.checkouts.clone())));
        for item in checkout_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // Sessions -- sorted by updated_at descending
    session_items.sort_by(|a, b| {
        let a_time = a.session_key.as_ref().and_then(|k| providers.sessions.get(k.as_str())).and_then(|s| s.updated_at.as_deref());
        let b_time = b.session_key.as_ref().and_then(|k| providers.sessions.get(k.as_str())).and_then(|s| s.updated_at.as_deref());
        b_time.cmp(&a_time)
    });
    if !session_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader(labels.sessions.clone())));
        for item in session_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // PRs -- sorted by id descending
    pr_items.sort_by(|a, b| {
        let a_num = a.pr_key.as_ref().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.pr_key.as_ref().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !pr_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader(labels.code_review.clone())));
        for item in pr_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // Remote branches -- sorted by branch name
    remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));
    if !remote_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader("Remote Branches".into())));
        for item in remote_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // Issues -- sorted by id descending
    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !issue_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader(labels.issues.clone())));
        for item in issue_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    TableView {
        table_entries: entries,
        selectable_indices: selectable,
    }
}

async fn run_command(cmd: &str, args: &[&str], cwd: Option<&PathBuf>) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args)
        .stdin(std::process::Stdio::null());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command.output().await.map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeleteConfirmInfo {
    pub branch: String,
    pub pr_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_work_item() -> WorkItem {
        WorkItem {
            kind: WorkItemKind::Checkout,
            branch: None,
            description: String::new(),
            checkout_key: None,
            is_main_worktree: false,
            pr_key: None,
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            correlation_group_idx: None,
        }
    }

    #[test]
    fn identity_checkout() {
        let wi = WorkItem {
            kind: WorkItemKind::Checkout,
            checkout_key: Some(PathBuf::from("/tmp/foo")),
            ..default_work_item()
        };
        assert_eq!(wi.identity(), Some(WorkItemIdentity::Checkout(PathBuf::from("/tmp/foo"))));
    }

    #[test]
    fn identity_pr() {
        let wi = WorkItem {
            kind: WorkItemKind::Pr,
            pr_key: Some("42".to_string()),
            ..default_work_item()
        };
        assert_eq!(wi.identity(), Some(WorkItemIdentity::ChangeRequest("42".to_string())));
    }

    #[test]
    fn identity_issue() {
        let wi = WorkItem {
            kind: WorkItemKind::Issue,
            issue_keys: vec!["7".to_string()],
            ..default_work_item()
        };
        assert_eq!(wi.identity(), Some(WorkItemIdentity::Issue("7".to_string())));
    }

    #[test]
    fn identity_remote_branch() {
        let wi = WorkItem {
            kind: WorkItemKind::RemoteBranch,
            branch: Some("feature/x".to_string()),
            ..default_work_item()
        };
        assert_eq!(wi.identity(), Some(WorkItemIdentity::RemoteBranch("feature/x".to_string())));
    }
}

pub async fn fetch_delete_confirm_info(
    branch: &str,
    worktree_path: Option<&Path>,
    pr_number: Option<&str>,
    repo_root: &Path,
) -> DeleteConfirmInfo {
    let branch_owned = branch.to_string();
    let repo = repo_root.to_path_buf();
    let wt_path = worktree_path.map(|p| p.to_path_buf());
    let pr_num = pr_number.map(|s| s.to_string());
    let repo2 = repo_root.to_path_buf();

    let repo_for_base = repo.clone();
    let branch_for_base = branch_owned.clone();

    let (unpushed, uncommitted, pr_info) = tokio::join!(
        async {
            let base = async {
                let upstream = run_command(
                    "git",
                    &["rev-parse", "--abbrev-ref", &format!("{branch_for_base}@{{upstream}}")],
                    Some(&repo_for_base),
                ).await;
                if let Ok(ref u) = upstream {
                    let u = u.trim();
                    if !u.is_empty() {
                        return Ok(u.to_string());
                    }
                }
                let remote_head = run_command(
                    "git",
                    &["rev-parse", "--abbrev-ref", "origin/HEAD"],
                    Some(&repo_for_base),
                ).await;
                if let Ok(ref rh) = remote_head {
                    let rh = rh.trim();
                    if !rh.is_empty() {
                        return Ok(rh.to_string());
                    }
                }
                Err("(could not determine base — unpushed status unknown)".to_string())
            }.await;

            match base {
                Ok(base_ref) => {
                    run_command(
                        "git",
                        &["log", &format!("{base_ref}..{branch_for_base}"), "--oneline"],
                        Some(&repo_for_base),
                    ).await.unwrap_or_default()
                }
                Err(warning) => warning,
            }
        },
        async {
            if let Some(path) = &wt_path {
                run_command("git", &["status", "--porcelain"], Some(path))
                    .await.unwrap_or_default()
            } else {
                String::new()
            }
        },
        async {
            if let Some(ref num) = pr_num {
                run_command(
                    "gh",
                    &["pr", "view", num, "--json", "state,mergeCommit"],
                    Some(&repo2),
                ).await.ok()
            } else {
                None
            }
        },
    );

    let mut info = DeleteConfirmInfo {
        branch: branch.to_string(),
        ..Default::default()
    };

    info.unpushed_commits = unpushed
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    info.has_uncommitted = !uncommitted.trim().is_empty();

    if let Some(pr_json) = pr_info {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pr_json) {
            info.pr_status = v.get("state").and_then(|s| s.as_str()).map(|s| s.to_string());
            info.merge_commit_sha = v.get("mergeCommit")
                .and_then(|mc| mc.get("oid"))
                .and_then(|s| s.as_str())
                .map(|s| s[..7.min(s.len())].to_string());
        }
    }

    info
}
