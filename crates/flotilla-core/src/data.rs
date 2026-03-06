use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

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
pub struct CheckoutRef {
    pub key: PathBuf,
    pub is_main_worktree: bool,
}

#[derive(Debug, Clone)]
pub enum CorrelatedAnchor {
    Checkout(CheckoutRef),
    Pr(String),
    Session(String),
}

#[derive(Debug, Clone)]
pub struct CorrelatedWorkItem {
    pub anchor: CorrelatedAnchor,
    pub branch: Option<String>,
    pub description: String,
    pub linked_pr: Option<String>,
    pub linked_session: Option<String>,
    pub linked_issues: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub correlation_group_idx: usize,
}

#[derive(Debug, Clone)]
pub enum StandaloneWorkItem {
    Issue { key: String, description: String },
    RemoteBranch { branch: String },
}

#[derive(Debug, Clone)]
pub enum WorkItem {
    Correlated(CorrelatedWorkItem),
    Standalone(StandaloneWorkItem),
}

impl WorkItem {
    pub fn kind(&self) -> WorkItemKind {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(_) => WorkItemKind::Checkout,
                CorrelatedAnchor::Pr(_) => WorkItemKind::Pr,
                CorrelatedAnchor::Session(_) => WorkItemKind::Session,
            },
            WorkItem::Standalone(s) => match s {
                StandaloneWorkItem::Issue { .. } => WorkItemKind::Issue,
                StandaloneWorkItem::RemoteBranch { .. } => WorkItemKind::RemoteBranch,
            },
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            WorkItem::Correlated(c) => c.branch.as_deref(),
            WorkItem::Standalone(StandaloneWorkItem::RemoteBranch { branch }) => Some(branch.as_str()),
            WorkItem::Standalone(StandaloneWorkItem::Issue { .. }) => None,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            WorkItem::Correlated(c) => &c.description,
            WorkItem::Standalone(StandaloneWorkItem::Issue { description, .. }) => description,
            WorkItem::Standalone(StandaloneWorkItem::RemoteBranch { branch }) => branch,
        }
    }

    pub fn checkout(&self) -> Option<&CheckoutRef> {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => Some(co),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn checkout_key(&self) -> Option<&Path> {
        self.checkout().map(|co| co.key.as_path())
    }

    pub fn is_main_worktree(&self) -> bool {
        self.checkout().is_some_and(|co| co.is_main_worktree)
    }

    pub fn pr_key(&self) -> Option<&str> {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Pr(key) => Some(key.as_str()),
                _ => c.linked_pr.as_deref(),
            },
            _ => None,
        }
    }

    pub fn session_key(&self) -> Option<&str> {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Session(key) => Some(key.as_str()),
                _ => c.linked_session.as_deref(),
            },
            _ => None,
        }
    }

    pub fn issue_keys(&self) -> &[String] {
        match self {
            WorkItem::Correlated(c) => &c.linked_issues,
            WorkItem::Standalone(StandaloneWorkItem::Issue { key, .. }) => std::slice::from_ref(key),
            WorkItem::Standalone(StandaloneWorkItem::RemoteBranch { .. }) => &[],
        }
    }

    pub fn workspace_refs(&self) -> &[String] {
        match self {
            WorkItem::Correlated(c) => &c.workspace_refs,
            _ => &[],
        }
    }

    pub fn correlation_group_idx(&self) -> Option<usize> {
        match self {
            WorkItem::Correlated(c) => Some(c.correlation_group_idx),
            _ => None,
        }
    }

    pub fn identity(&self) -> WorkItemIdentity {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => WorkItemIdentity::Checkout(co.key.clone()),
                CorrelatedAnchor::Pr(key) => WorkItemIdentity::ChangeRequest(key.clone()),
                CorrelatedAnchor::Session(key) => WorkItemIdentity::Session(key.clone()),
            },
            WorkItem::Standalone(s) => match s {
                StandaloneWorkItem::Issue { key, .. } => WorkItemIdentity::Issue(key.clone()),
                StandaloneWorkItem::RemoteBranch { branch } => WorkItemIdentity::RemoteBranch(branch.clone()),
            },
        }
    }

    pub fn as_correlated_mut(&mut self) -> Option<&mut CorrelatedWorkItem> {
        match self {
            WorkItem::Correlated(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct TableView {
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
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
    let mut checkout_ref: Option<CheckoutRef> = None;
    let mut pr_key: Option<String> = None;
    let mut session_key: Option<String> = None;
    let mut workspace_refs: Vec<String> = Vec::new();

    for item in &group.items {
        match (&item.kind, &item.source_key) {
            (CorItemKind::Checkout, ProviderItemKey::Checkout(path)) => {
                if checkout_ref.is_none() {
                    let is_main_worktree = providers.checkouts.get(path)
                        .is_some_and(|co| co.is_trunk);
                    checkout_ref = Some(CheckoutRef {
                        key: path.clone(),
                        is_main_worktree,
                    });
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

    let (anchor, linked_pr, linked_session) = if let Some(co) = checkout_ref {
        (CorrelatedAnchor::Checkout(co), pr_key, session_key)
    } else if let Some(key) = pr_key {
        (CorrelatedAnchor::Pr(key), None, session_key)
    } else if let Some(key) = session_key {
        (CorrelatedAnchor::Session(key), None, None)
    } else {
        return None;
    };

    let branch = group.branch().map(|s| s.to_string());

    let pr_ref = match &anchor {
        CorrelatedAnchor::Pr(k) => Some(k.as_str()),
        _ => linked_pr.as_deref(),
    };
    let session_ref = match &anchor {
        CorrelatedAnchor::Session(k) => Some(k.as_str()),
        _ => linked_session.as_deref(),
    };

    let pr_title = pr_ref
        .and_then(|k| providers.change_requests.get(k))
        .map(|cr| cr.title.clone())
        .filter(|t| !t.is_empty());
    let session_title = session_ref
        .and_then(|k| providers.sessions.get(k))
        .map(|s| s.title.clone())
        .filter(|t| !t.is_empty());
    let description = pr_title
        .or(session_title)
        .or_else(|| branch.clone())
        .unwrap_or_default();

    Some(WorkItem::Correlated(CorrelatedWorkItem {
        anchor,
        branch,
        description,
        linked_pr,
        linked_session,
        linked_issues: Vec::new(),
        workspace_refs,
        correlation_group_idx: group_idx,
    }))
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
        if let Some(pr_key) = work_item.pr_key() {
            if let Some(cr) = providers.change_requests.get(pr_key) {
                let issue_ids: Vec<String> = cr.association_keys.iter()
                    .filter_map(|key| {
                        let AssociationKey::IssueRef(_, issue_id) = key;
                        if providers.issues.contains_key(issue_id.as_str()) {
                            Some(issue_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(c) = work_item.as_correlated_mut() {
                    for id in issue_ids {
                        if !c.linked_issues.contains(&id) {
                            c.linked_issues.push(id.clone());
                            linked_issue_keys.insert(id);
                        }
                    }
                }
            }
        }

        // Also link issues via association keys on checkouts (from git config)
        if let Some(co_key) = work_item.checkout_key().map(|p| p.to_path_buf()) {
            if let Some(co) = providers.checkouts.get(&co_key) {
                let issue_ids: Vec<String> = co.association_keys.iter()
                    .filter_map(|key| {
                        let AssociationKey::IssueRef(_, issue_id) = key;
                        if providers.issues.contains_key(issue_id.as_str()) {
                            Some(issue_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(c) = work_item.as_correlated_mut() {
                    for id in issue_ids {
                        if !c.linked_issues.contains(&id) {
                            c.linked_issues.push(id.clone());
                            linked_issue_keys.insert(id);
                        }
                    }
                }
            }
        }

        work_items.push(work_item);
    }

    // Add standalone issues (not linked to any PR)
    for (id, issue) in &providers.issues {
        if !linked_issue_keys.contains(id.as_str()) {
            work_items.push(WorkItem::Standalone(StandaloneWorkItem::Issue {
                key: id.clone(),
                description: issue.title.clone(),
            }));
        }
    }

    // Add remote-only branches
    let known_branches: HashSet<String> = work_items.iter()
        .filter_map(|wi| wi.branch().map(|b| b.to_string()))
        .collect();
    let merged_set: HashSet<&str> = providers.merged_branches.iter()
        .map(|s| s.as_str())
        .collect();
    for b in &providers.remote_branches {
        if b.as_str() != "HEAD" && b.as_str() != "main" && b.as_str() != "master"
            && !known_branches.contains(b.as_str())
            && !merged_set.contains(b.as_str())
        {
            work_items.push(WorkItem::Standalone(StandaloneWorkItem::RemoteBranch {
                branch: b.clone(),
            }));
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
        match item.kind() {
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
    checkout_items.sort_by(|a, b| a.branch().cmp(&b.branch()));
    if !checkout_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader(labels.checkouts.clone())));
        for item in checkout_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // Sessions -- sorted by updated_at descending
    session_items.sort_by(|a, b| {
        let a_time = a.session_key().and_then(|k| providers.sessions.get(k)).and_then(|s| s.updated_at.as_deref());
        let b_time = b.session_key().and_then(|k| providers.sessions.get(k)).and_then(|s| s.updated_at.as_deref());
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
        let a_num = a.pr_key().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.pr_key().and_then(|k| k.parse::<i64>().ok());
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
    remote_items.sort_by(|a, b| a.branch().cmp(&b.branch()));
    if !remote_items.is_empty() {
        entries.push(TableEntry::Header(SectionHeader("Remote Branches".into())));
        for item in remote_items {
            selectable.push(entries.len());
            entries.push(TableEntry::Item(item.clone()));
        }
    }

    // Issues -- sorted by id descending
    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys().first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys().first().and_then(|k| k.parse::<i64>().ok());
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
    pub base_detection_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_checkout() {
        let wi = WorkItem::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/foo"),
                is_main_worktree: false,
            }),
            branch: None,
            description: String::new(),
            linked_pr: None,
            linked_session: None,
            linked_issues: Vec::new(),
            workspace_refs: Vec::new(),
            correlation_group_idx: 0,
        });
        assert_eq!(wi.identity(), WorkItemIdentity::Checkout(PathBuf::from("/tmp/foo")));
    }

    #[test]
    fn identity_pr() {
        let wi = WorkItem::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Pr("42".to_string()),
            branch: None,
            description: String::new(),
            linked_pr: None,
            linked_session: None,
            linked_issues: Vec::new(),
            workspace_refs: Vec::new(),
            correlation_group_idx: 0,
        });
        assert_eq!(wi.identity(), WorkItemIdentity::ChangeRequest("42".to_string()));
    }

    #[test]
    fn identity_session() {
        let wi = WorkItem::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Session("sess-1".to_string()),
            branch: None,
            description: String::new(),
            linked_pr: None,
            linked_session: None,
            linked_issues: Vec::new(),
            workspace_refs: Vec::new(),
            correlation_group_idx: 0,
        });
        assert_eq!(wi.identity(), WorkItemIdentity::Session("sess-1".to_string()));
    }

    #[test]
    fn identity_issue() {
        let wi = WorkItem::Standalone(StandaloneWorkItem::Issue {
            key: "7".to_string(),
            description: String::new(),
        });
        assert_eq!(wi.identity(), WorkItemIdentity::Issue("7".to_string()));
    }

    #[test]
    fn identity_remote_branch() {
        let wi = WorkItem::Standalone(StandaloneWorkItem::RemoteBranch {
            branch: "feature/x".to_string(),
        });
        assert_eq!(wi.identity(), WorkItemIdentity::RemoteBranch("feature/x".to_string()));
    }

    #[test]
    fn checkout_association_keys_link_issues() {
        use crate::provider_data::ProviderData;
        use crate::providers::types::*;

        let mut providers = ProviderData::default();

        // A checkout with an association key pointing to issue "42"
        let co_path = PathBuf::from("/tmp/feat-x");
        providers.checkouts.insert(co_path.clone(), Checkout {
            branch: "feat-x".to_string(),
            path: co_path.clone(),
            is_trunk: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![
                CorrelationKey::Branch("feat-x".to_string()),
                CorrelationKey::CheckoutPath(co_path.clone()),
            ],
            association_keys: vec![
                AssociationKey::IssueRef("github".to_string(), "42".to_string()),
            ],
        });

        // An issue with id "42"
        providers.issues.insert("42".to_string(), Issue {
            id: "42".to_string(),
            title: "Fix the thing".to_string(),
            labels: vec![],
            association_keys: vec![],
        });

        let (work_items, _groups) = correlate(&providers);

        // The checkout work item should have issue "42" linked
        let checkout_wi = work_items.iter()
            .find(|wi| wi.kind() == WorkItemKind::Checkout)
            .expect("should have a checkout work item");
        assert!(checkout_wi.issue_keys().contains(&"42".to_string()),
            "checkout should link issue 42 via association key, got: {:?}", checkout_wi.issue_keys());

        // Issue "42" should NOT appear as a standalone work item
        let standalone_issues: Vec<_> = work_items.iter()
            .filter(|wi| wi.kind() == WorkItemKind::Issue)
            .collect();
        assert!(standalone_issues.is_empty(),
            "issue 42 should be linked, not standalone");
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

    let (unpushed_result, uncommitted, pr_info) = tokio::join!(
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
                Err("Could not determine base branch — unpushed commit status unknown".to_string())
            }.await;

            match base {
                Ok(base_ref) => {
                    let log = run_command(
                        "git",
                        &["log", &format!("{base_ref}..{branch_for_base}"), "--oneline"],
                        Some(&repo_for_base),
                    ).await.unwrap_or_default();
                    Ok(log)
                }
                Err(warning) => Err(warning),
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

    match unpushed_result {
        Ok(log_output) => {
            info.unpushed_commits = log_output
                .lines()
                .map(|l| l.to_string())
                .filter(|l| !l.is_empty())
                .collect();
        }
        Err(warning) => {
            info.base_detection_warning = Some(warning);
        }
    }

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
