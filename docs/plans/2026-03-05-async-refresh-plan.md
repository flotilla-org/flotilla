# Async Refresh Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the blocking `refresh_all` with background tasks and watch channels so the UI never freezes during provider queries.

**Architecture:** Three-layer reactive split — ProviderData (background), CorrelatedData (background), TableView (UI). Connected by `tokio::watch` channels carrying `Arc` snapshots. See `docs/plans/2026-03-05-async-refresh-design.md`.

**Tech Stack:** tokio (watch, sync::Notify, spawn), Arc, existing ratatui/crossterm

---

### Task 1: Extract ProviderData from DataStore

Split the raw provider fields out of `DataStore` into a standalone struct. This is a pure refactor — no async changes yet.

**Files:**
- Create: `src/provider_data.rs`
- Modify: `src/data.rs:63-79` (remove raw fields)
- Modify: `src/lib.rs` (add module)
- Modify: `src/app/executor.rs:232-363` (update refresh_all)

**Step 1: Create ProviderData struct**

Create `src/provider_data.rs`:

```rust
use std::collections::HashMap;
use crate::providers::types::*;

#[derive(Debug, Default, Clone)]
pub struct ProviderData {
    pub checkouts: Vec<Checkout>,
    pub change_requests: Vec<ChangeRequest>,
    pub issues: Vec<Issue>,
    pub sessions: Vec<CloudAgentSession>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
    pub workspaces: Vec<Workspace>,
    pub provider_health: HashMap<&'static str, bool>,
}
```

**Step 2: Update DataStore to contain ProviderData**

In `src/data.rs`, replace the individual fields with:

```rust
pub struct DataStore {
    pub providers: ProviderData,
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
    pub loading: bool,
    pub correlation_groups: Vec<CorrelatedGroup>,
}
```

**Step 3: Update all references**

Every `self.checkouts` in data.rs becomes `self.providers.checkouts`, etc. Every `rm.data.checkouts` in executor.rs and elsewhere becomes `rm.data.providers.checkouts`. Same for `change_requests`, `issues`, `sessions`, `remote_branches`, `merged_branches`, `workspaces`, `provider_health`.

Key files to update:
- `src/data.rs` — `refresh()`, `correlate()`, `group_to_work_item()`, `data_snapshot()`
- `src/app/executor.rs` — `refresh_all()`, `execute()` (reads checkouts, sessions, issues, change_requests, table_entries, selectable_indices)
- `src/app/mod.rs` — `selected_work_item()` reads `data.table_entries` and `data.selectable_indices`
- `src/ui.rs` — reads `checkouts`, `change_requests`, `issues`, `sessions` for detail rendering

**Step 4: Build and run tests**

Run: `cargo build && cargo test`
Expected: All 7 tests pass, no behavior change.

**Step 5: Commit**

```bash
git add -A
git commit -m "refactor: extract ProviderData struct from DataStore"
```

---

### Task 2: Extract CorrelatedData from DataStore

Move the correlation output (WorkItems, groups) into its own struct, separate from the table presentation.

**Files:**
- Create: `src/correlated_data.rs`
- Modify: `src/data.rs` (split correlate into two phases)
- Modify: `src/lib.rs`

**Step 1: Create CorrelatedData struct**

Create `src/correlated_data.rs`:

```rust
use std::sync::Arc;
use crate::provider_data::ProviderData;
use crate::providers::correlation::CorrelatedGroup;
use crate::data::WorkItem;

#[derive(Debug, Clone)]
pub struct CorrelatedData {
    pub provider_data: Arc<ProviderData>,
    pub work_items: Vec<WorkItem>,
    pub correlation_groups: Vec<CorrelatedGroup>,
}
```

**Step 2: Split correlate() into two functions**

In `src/data.rs`, split the existing `correlate()` (lines 280-494) into:

1. `pub fn correlate(provider_data: &ProviderData) -> CorrelatedData` — free function that runs phases 1-3 (build CorrelatedItems, run union-find, convert groups to WorkItems). Returns unordered WorkItems.

2. `pub fn build_table_view(correlated: &CorrelatedData, labels: &SectionLabels) -> TableView` — free function that runs phase 4 (sort, section headers, selectable indices). Returns a new `TableView` struct.

**Step 3: Create TableView struct**

In `src/data.rs`:

```rust
pub struct TableView {
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
}
```

Remove `table_entries` and `selectable_indices` from `DataStore`. `DataStore` now only has `providers: ProviderData`, `loading: bool`, and `correlation_groups` (move to CorrelatedData).

**Step 4: Update all consumers**

- `src/app/executor.rs` — `refresh_all()` calls `correlate()` then `build_table_view()`, stores results
- `src/app/mod.rs` — `selected_work_item()` now reads from wherever TableView lives (initially keep on RepoModel or RepoUiState)
- `src/ui.rs` — reads TableView for rendering

Temporarily store `CorrelatedData` and `TableView` on `RepoModel` to keep things compiling. The watch channels come in Task 4.

**Step 5: Build and run tests**

Run: `cargo build && cargo test`

**Step 6: Commit**

```bash
git add -A
git commit -m "refactor: extract CorrelatedData and TableView from DataStore"
```

---

### Task 3: Convert ProviderRegistry to use Arc

Change `Box<dyn Trait>` to `Arc<dyn Trait>` in the registry so providers can be shared with background tasks.

**Files:**
- Modify: `src/providers/registry.rs` (Box → Arc)
- Modify: `src/providers/discovery.rs` (Box::new → Arc::new)

**Step 1: Update ProviderRegistry**

In `src/providers/registry.rs`, change:

```rust
pub struct ProviderRegistry {
    pub vcs: IndexMap<String, Arc<dyn Vcs>>,
    pub checkout_managers: IndexMap<String, Arc<dyn CheckoutManager>>,
    pub code_review: IndexMap<String, Arc<dyn CodeReview>>,
    pub issue_trackers: IndexMap<String, Arc<dyn IssueTracker>>,
    pub coding_agents: IndexMap<String, Arc<dyn CodingAgent>>,
    pub ai_utilities: IndexMap<String, Arc<dyn AiUtility>>,
    pub workspace_manager: Option<(String, Arc<dyn WorkspaceManager>)>,
}
```

Note: `coding_agents` is already `Arc`. The others change from `Box` to `Arc`.

**Step 2: Update discovery.rs**

Every `Box::new(...)` becomes `Arc::new(...)` for provider construction in `detect_providers()`.

**Step 3: Update executor.rs and any callers**

`Box<dyn Trait>` and `Arc<dyn Trait>` have the same deref behavior, so most call sites need no change. Fix any type annotation issues.

**Step 4: Build and run tests**

Run: `cargo build && cargo test`

**Step 5: Commit**

```bash
git add -A
git commit -m "refactor: use Arc for provider trait objects in registry"
```

---

### Task 4: Introduce watch channels and background refresh task

The main event — move refresh into a background tokio task per repo.

**Files:**
- Create: `src/refresh.rs` (background refresh task)
- Modify: `src/main.rs` (event loop no longer awaits refresh)
- Modify: `src/app/model.rs` (RepoModel gets watch receivers)
- Modify: `src/app/executor.rs` (remove refresh_all, commands poke Notify)
- Modify: `src/lib.rs`

**Step 1: Create the refresh module**

Create `src/refresh.rs` with:

```rust
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::provider_data::ProviderData;
use crate::correlated_data::CorrelatedData;
use crate::data;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;
use crate::app::model::RepoLabels;

pub struct RepoRefreshHandle {
    pub refresh_trigger: Arc<Notify>,
    pub correlated_rx: watch::Receiver<Arc<CorrelatedData>>,
    pub labels_rx: watch::Receiver<RepoLabels>,
    pub errors_rx: watch::Receiver<Vec<data::ProviderError>>,
    task_handle: JoinHandle<()>,
}

impl RepoRefreshHandle {
    pub fn spawn(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        interval: Duration,
    ) -> Self {
        let (correlated_tx, correlated_rx) = watch::channel(Arc::new(CorrelatedData::default()));
        let (labels_tx, labels_rx) = watch::channel(RepoLabels::default());
        let (errors_tx, errors_rx) = watch::channel(Vec::new());
        let refresh_trigger = Arc::new(Notify::new());
        let trigger = refresh_trigger.clone();

        let task_handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = trigger.notified() => {}
                }

                // Fetch all provider data
                let mut provider_data = ProviderData::default();
                let errors = provider_data.refresh(&repo_root, &registry, &criteria).await;

                // Correlate
                let pd = Arc::new(provider_data);
                let correlated = Arc::new(data::correlate(&pd));

                // Compute labels from registry
                let labels = RepoLabels::from_registry(&registry);

                // Publish
                let _ = correlated_tx.send(correlated);
                let _ = labels_tx.send(labels);
                let _ = errors_tx.send(errors);
            }
        });

        Self {
            refresh_trigger,
            correlated_rx,
            labels_rx,
            errors_rx,
            task_handle,
        }
    }

    pub fn trigger_refresh(&self) {
        self.refresh_trigger.notify_one();
    }
}
```

**Step 2: Move refresh logic into ProviderData**

Move the `refresh()` method from `DataStore` onto `ProviderData`. It fetches all raw data via `join!` and populates the struct. Strip out the correlation call — that happens separately now.

**Step 3: Move label computation to RepoLabels**

Add `RepoLabels::from_registry(registry: &ProviderRegistry) -> Self` — extract the label-building code from the old `refresh_all`.

**Step 4: Update RepoModel**

```rust
pub struct RepoModel {
    pub repo_root: PathBuf,
    pub registry: Arc<ProviderRegistry>,
    pub repo_criteria: RepoCriteria,
    pub refresh_handle: RepoRefreshHandle,
}
```

Remove `data: DataStore` — data now comes from the watch channel.

**Step 5: Update the main event loop**

In `src/main.rs`, before each render:

```rust
// Drain watch channels
for path in &app.model.repo_order {
    let rm = app.model.repos.get_mut(path).unwrap();
    if rm.refresh_handle.correlated_rx.has_changed().unwrap_or(false) {
        let correlated = rm.refresh_handle.correlated_rx.borrow_and_update().clone();
        let labels = rm.refresh_handle.labels_rx.borrow_and_update().clone();
        // Rebuild table view
        let rui = app.ui.repo_ui.get_mut(path).unwrap();
        rui.table_view = data::build_table_view(&correlated, &labels.section_labels());
        // ... change detection, selection clamping ...
    }
}
```

Remove the `Event::Tick` refresh logic. Tick becomes a pure render heartbeat.

**Step 6: Update executor**

Remove `refresh_all()`. Each command that previously called `refresh_all(app).await` now calls:

```rust
app.model.active().refresh_handle.trigger_refresh();
```

Commands that need provider data (e.g. `FetchDeleteInfo`, `GenerateBranchName`) read from the latest `CorrelatedData` snapshot via the watch receiver's `borrow()`.

**Step 7: Build and run tests**

Run: `cargo build && cargo test`

**Step 8: Commit**

```bash
git add -A
git commit -m "feat: background refresh with watch channels, non-blocking UI"
```

---

### Task 5: Move TableView to RepoUiState

Complete the layer separation: TableView is a UI concern.

**Files:**
- Modify: `src/app/ui_state.rs` (add TableView field)
- Modify: `src/app/mod.rs` (selected_work_item reads from TableView)
- Modify: `src/ui.rs` (render reads from TableView)

**Step 1: Add TableView to RepoUiState**

```rust
pub struct RepoUiState {
    pub table_view: data::TableView,
    // ... existing fields ...
}
```

**Step 2: Update selected_work_item and other accessors**

`App::selected_work_item()` currently reads `self.model.active().data.table_entries`. Change to read from `self.active_ui().table_view.table_entries`.

Similarly, `FetchDeleteInfo` in executor reads `selectable_indices` and `table_entries` — route through the UI state.

**Step 3: Update ui.rs render functions**

Render functions currently get data from `model.repos[path].data.table_entries`. Route through `ui.repo_ui[path].table_view` instead.

**Step 4: Build and run tests**

Run: `cargo build && cargo test`

**Step 5: Commit**

```bash
git add -A
git commit -m "refactor: move TableView to RepoUiState, completing layer separation"
```

---

### Task 6: Clean up and remove dead code

Remove `DataStore` if it's now empty, clean up imports, remove `refresh_all`.

**Files:**
- Modify: `src/data.rs` (remove DataStore if empty)
- Modify: various files (clean imports)

**Step 1: Audit remaining uses of DataStore**

Check if `DataStore` still has any fields or methods. If empty, remove it. If it still holds `loading`, consider moving that to the refresh handle or RepoModel.

**Step 2: Remove unused imports and dead code**

Run: `cargo build 2>&1 | grep warning` and fix all warnings.

**Step 3: Build and run tests**

Run: `cargo build && cargo test`

**Step 4: Commit**

```bash
git add -A
git commit -m "chore: remove DataStore, clean up dead code after async refactor"
```

---

### Task 7: Integration testing

Verify the full flow works end-to-end.

**Step 1: Manual smoke test**

Run: `cargo run`

Verify:
- UI renders immediately (no freeze on startup)
- Data populates within a few seconds
- `r` key triggers immediate refresh
- Tab switching works
- Action menu works (Space on an item)
- Creating a worktree triggers a refresh (new data appears)
- Multiple repos all refresh independently

**Step 2: Verify no regressions in existing tests**

Run: `cargo test`

**Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: address issues found during async refresh integration testing"
```
