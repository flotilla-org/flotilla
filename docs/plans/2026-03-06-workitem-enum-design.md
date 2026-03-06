# WorkItem Enum Refactor Design

## Problem

`WorkItem` is a flat struct with 10 fields where field validity depends on `kind`. A `Session`-kind item carries `checkout_key: None`, `pr_key: None`, `is_main_worktree: false` — fields that have no meaning for it. An `Issue` carries empty vecs and Nones for everything except `issue_keys`. The type system doesn't prevent constructing invalid states.

## Design

Replace the flat struct with an enum that separates correlated items (produced by the correlation engine) from standalone items (produced post-correlation).

### Core Types

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
    pub correlation_group_idx: usize,  // not Option — correlated items always have one
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
```

`WorkItemKind` and `WorkItemIdentity` remain unchanged — `kind()` is derived via helper method.

### Anchor Priority

The existing priority system determines the anchor when a correlation group contains multiple item types:

- Checkout > Pr > Session

Remaining keys become cross-references (`linked_pr`, `linked_session`). For example, a group with a checkout, PR, and session produces:

- `anchor: CorrelatedAnchor::Checkout(CheckoutRef { ... })`
- `linked_pr: Some(pr_key)`
- `linked_session: Some(session_key)`

### Helper Methods

Preserve the current field-access API so consuming code changes minimally:

```rust
impl WorkItem {
    pub fn kind(&self) -> WorkItemKind;
    pub fn branch(&self) -> Option<&str>;
    pub fn description(&self) -> &str;
    pub fn checkout(&self) -> Option<&CheckoutRef>;
    pub fn checkout_key(&self) -> Option<&Path>;
    pub fn is_main_worktree(&self) -> bool;
    pub fn pr_key(&self) -> Option<&str>;       // checks anchor, then linked_pr
    pub fn session_key(&self) -> Option<&str>;   // checks anchor, then linked_session
    pub fn issue_keys(&self) -> &[String];
    pub fn workspace_refs(&self) -> &[String];
    pub fn correlation_group_idx(&self) -> Option<usize>;
    pub fn identity(&self) -> Option<WorkItemIdentity>;
    pub fn as_correlated_mut(&mut self) -> Option<&mut CorrelatedWorkItem>;
}
```

### Construction

**`group_to_work_item`**: Collects keys from correlation group as today. Determines anchor by priority. Remaining keys go into `linked_*` fields. Returns `WorkItem::Correlated(...)`.

**Post-correlation issue linking**: Uses `as_correlated_mut()` to push to `linked_issues`.

**Standalone items**: `WorkItem::Standalone(StandaloneWorkItem::Issue { ... })` and `WorkItem::Standalone(StandaloneWorkItem::RemoteBranch { ... })`.

### Files Changed

| File | Change |
|------|--------|
| `src/data.rs` | Type definitions, `group_to_work_item`, `correlate`, `build_table_view`, tests |
| `src/ui.rs` | Field access to helper method calls |
| `src/app/intent.rs` | Field access to helper method calls |
| `src/app/executor.rs` | Field access to helper method calls |
| `src/main.rs` | `drain_snapshots` — mutation via `as_correlated_mut()` |
| `src/app/mod.rs` | Multi-select identity — trivial |

### What Doesn't Change

- `WorkItemKind` enum, `WorkItemIdentity` enum
- Provider types, correlation engine, commands, UI layout
- External behavior

## Future Work (separate issues)

- **RemoteBranch promotion**: When we enrich remote branches with issue links from git config, promote RemoteBranch to a `CorrelatedAnchor` variant.
- **Multi-session support**: `linked_session: Option<String>` becomes `Vec<String>` when we support multiple sessions per work item (e.g. Codex + Claude on the same branch).
- **DataStore flattening** (issue #16): Flatten vestigial DataStore fields into RepoModel.
