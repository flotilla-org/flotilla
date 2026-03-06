# Key-Based Provider Data Model

Resolves [#21](https://github.com/rjwittams/flotilla/issues/21).

## Problem

`ProviderData` stores items in flat `Vec<T>` collections referenced by positional index. This causes:

- **C1:** Checkout rows silently merged when correlation groups contain multiple checkouts (last overwrites)
- **C3:** Selection drifts to wrong item after refresh (index clamping)
- **C4:** Unseen-change badge misses content changes (length-only comparison)
- **P1:** O(n^2) multi-select lookup in render (linear scan per row)
- **P2:** PR-to-issue linking does repeated linear searches

## Design

### 1. ProviderData — keyed collections

Replace `Vec<T>` with `IndexMap<K, T>` using natural identities:

```rust
pub struct ProviderData {
    pub checkouts: IndexMap<PathBuf, Checkout>,
    pub change_requests: IndexMap<String, ChangeRequest>,
    pub issues: IndexMap<String, Issue>,
    pub sessions: IndexMap<String, CloudAgentSession>,
    pub workspaces: IndexMap<String, Workspace>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
}
```

Keys: checkout path, PR id, session id, issue id, ws_ref. `remote_branches` and `merged_branches` stay as `Vec<String>` — simple filter lists, not looked up by index.

Key fields remain on the structs (e.g. `Checkout.path`, `Issue.id`) as owned data. The map key is the lookup handle.

Adds `indexmap` crate dependency.

### 2. Correlation — emit keys instead of indices

```rust
pub enum ProviderItemKey {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Workspace(String),
}

pub struct CorrelatedItem {
    pub provider_name: String,
    pub kind: ItemKind,
    pub title: String,
    pub correlation_keys: Vec<CorrelationKey>,
    pub source_key: ProviderItemKey,  // was source_index: usize
}
```

Union-find logic unchanged — still groups by shared `CorrelationKey`. Only the identity carried through changes.

### 3. WorkItem — keyed references

```rust
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub branch: Option<String>,
    pub description: String,
    pub checkout_key: Option<PathBuf>,       // was worktree_idx
    pub is_main_worktree: bool,
    pub pr_key: Option<String>,              // was pr_idx
    pub session_key: Option<String>,         // was session_idx
    pub issue_keys: Vec<String>,             // was issue_idxs
    pub workspace_refs: Vec<String>,         // unchanged
    pub correlation_group_idx: Option<usize>, // debug display, unchanged
}
```

All call sites change from `providers.checkouts[idx]` to `providers.checkouts.get(&key)`. Affects `ui.rs`, `intent.rs`, `executor.rs`.

### 4. Selection persistence

```rust
pub enum WorkItemIdentity {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
}
```

Every `WorkItem` derives its identity from kind + primary key via `WorkItem::identity()`.

Refresh path in `drain_snapshots`:
1. Save `identity()` of selected item
2. Rebuild table view
3. Scan for matching identity, restore selection
4. If not found (item removed), clamp as today

`multi_selected` changes from `BTreeSet<usize>` to `HashSet<WorkItemIdentity>`.

Action-driven selection transitions (e.g. creating a checkout selects the new row) are separate from refresh persistence — handled at the command level.

### 5. Change detection

Derive `PartialEq` on `ProviderData` and all item types. Detection becomes:

```rust
let changed = old_providers != new_providers;
```

Triggers unseen-change badge on any content change (title, status, commits), not just count changes. Fixes C4.

### 6. Render performance

**P1 fix:** `multi_selected` stores `WorkItemIdentity`. Check becomes O(1):
```rust
let is_multi_selected = multi_selected.contains(&work_item.identity());
```

**P2 fix:** Issue linking uses `providers.issues.get(&issue_id)` — O(1) by key. `linked_issue_indices` becomes `linked_issue_keys: HashSet<String>`.

### 7. Multi-checkout gap

When a correlation group contains multiple checkouts, keep the first `checkout_key`. Explicit choice rather than accidental last-wins overwrite. Documented gap per `docs/architecture/workspace-manager-model.md`.

## Files affected

- `Cargo.toml` — add `indexmap`
- `src/provider_data.rs` — collection types
- `src/providers/types.rs` — `PartialEq` derives, `ProviderItemKey` enum
- `src/providers/correlation.rs` — `source_key` replaces `source_index`
- `src/data.rs` — `WorkItem` fields, `correlate()`, `group_to_work_item()`, `build_table_view()`
- `src/app/ui_state.rs` — `WorkItemIdentity`, `multi_selected` type
- `src/app/mod.rs` — selection save/restore, multi-select logic
- `src/app/intent.rs` — keyed lookups
- `src/app/executor.rs` — keyed lookups
- `src/ui.rs` — render lookups, multi-select check
- `src/main.rs` — `drain_snapshots` change detection + selection persistence
- `src/refresh.rs` — snapshot building
- All provider implementations — emit keyed data into IndexMaps
