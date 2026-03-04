use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::providers::correlation::{self, CorrelatedItem, CorrelatedGroup, ItemKind as CorItemKind};
use crate::providers::types::{
    ChangeRequest, Checkout, CloudAgentSession, CorrelationKey, Issue, Workspace,
};
use crate::providers::registry::ProviderRegistry;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkItemKind {
    Checkout,
    Session,
    Pr,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SectionHeader {
    Checkouts,
    Sessions,
    PullRequests,
    RemoteBranches,
    Issues,
}

impl fmt::Display for SectionHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SectionHeader::Checkouts => write!(f, "Worktrees"),
            SectionHeader::Sessions => write!(f, "Sessions"),
            SectionHeader::PullRequests => write!(f, "Pull Requests"),
            SectionHeader::RemoteBranches => write!(f, "Remote Branches"),
            SectionHeader::Issues => write!(f, "Issues"),
        }
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
    pub worktree_idx: Option<usize>,
    pub is_main_worktree: bool,
    pub pr_idx: Option<usize>,
    pub session_idx: Option<usize>,
    pub issue_idxs: Vec<usize>,
    pub workspace_refs: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub checkouts: Vec<Checkout>,
    pub change_requests: Vec<ChangeRequest>,
    pub issues: Vec<Issue>,
    pub workspaces: Vec<Workspace>,
    pub sessions: Vec<CloudAgentSession>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
    pub loading: bool,
}

impl DataStore {
    pub async fn refresh(&mut self, repo_root: &Path, registry: &ProviderRegistry) -> Vec<String> {
        self.loading = true;
        let mut errors = Vec::new();

        // Checkouts through registry
        let checkouts_fut = async {
            if let Some(cm) = registry.checkout_managers.values().next() {
                cm.list_checkouts(repo_root).await
            } else {
                Ok(vec![])
            }
        };

        // Change requests through registry
        let cr_fut = async {
            if let Some(cr) = registry.code_review.values().next() {
                cr.list_change_requests(repo_root, 20).await
            } else {
                Ok(vec![])
            }
        };

        // Issues through registry
        let issues_fut = async {
            if let Some(it) = registry.issue_trackers.values().next() {
                it.list_issues(repo_root, 20).await
            } else {
                Ok(vec![])
            }
        };

        // Sessions through registry
        let sessions_fut = async {
            if let Some(ca) = registry.coding_agents.values().next() {
                ca.list_sessions().await
            } else {
                Ok(vec![])
            }
        };

        // Remote branches through registry
        let branches_fut = async {
            if let Some(vcs) = registry.vcs.values().next() {
                vcs.list_remote_branches(repo_root).await
            } else {
                Ok(vec![])
            }
        };

        // Merged branches through registry
        let merged_fut = async {
            if let Some(cr) = registry.code_review.values().next() {
                cr.list_merged_branch_names(repo_root, 50).await
            } else {
                Ok(vec![])
            }
        };

        // Workspaces through registry
        let ws_fut = async {
            if let Some((_, ws_mgr)) = &registry.workspace_manager {
                ws_mgr.list_workspaces().await
            } else {
                Ok(vec![])
            }
        };

        let (checkouts, crs, issues, sessions, branches, merged, workspaces) = tokio::join!(
            checkouts_fut, cr_fut, issues_fut, sessions_fut, branches_fut, merged_fut, ws_fut
        );

        self.checkouts = checkouts.unwrap_or_else(|e| { errors.push(format!("checkouts: {e}")); Vec::new() });
        self.change_requests = crs.unwrap_or_else(|e| { errors.push(format!("PRs: {e}")); Vec::new() });
        self.issues = issues.unwrap_or_else(|e| { errors.push(format!("issues: {e}")); Vec::new() });
        self.workspaces = workspaces.unwrap_or_else(|e| { errors.push(format!("workspaces: {e}")); Vec::new() });
        self.sessions = sessions.unwrap_or_else(|e| { errors.push(format!("sessions: {e}")); Vec::new() });
        self.remote_branches = branches.unwrap_or_else(|e| { errors.push(format!("branches: {e}")); Vec::new() });
        self.merged_branches = merged.unwrap_or_else(|e| { errors.push(format!("merged: {e}")); Vec::new() });
        self.correlate();
        self.loading = false;
        errors
    }

    /// Convert a correlation group into a WorkItem.
    /// Issues are NOT in groups — they are linked post-correlation via IssueRef.
    fn group_to_work_item(&self, group: &CorrelatedGroup) -> WorkItem {
        let mut worktree_idx: Option<usize> = None;
        let mut pr_idx: Option<usize> = None;
        let mut session_idx: Option<usize> = None;
        let mut workspace_refs: Vec<String> = Vec::new();
        let mut is_main_worktree = false;

        for item in &group.items {
            match item.kind {
                CorItemKind::Checkout => {
                    worktree_idx = Some(item.source_index);
                    if let Some(co) = self.checkouts.get(item.source_index) {
                        is_main_worktree = co.is_trunk;
                    }
                }
                CorItemKind::ChangeRequest => {
                    pr_idx = Some(item.source_index);
                }
                CorItemKind::CloudSession => {
                    if session_idx.is_none() {
                        session_idx = Some(item.source_index);
                    }
                }
                CorItemKind::Workspace => {
                    if let Some(ws) = self.workspaces.get(item.source_index) {
                        workspace_refs.push(ws.ws_ref.clone());
                    }
                }
                _ => {}
            }
        }

        // Determine kind by priority: Checkout > Pr > Session
        let kind = if worktree_idx.is_some() {
            WorkItemKind::Checkout
        } else if pr_idx.is_some() {
            WorkItemKind::Pr
        } else {
            WorkItemKind::Session
        };

        let branch = group.branch().map(|s| s.to_string());

        let description = match kind {
            WorkItemKind::Checkout => branch.clone().unwrap_or_default(),
            WorkItemKind::Pr => pr_idx
                .and_then(|i| self.change_requests.get(i))
                .map(|cr| cr.title.clone())
                .unwrap_or_default(),
            WorkItemKind::Session => session_idx
                .and_then(|i| self.sessions.get(i))
                .map(|s| s.title.clone())
                .unwrap_or_default(),
            _ => branch.clone().unwrap_or_default(),
        };

        WorkItem {
            kind,
            branch,
            description,
            worktree_idx,
            is_main_worktree,
            pr_idx,
            session_idx,
            issue_idxs: Vec::new(), // populated post-correlation
            workspace_refs,
        }
    }

    fn correlate(&mut self) {
        // Phase 1: Build CorrelatedItems from identity-keyed sources.
        // IssueRef keys are excluded — they are association keys, not identity
        // keys. Two PRs referencing the same issue are separate work units.
        // Issues are linked post-correlation via IssueRef on change requests.
        let mut items: Vec<CorrelatedItem> = Vec::new();

        let strip_issue_refs = |keys: &[CorrelationKey]| -> Vec<CorrelationKey> {
            keys.iter()
                .filter(|k| !matches!(k, CorrelationKey::IssueRef(_, _)))
                .cloned()
                .collect()
        };

        for (i, co) in self.checkouts.iter().enumerate() {
            items.push(CorrelatedItem {
                provider_name: "checkout".to_string(),
                kind: CorItemKind::Checkout,
                title: co.branch.clone(),
                correlation_keys: strip_issue_refs(&co.correlation_keys),
                source_index: i,
            });
        }

        for (i, cr) in self.change_requests.iter().enumerate() {
            items.push(CorrelatedItem {
                provider_name: "change_request".to_string(),
                kind: CorItemKind::ChangeRequest,
                title: cr.title.clone(),
                correlation_keys: strip_issue_refs(&cr.correlation_keys),
                source_index: i,
            });
        }

        // Issues are NOT submitted to the correlation engine — they link
        // only via IssueRef, which is an association key handled below.

        for (i, session) in self.sessions.iter().enumerate() {
            items.push(CorrelatedItem {
                provider_name: "session".to_string(),
                kind: CorItemKind::CloudSession,
                title: session.title.clone(),
                correlation_keys: strip_issue_refs(&session.correlation_keys),
                source_index: i,
            });
        }

        for (i, ws) in self.workspaces.iter().enumerate() {
            items.push(CorrelatedItem {
                provider_name: "workspace".to_string(),
                kind: CorItemKind::Workspace,
                title: ws.name.clone(),
                correlation_keys: strip_issue_refs(&ws.correlation_keys),
                source_index: i,
            });
        }

        // Phase 2: Run correlation engine (identity keys only)
        let groups = correlation::correlate(items);

        // Phase 3: Convert groups to WorkItems and categorize
        let mut checkout_items: Vec<WorkItem> = Vec::new();
        let mut session_items: Vec<WorkItem> = Vec::new();
        let mut pr_items: Vec<WorkItem> = Vec::new();
        let mut linked_issue_indices: HashSet<usize> = HashSet::new();

        for group in &groups {
            let mut work_item = self.group_to_work_item(group);

            // Post-correlation: link issues via IssueRef keys on change requests
            if let Some(pr_i) = work_item.pr_idx {
                if let Some(cr) = self.change_requests.get(pr_i) {
                    for key in &cr.correlation_keys {
                        if let CorrelationKey::IssueRef(_, issue_id) = key {
                            if let Some(issue_idx) = self.issues.iter().position(|i| &i.id == issue_id) {
                                if !work_item.issue_idxs.contains(&issue_idx) {
                                    work_item.issue_idxs.push(issue_idx);
                                    linked_issue_indices.insert(issue_idx);
                                }
                            }
                        }
                    }
                }
            }

            match work_item.kind {
                WorkItemKind::Checkout => checkout_items.push(work_item),
                WorkItemKind::Session => session_items.push(work_item),
                WorkItemKind::Pr => pr_items.push(work_item),
                WorkItemKind::Issue => {} // not possible — issues not in engine
                WorkItemKind::RemoteBranch => {} // handled separately
            }
        }

        // Phase 4: Build table entries in section order
        let mut entries: Vec<TableEntry> = Vec::new();
        let mut selectable: Vec<usize> = Vec::new();

        // Checkouts section -- sorted by branch name ascending
        checkout_items.sort_by(|a, b| a.branch.cmp(&b.branch));
        if !checkout_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Checkouts));
            for item in checkout_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Sessions section -- sorted by updated_at descending (most recent first)
        session_items.sort_by(|a, b| {
            let a_time = a.session_idx.and_then(|i| self.sessions.get(i)).and_then(|s| s.updated_at.as_deref());
            let b_time = b.session_idx.and_then(|i| self.sessions.get(i)).and_then(|s| s.updated_at.as_deref());
            b_time.cmp(&a_time) // descending
        });
        if !session_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Sessions));
            for item in session_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // PRs section -- sorted by id descending (most recent first)
        pr_items.sort_by(|a, b| {
            let a_num = a.pr_idx.and_then(|i| self.change_requests.get(i)).and_then(|cr| cr.id.parse::<i64>().ok());
            let b_num = b.pr_idx.and_then(|i| self.change_requests.get(i)).and_then(|cr| cr.id.parse::<i64>().ok());
            b_num.cmp(&a_num) // descending
        });
        if !pr_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::PullRequests));
            for item in pr_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Remote branches section -- sorted by branch name
        // Collect known branches from all correlated groups
        let mut known_branches: HashSet<String> = HashSet::new();
        for entry in &entries {
            if let TableEntry::Item(item) = entry {
                if let Some(ref b) = item.branch {
                    known_branches.insert(b.clone());
                }
            }
        }
        let merged_set: HashSet<&str> = self.merged_branches.iter()
            .map(|s| s.as_str())
            .collect();
        let mut remote_items: Vec<WorkItem> = self.remote_branches.iter()
            .filter(|b| {
                b.as_str() != "HEAD" && b.as_str() != "main" && b.as_str() != "master"
                    && !known_branches.contains(b.as_str())
                    && !merged_set.contains(b.as_str())
            })
            .map(|b| {
                WorkItem {
                    kind: WorkItemKind::RemoteBranch,
                    branch: Some(b.clone()),
                    description: b.clone(),
                    worktree_idx: None,
                    is_main_worktree: false,
                    pr_idx: None,
                    session_idx: None,
                    issue_idxs: Vec::new(),
                    workspace_refs: Vec::new(),
                }
            })
            .collect();
        remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));
        if !remote_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::RemoteBranches));
            for item in remote_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Issues section -- standalone only (not in linked_issue_indices)
        let mut issue_items: Vec<WorkItem> = self.issues.iter()
            .enumerate()
            .filter(|(i, _)| !linked_issue_indices.contains(i))
            .map(|(i, issue)| WorkItem {
                kind: WorkItemKind::Issue,
                branch: None,
                description: issue.title.clone(),
                worktree_idx: None,
                is_main_worktree: false,
                pr_idx: None,
                session_idx: None,
                issue_idxs: vec![i],
                workspace_refs: Vec::new(),
            })
            .collect();
        issue_items.sort_by(|a, b| {
            let a_num = a.issue_idxs.first().and_then(|&i| self.issues.get(i)).and_then(|iss| iss.id.parse::<i64>().ok());
            let b_num = b.issue_idxs.first().and_then(|&i| self.issues.get(i)).and_then(|iss| iss.id.parse::<i64>().ok());
            b_num.cmp(&a_num) // descending
        });
        if !issue_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Issues));
            for item in issue_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        self.table_entries = entries;
        self.selectable_indices = selectable;
    }
}

async fn run_command(cmd: &str, args: &[&str], cwd: Option<&PathBuf>) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
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

    let (unpushed, uncommitted, pr_info) = tokio::join!(
        async {
            run_command(
                "git",
                &["log", &format!("origin/main..{}", branch_owned), "--oneline"],
                Some(&repo),
            ).await.unwrap_or_default()
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
