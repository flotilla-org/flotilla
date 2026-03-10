use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

// Re-export protocol types that are used throughout the crate and by consumers.
pub use flotilla_protocol::{CheckoutRef, CheckoutStatus, WorkItemIdentity, WorkItemKind};

use crate::provider_data::ProviderData;
use crate::providers::correlation::{
    self, CorrelatedGroup, CorrelatedItem, ItemKind as CorItemKind, ProviderItemKey,
};
use crate::providers::types::AssociationKey;

#[derive(Debug, Clone)]
pub struct RefreshError {
    pub category: &'static str,
    pub message: String,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.category, self.message)
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
pub enum GroupEntry {
    Header(SectionHeader),
    Item(Box<flotilla_protocol::WorkItem>),
}

#[derive(Debug, Clone)]
pub enum CorrelatedAnchor {
    Checkout(CheckoutRef),
    ChangeRequest(String),
    Session(String),
}

#[derive(Debug, Clone)]
pub struct CorrelatedWorkItem {
    pub anchor: CorrelatedAnchor,
    pub branch: Option<String>,
    pub description: String,
    pub linked_change_request: Option<String>,
    pub linked_session: Option<String>,
    pub linked_issues: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub correlation_group_idx: usize,
}

#[derive(Debug, Clone)]
pub enum StandaloneResult {
    Issue { key: String, description: String },
    RemoteBranch { branch: String },
}

#[derive(Debug, Clone)]
pub enum CorrelationResult {
    Correlated(CorrelatedWorkItem),
    Standalone(StandaloneResult),
}

impl CorrelationResult {
    pub fn kind(&self) -> WorkItemKind {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(_) => WorkItemKind::Checkout,
                CorrelatedAnchor::ChangeRequest(_) => WorkItemKind::ChangeRequest,
                CorrelatedAnchor::Session(_) => WorkItemKind::Session,
            },
            CorrelationResult::Standalone(s) => match s {
                StandaloneResult::Issue { .. } => WorkItemKind::Issue,
                StandaloneResult::RemoteBranch { .. } => WorkItemKind::RemoteBranch,
            },
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => c.branch.as_deref(),
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch }) => {
                Some(branch.as_str())
            }
            CorrelationResult::Standalone(StandaloneResult::Issue { .. }) => None,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            CorrelationResult::Correlated(c) => &c.description,
            CorrelationResult::Standalone(StandaloneResult::Issue { description, .. }) => {
                description
            }
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch }) => branch,
        }
    }

    pub fn checkout(&self) -> Option<&CheckoutRef> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => Some(co),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn checkout_key(&self) -> Option<&Path> {
        self.checkout().map(|co| co.key.as_path())
    }

    pub fn is_main_checkout(&self) -> bool {
        self.checkout().is_some_and(|co| co.is_main_checkout)
    }

    pub fn change_request_key(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::ChangeRequest(key) => Some(key.as_str()),
                _ => c.linked_change_request.as_deref(),
            },
            _ => None,
        }
    }

    pub fn session_key(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Session(key) => Some(key.as_str()),
                _ => c.linked_session.as_deref(),
            },
            _ => None,
        }
    }

    pub fn issue_keys(&self) -> &[String] {
        match self {
            CorrelationResult::Correlated(c) => &c.linked_issues,
            CorrelationResult::Standalone(StandaloneResult::Issue { key, .. }) => {
                std::slice::from_ref(key)
            }
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { .. }) => &[],
        }
    }

    pub fn workspace_refs(&self) -> &[String] {
        match self {
            CorrelationResult::Correlated(c) => &c.workspace_refs,
            _ => &[],
        }
    }

    pub fn correlation_group_idx(&self) -> Option<usize> {
        match self {
            CorrelationResult::Correlated(c) => Some(c.correlation_group_idx),
            _ => None,
        }
    }

    pub fn identity(&self) -> WorkItemIdentity {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => WorkItemIdentity::Checkout(co.key.clone()),
                CorrelatedAnchor::ChangeRequest(key) => {
                    WorkItemIdentity::ChangeRequest(key.clone())
                }
                CorrelatedAnchor::Session(key) => WorkItemIdentity::Session(key.clone()),
            },
            CorrelationResult::Standalone(s) => match s {
                StandaloneResult::Issue { key, .. } => WorkItemIdentity::Issue(key.clone()),
                StandaloneResult::RemoteBranch { branch } => {
                    WorkItemIdentity::RemoteBranch(branch.clone())
                }
            },
        }
    }

    pub fn as_correlated_mut(&mut self) -> Option<&mut CorrelatedWorkItem> {
        match self {
            CorrelationResult::Correlated(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct GroupedWorkItems {
    pub table_entries: Vec<GroupEntry>,
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

/// Convert a correlation group into a CorrelationResult.
/// Returns None for groups that contain only workspaces (no checkout, PR, or session).
fn group_to_work_item(
    providers: &ProviderData,
    group: &CorrelatedGroup,
    group_idx: usize,
) -> Option<CorrelationResult> {
    let mut checkout_ref: Option<CheckoutRef> = None;
    let mut change_request_key: Option<String> = None;
    let mut session_key: Option<String> = None;
    let mut workspace_refs: Vec<String> = Vec::new();

    for item in &group.items {
        match (&item.kind, &item.source_key) {
            (CorItemKind::Checkout, ProviderItemKey::Checkout(path)) => {
                if checkout_ref.is_none() {
                    let is_main_checkout =
                        providers.checkouts.get(path).is_some_and(|co| co.is_trunk);
                    checkout_ref = Some(CheckoutRef {
                        key: path.clone(),
                        is_main_checkout,
                    });
                }
            }
            (CorItemKind::ChangeRequest, ProviderItemKey::ChangeRequest(id)) => {
                change_request_key = Some(id.clone());
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
            (CorItemKind::ManagedTerminal, ProviderItemKey::ManagedTerminal(_key)) => {
                // Managed terminals contribute to correlation but don't need
                // explicit tracking on work items yet — their presence in the
                // group is enough. The terminal pool provider shows them in
                // ProviderData.managed_terminals.
            }
            _ => {}
        }
    }

    let (anchor, linked_change_request, linked_session) = if let Some(co) = checkout_ref {
        (
            CorrelatedAnchor::Checkout(co),
            change_request_key,
            session_key,
        )
    } else if let Some(key) = change_request_key {
        (CorrelatedAnchor::ChangeRequest(key), None, session_key)
    } else if let Some(key) = session_key {
        (CorrelatedAnchor::Session(key), None, None)
    } else {
        return None;
    };

    let branch = group.branch().map(|s| s.to_string());

    let pr_ref = match &anchor {
        CorrelatedAnchor::ChangeRequest(k) => Some(k.as_str()),
        _ => linked_change_request.as_deref(),
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

    Some(CorrelationResult::Correlated(CorrelatedWorkItem {
        anchor,
        branch,
        description,
        linked_change_request,
        linked_session,
        linked_issues: Vec::new(),
        workspace_refs,
        correlation_group_idx: group_idx,
    }))
}

/// Phases 1-3: Build CorrelatedItems, run union-find, convert to CorrelationResults.
/// Returns (work_items, correlation_groups).
pub fn correlate(providers: &ProviderData) -> (Vec<CorrelationResult>, Vec<CorrelatedGroup>) {
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

    for (key, terminal) in &providers.managed_terminals {
        let mut keys = vec![crate::providers::types::CorrelationKey::Branch(
            terminal.id.checkout.clone(),
        )];
        if !terminal.working_directory.as_os_str().is_empty() {
            keys.push(crate::providers::types::CorrelationKey::CheckoutPath(
                terminal.working_directory.clone(),
            ));
        }
        items.push(CorrelatedItem {
            provider_name: "terminal".to_string(),
            kind: CorItemKind::ManagedTerminal,
            title: format!("{} ({})", terminal.id, terminal.role),
            correlation_keys: keys,
            source_key: ProviderItemKey::ManagedTerminal(key.clone()),
        });
    }

    // Phase 2: Run correlation engine
    let groups = correlation::correlate(items);

    // Phase 3: Convert groups to CorrelationResults
    let mut work_items: Vec<CorrelationResult> = Vec::new();
    let mut linked_issue_keys: HashSet<String> = HashSet::new();

    for (group_idx, group) in groups.iter().enumerate() {
        let mut work_item = match group_to_work_item(providers, group, group_idx) {
            Some(wi) => wi,
            None => continue,
        };

        // Post-correlation: link issues via association keys on change requests
        if let Some(change_request_key) = work_item.change_request_key() {
            if let Some(cr) = providers.change_requests.get(change_request_key) {
                let issue_ids: Vec<String> = cr
                    .association_keys
                    .iter()
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
                let issue_ids: Vec<String> = co
                    .association_keys
                    .iter()
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
            work_items.push(CorrelationResult::Standalone(StandaloneResult::Issue {
                key: id.clone(),
                description: issue.title.clone(),
            }));
        }
    }

    // Add remote-only branches
    let known_branches: HashSet<String> = work_items
        .iter()
        .filter_map(|wi| wi.branch().map(|b| b.to_string()))
        .collect();
    for (b, branch_info) in &providers.branches {
        if branch_info.status == flotilla_protocol::BranchStatus::Remote
            && b.as_str() != "HEAD"
            && b.as_str() != "main"
            && b.as_str() != "master"
            && !known_branches.contains(b.as_str())
        {
            work_items.push(CorrelationResult::Standalone(
                StandaloneResult::RemoteBranch { branch: b.clone() },
            ));
        }
    }

    (work_items, groups)
}

/// Phase 4: Sort work items into sections and build table entries.
///
/// Accepts protocol `WorkItem` (flat, serializable) so this function can be
/// used both in-process (core side) and in the TUI after receiving a Snapshot.
pub fn group_work_items(
    work_items: &[flotilla_protocol::WorkItem],
    providers: &ProviderData,
    labels: &SectionLabels,
) -> GroupedWorkItems {
    let mut checkout_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut session_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut pr_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut remote_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut issue_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();

    for item in work_items {
        match item.kind {
            WorkItemKind::Checkout => checkout_items.push(item),
            WorkItemKind::Session => session_items.push(item),
            WorkItemKind::ChangeRequest => pr_items.push(item),
            WorkItemKind::RemoteBranch => remote_items.push(item),
            WorkItemKind::Issue => issue_items.push(item),
        }
    }

    let mut entries: Vec<GroupEntry> = Vec::new();
    let mut selectable: Vec<usize> = Vec::new();

    // Checkouts -- main first, then sorted by path ascending
    checkout_items.sort_by(|a, b| match (a.is_main_checkout, b.is_main_checkout) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.checkout_key().cmp(&b.checkout_key()),
    });
    if !checkout_items.is_empty() {
        entries.push(GroupEntry::Header(SectionHeader(labels.checkouts.clone())));
        for item in checkout_items {
            selectable.push(entries.len());
            entries.push(GroupEntry::Item(Box::new(item.clone())));
        }
    }

    // Sessions -- sorted by updated_at descending
    session_items.sort_by(|a, b| {
        let a_time = a
            .session_key
            .as_deref()
            .and_then(|k| providers.sessions.get(k))
            .and_then(|s| s.updated_at.as_deref());
        let b_time = b
            .session_key
            .as_deref()
            .and_then(|k| providers.sessions.get(k))
            .and_then(|s| s.updated_at.as_deref());
        b_time.cmp(&a_time)
    });
    if !session_items.is_empty() {
        entries.push(GroupEntry::Header(SectionHeader(labels.sessions.clone())));
        for item in session_items {
            selectable.push(entries.len());
            entries.push(GroupEntry::Item(Box::new(item.clone())));
        }
    }

    // PRs -- sorted by id descending
    pr_items.sort_by(|a, b| {
        let a_num = a
            .change_request_key
            .as_deref()
            .and_then(|k| k.parse::<i64>().ok());
        let b_num = b
            .change_request_key
            .as_deref()
            .and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !pr_items.is_empty() {
        entries.push(GroupEntry::Header(SectionHeader(
            labels.code_review.clone(),
        )));
        for item in pr_items {
            selectable.push(entries.len());
            entries.push(GroupEntry::Item(Box::new(item.clone())));
        }
    }

    // Remote branches -- sorted by branch name
    remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));
    if !remote_items.is_empty() {
        entries.push(GroupEntry::Header(SectionHeader("Remote Branches".into())));
        for item in remote_items {
            selectable.push(entries.len());
            entries.push(GroupEntry::Item(Box::new(item.clone())));
        }
    }

    // Issues -- sorted by id descending
    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
    if !issue_items.is_empty() {
        entries.push(GroupEntry::Header(SectionHeader(labels.issues.clone())));
        for item in issue_items {
            selectable.push(entries.len());
            entries.push(GroupEntry::Item(Box::new(item.clone())));
        }
    }

    GroupedWorkItems {
        table_entries: entries,
        selectable_indices: selectable,
    }
}

pub async fn fetch_checkout_status(
    branch: &str,
    checkout_path: Option<&Path>,
    change_request_id: Option<&str>,
    repo_root: &Path,
    runner: &dyn crate::providers::CommandRunner,
) -> CheckoutStatus {
    let branch_owned = branch.to_string();
    let repo = repo_root.to_path_buf();
    let checkout_path = checkout_path.map(|p| p.to_path_buf());
    let cr_id = change_request_id.map(|s| s.to_string());
    let repo2 = repo_root.to_path_buf();

    let repo_for_base = repo.clone();
    let branch_for_base = branch_owned.clone();

    let (unpushed_result, uncommitted, pr_info) = tokio::join!(
        async {
            let base = async {
                let upstream = runner
                    .run(
                        "git",
                        &[
                            "rev-parse",
                            "--abbrev-ref",
                            &format!("{branch_for_base}@{{upstream}}"),
                        ],
                        &repo_for_base,
                    )
                    .await;
                if let Ok(ref u) = upstream {
                    let u = u.trim();
                    if !u.is_empty() {
                        return Ok(u.to_string());
                    }
                }
                let remote_head = runner
                    .run(
                        "git",
                        &["rev-parse", "--abbrev-ref", "origin/HEAD"],
                        &repo_for_base,
                    )
                    .await;
                if let Ok(ref rh) = remote_head {
                    let rh = rh.trim();
                    if !rh.is_empty() {
                        return Ok(rh.to_string());
                    }
                }
                Err("Could not determine base branch — unpushed commit status unknown".to_string())
            }
            .await;

            match base {
                Ok(base_ref) => {
                    let log = runner
                        .run(
                            "git",
                            &[
                                "log",
                                &format!("{base_ref}..{branch_for_base}"),
                                "--oneline",
                            ],
                            &repo_for_base,
                        )
                        .await
                        .unwrap_or_default();
                    Ok(log)
                }
                Err(warning) => Err(warning),
            }
        },
        async {
            if let Some(path) = &checkout_path {
                runner
                    .run("git", &["status", "--porcelain"], path)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            }
        },
        async {
            if let Some(ref num) = cr_id {
                runner
                    .run(
                        "gh",
                        &["pr", "view", num, "--json", "state,mergeCommit"],
                        &repo2,
                    )
                    .await
                    .ok()
            } else {
                None
            }
        },
    );

    let mut info = CheckoutStatus {
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
            info.change_request_status = v
                .get("state")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            info.merge_commit_sha = v
                .get("mergeCommit")
                .and_then(|mc| mc.get("oid"))
                .and_then(|s| s.as_str())
                .map(|s| s[..7.min(s.len())].to_string());
        }
    }

    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_data::ProviderData;
    use crate::providers::types::*;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Helper: build a minimal CorrelatedWorkItem with sensible defaults
    // -----------------------------------------------------------------------

    fn correlated(anchor: CorrelatedAnchor) -> CorrelatedWorkItem {
        CorrelatedWorkItem {
            anchor,
            branch: None,
            description: String::new(),
            linked_change_request: None,
            linked_session: None,
            linked_issues: Vec::new(),
            workspace_refs: Vec::new(),
            correlation_group_idx: 0,
        }
    }

    fn checkout_item(path: &str, branch: Option<&str>, is_main: bool) -> CorrelationResult {
        CorrelationResult::Correlated(CorrelatedWorkItem {
            branch: branch.map(|s| s.to_string()),
            description: branch.unwrap_or("").to_string(),
            ..correlated(CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from(path),
                is_main_checkout: is_main,
            }))
        })
    }

    fn cr_item(key: &str, desc: &str) -> CorrelationResult {
        CorrelationResult::Correlated(CorrelatedWorkItem {
            description: desc.to_string(),
            ..correlated(CorrelatedAnchor::ChangeRequest(key.to_string()))
        })
    }

    fn session_item(key: &str, desc: &str) -> CorrelationResult {
        CorrelationResult::Correlated(CorrelatedWorkItem {
            description: desc.to_string(),
            ..correlated(CorrelatedAnchor::Session(key.to_string()))
        })
    }

    fn issue_item(key: &str, desc: &str) -> CorrelationResult {
        CorrelationResult::Standalone(StandaloneResult::Issue {
            key: key.to_string(),
            description: desc.to_string(),
        })
    }

    fn remote_branch_item(branch: &str) -> CorrelationResult {
        CorrelationResult::Standalone(StandaloneResult::RemoteBranch {
            branch: branch.to_string(),
        })
    }

    fn make_checkout(branch: &str, path: &str, is_trunk: bool) -> Checkout {
        let p = PathBuf::from(path);
        Checkout {
            branch: branch.to_string(),
            is_trunk,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![
                CorrelationKey::Branch(branch.to_string()),
                CorrelationKey::CheckoutPath(p),
            ],
            association_keys: vec![],
        }
    }

    fn make_change_request(_id: &str, title: &str, branch: &str) -> ChangeRequest {
        ChangeRequest {
            title: title.to_string(),
            branch: branch.to_string(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.to_string())],
            association_keys: vec![],
        }
    }

    fn make_session(id: &str, title: &str, branch: Option<&str>) -> CloudAgentSession {
        let mut keys = Vec::new();
        if let Some(b) = branch {
            keys.push(CorrelationKey::Branch(b.to_string()));
        }
        keys.push(CorrelationKey::SessionRef(
            "claude".to_string(),
            id.to_string(),
        ));
        CloudAgentSession {
            title: title.to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: keys,
        }
    }

    fn make_issue(_id: &str, title: &str) -> Issue {
        Issue {
            title: title.to_string(),
            labels: vec![],
            association_keys: vec![],
        }
    }

    fn make_workspace(
        _ws_ref: &str,
        name: &str,
        directories: Vec<PathBuf>,
        correlation_keys: Vec<CorrelationKey>,
    ) -> Workspace {
        Workspace {
            name: name.to_string(),
            directories,
            correlation_keys,
        }
    }

    // Convert CorrelationResult to protocol WorkItem for group_work_items tests
    fn to_proto(item: &CorrelationResult) -> flotilla_protocol::WorkItem {
        crate::convert::correlation_result_to_work_item(item, &[])
    }

    fn new_providers() -> ProviderData {
        ProviderData::default()
    }

    fn default_labels() -> SectionLabels {
        SectionLabels::default()
    }

    fn header_titles(entries: &[GroupEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Header(h) => Some(h.0.clone()),
                GroupEntry::Item(_) => None,
            })
            .collect()
    }

    fn item_branches(entries: &[GroupEntry]) -> Vec<Option<String>> {
        entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Header(_) => None,
                GroupEntry::Item(item) => Some(item.branch.clone()),
            })
            .collect()
    }

    fn item_change_request_keys(entries: &[GroupEntry]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Header(_) => None,
                GroupEntry::Item(item) => item.change_request_key.clone(),
            })
            .collect()
    }

    fn issue_key_groups(entries: &[GroupEntry]) -> Vec<Vec<String>> {
        entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Header(_) => None,
                GroupEntry::Item(item) => {
                    if item.kind == WorkItemKind::Issue {
                        Some(item.issue_keys.clone())
                    } else {
                        None
                    }
                }
            })
            .collect()
    }

    fn session_descriptions(entries: &[GroupEntry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Header(_) => None,
                GroupEntry::Item(item) => {
                    if item.kind == WorkItemKind::Session {
                        Some(item.description.as_str())
                    } else {
                        None
                    }
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Display / formatting tests
    // -----------------------------------------------------------------------

    #[test]
    fn refresh_error_display() {
        let err = RefreshError {
            category: "github",
            message: "rate limited".to_string(),
        };
        assert_eq!(format!("{err}"), "github: rate limited");
    }

    #[test]
    fn section_header_display() {
        let hdr = SectionHeader("Checkouts".to_string());
        assert_eq!(format!("{hdr}"), "Checkouts");
    }

    // -----------------------------------------------------------------------
    // WorkItemKind classification tests
    // -----------------------------------------------------------------------

    #[test]
    fn kind_checkout() {
        let wi = checkout_item("/tmp/foo", None, false);
        assert_eq!(wi.kind(), WorkItemKind::Checkout);
    }

    #[test]
    fn kind_change_request() {
        let wi = cr_item("42", "PR title");
        assert_eq!(wi.kind(), WorkItemKind::ChangeRequest);
    }

    #[test]
    fn kind_session() {
        let wi = session_item("sess-1", "Session title");
        assert_eq!(wi.kind(), WorkItemKind::Session);
    }

    #[test]
    fn kind_issue() {
        let wi = issue_item("7", "Fix bug");
        assert_eq!(wi.kind(), WorkItemKind::Issue);
    }

    #[test]
    fn kind_remote_branch() {
        let wi = remote_branch_item("feature/x");
        assert_eq!(wi.kind(), WorkItemKind::RemoteBranch);
    }

    // -----------------------------------------------------------------------
    // Accessor tests: branch()
    // -----------------------------------------------------------------------

    #[test]
    fn branch_from_checkout_with_branch() {
        let wi = checkout_item("/tmp/wt", Some("feat-x"), false);
        assert_eq!(wi.branch(), Some("feat-x"));
    }

    #[test]
    fn branch_from_checkout_without_branch() {
        let wi = checkout_item("/tmp/wt", None, false);
        assert_eq!(wi.branch(), None);
    }

    #[test]
    fn branch_from_remote_branch() {
        let wi = remote_branch_item("origin/develop");
        assert_eq!(wi.branch(), Some("origin/develop"));
    }

    #[test]
    fn branch_from_issue_is_none() {
        let wi = issue_item("1", "desc");
        assert_eq!(wi.branch(), None);
    }

    #[test]
    fn branch_from_change_request_correlated() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            branch: Some("cr-branch".to_string()),
            ..correlated(CorrelatedAnchor::ChangeRequest("10".to_string()))
        });
        assert_eq!(wi.branch(), Some("cr-branch"));
    }

    // -----------------------------------------------------------------------
    // Accessor tests: description()
    // -----------------------------------------------------------------------

    #[test]
    fn description_from_correlated() {
        let wi = cr_item("1", "Fix login flow");
        assert_eq!(wi.description(), "Fix login flow");
    }

    #[test]
    fn description_from_standalone_issue() {
        let wi = issue_item("5", "Add caching");
        assert_eq!(wi.description(), "Add caching");
    }

    #[test]
    fn description_from_remote_branch_is_branch_name() {
        let wi = remote_branch_item("feature/auth");
        assert_eq!(wi.description(), "feature/auth");
    }

    // -----------------------------------------------------------------------
    // Accessor tests: checkout(), checkout_key(), is_main_checkout()
    // -----------------------------------------------------------------------

    #[test]
    fn checkout_returns_some_for_checkout_anchor() {
        let wi = checkout_item("/tmp/wt", Some("main"), true);
        let co = wi.checkout().expect("should return checkout");
        assert_eq!(co.key, PathBuf::from("/tmp/wt"));
        assert!(co.is_main_checkout);
    }

    #[test]
    fn checkout_returns_none_for_non_checkout() {
        assert!(cr_item("1", "d").checkout().is_none());
        assert!(session_item("s", "d").checkout().is_none());
        assert!(issue_item("i", "d").checkout().is_none());
        assert!(remote_branch_item("b").checkout().is_none());
    }

    #[test]
    fn checkout_key_returns_path() {
        let wi = checkout_item("/repos/proj", None, false);
        assert_eq!(wi.checkout_key(), Some(Path::new("/repos/proj")));
    }

    #[test]
    fn checkout_key_none_for_standalone() {
        assert!(issue_item("1", "d").checkout_key().is_none());
    }

    #[test]
    fn is_main_checkout_true() {
        let wi = checkout_item("/repos/main", Some("main"), true);
        assert!(wi.is_main_checkout());
    }

    #[test]
    fn is_main_checkout_false_for_non_main() {
        let wi = checkout_item("/repos/feat", Some("feat"), false);
        assert!(!wi.is_main_checkout());
    }

    #[test]
    fn is_main_checkout_false_for_non_checkout() {
        assert!(!cr_item("1", "d").is_main_checkout());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: change_request_key()
    // -----------------------------------------------------------------------

    #[test]
    fn change_request_key_from_cr_anchor() {
        let wi = cr_item("42", "PR");
        assert_eq!(wi.change_request_key(), Some("42"));
    }

    #[test]
    fn change_request_key_from_linked_on_checkout() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            linked_change_request: Some("99".to_string()),
            ..correlated(CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_checkout: false,
            }))
        });
        assert_eq!(wi.change_request_key(), Some("99"));
    }

    #[test]
    fn change_request_key_none_for_standalone() {
        assert!(issue_item("1", "d").change_request_key().is_none());
        assert!(remote_branch_item("b").change_request_key().is_none());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: session_key()
    // -----------------------------------------------------------------------

    #[test]
    fn session_key_from_session_anchor() {
        let wi = session_item("sess-x", "title");
        assert_eq!(wi.session_key(), Some("sess-x"));
    }

    #[test]
    fn session_key_from_linked_on_checkout() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            linked_session: Some("linked-sess".to_string()),
            ..correlated(CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_checkout: false,
            }))
        });
        assert_eq!(wi.session_key(), Some("linked-sess"));
    }

    #[test]
    fn session_key_none_for_standalone() {
        assert!(issue_item("1", "d").session_key().is_none());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: issue_keys()
    // -----------------------------------------------------------------------

    #[test]
    fn issue_keys_from_correlated_with_linked_issues() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            linked_issues: vec!["10".to_string(), "20".to_string()],
            ..correlated(CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_checkout: false,
            }))
        });
        assert_eq!(wi.issue_keys(), &["10".to_string(), "20".to_string()]);
    }

    #[test]
    fn issue_keys_from_standalone_issue_returns_single() {
        let wi = issue_item("42", "desc");
        assert_eq!(wi.issue_keys(), &["42".to_string()]);
    }

    #[test]
    fn issue_keys_empty_for_remote_branch() {
        let wi = remote_branch_item("b");
        assert!(wi.issue_keys().is_empty());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: workspace_refs()
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_refs_from_correlated() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            workspace_refs: vec!["ws-1".to_string()],
            ..correlated(CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_checkout: false,
            }))
        });
        assert_eq!(wi.workspace_refs(), &["ws-1".to_string()]);
    }

    #[test]
    fn workspace_refs_empty_for_standalone() {
        assert!(issue_item("1", "d").workspace_refs().is_empty());
        assert!(remote_branch_item("b").workspace_refs().is_empty());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: correlation_group_idx()
    // -----------------------------------------------------------------------

    #[test]
    fn correlation_group_idx_from_correlated() {
        let wi = CorrelationResult::Correlated(CorrelatedWorkItem {
            correlation_group_idx: 7,
            ..correlated(CorrelatedAnchor::Session("s".to_string()))
        });
        assert_eq!(wi.correlation_group_idx(), Some(7));
    }

    #[test]
    fn correlation_group_idx_none_for_standalone() {
        assert!(issue_item("1", "d").correlation_group_idx().is_none());
        assert!(remote_branch_item("b").correlation_group_idx().is_none());
    }

    // -----------------------------------------------------------------------
    // Accessor tests: as_correlated_mut()
    // -----------------------------------------------------------------------

    #[test]
    fn as_correlated_mut_returns_some_for_correlated() {
        let mut wi = checkout_item("/tmp/wt", Some("feat"), false);
        let inner = wi.as_correlated_mut().expect("should be Some");
        inner.linked_issues.push("99".to_string());
        assert_eq!(wi.issue_keys(), &["99".to_string()]);
    }

    #[test]
    fn as_correlated_mut_returns_none_for_standalone() {
        let mut wi = issue_item("1", "d");
        assert!(wi.as_correlated_mut().is_none());
    }

    // -----------------------------------------------------------------------
    // Identity tests (existing)
    // -----------------------------------------------------------------------

    #[test]
    fn identity_checkout() {
        let wi = checkout_item("/tmp/foo", None, false);
        assert_eq!(
            wi.identity(),
            WorkItemIdentity::Checkout(PathBuf::from("/tmp/foo"))
        );
    }

    #[test]
    fn identity_pr() {
        let wi = cr_item("42", "PR");
        assert_eq!(
            wi.identity(),
            WorkItemIdentity::ChangeRequest("42".to_string())
        );
    }

    #[test]
    fn identity_session() {
        let wi = session_item("sess-1", "title");
        assert_eq!(
            wi.identity(),
            WorkItemIdentity::Session("sess-1".to_string())
        );
    }

    #[test]
    fn identity_issue() {
        let wi = issue_item("7", "desc");
        assert_eq!(wi.identity(), WorkItemIdentity::Issue("7".to_string()));
    }

    #[test]
    fn identity_remote_branch() {
        let wi = remote_branch_item("feature/x");
        assert_eq!(
            wi.identity(),
            WorkItemIdentity::RemoteBranch("feature/x".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // correlate() tests
    // -----------------------------------------------------------------------

    #[test]
    fn correlate_empty_provider_data() {
        let providers = new_providers();
        let (items, groups) = correlate(&providers);
        assert!(items.is_empty());
        assert!(groups.is_empty());
    }

    #[test]
    fn correlate_single_checkout() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat"),
            make_checkout("feat", "/tmp/feat", false),
        );

        let (items, groups) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::Checkout);
        assert_eq!(items[0].branch(), Some("feat"));
    }

    #[test]
    fn correlate_trunk_checkout_marked_as_main() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/main"),
            make_checkout("main", "/tmp/main", true),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_main_checkout());
    }

    #[test]
    fn correlate_checkout_and_pr_merge_on_branch() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat-x"),
            make_checkout("feat-x", "/tmp/feat-x", false),
        );
        providers.change_requests.insert(
            "10".to_string(),
            make_change_request("10", "Add auth", "feat-x"),
        );

        let (items, _) = correlate(&providers);
        // Should merge into one work item
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::Checkout); // checkout is preferred anchor
        assert_eq!(items[0].change_request_key(), Some("10"));
        // Description comes from PR title since it's non-empty
        assert_eq!(items[0].description(), "Add auth");
    }

    #[test]
    fn correlate_checkout_pr_session_merge_on_branch() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat-y"),
            make_checkout("feat-y", "/tmp/feat-y", false),
        );
        providers.change_requests.insert(
            "20".to_string(),
            make_change_request("20", "Improve perf", "feat-y"),
        );
        providers.sessions.insert(
            "sess-a".to_string(),
            make_session("sess-a", "Debug perf", Some("feat-y")),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::Checkout);
        assert_eq!(items[0].change_request_key(), Some("20"));
        assert_eq!(items[0].session_key(), Some("sess-a"));
    }

    #[test]
    fn correlate_session_only_becomes_session_anchor() {
        let mut providers = new_providers();
        providers.sessions.insert(
            "sess-lonely".to_string(),
            make_session("sess-lonely", "Solo session", None),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::Session);
        assert_eq!(items[0].session_key(), Some("sess-lonely"));
    }

    #[test]
    fn correlate_pr_only_becomes_cr_anchor() {
        let mut providers = new_providers();
        providers.change_requests.insert(
            "50".to_string(),
            make_change_request("50", "Orphan PR", "no-checkout-branch"),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::ChangeRequest);
        assert_eq!(items[0].change_request_key(), Some("50"));
    }

    #[test]
    fn correlate_standalone_issue_appears_as_issue() {
        let mut providers = new_providers();
        providers
            .issues
            .insert("100".to_string(), make_issue("100", "Standalone bug"));

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::Issue);
        assert_eq!(items[0].description(), "Standalone bug");
    }

    #[test]
    fn correlate_remote_branches_appear_as_standalone() {
        let mut providers = new_providers();
        providers.branches.insert(
            "feature/remote-only".to_string(),
            flotilla_protocol::delta::Branch {
                status: flotilla_protocol::BranchStatus::Remote,
            },
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind(), WorkItemKind::RemoteBranch);
        assert_eq!(items[0].branch(), Some("feature/remote-only"));
    }

    #[test]
    fn correlate_remote_branches_excludes_head_main_master() {
        let mut providers = new_providers();
        let remote = flotilla_protocol::delta::Branch {
            status: flotilla_protocol::BranchStatus::Remote,
        };
        providers
            .branches
            .insert("HEAD".to_string(), remote.clone());
        providers
            .branches
            .insert("main".to_string(), remote.clone());
        providers
            .branches
            .insert("master".to_string(), remote.clone());
        providers
            .branches
            .insert("feature/visible".to_string(), remote);

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].branch(), Some("feature/visible"));
    }

    #[test]
    fn correlate_remote_branches_excludes_already_known() {
        let mut providers = new_providers();
        // A checkout on branch "feat-z"
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat-z"),
            make_checkout("feat-z", "/tmp/feat-z", false),
        );
        // Same branch also in remote
        providers.branches.insert(
            "feat-z".to_string(),
            flotilla_protocol::delta::Branch {
                status: flotilla_protocol::BranchStatus::Remote,
            },
        );

        let (items, _) = correlate(&providers);
        // Should only have the checkout, not a duplicate remote
        let remote_items: Vec<_> = items
            .iter()
            .filter(|wi| wi.kind() == WorkItemKind::RemoteBranch)
            .collect();
        assert!(remote_items.is_empty());
    }

    #[test]
    fn correlate_merged_branches_excluded() {
        let mut providers = new_providers();
        providers.branches.insert(
            "already-merged".to_string(),
            flotilla_protocol::delta::Branch {
                status: flotilla_protocol::BranchStatus::Merged,
            },
        );

        let (items, _) = correlate(&providers);
        assert!(items.is_empty());
    }

    #[test]
    fn correlate_pr_links_issue_via_association_key() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat"),
            make_checkout("feat", "/tmp/feat", false),
        );
        let mut cr = make_change_request("5", "Impl feature", "feat");
        cr.association_keys
            .push(AssociationKey::IssueRef("gh".to_string(), "77".to_string()));
        providers.change_requests.insert("5".to_string(), cr);
        providers
            .issues
            .insert("77".to_string(), make_issue("77", "Feature request"));

        let (items, _) = correlate(&providers);
        let checkout = items
            .iter()
            .find(|wi| wi.kind() == WorkItemKind::Checkout)
            .expect("should have checkout");
        assert!(checkout.issue_keys().contains(&"77".to_string()));
        // Issue should not appear standalone
        assert!(!items.iter().any(|wi| wi.kind() == WorkItemKind::Issue));
    }

    #[test]
    fn checkout_association_keys_link_issues() {
        let mut providers = new_providers();

        let co_path = PathBuf::from("/tmp/feat-x");
        let mut co = make_checkout("feat-x", "/tmp/feat-x", false);
        co.association_keys
            .push(AssociationKey::IssueRef("github".into(), "42".into()));
        providers.checkouts.insert(co_path, co);
        providers
            .issues
            .insert("42".to_string(), make_issue("42", "Fix the thing"));

        let (work_items, _groups) = correlate(&providers);
        let checkout_wi = work_items
            .iter()
            .find(|wi| wi.kind() == WorkItemKind::Checkout)
            .expect("should have a checkout work item");
        assert!(
            checkout_wi.issue_keys().contains(&"42".to_string()),
            "checkout should link issue 42 via association key, got: {:?}",
            checkout_wi.issue_keys()
        );
        let standalone_issues: Vec<_> = work_items
            .iter()
            .filter(|wi| wi.kind() == WorkItemKind::Issue)
            .collect();
        assert!(
            standalone_issues.is_empty(),
            "issue 42 should be linked, not standalone"
        );
    }

    #[test]
    fn correlate_workspace_only_group_is_skipped() {
        // A workspace with no checkout/PR/session should be excluded
        let mut providers = new_providers();
        providers.workspaces.insert(
            "ws-orphan".to_string(),
            make_workspace("ws-orphan", "orphan", vec![], vec![]),
        );

        let (items, _) = correlate(&providers);
        assert!(items.is_empty(), "workspace-only group should be skipped");
    }

    #[test]
    fn correlate_workspace_linked_to_checkout() {
        let mut providers = new_providers();
        let co_path = PathBuf::from("/tmp/feat-ws");
        providers.checkouts.insert(
            co_path.clone(),
            make_checkout("feat-ws", "/tmp/feat-ws", false),
        );
        providers.workspaces.insert(
            "ws-1".to_string(),
            make_workspace(
                "ws-1",
                "dev-session",
                vec![co_path.clone()],
                vec![CorrelationKey::CheckoutPath(co_path)],
            ),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].workspace_refs(), &["ws-1".to_string()]);
    }

    #[test]
    fn correlate_description_prefers_pr_title() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat"),
            make_checkout("feat", "/tmp/feat", false),
        );
        providers.change_requests.insert(
            "1".to_string(),
            make_change_request("1", "My PR Title", "feat"),
        );
        providers.sessions.insert(
            "s1".to_string(),
            make_session("s1", "My Session Title", Some("feat")),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items[0].description(), "My PR Title");
    }

    #[test]
    fn correlate_description_falls_back_to_session_title() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/feat"),
            make_checkout("feat", "/tmp/feat", false),
        );
        providers.sessions.insert(
            "s1".to_string(),
            make_session("s1", "Session Title", Some("feat")),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items[0].description(), "Session Title");
    }

    #[test]
    fn correlate_description_falls_back_to_branch() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/my-branch"),
            make_checkout("my-branch", "/tmp/my-branch", false),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items[0].description(), "my-branch");
    }

    #[test]
    fn correlate_multiple_items_sharing_branch_merge() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/shared"),
            make_checkout("shared-branch", "/tmp/shared", false),
        );
        providers.change_requests.insert(
            "1".to_string(),
            make_change_request("1", "PR on shared", "shared-branch"),
        );
        providers.sessions.insert(
            "s1".to_string(),
            make_session("s1", "Session on shared", Some("shared-branch")),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 1, "all items should merge into one");
        assert_eq!(items[0].kind(), WorkItemKind::Checkout);
        assert_eq!(items[0].change_request_key(), Some("1"));
        assert_eq!(items[0].session_key(), Some("s1"));
        assert_eq!(items[0].branch(), Some("shared-branch"));
    }

    #[test]
    fn correlate_two_checkouts_stay_separate() {
        let mut providers = new_providers();
        providers.checkouts.insert(
            PathBuf::from("/tmp/a"),
            make_checkout("branch-a", "/tmp/a", false),
        );
        providers.checkouts.insert(
            PathBuf::from("/tmp/b"),
            make_checkout("branch-b", "/tmp/b", false),
        );

        let (items, _) = correlate(&providers);
        assert_eq!(items.len(), 2);
        let branches: HashSet<_> = items.iter().filter_map(|wi| wi.branch()).collect();
        assert!(branches.contains("branch-a"));
        assert!(branches.contains("branch-b"));
    }

    #[test]
    fn correlate_issue_not_in_provider_data_ignored_by_association() {
        // An association key pointing to a non-existent issue should be ignored
        let mut providers = new_providers();
        let mut cr = make_change_request("5", "PR", "feat");
        cr.association_keys
            .push(AssociationKey::IssueRef("gh".into(), "999".into()));
        providers.change_requests.insert("5".to_string(), cr);
        // Note: no issue "999" in providers.issues

        let (items, _) = correlate(&providers);
        let cr_item = items
            .iter()
            .find(|wi| wi.kind() == WorkItemKind::ChangeRequest)
            .unwrap();
        assert!(cr_item.issue_keys().is_empty());
    }

    // -----------------------------------------------------------------------
    // group_work_items() tests
    // -----------------------------------------------------------------------

    #[test]
    fn group_work_items_empty_input() {
        let providers = new_providers();
        let labels = default_labels();
        let result = group_work_items(&[], &providers, &labels);
        assert!(result.table_entries.is_empty());
        assert!(result.selectable_indices.is_empty());
    }

    #[test]
    fn group_work_items_single_checkout() {
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![to_proto(&checkout_item("/tmp/wt", Some("feat"), false))];
        let result = group_work_items(&items, &providers, &labels);

        // Should have 1 header + 1 item
        assert_eq!(result.table_entries.len(), 2);
        assert!(matches!(result.table_entries[0], GroupEntry::Header(_)));
        assert!(matches!(result.table_entries[1], GroupEntry::Item(_)));
        assert_eq!(result.selectable_indices, vec![1]);
    }

    #[test]
    fn group_work_items_sections_appear_in_order() {
        // checkouts, sessions, PRs, remote branches, issues
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![
            to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
            to_proto(&session_item("s1", "Session")),
            to_proto(&cr_item("10", "PR")),
            to_proto(&remote_branch_item("origin/dev")),
            to_proto(&issue_item("1", "Bug")),
        ];
        let result = group_work_items(&items, &providers, &labels);

        // Expect 5 headers + 5 items = 10 entries
        assert_eq!(result.table_entries.len(), 10);

        let headers = header_titles(&result.table_entries);
        assert_eq!(
            headers,
            vec![
                "Checkouts",
                "Sessions",
                "Change Requests",
                "Remote Branches",
                "Issues",
            ]
        );
    }

    #[test]
    fn group_work_items_checkouts_sorted_by_path_main_first() {
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![
            to_proto(&checkout_item("/tmp/z", Some("z-branch"), false)),
            to_proto(&checkout_item("/tmp/a", Some("a-branch"), false)),
            to_proto(&checkout_item("/tmp/main", Some("main"), true)),
            to_proto(&checkout_item("/tmp/m", Some("m-branch"), false)),
        ];
        let result = group_work_items(&items, &providers, &labels);

        let branches = item_branches(&result.table_entries);
        assert_eq!(
            branches,
            vec![
                Some("main".to_string()),     // main always first
                Some("a-branch".to_string()), // then by path ascending
                Some("m-branch".to_string()),
                Some("z-branch".to_string()),
            ]
        );
    }

    #[test]
    fn group_work_items_prs_sorted_by_id_descending() {
        let providers = new_providers();
        let labels = default_labels();
        let pr1 = to_proto(&cr_item("1", "PR one"));
        let pr5 = to_proto(&cr_item("5", "PR five"));
        let pr3 = to_proto(&cr_item("3", "PR three"));

        let items = vec![pr1, pr5, pr3];
        let result = group_work_items(&items, &providers, &labels);

        let cr_keys = item_change_request_keys(&result.table_entries);
        assert_eq!(cr_keys, vec!["5", "3", "1"]);
    }

    #[test]
    fn group_work_items_issues_sorted_by_id_descending() {
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![
            to_proto(&issue_item("3", "Issue three")),
            to_proto(&issue_item("10", "Issue ten")),
            to_proto(&issue_item("1", "Issue one")),
        ];
        let result = group_work_items(&items, &providers, &labels);

        let issue_keys = issue_key_groups(&result.table_entries);
        assert_eq!(
            issue_keys,
            vec![
                vec!["10".to_string()],
                vec!["3".to_string()],
                vec!["1".to_string()]
            ]
        );
    }

    #[test]
    fn group_work_items_remote_branches_sorted_by_name() {
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![
            to_proto(&remote_branch_item("z-remote")),
            to_proto(&remote_branch_item("a-remote")),
        ];
        let result = group_work_items(&items, &providers, &labels);

        let branches = item_branches(&result.table_entries);
        assert_eq!(
            branches,
            vec![Some("a-remote".to_string()), Some("z-remote".to_string()),]
        );
    }

    #[test]
    fn group_work_items_selectable_indices_skip_headers() {
        let providers = new_providers();
        let labels = default_labels();
        let items = vec![
            to_proto(&checkout_item("/tmp/a", Some("a"), false)),
            to_proto(&checkout_item("/tmp/b", Some("b"), false)),
            to_proto(&issue_item("1", "Bug")),
        ];
        let result = group_work_items(&items, &providers, &labels);

        // Layout: Header(0), Item(1), Item(2), Header(3), Item(4)
        assert_eq!(result.selectable_indices, vec![1, 2, 4]);
    }

    #[test]
    fn group_work_items_empty_sections_omitted() {
        let providers = new_providers();
        let labels = default_labels();
        // Only issues, no checkouts/sessions/PRs/remote
        let items = vec![to_proto(&issue_item("1", "Bug"))];
        let result = group_work_items(&items, &providers, &labels);

        assert_eq!(result.table_entries.len(), 2); // 1 header + 1 item
        let headers = header_titles(&result.table_entries);
        assert_eq!(headers, vec!["Issues"]);
    }

    #[test]
    fn group_work_items_uses_custom_labels() {
        let providers = new_providers();
        let labels = SectionLabels {
            checkouts: "Checkouts".into(),
            code_review: "Pull Requests".into(),
            issues: "Tickets".into(),
            sessions: "Agents".into(),
        };
        let items = vec![
            to_proto(&checkout_item("/tmp/wt", Some("feat"), false)),
            to_proto(&session_item("s1", "Agent")),
            to_proto(&cr_item("1", "PR")),
            to_proto(&issue_item("1", "Ticket")),
        ];
        let result = group_work_items(&items, &providers, &labels);

        let headers = header_titles(&result.table_entries);
        assert_eq!(
            headers,
            vec!["Checkouts", "Agents", "Pull Requests", "Tickets"]
        );
    }

    #[test]
    fn group_work_items_sessions_sorted_by_updated_at_descending() {
        let mut providers = new_providers();
        // Populate providers with sessions that have updated_at
        providers.sessions.insert(
            "s-old".to_string(),
            CloudAgentSession {
                title: "Old".to_string(),
                status: SessionStatus::Idle,
                model: None,
                updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                correlation_keys: vec![],
            },
        );
        providers.sessions.insert(
            "s-new".to_string(),
            CloudAgentSession {
                title: "New".to_string(),
                status: SessionStatus::Running,
                model: None,
                updated_at: Some("2026-03-01T00:00:00Z".to_string()),
                correlation_keys: vec![],
            },
        );
        providers.sessions.insert(
            "s-mid".to_string(),
            CloudAgentSession {
                title: "Mid".to_string(),
                status: SessionStatus::Running,
                model: None,
                updated_at: Some("2026-02-01T00:00:00Z".to_string()),
                correlation_keys: vec![],
            },
        );

        let labels = default_labels();
        let si1 = to_proto(&session_item("s-old", "Old"));
        let si2 = to_proto(&session_item("s-new", "New"));
        let si3 = to_proto(&session_item("s-mid", "Mid"));

        let items = vec![si1, si2, si3];
        let result = group_work_items(&items, &providers, &labels);

        let session_descs = session_descriptions(&result.table_entries);
        assert_eq!(session_descs, vec!["New", "Mid", "Old"]);
    }

    // -----------------------------------------------------------------------
    // SectionLabels default test
    // -----------------------------------------------------------------------

    #[test]
    fn section_labels_default_values() {
        let labels = default_labels();
        assert_eq!(labels.checkouts, "Checkouts");
        assert_eq!(labels.code_review, "Change Requests");
        assert_eq!(labels.issues, "Issues");
        assert_eq!(labels.sessions, "Sessions");
    }

    // -----------------------------------------------------------------------
    // GroupedWorkItems default test
    // -----------------------------------------------------------------------

    #[test]
    fn grouped_work_items_default_is_empty() {
        let g = GroupedWorkItems::default();
        assert!(g.table_entries.is_empty());
        assert!(g.selectable_indices.is_empty());
    }

    // -----------------------------------------------------------------------
    // DataStore default test
    // -----------------------------------------------------------------------

    #[test]
    fn data_store_default() {
        let ds = DataStore::default();
        assert!(!ds.loading);
        assert!(ds.correlation_groups.is_empty());
        assert!(ds.provider_health.is_empty());
    }

    // -----------------------------------------------------------------------
    // Integration-style: end-to-end correlate + group
    // -----------------------------------------------------------------------

    #[test]
    fn end_to_end_mixed_providers() {
        let mut providers = new_providers();

        // trunk checkout
        providers
            .checkouts
            .insert(PathBuf::from("/repo"), make_checkout("main", "/repo", true));
        // feature checkout + PR
        providers.checkouts.insert(
            PathBuf::from("/repo.feat"),
            make_checkout("feat-login", "/repo.feat", false),
        );
        providers.change_requests.insert(
            "10".to_string(),
            make_change_request("10", "Add login", "feat-login"),
        );
        // standalone session
        providers.sessions.insert(
            "s-solo".to_string(),
            make_session("s-solo", "Solo work", None),
        );
        // standalone issue
        providers
            .issues
            .insert("55".to_string(), make_issue("55", "Improve docs"));
        // remote-only branch
        providers.branches.insert(
            "experiment/alpha".to_string(),
            flotilla_protocol::delta::Branch {
                status: flotilla_protocol::BranchStatus::Remote,
            },
        );

        let (work_items, _) = correlate(&providers);

        // Expected: main checkout, feat checkout (with PR), solo session, issue, remote branch
        assert_eq!(work_items.len(), 5);

        let kinds: Vec<WorkItemKind> = work_items.iter().map(|wi| wi.kind()).collect();
        assert!(kinds.contains(&WorkItemKind::Checkout));
        assert!(kinds.contains(&WorkItemKind::Session));
        assert!(kinds.contains(&WorkItemKind::Issue));
        assert!(kinds.contains(&WorkItemKind::RemoteBranch));

        // The feat checkout should have the PR linked
        let feat = work_items
            .iter()
            .find(|wi| wi.branch() == Some("feat-login"))
            .expect("should have feat-login");
        assert_eq!(feat.change_request_key(), Some("10"));
        assert!(!feat.is_main_checkout());

        // main checkout should be flagged as main
        let main_item = work_items
            .iter()
            .find(|wi| wi.branch() == Some("main"))
            .expect("should have main");
        assert!(main_item.is_main_checkout());

        // Now group them
        let labels = default_labels();
        let proto_items: Vec<_> = work_items.iter().map(to_proto).collect();
        let grouped = group_work_items(&proto_items, &providers, &labels);

        // Should have sections for checkouts, sessions, remote, issues
        let header_count = grouped
            .table_entries
            .iter()
            .filter(|e| matches!(e, GroupEntry::Header(_)))
            .count();
        assert_eq!(header_count, 4, "should have exactly 4 section headers");

        // All items should be selectable
        assert_eq!(grouped.selectable_indices.len(), 5);
    }
}
