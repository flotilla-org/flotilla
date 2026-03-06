# WorkItem Enum Refactor Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the flat `WorkItem` struct with a `Correlated`/`Standalone` enum to encode valid states in the type system.

**Architecture:** The new `WorkItem` enum has two variants: `Correlated(CorrelatedWorkItem)` for items produced by the correlation engine (checkouts, PRs, sessions), and `Standalone(StandaloneWorkItem)` for post-correlation items (issues, remote branches). Helper methods on `WorkItem` preserve the existing field-access API so consuming code changes are mechanical.

**Tech Stack:** Rust, ratatui (UI unchanged)

**Design doc:** `docs/plans/2026-03-06-workitem-enum-design.md`

---

### Task 1: Define New Types and Helper Methods

**Files:**
- Modify: `src/data.rs:57-82` (replace WorkItem struct + impl)

**Step 1: Replace the WorkItem struct and impl with new types**

Replace lines 57-82 of `src/data.rs` with:

```rust
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

    pub fn identity(&self) -> Option<WorkItemIdentity> {
        match self {
            WorkItem::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => Some(WorkItemIdentity::Checkout(co.key.clone())),
                CorrelatedAnchor::Pr(key) => Some(WorkItemIdentity::ChangeRequest(key.clone())),
                CorrelatedAnchor::Session(key) => Some(WorkItemIdentity::Session(key.clone())),
            },
            WorkItem::Standalone(s) => match s {
                StandaloneWorkItem::Issue { key, .. } => Some(WorkItemIdentity::Issue(key.clone())),
                StandaloneWorkItem::RemoteBranch { branch } => Some(WorkItemIdentity::RemoteBranch(branch.clone())),
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
```

**Step 2: Run `cargo check` — expect errors only in construction sites and consumers**

Run: `cargo check 2>&1 | head -60`
Expected: Errors in `group_to_work_item`, `correlate`, `build_table_view`, `intent.rs`, `executor.rs`, `ui.rs`, `main.rs`, and tests. No errors in the new type definitions themselves.

---

### Task 2: Update Construction in `correlate()` and `group_to_work_item()`

**Files:**
- Modify: `src/data.rs:120-191` (`group_to_work_item`)
- Modify: `src/data.rs:243-331` (post-correlation linking, standalone construction in `correlate`)

**Step 1: Rewrite `group_to_work_item`**

Replace `src/data.rs:120-191` with:

```rust
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
```

**Step 2: Update post-correlation issue linking in `correlate()`**

In the `correlate` function, the post-correlation issue linking (currently lines ~252-280) uses direct field access on WorkItem. Update to use helper methods for reads and `as_correlated_mut()` for writes:

```rust
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
```

**Step 3: Update standalone issue construction**

Replace the standalone issue block (currently ~lines 286-301):

```rust
    for (id, issue) in &providers.issues {
        if !linked_issue_keys.contains(id.as_str()) {
            work_items.push(WorkItem::Standalone(StandaloneWorkItem::Issue {
                key: id.clone(),
                description: issue.title.clone(),
            }));
        }
    }
```

**Step 4: Update remote branch construction**

Replace the remote branch block (currently ~lines 303-328):

```rust
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
```

**Step 5: Run `cargo check 2>&1 | head -60`**

Expected: Errors only in `build_table_view`, `ui.rs`, `intent.rs`, `executor.rs`, `main.rs`, tests. No errors in `data.rs` construction code.

---

### Task 3: Update `build_table_view`

**Files:**
- Modify: `src/data.rs:334-420` (`build_table_view`)

**Step 1: Update field access to use helper methods**

The only direct field accesses in `build_table_view` are:
- `item.kind` → `item.kind()`
- `a.branch.cmp(&b.branch)` → `a.branch().cmp(&b.branch())`
- `a.session_key.as_ref().and_then(...)` → `a.session_key().and_then(...)`
- `a.pr_key.as_ref().and_then(...)` → `a.pr_key().and_then(...)`
- `a.issue_keys.first().and_then(...)` → `a.issue_keys().first().and_then(...)`

Apply these changes throughout `build_table_view`. Key lines:

```rust
    for item in work_items {
        match item.kind() {
            // ...
        }
    }

    checkout_items.sort_by(|a, b| a.branch().cmp(&b.branch()));

    session_items.sort_by(|a, b| {
        let a_time = a.session_key().and_then(|k| providers.sessions.get(k)).and_then(|s| s.updated_at.as_deref());
        let b_time = b.session_key().and_then(|k| providers.sessions.get(k)).and_then(|s| s.updated_at.as_deref());
        b_time.cmp(&a_time)
    });

    pr_items.sort_by(|a, b| {
        let a_num = a.pr_key().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.pr_key().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });

    remote_items.sort_by(|a, b| a.branch().cmp(&b.branch()));

    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys().first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys().first().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });
```

**Step 2: Run `cargo check 2>&1 | head -60`**

Expected: Errors only in consumers (`ui.rs`, `intent.rs`, `executor.rs`, `main.rs`, tests).

---

### Task 4: Update `intent.rs`

**Files:**
- Modify: `src/app/intent.rs:35-138`

**Step 1: Update `is_available` — field access to helper methods**

```rust
    pub fn is_available(&self, item: &WorkItem) -> bool {
        match self {
            Intent::SwitchToWorkspace => !item.workspace_refs().is_empty(),
            Intent::CreateWorkspace => item.checkout_key().is_some() && item.workspace_refs().is_empty(),
            Intent::RemoveWorktree => item.checkout_key().is_some() && !item.is_main_worktree(),
            Intent::CreateWorktreeAndWorkspace => item.checkout_key().is_none() && item.branch().is_some(),
            Intent::GenerateBranchName => item.branch().is_none() && !item.issue_keys().is_empty(),
            Intent::OpenPr => item.pr_key().is_some(),
            Intent::OpenIssue => !item.issue_keys().is_empty(),
            Intent::LinkIssuesToPr => {
                item.pr_key().is_some() && item.checkout_key().is_some() && !item.issue_keys().is_empty()
            }
            Intent::TeleportSession => item.session_key().is_some(),
            Intent::ArchiveSession => item.session_key().is_some(),
        }
    }
```

**Step 2: Update `resolve` — field access to helper methods**

Key changes in `resolve`:

```rust
    pub fn resolve(&self, item: &WorkItem, app: &App) -> Option<Command> {
        match self {
            Intent::SwitchToWorkspace => {
                item.workspace_refs().first().map(|ws_ref| Command::SelectWorkspace(ws_ref.clone()))
            }
            Intent::CreateWorkspace => {
                item.checkout_key().map(|p| Command::SwitchWorktree(p.to_path_buf()))
            }
            Intent::RemoveWorktree => {
                if item.kind() != WorkItemKind::Checkout || item.is_main_worktree() {
                    return None;
                }
                app.active_ui().selected_selectable_idx.map(Command::FetchDeleteInfo)
            }
            Intent::CreateWorktreeAndWorkspace => {
                item.branch().map(|branch| Command::CreateWorktree {
                    branch: branch.to_string(),
                    create_branch: item.kind() != WorkItemKind::RemoteBranch && item.kind() != WorkItemKind::Pr,
                    issue_ids: Vec::new(),
                })
            }
            Intent::GenerateBranchName => {
                if !item.issue_keys().is_empty() {
                    Some(Command::GenerateBranchName(item.issue_keys().to_vec()))
                } else {
                    None
                }
            }
            Intent::OpenPr => {
                item.pr_key().map(|k| Command::OpenPr(k.to_string()))
            }
            Intent::OpenIssue => {
                item.issue_keys().first().map(|k| Command::OpenIssueBrowser(k.clone()))
            }
            Intent::LinkIssuesToPr => {
                let pr_key = item.pr_key()?;
                let co_key = item.checkout_key()?;
                let data = &app.model.active().data.providers;
                let cr = data.change_requests.get(pr_key)?;
                let co = data.checkouts.get(co_key)?;

                let pr_issue_ids: std::collections::HashSet<&str> = cr.association_keys.iter()
                    .map(|k| {
                        let crate::providers::types::AssociationKey::IssueRef(_, id) = k;
                        id.as_str()
                    })
                    .collect();
                let missing: Vec<String> = co.association_keys.iter()
                    .filter_map(|k| {
                        let crate::providers::types::AssociationKey::IssueRef(_, id) = k;
                        if !pr_issue_ids.contains(id.as_str()) {
                            Some(id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                if missing.is_empty() {
                    return None;
                }
                Some(Command::LinkIssuesToPr { pr_id: cr.id.clone(), issue_ids: missing })
            }
            Intent::TeleportSession => {
                item.session_key().map(|k| {
                    Command::TeleportSession {
                        session_id: k.to_string(),
                        branch: item.branch().map(|b| b.to_string()),
                        checkout_key: item.checkout_key().map(|p| p.to_path_buf()),
                    }
                })
            }
            Intent::ArchiveSession => {
                item.session_key().map(|k| Command::ArchiveSession(k.to_string()))
            }
        }
    }
```

**Step 3: Run `cargo check 2>&1 | head -40`**

Expected: Errors only in `ui.rs`, `executor.rs`, `main.rs`, tests.

---

### Task 5: Update `executor.rs`

**Files:**
- Modify: `src/app/executor.rs:37-56` (`FetchDeleteInfo` branch)

**Step 1: Update field access in `FetchDeleteInfo`**

Lines 41-43 change from direct field access to helper methods:

```rust
        Command::FetchDeleteInfo(si) => {
            let table_idx = app.active_ui().table_view.selectable_indices.get(si).copied();
            if let Some(table_idx) = table_idx {
                if let Some(data::TableEntry::Item(item)) = app.active_ui().table_view.table_entries.get(table_idx).cloned() {
                    let branch = item.branch().unwrap_or_default().to_string();
                    let wt_path = item.checkout_key().map(|p| p.to_path_buf());
                    let pr_id = item.pr_key().map(|s| s.to_string());
                    // ... rest unchanged
                }
            }
        }
```

**Step 2: Run `cargo check 2>&1 | head -40`**

Expected: Errors only in `ui.rs`, `main.rs`, tests.

---

### Task 6: Update `ui.rs`

**Files:**
- Modify: `src/ui.rs:358-569` (`build_item_row`, `render_preview_content`, `render_debug_panel`)

**Step 1: Update `build_item_row` (lines 358-488)**

All direct field access becomes helper method calls:

- `item.kind` → `item.kind()`
- `item.workspace_refs` → `item.workspace_refs()`
- `item.session_key.as_ref()` → `item.session_key()`
- `item.description` → `item.description().to_string()` (or use as `&str`)
- `item.is_main_worktree` → `item.is_main_worktree()`
- `item.checkout_key` → `item.checkout_key()`
- `item.branch.as_deref()` → `item.branch()`
- `item.pr_key` → `item.pr_key()`
- `item.issue_keys` → `item.issue_keys()`

Key snippets:

```rust
fn build_item_row<'a>(item: &WorkItem, data: &crate::data::DataStore, col_widths: &[u16]) -> Row<'a> {
    let (icon, icon_color) = match item.kind() {
        WorkItemKind::Checkout => {
            if !item.workspace_refs().is_empty() {
                ("●", Color::Green)
            } else {
                ("○", Color::Green)
            }
        }
        WorkItemKind::Session => {
            let session = item.session_key().and_then(|k| data.providers.sessions.get(k));
            // ...
        }
        // ...
    };

    let description = truncate(item.description(), desc_width);

    let wt_indicator = if item.is_main_worktree() {
        "◆"
    } else if item.checkout_key().is_some() {
        "✓"
    } else {
        ""
    };

    let ws_indicator = match item.workspace_refs().len() { /* ... */ };

    let branch = item.branch().unwrap_or("—");

    let pr_display = if let Some(pr_key) = item.pr_key() {
        if let Some(cr) = data.providers.change_requests.get(pr_key) { /* ... */ }
        // ...
    };

    let session_display = if let Some(ses_key) = item.session_key() {
        if let Some(ses) = data.providers.sessions.get(ses_key) { /* ... */ }
        // ...
    };

    let issues_display = item.issue_keys().iter()
        .filter_map(|k| data.providers.issues.get(k.as_str()))
        // ...

    let git_display = if let Some(wt_key) = item.checkout_key() {
        if let Some(co) = data.providers.checkouts.get(wt_key) { /* ... */ }
        // ...
    };
    // ...
}
```

**Step 2: Update `render_preview_content` (lines 503-574)**

Same pattern — all `item.field` becomes `item.field()`:

```rust
    let text = if let Some(item) = selected_work_item(model, ui) {
        let mut lines = Vec::new();

        lines.push(format!("Description: {}", item.description()));

        if let Some(branch) = item.branch() {
            lines.push(format!("Branch: {}", branch));
        }

        if let Some(wt_key) = item.checkout_key() {
            if let Some(co) = model.active().data.providers.checkouts.get(wt_key) {
                // ... same body
            }
        }

        if let Some(pr_key) = item.pr_key() {
            if let Some(cr) = model.active().data.providers.change_requests.get(pr_key) {
                // ... same body
            }
        }

        if let Some(ses_key) = item.session_key() {
            if let Some(ses) = model.active().data.providers.sessions.get(ses_key) {
                // ... same body
            }
        }

        for ws_ref in item.workspace_refs() {
            // ... same body
        }

        for issue_key in item.issue_keys() {
            // ... same body
        }

        lines.join("\n")
    };
```

**Step 3: Update `render_debug_panel` (line 594)**

```rust
    if let Some(group_idx) = item.correlation_group_idx() {
```

**Step 4: Run `cargo check 2>&1 | head -40`**

Expected: Errors only in `main.rs` and tests.

---

### Task 7: Update `main.rs` drain_snapshots

**Files:**
- Modify: `src/main.rs:270-313`

**Step 1: No changes needed for selection identity**

Lines 273-278 use `item.identity()` which is already a method — no change needed.

**Step 2: No changes needed for multi-select cleanup**

Lines 307-313 also use `item.identity()` — no change needed.

**Step 3: Run `cargo check 2>&1 | head -40`**

Expected: Errors only in tests.

---

### Task 8: Update Tests

**Files:**
- Modify: `src/data.rs:446-555` (tests module)

**Step 1: Replace `default_work_item` helper and identity tests**

The test helper and identity tests need to construct the new types:

```rust
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
        assert_eq!(wi.identity(), Some(WorkItemIdentity::Checkout(PathBuf::from("/tmp/foo"))));
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
        assert_eq!(wi.identity(), Some(WorkItemIdentity::ChangeRequest("42".to_string())));
    }

    #[test]
    fn identity_issue() {
        let wi = WorkItem::Standalone(StandaloneWorkItem::Issue {
            key: "7".to_string(),
            description: String::new(),
        });
        assert_eq!(wi.identity(), Some(WorkItemIdentity::Issue("7".to_string())));
    }

    #[test]
    fn identity_remote_branch() {
        let wi = WorkItem::Standalone(StandaloneWorkItem::RemoteBranch {
            branch: "feature/x".to_string(),
        });
        assert_eq!(wi.identity(), Some(WorkItemIdentity::RemoteBranch("feature/x".to_string())));
    }

    #[test]
    fn checkout_association_keys_link_issues() {
        use crate::provider_data::ProviderData;
        use crate::providers::types::*;

        let mut providers = ProviderData::default();

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

        providers.issues.insert("42".to_string(), Issue {
            id: "42".to_string(),
            title: "Fix the thing".to_string(),
            labels: vec![],
            association_keys: vec![],
        });

        let (work_items, _groups) = correlate(&providers);

        let checkout_wi = work_items.iter()
            .find(|wi| wi.kind() == WorkItemKind::Checkout)
            .expect("should have a checkout work item");
        assert!(checkout_wi.issue_keys().contains(&"42".to_string()),
            "checkout should link issue 42 via association key, got: {:?}", checkout_wi.issue_keys());

        let standalone_issues: Vec<_> = work_items.iter()
            .filter(|wi| wi.kind() == WorkItemKind::Issue)
            .collect();
        assert!(standalone_issues.is_empty(),
            "issue 42 should be linked, not standalone");
    }
}
```

**Step 2: Run `cargo check`**

Expected: Clean compilation.

**Step 3: Run `cargo test`**

Run: `cargo test`
Expected: All tests pass.

**Step 4: Run `cargo clippy`**

Run: `cargo clippy`
Expected: No new warnings.

**Step 5: Commit**

```bash
git add -A
git commit -m "refactor: replace flat WorkItem struct with Correlated/Standalone enum

Encode valid work item states in the type system. Correlated items
(checkouts, PRs, sessions) carry an anchor and optional cross-refs.
Standalone items (issues, remote branches) carry only their data.
Helper methods preserve the field-access API for consumers."
```

---

### Task 9: Verify and Clean Up

**Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass.

**Step 2: Run the app**

Run: `cargo run`
Expected: App renders identically to before the refactor.

**Step 3: Check for any remaining direct field access patterns**

Search for patterns that shouldn't exist anymore:
- `item.kind` (without `()`) in non-data.rs files
- `item.branch` (without `()`) in non-data.rs files
- `item.checkout_key` (without `()`) in non-data.rs files

Run: `rg 'item\.(kind|branch|checkout_key|pr_key|session_key|issue_keys|workspace_refs|is_main_worktree|description|correlation_group_idx)[^(_]' --type rust src/ -g '!src/data.rs'`
Expected: No matches (all accesses use helper methods).
