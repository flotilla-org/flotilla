# Delta Snapshots Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Introduce event-sourced delta snapshots with per-provider delta streams, replacing full snapshot broadcasts.

**Architecture:** Deltas are primary; snapshots are materialized views. Per-provider `DeltaSource` traits produce collection-level enter/exit/update changes, merged into a bounded delta log. Keys move from value types into map/envelope positions (Kafka-style). Branches unified into a single keyed collection.

**Tech Stack:** Rust, serde, tokio, IndexMap

**Design doc:** `docs/plans/2026-03-07-delta-snapshots-design.md`

---

## PR 1: Structural Refactor (no behavior change)

Goal: extract keys from value types, unify branches, add delta protocol types. All tests pass with identical runtime behavior.

---

### Task 1: Add new protocol types

Add `EntryOp<T>`, `Change`, `Branch`, `BranchStatus` to flotilla-protocol. Purely additive — nothing uses them yet.

**Files:**
- Create: `crates/flotilla-protocol/src/delta.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`

**Step 1: Write tests for new types**

Add to `crates/flotilla-protocol/src/delta.rs`:

```rust
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ChangeRequest, Checkout, CloudAgentSession, Issue, ProviderError, Workspace, WorkItem,
    WorkItemIdentity,
};

/// Operation on a keyed collection entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", content = "value")]
pub enum EntryOp<T> {
    #[serde(rename = "added")]
    Added(T),
    #[serde(rename = "updated")]
    Updated(T),
    #[serde(rename = "removed")]
    Removed,
}

/// Status of a git branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchStatus {
    Remote,
    Merged,
}

/// A git branch with status metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub status: BranchStatus,
}

/// A single change within a delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    Checkout { key: PathBuf, op: EntryOp<Checkout> },
    ChangeRequest { key: String, op: EntryOp<ChangeRequest> },
    Issue { key: String, op: EntryOp<Issue> },
    Session { key: String, op: EntryOp<CloudAgentSession> },
    Workspace { key: String, op: EntryOp<Workspace> },
    Branch { key: String, op: EntryOp<Branch> },
    WorkItem { identity: WorkItemIdentity, op: EntryOp<WorkItem> },
    ProviderHealth { provider: String, op: EntryOp<bool> },
    /// Full replacement — errors lack stable identity, so keyed deltas don't apply.
    ErrorsChanged(Vec<ProviderError>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_op_added_roundtrip() {
        let op: EntryOp<bool> = EntryOp::Added(true);
        let json = serde_json::to_string(&op).unwrap();
        let decoded: EntryOp<bool> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn entry_op_removed_roundtrip() {
        let op: EntryOp<String> = EntryOp::Removed;
        let json = serde_json::to_string(&op).unwrap();
        let decoded: EntryOp<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn branch_status_roundtrip() {
        for status in [BranchStatus::Remote, BranchStatus::Merged] {
            let json = serde_json::to_string(&status).unwrap();
            let decoded: BranchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn change_checkout_roundtrip() {
        let change = Change::Checkout {
            key: PathBuf::from("/repos/wt-1"),
            op: EntryOp::Added(Checkout {
                branch: "feat-x".into(),
                is_trunk: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
            }),
        };
        let json = serde_json::to_string(&change).unwrap();
        let decoded: Change = serde_json::from_str(&json).unwrap();
        // Verify it round-trips (Change doesn't derive PartialEq, so check JSON)
        let json2 = serde_json::to_string(&decoded).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn change_branch_removed_roundtrip() {
        let change = Change::Branch {
            key: "feature/old".into(),
            op: EntryOp::Removed,
        };
        let json = serde_json::to_string(&change).unwrap();
        let decoded: Change = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&decoded).unwrap();
        assert_eq!(json, json2);
    }
}
```

**Step 2: Wire up module**

In `crates/flotilla-protocol/src/lib.rs`, add:
```rust
pub mod delta;
pub use delta::{Branch, BranchStatus, Change, EntryOp};
```

**Step 3: Run tests**

Run: `cargo test -p flotilla-protocol`
Expected: all pass

**Step 4: Commit**

```
feat: add delta protocol types (EntryOp, Change, Branch, BranchStatus)
```

---

### Task 2: Extract `path` from Checkout

Remove `path: PathBuf` from the `Checkout` struct. The path is already the key in `IndexMap<PathBuf, Checkout>`.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — remove `path` field
- Modify: `crates/flotilla-core/src/refresh.rs:205` — provider returns `(PathBuf, Checkout)` tuples; stop cloning `co.path`
- Modify: `crates/flotilla-core/src/executor.rs:33,73,76,293` — use map key instead of `co.path`
- Modify: `crates/flotilla-tui/src/ui.rs:504` — use map key instead of `co.path`
- Modify: tests in `provider_data.rs`, `convert.rs`, `snapshot.rs`, etc.

**Step 1: Update tests in `provider_data.rs`**

Remove `path` field from all `Checkout` constructions in tests. Tests should fail to compile.

**Step 2: Remove `path` from `Checkout` struct**

In `crates/flotilla-protocol/src/provider_data.rs`, change:
```rust
pub struct Checkout {
    pub branch: String,
    // REMOVE: pub path: PathBuf,
    pub is_trunk: bool,
    // ... rest unchanged
}
```

**Step 3: Fix all compilation errors**

The compiler will guide you. Key changes:

- `crates/flotilla-core/src/refresh.rs:205`: The provider trait `list_checkouts` returns `Vec<Checkout>`. Currently each `Checkout` has its `path` set by the provider. After removing the field, the provider must return `(PathBuf, Checkout)` tuples instead — OR the `Checkout` is constructed without `path` and the map key comes from elsewhere. Check the `CheckoutManager` trait and its implementations (`git.rs`, `wt.rs`) to see where `path` is set. The provider should return `Vec<(PathBuf, Checkout)>` or the refresh code should extract the path before inserting. Simplest: change `refresh.rs` to use `(co.path.clone(), co)` pattern — but `co.path` no longer exists. The fix depends on how providers construct checkouts.

- `crates/flotilla-core/src/executor.rs:33`: `workspace_config(repo_root, &co.branch, &co.path, "claude")` → use the map key: `workspace_config(repo_root, &co.branch, &checkout_path, "claude")` where `checkout_path` is the key used to look up `co`.

- `crates/flotilla-core/src/executor.rs:73,76`: After `create_checkout` returns a new `Checkout`, the path comes from the returned value. The `CheckoutManager::create_checkout` return type needs to change to `(PathBuf, Checkout)`.

- `crates/flotilla-core/src/executor.rs:293`: `co.path.clone()` → use the map key.

- `crates/flotilla-tui/src/ui.rs:504`: `co.path.display()` → use the map key (which is the `wt_key` already available in scope).

**Step 4: Fix provider traits and implementations**

The `CheckoutManager` trait in `crates/flotilla-core/src/providers/vcs/mod.rs` likely has:
```rust
async fn list_checkouts(&self, ...) -> Result<Vec<Checkout>, String>;
async fn create_checkout(&self, ...) -> Result<Checkout, String>;
```

These need to become:
```rust
async fn list_checkouts(&self, ...) -> Result<Vec<(PathBuf, Checkout)>, String>;
async fn create_checkout(&self, ...) -> Result<(PathBuf, Checkout), String>;
```

Update the `git.rs` and `wt.rs` implementations to return the path alongside the checkout. Find where `Checkout { path: ..., ... }` is constructed and extract the path into the tuple.

**Step 5: Run tests**

Run: `cargo test --workspace`
Expected: all pass

**Step 6: Commit**

```
refactor: extract path key from Checkout value type
```

---

### Task 3: Extract `id` from ChangeRequest

Same pattern as Task 2. Remove `id: String` from `ChangeRequest`.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — remove `id` field
- Modify: `crates/flotilla-core/src/refresh.rs:216` — adjust map construction
- Modify: `crates/flotilla-tui/src/ui.rs:393,531` — use map key
- Modify: `crates/flotilla-tui/src/app/intent.rs:163` — use map key
- Modify: provider trait `CodeReview::list_change_requests` and `github.rs` impl

**Step 1: Remove `id` from `ChangeRequest` struct**

**Step 2: Fix provider trait**

`CodeReview::list_change_requests` returns `Vec<ChangeRequest>` → `Vec<(String, ChangeRequest)>`.

**Step 3: Fix refresh.rs:216**

Change `.map(|cr| (cr.id.clone(), cr))` to use the tuple from the provider.

**Step 4: Fix ui.rs:393**

`format!("#{}{}", cr.id, state_icon)` → use the map key. The key is available as `pr_key` from the `WorkItem::change_request_key` field, which is already the CR id.

**Step 5: Fix ui.rs:531**

`cr.id` in the detail view → use `pr_key`.

**Step 6: Fix intent.rs:163**

`change_request_id: cr.id.clone()` → the CR is looked up by key, use that key.

**Step 7: Run tests, commit**

Run: `cargo test --workspace`

```
refactor: extract id key from ChangeRequest value type
```

---

### Task 4: Extract `id` from Issue

**Larger scope than Tasks 2-3** because PR #113 added `IssueCache`, `IssuePage`, and new `IssueTracker` methods that all use `issue.id`.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — remove `id` field from `Issue`
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — `IssuePage.issues` becomes `Vec<(String, Issue)>` or `IndexMap<String, Issue>`
- Modify: `crates/flotilla-core/src/refresh.rs:227` — adjust map construction
- Modify: `crates/flotilla-core/src/executor.rs:228` — use map key
- Modify: `crates/flotilla-tui/src/ui.rs:419,566` — use map key
- Modify: `crates/flotilla-core/src/issue_cache.rs` — `merge_page`, `add_pinned` need IDs from tuple/key not `issue.id`
- Modify: `crates/flotilla-core/src/in_process.rs` — `inject_issues` uses `i.id.clone()` to key the IndexMap; `collect_linked_issue_ids` unaffected (uses AssociationKey)
- Modify: provider trait `IssueTracker` — `list_issues`, `list_issues_page`, `fetch_issues_by_id`, `search_issues` return types
- Modify: `crates/flotilla-core/src/providers/issue_tracker/github.rs` — impl changes

**Key changes beyond the standard pattern:**

1. `IssueTracker::list_issues` → `Vec<(String, Issue)>`
2. `IssueTracker::list_issues_page` → `IssuePage` with `issues: Vec<(String, Issue)>` (or change `IssuePage.issues` to `IndexMap<String, Issue>`)
3. `IssueTracker::fetch_issues_by_id` → `Vec<(String, Issue)>`
4. `IssueTracker::search_issues` → `Vec<(String, Issue)>`
5. `IssueCache::merge_page` — insert using tuple key instead of `issue.id.clone()`
6. `IssueCache::add_pinned` — takes `Vec<(String, Issue)>` instead of `Vec<Issue>`
7. `inject_issues` — search results stored as `Vec<(String, Issue)>` or keyed; map construction uses tuple key
8. `Snapshot.issue_search_results: Option<Vec<Issue>>` → `Option<Vec<(String, Issue)>>` or `Option<IndexMap<String, Issue>>`

**Run tests, commit:**

```
refactor: extract id key from Issue value type
```

---

### Task 5: Extract `id` from CloudAgentSession

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — remove `id` field
- Modify: `crates/flotilla-core/src/refresh.rs:249` — adjust map construction
- Modify: provider trait `CodingAgent::list_sessions` and `claude.rs` impl

Fewer downstream usages — the session `id` is mostly used as the map key already. The `ses_key` from `WorkItem::session_key` serves as the identifier in the TUI.

**Run tests, commit:**

```
refactor: extract id key from CloudAgentSession value type
```

---

### Task 6: Extract `ws_ref` from Workspace

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — remove `ws_ref` field
- Modify: `crates/flotilla-core/src/refresh.rs:238` — adjust map construction
- Modify: `crates/flotilla-tui/src/ui.rs:555` — use map key
- Modify: provider trait `WorkspaceManager::list_workspaces` and `cmux.rs` impl

In `ui.rs:554-555`, the fallback `if ws.name.is_empty() { &ws.ws_ref }` needs to use the map key instead. The map key is `ws_ref` from `item.workspace_refs`, already in scope.

**Run tests, commit:**

```
refactor: extract ws_ref key from Workspace value type
```

---

### Task 7: Unify branches into keyed collection

Replace `remote_branches: Vec<String>` and `merged_branches: Vec<String>` with `branches: IndexMap<String, Branch>`.

**Files:**
- Modify: `crates/flotilla-protocol/src/provider_data.rs` — replace fields
- Modify: `crates/flotilla-core/src/refresh.rs:251-264` — build `branches` map
- Modify: `crates/flotilla-core/src/data.rs:449-464` — use `branches` for remote/merged filtering

**Step 1: Update `ProviderData` struct**

```rust
// Remove:
// pub remote_branches: Vec<String>,
// pub merged_branches: Vec<String>,

// Add:
pub branches: IndexMap<String, Branch>,
```

Use `Branch` and `BranchStatus` from `crate::delta` (or re-export from `provider_data`).

**Step 2: Update refresh.rs**

Replace the two separate assignments with a single `branches` map:

```rust
let mut branches_map = IndexMap::new();

for name in remote.unwrap_or_else(|e| { errors.push(...); Vec::new() }) {
    branches_map.insert(name, Branch { status: BranchStatus::Remote });
}
for name in merged.unwrap_or_else(|e| { errors.push(...); Vec::new() }) {
    // Merged overrides Remote if branch appears in both lists
    branches_map.insert(name, Branch { status: BranchStatus::Merged });
}
pd.branches = branches_map;
```

**Step 3: Update data.rs:449-464**

Replace:
```rust
let merged_set: HashSet<&str> = providers.merged_branches.iter()...
for b in &providers.remote_branches { ... && !merged_set.contains(b.as_str()) ... }
```

With:
```rust
for (name, branch) in &providers.branches {
    if branch.status == BranchStatus::Remote
        && name != "HEAD"
        && name != "main"
        && name != "master"
        && !known_branches.contains(name.as_str())
    {
        work_items.push(CorrelationResult::Standalone(
            StandaloneResult::RemoteBranch { branch: name.clone() },
        ));
    }
}
```

**Step 4: Update all other references**

Search for `remote_branches` and `merged_branches` across the codebase. Update any remaining references (e.g., the branch delete flow in executor, action menu filtering).

**Step 5: Update tests**

Update `ProviderData::default()` assertions and any test that constructs `ProviderData` with the old fields.

**Step 6: Run tests, commit**

Run: `cargo test --workspace`

```
refactor: unify remote_branches and merged_branches into keyed branches collection
```

---

### Task 8: Verify and clean up PR 1

**Step 1: Run full CI checks**

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

**Step 2: Review all changes**

Verify no behavior change — the app should function identically.

**Step 3: Commit any cleanup, open PR**

---

## PR 2: DeltaSource Trait and Diff Logic

Goal: implement the `DeltaSource` trait with a default IndexMap diff, wire up per-provider delta sources, and verify correctness with tests.

---

### Task 9: Implement DeltaSource trait with default IndexMap diff

**Files:**
- Create: `crates/flotilla-core/src/delta.rs`
- Modify: `crates/flotilla-core/src/lib.rs`

**Step 1: Write failing tests**

Test the diff function: given two IndexMaps, produce the correct `Vec<(K, EntryOp<V>)>`.

Cases:
- Empty → empty = no changes
- Empty → {a, b} = Added(a), Added(b)
- {a, b} → empty = Removed(a), Removed(b)
- {a, b} → {a, c} = Updated(a) if values differ, Removed(b), Added(c)
- {a, b} → {a, b} = no changes (values equal)

**Step 2: Implement**

```rust
use indexmap::IndexMap;
use flotilla_protocol::EntryOp;

/// Produces deltas for a keyed collection by diffing two IndexMaps.
pub fn diff_indexmap<K, V>(
    prev: &IndexMap<K, V>,
    curr: &IndexMap<K, V>,
) -> Vec<(K, EntryOp<V>)>
where
    K: Clone + Eq + std::hash::Hash,
    V: Clone + PartialEq,
{
    let mut changes = Vec::new();

    for (key, curr_val) in curr {
        match prev.get(key) {
            Some(prev_val) if prev_val == curr_val => {}
            Some(_) => changes.push((key.clone(), EntryOp::Updated(curr_val.clone()))),
            None => changes.push((key.clone(), EntryOp::Added(curr_val.clone()))),
        }
    }

    for key in prev.keys() {
        if !curr.contains_key(key) {
            changes.push((key.clone(), EntryOp::Removed));
        }
    }

    changes
}
```

**Step 3: Run tests, commit**

```
feat: add DeltaSource IndexMap diff function
```

---

### Task 10: Compute full snapshot deltas

Build a function that takes two `ProviderData` snapshots and produces `Vec<Change>` by diffing each collection.

**Files:**
- Modify: `crates/flotilla-core/src/delta.rs`

**Step 1: Write tests**

Test `diff_provider_data(prev, curr) -> Vec<Change>` with:
- Checkout added/removed/updated
- ChangeRequest added/removed
- Branch added/removed
- Mixed changes across multiple collections

**Step 2: Implement**

```rust
pub fn diff_provider_data(
    prev: &ProviderData,
    curr: &ProviderData,
) -> Vec<Change> {
    let mut changes = Vec::new();

    for (key, op) in diff_indexmap(&prev.checkouts, &curr.checkouts) {
        changes.push(Change::Checkout { key, op });
    }
    for (key, op) in diff_indexmap(&prev.change_requests, &curr.change_requests) {
        changes.push(Change::ChangeRequest { key, op });
    }
    for (key, op) in diff_indexmap(&prev.issues, &curr.issues) {
        changes.push(Change::Issue { key, op });
    }
    for (key, op) in diff_indexmap(&prev.sessions, &curr.sessions) {
        changes.push(Change::Session { key, op });
    }
    for (key, op) in diff_indexmap(&prev.workspaces, &curr.workspaces) {
        changes.push(Change::Workspace { key, op });
    }
    for (key, op) in diff_indexmap(&prev.branches, &curr.branches) {
        changes.push(Change::Branch { key, op });
    }

    changes
}
```

**Step 3: Run tests, commit**

```
feat: compute provider data deltas via IndexMap diff
```

---

### Task 11: Work item delta computation

Diff two `Vec<WorkItem>` (keyed by `WorkItemIdentity`) to produce work item changes.

**Files:**
- Modify: `crates/flotilla-core/src/delta.rs`

**Step 1: Write tests**

Test work item add/remove/update scenarios.

**Step 2: Implement**

Convert `Vec<WorkItem>` to `IndexMap<WorkItemIdentity, WorkItem>` and diff.

**Step 3: Run tests, commit**

```
feat: compute work item deltas
```

---

### Task 12: Apply deltas to ProviderData

Implement `apply_changes(snapshot: &mut ProviderData, changes: &[Change])` that materializes deltas.

**Files:**
- Modify: `crates/flotilla-core/src/delta.rs`

**Step 1: Write roundtrip tests**

For each test in Task 10, verify: `apply(prev, diff(prev, curr)) == curr`.

**Step 2: Implement**

Match on each `Change` variant, insert/update/remove from the appropriate IndexMap.

**Step 3: Run tests, commit**

```
feat: apply provider data deltas to materialized state
```

---

## PR 3: Delta Log and Broadcast

### Task 13: Add DeltaEntry and delta log to RepoState

Add `VecDeque<DeltaEntry>` to `RepoState` in `crates/flotilla-core/src/in_process.rs`. Compute deltas in `poll_snapshots` alongside current full snapshot broadcast (emit both for now).

**Note (PR #113 impact):** `poll_snapshots` now has a multi-phase structure: collect changes under write lock, correlate outside lock, apply and broadcast under write lock, then `fetch_missing_linked_issues`. Issue data comes from `IssueCache` via `inject_issues`, not directly from provider refresh. Deltas should diff the injected (cache-merged) providers, not the raw refresh providers. Issue-specific commands (`SetIssueViewport`, `FetchMoreIssues`, `SearchIssues`, `ClearIssueSearch`) also trigger `broadcast_snapshot` — these paths need delta computation too. `issue_total`, `issue_has_more`, `issue_search_results` are snapshot metadata, not part of the delta log.

### Task 14: Switch broadcast to SnapshotDelta

Update `DaemonEvent` to `SnapshotFull` / `SnapshotDelta` variants. `poll_snapshots` emits `SnapshotDelta` when delta is smaller than full, `SnapshotFull` otherwise.

### Task 15: Update socket server to forward new event types

`flotilla-daemon/src/server.rs` — the server already forwards all `DaemonEvent` variants. Verify new variants serialize correctly over the wire. Add integration test in `tests/socket_roundtrip.rs`.

---

## PR 4: Client-Side Materialization

### Task 16: SocketDaemon local state

Add `ClientRepoState { snapshot, seq }` per repo to `SocketDaemon`. Apply incoming deltas. `get_state` returns local copy.

### Task 17: Subscribe with last_seen seq map

Update `DaemonHandle::subscribe` signature. Daemon replays delta log or sends full snapshot on connect.

---

## PR 5: TUI Integration

### Task 18: Handle SnapshotDelta in TUI

`App::handle_daemon_event` dispatches `SnapshotFull` (existing `apply_snapshot`) and `SnapshotDelta` (apply changes incrementally, rebuild table view).

### Task 19: Simplified change detection

Non-empty delta on inactive tab → set `has_unseen_changes`. No full `ProviderData` comparison needed.

---

## Task Dependencies

```
Task 1 (protocol types) ──┐
Tasks 2-6 (key extraction) ├── can run in parallel after Task 1
Task 7 (branch unify)     ─┘
Task 8 (PR 1 verify)      ── after all of 1-7

Task 9 (diff function)    ── after PR 1 merged
Task 10 (snapshot diff)   ── after Task 9
Task 11 (work item diff)  ── after Task 9
Task 12 (apply deltas)    ── after Tasks 10, 11

Task 13 (delta log)       ── after PR 2 merged
Task 14 (broadcast)       ── after Task 13
Task 15 (socket verify)   ── after Task 14

Task 16 (client state)    ── after PR 3 merged
Task 17 (subscribe seq)   ── after Task 16

Task 18 (TUI delta)       ── after PR 4 merged
Task 19 (change detect)   ── after Task 18
```
