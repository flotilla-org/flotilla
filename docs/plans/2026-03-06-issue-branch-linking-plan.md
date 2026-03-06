# Issue-Branch Linking Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Link branches to issues via git config so checkout work items display their linked issues before a PR exists, and offer to repair PRs that are missing issue links.

**Architecture:** Store `branch.<name>.flotilla.issues.<provider>` in git config. A shared helper reads this during checkout enrichment. The existing post-correlation issue-linking in `data.rs` is extended to also check checkout association keys. A new `LinkIssuesToPr` intent edits PR bodies when links are missing.

**Tech Stack:** Rust, git config, `gh` CLI for PR editing, existing association key infrastructure.

---

### Task 1: Add `association_keys` to `Checkout` struct

**Files:**
- Modify: `src/providers/types.rs:36-45`
- Modify: `src/providers/vcs/wt.rs:60-83` (WtWorktree::into_checkout)
- Modify: `src/providers/vcs/git_worktree.rs:185-197` (enrich_checkout return)

**Step 1: Add the field to Checkout**

In `src/providers/types.rs`, add `association_keys` after `correlation_keys`:

```rust
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
```

**Step 2: Fix compilation — add empty `association_keys` to all Checkout constructors**

In `src/providers/vcs/wt.rs` `into_checkout()` (~line 66-82), add:
```rust
            association_keys: Vec::new(),
```

In `src/providers/vcs/git_worktree.rs` `enrich_checkout()` (~line 185-197), add:
```rust
            association_keys: Vec::new(),
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles with no new errors.

**Step 4: Commit**

```bash
git add src/providers/types.rs src/providers/vcs/wt.rs src/providers/vcs/git_worktree.rs
git commit -m "feat: add association_keys field to Checkout struct"
```

---

### Task 2: Shared helper to read branch issue links from git config

**Files:**
- Modify: `src/providers/vcs/mod.rs` (add helper function)

**Step 1: Write a test**

Add to `src/providers/vcs/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::AssociationKey;

    #[test]
    fn parse_issue_links_single_provider() {
        let git_output = "branch.feat-x.flotilla.issues.github 123,456\n";
        let keys = parse_issue_config_output(git_output);
        assert_eq!(keys, vec![
            AssociationKey::IssueRef("github".into(), "123".into()),
            AssociationKey::IssueRef("github".into(), "456".into()),
        ]);
    }

    #[test]
    fn parse_issue_links_multiple_providers() {
        let git_output = "branch.feat-x.flotilla.issues.github 42\nbranch.feat-x.flotilla.issues.linear ABC-123\n";
        let keys = parse_issue_config_output(git_output);
        assert_eq!(keys, vec![
            AssociationKey::IssueRef("github".into(), "42".into()),
            AssociationKey::IssueRef("linear".into(), "ABC-123".into()),
        ]);
    }

    #[test]
    fn parse_issue_links_empty() {
        let keys = parse_issue_config_output("");
        assert!(keys.is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla vcs::tests`
Expected: FAIL — `parse_issue_config_output` not found.

**Step 3: Implement the parser and the async reader**

Add to `src/providers/vcs/mod.rs`:

```rust
use crate::providers::types::AssociationKey;

/// Parse the output of `git config --get-regexp 'branch\.<branch>\.flotilla\.issues\.'`
/// into association keys. Each line has the format:
/// `branch.<name>.flotilla.issues.<provider> id1,id2,...`
pub fn parse_issue_config_output(output: &str) -> Vec<AssociationKey> {
    let mut keys = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Split "branch.foo.flotilla.issues.github 123,456" into key and value
        let Some((config_key, value)) = line.split_once(' ') else {
            continue;
        };
        // Extract provider name from the key suffix after "flotilla.issues."
        let Some(provider) = config_key.rsplit_once(".issues.").map(|(_, p)| p) else {
            continue;
        };
        for id in value.split(',') {
            let id = id.trim();
            if !id.is_empty() {
                keys.push(AssociationKey::IssueRef(provider.to_string(), id.to_string()));
            }
        }
    }
    keys
}

/// Read issue links from git config for a specific branch.
/// Returns empty vec if no links or on error (non-fatal).
pub async fn read_branch_issue_links(repo_root: &Path, branch: &str) -> Vec<AssociationKey> {
    let pattern = format!("branch\\.{}\\.flotilla\\.issues\\.", regex_escape_branch(branch));
    let result = crate::providers::run_cmd(
        "git",
        &["config", "--get-regexp", &pattern],
        repo_root,
    )
    .await;
    match result {
        Ok(output) => parse_issue_config_output(&output),
        Err(_) => Vec::new(), // no config keys found, or git error — not fatal
    }
}

/// Escape special regex characters in branch names for git config --get-regexp.
fn regex_escape_branch(branch: &str) -> String {
    let mut escaped = String::with_capacity(branch.len());
    for c in branch.chars() {
        match c {
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla vcs::tests`
Expected: PASS — all 3 tests pass.

**Step 5: Commit**

```bash
git add src/providers/vcs/mod.rs
git commit -m "feat: helper to read branch issue links from git config"
```

---

### Task 3: Call the helper during checkout enrichment

**Files:**
- Modify: `src/providers/vcs/git_worktree.rs:124-197` (enrich_checkout)
- Modify: `src/providers/vcs/wt.rs:60-83` (into_checkout + list_checkouts)

**Step 1: GitCheckoutManager — read issue links in enrich_checkout**

In `src/providers/vcs/git_worktree.rs`, `enrich_checkout` currently takes `(path, branch, is_trunk, default_branch)`. It runs several git commands in `tokio::join!`. Add `read_branch_issue_links` to the join:

In the `tokio::join!` block (~line 139), add a fifth future:

```rust
async {
    super::read_branch_issue_links(path, branch).await
},
```

Destructure the result as `(trunk_ab, remote_ab, wt_status, commit, issue_links)`.

Then set `association_keys: issue_links` in the returned `Checkout`.

**Step 2: WtCheckoutManager — read issue links after parsing JSON**

In `src/providers/vcs/wt.rs`, the `into_checkout()` method on `WtWorktree` doesn't have async access. Instead, modify `list_checkouts` to read issue links after building checkouts.

In `list_checkouts` (~line 116-127), after building `checkouts` from the JSON, enrich each one:

```rust
async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<Checkout>, String> {
    // ... existing JSON parsing to get Vec<Checkout> ...
    let mut checkouts = /* existing code */;

    // Enrich with issue links from git config
    let futures: Vec<_> = checkouts.iter().map(|co| {
        super::read_branch_issue_links(repo_root, &co.branch)
    }).collect();
    let all_links = futures::future::join_all(futures).await;
    for (co, links) in checkouts.iter_mut().zip(all_links) {
        co.association_keys = links;
    }

    Ok(checkouts)
}
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles.

**Step 4: Manual test**

Set a test config value and run flotilla:
```bash
git config branch.main.flotilla.issues.github "1,2"
cargo run
# Verify main worktree work item shows linked issues (if issues #1, #2 exist)
git config --unset branch.main.flotilla.issues.github
```

**Step 5: Commit**

```bash
git add src/providers/vcs/git_worktree.rs src/providers/vcs/wt.rs
git commit -m "feat: read branch issue links during checkout enrichment"
```

---

### Task 4: Extend post-correlation issue linking to checkouts

**Files:**
- Modify: `src/data.rs:226-240`

**Step 1: Read the existing post-correlation code**

Currently at ~line 226-240 in `src/data.rs`:

```rust
// Post-correlation: link issues via association keys on change requests
if let Some(pr_i) = work_item.pr_idx {
    if let Some(cr) = providers.change_requests.get(pr_i) {
        for key in &cr.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            if let Some(issue_idx) = providers.issues.iter().position(|i| &i.id == issue_id) {
                if !work_item.issue_idxs.contains(&issue_idx) {
                    work_item.issue_idxs.push(issue_idx);
                    linked_issue_indices.insert(issue_idx);
                }
            }
        }
    }
}
```

**Step 2: Add checkout association key linking**

After the PR linking block, add a similar block for checkouts:

```rust
// Also link issues via association keys on checkouts (from git config)
if let Some(co_i) = work_item.worktree_idx {
    if let Some(co) = providers.checkouts.get(co_i) {
        for key in &co.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            if let Some(issue_idx) = providers.issues.iter().position(|i| &i.id == issue_id) {
                if !work_item.issue_idxs.contains(&issue_idx) {
                    work_item.issue_idxs.push(issue_idx);
                    linked_issue_indices.insert(issue_idx);
                }
            }
        }
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compiles.

**Step 4: Commit**

```bash
git add src/data.rs
git commit -m "feat: link issues to checkouts via association keys"
```

---

### Task 5: Write git config when creating a branch from issues

**Files:**
- Modify: `src/app/command.rs:7` (CreateWorktree variant)
- Modify: `src/app/intent.rs:71-74` (resolve CreateWorktreeAndWorkspace)
- Modify: `src/app/executor.rs:170-203` (GenerateBranchName)
- Modify: `src/app/executor.rs:98-123` (CreateWorktree execution)
- Modify: `src/app/mod.rs:515` (manual branch creation)

**Step 1: Add issue_ids to CreateWorktree command**

In `src/app/command.rs`, change:

```rust
CreateWorktree { branch: String, create_branch: bool },
```

to:

```rust
CreateWorktree { branch: String, create_branch: bool, issue_ids: Vec<(String, String)> },
```

Where `issue_ids` contains `(provider_name, issue_id)` pairs.

**Step 2: Update all CreateWorktree construction sites**

In `src/app/intent.rs` (~line 71-74), add `issue_ids: Vec::new()`:

```rust
Intent::CreateWorktreeAndWorkspace => {
    item.branch.as_ref().map(|branch| Command::CreateWorktree {
        branch: branch.clone(),
        create_branch: item.kind != WorkItemKind::RemoteBranch && item.kind != WorkItemKind::Pr,
        issue_ids: Vec::new(),
    })
}
```

In `src/app/mod.rs` (~line 515), the manual `n` key path:

```rust
self.commands.push(Command::CreateWorktree { branch, create_branch: true, issue_ids: Vec::new() });
```

**Step 3: Store generated issue IDs and pass them to CreateWorktree**

In `src/app/executor.rs`, the `GenerateBranchName` handler (~line 170-203) needs to store the issue IDs so they can be passed to `CreateWorktree`. Currently it calls `app.prefill_branch_input(&branch)`.

Add a field to track pending issue IDs. In `src/app/mod.rs`, add to `App`:

```rust
pub pending_issue_ids: Vec<(String, String)>,
```

In executor.rs `GenerateBranchName` handler, before calling `prefill_branch_input`, store the issue IDs:

```rust
// Collect (provider_name, issue_id) pairs for the created branch
let issue_id_pairs: Vec<(String, String)> = issues.iter()
    .map(|(id, _title)| {
        let provider = app.model.active().registry.issue_trackers
            .keys().next().cloned().unwrap_or_else(|| "github".to_string());
        (provider, id.clone())
    })
    .collect();
app.pending_issue_ids = issue_id_pairs;
```

Then in `src/app/mod.rs`, where the branch input Enter key creates `CreateWorktree` (~line 515), pass the pending IDs:

```rust
let issue_ids = std::mem::take(&mut self.pending_issue_ids);
self.commands.push(Command::CreateWorktree { branch, create_branch: true, issue_ids });
```

**Step 4: Write git config in CreateWorktree executor**

In `src/app/executor.rs`, the `CreateWorktree` handler (~line 98-123), after successful checkout creation, write the git config:

```rust
Command::CreateWorktree { branch, create_branch, issue_ids } => {
    // ... existing checkout creation code ...
    match checkout_result {
        Some(Ok(checkout)) => {
            // Write issue links to git config
            if !issue_ids.is_empty() {
                write_branch_issue_links(&repo, &branch, &issue_ids).await;
            }
            // ... existing workspace creation code ...
        }
        // ...
    }
}
```

Add a helper function in executor.rs:

```rust
async fn write_branch_issue_links(repo_root: &std::path::Path, branch: &str, issue_ids: &[(String, String)]) {
    use std::collections::HashMap;
    // Group by provider
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        let _ = crate::providers::run_cmd("git", &["config", &key, &value], repo_root).await;
    }
}
```

**Step 5: Verify it compiles**

Run: `cargo check`
Expected: Compiles.

**Step 6: Commit**

```bash
git add src/app/command.rs src/app/intent.rs src/app/executor.rs src/app/mod.rs
git commit -m "feat: write branch issue links to git config on worktree creation"
```

---

### Task 6: LinkIssuesToPr intent

**Files:**
- Modify: `src/app/intent.rs` (add variant + logic)
- Modify: `src/app/command.rs` (add command)
- Modify: `src/app/executor.rs` (execute the command)

**Step 1: Add the intent and command**

In `src/app/intent.rs`, add to the `Intent` enum:

```rust
LinkIssuesToPr,
```

Add the label:

```rust
Intent::LinkIssuesToPr => format!("Link issues to {}", labels.code_review.noun),
```

Add availability check — available when there's a PR and checkout with association keys that the PR is missing:

```rust
Intent::LinkIssuesToPr => {
    if item.pr_idx.is_none() || item.worktree_idx.is_none() {
        return false;
    }
    // Actual check for missing links happens at resolve time
    // since we need access to the data store
    true
}
```

Add to `all_in_menu_order()` (after `OpenIssue`).

In `src/app/command.rs`, add:

```rust
LinkIssuesToPr { pr_id: String, issue_ids: Vec<String> },
```

**Step 2: Implement resolve**

In `src/app/intent.rs`, add resolve logic:

```rust
Intent::LinkIssuesToPr => {
    let pr_idx = item.pr_idx?;
    let co_idx = item.worktree_idx?;
    let data = &app.model.active().data.providers;
    let cr = data.change_requests.get(pr_idx)?;
    let co = data.checkouts.get(co_idx)?;

    // Find issue IDs from checkout that aren't in the PR
    let pr_issue_ids: std::collections::HashSet<&str> = cr.association_keys.iter()
        .filter_map(|k| match k {
            AssociationKey::IssueRef(_, id) => Some(id.as_str()),
        })
        .collect();
    let missing: Vec<String> = co.association_keys.iter()
        .filter_map(|k| match k {
            AssociationKey::IssueRef(_, id) if !pr_issue_ids.contains(id.as_str()) => Some(id.clone()),
        })
        .collect();

    if missing.is_empty() {
        return None;
    }
    Some(Command::LinkIssuesToPr { pr_id: cr.id.clone(), issue_ids: missing })
}
```

Also update `is_available` to return false when there are no missing links (need app access — or keep it simple and let resolve return None).

**Step 3: Implement executor**

In `src/app/executor.rs`, add handler:

```rust
Command::LinkIssuesToPr { pr_id, issue_ids } => {
    info!("linking issues {:?} to PR #{pr_id}", issue_ids);
    let repo = app.model.active_repo_root().clone();
    // Get current PR body
    let body_result = crate::providers::run_cmd(
        "gh",
        &["pr", "view", &pr_id, "--json", "body", "--jq", ".body"],
        &repo,
    ).await;
    match body_result {
        Ok(current_body) => {
            let fixes_lines: Vec<String> = issue_ids.iter()
                .map(|id| format!("Fixes #{id}"))
                .collect();
            let new_body = if current_body.trim().is_empty() {
                fixes_lines.join("\n")
            } else {
                format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
            };
            let result = crate::providers::run_cmd(
                "gh",
                &["pr", "edit", &pr_id, "--body", &new_body],
                &repo,
            ).await;
            if let Err(e) = result {
                error!("failed to edit PR: {e}");
                app.model.status_message = Some(e);
            } else {
                info!("linked issues to PR #{pr_id}");
            }
        }
        Err(e) => {
            error!("failed to read PR body: {e}");
            app.model.status_message = Some(e);
        }
    }
    trigger_active_refresh(app);
}
```

**Step 4: Verify it compiles**

Run: `cargo check`
Expected: Compiles.

**Step 5: Commit**

```bash
git add src/app/intent.rs src/app/command.rs src/app/executor.rs
git commit -m "feat: LinkIssuesToPr intent to repair PR bodies with missing issue links"
```

---

### Task 7: Clean up and final test

**Step 1: Run all tests**

Run: `cargo test`
Expected: All tests pass.

**Step 2: Run clippy**

Run: `cargo clippy`
Expected: No new warnings.

**Step 3: Manual end-to-end test**

1. Start flotilla with the git checkout manager
2. Select an issue, press Enter to generate a branch name
3. Confirm the branch name — worktree is created
4. Verify the issue now appears on the checkout work item in the table
5. Verify `git config --get-regexp flotilla.issues` shows the stored link
6. Create a PR for the branch (without "Fixes #N" in body)
7. In flotilla, open the action menu on the work item
8. Verify "Link issues to PR" action appears
9. Select it — verify the PR body is updated

**Step 4: Commit any fixes**

```bash
git commit -m "chore: cleanup and fixes from end-to-end testing"
```
