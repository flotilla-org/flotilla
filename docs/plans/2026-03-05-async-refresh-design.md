# Async Refresh: Three-Layer Reactive Architecture

## Problem

`refresh_all` blocks the UI event loop while querying all providers (GitHub API, Anthropic API, git, wt). During refresh the TUI is frozen — no key/mouse events processed, no rendering.

## Design

Split the monolithic `DataStore` into three layers connected by `tokio::watch` channels. Each layer has a single responsibility and a clear data-flow direction.

```
Provider tasks (background)
  │ watch::Sender<Arc<ProviderData>>
  ▼
Correlation task (background)
  │ watch::Sender<Arc<CorrelatedData>>
  ▼
UI render loop (foreground)
  │ owns TableView, TableState, selection
  ▼
Terminal
```

## Layer 1 — Provider Data

Raw data from external tools and APIs. One background task per repo queries all providers via `join!` on a 10-second timer.

```rust
struct ProviderData {
    checkouts: Vec<Checkout>,
    change_requests: Vec<ChangeRequest>,
    issues: Vec<Issue>,
    sessions: Vec<CloudAgentSession>,
    remote_branches: Vec<String>,
    merged_branches: Vec<String>,
    workspaces: Vec<Workspace>,
    provider_health: HashMap<&'static str, bool>,
}
```

Published as `Arc<ProviderData>` through a `watch::Sender`. Downstream layers receive cheap shared references — they can index into the raw vecs when they need full details (e.g. resolving a `WorkItem`'s `pr_idx` back to the actual `ChangeRequest`).

### Provider ownership

All provider traits gain `Send + Sync` bounds. Registry stores `Arc<dyn Trait + Send + Sync>` instead of `Box<dyn Trait>`. This is a mechanical change across all providers.

### Refresh trigger

The background task `select!`s between its timer and a `tokio::sync::Notify`. Manual refresh (`r` key) and post-command refresh both poke the Notify rather than calling `refresh_all` inline.

## Layer 2 — Correlated Data

UI-independent derived data. A background task per repo subscribes to Layer 1's watch channel. When provider data changes, it re-runs the union-find correlation to produce `WorkItem`s.

```rust
struct CorrelatedData {
    provider_data: Arc<ProviderData>,  // retained for index lookups
    work_items: Vec<WorkItem>,
    correlation_groups: Vec<CorrelatedGroup>,
    labels: RepoLabels,
}
```

`WorkItem` keeps its index-based references into `ProviderData` (worktree_idx, pr_idx, etc). These are stable within a single `Arc<ProviderData>` snapshot.

Published as `Arc<CorrelatedData>` through its own watch channel. This layer is the subscription point for any non-TUI observer (future web UI, API, etc).

### What moves out of WorkItem

`WorkItem` remains a domain concept — kind, branch, description, indices, workspace_refs. No sorting, no section headers, no table position.

## Layer 3 — UI View

TUI-specific presentation. The render loop checks `watch::Receiver<Arc<CorrelatedData>>` before each frame. If changed, rebuilds:

```rust
struct TableView {
    table_entries: Vec<TableEntry>,   // sorted, with section headers interleaved
    selectable_indices: Vec<usize>,   // indices into table_entries that are items (not headers)
}
```

Section headers, sort order, and grouping are determined here — different UIs could sort/filter differently on the same `CorrelatedData`.

`TableState` (scroll offset, selection) is preserved across rebuilds. Selection is stabilised by matching on branch name or item identity rather than raw index.

## Event Loop

```rust
loop {
    // Check for new correlated data (non-blocking)
    for repo in repos {
        if repo.correlated_rx.has_changed() {
            rebuild_table_view(repo);
        }
    }

    terminal.draw(|f| ui::render(...))?;

    match events.next().await {
        Event::Key(k) => { /* handle key, possibly poke refresh_trigger */ }
        Event::Mouse(m) => { /* handle mouse */ }
        Event::Tick => { /* nothing — refresh is background */ }
    }

    // Execute commands (create worktree, archive, etc.)
    while let Some(cmd) = commands.take_next() {
        executor::execute(cmd, ...).await;
        // poke refresh_trigger after mutation
    }
}
```

The Tick event becomes purely a render-rate heartbeat. No I/O is performed in the event loop.

## Struct Overview

```rust
// Per-repo handle for the background refresh system
struct RepoHandle {
    provider_data_tx: watch::Sender<Arc<ProviderData>>,
    correlated_data_tx: watch::Sender<Arc<CorrelatedData>>,
    refresh_trigger: Notify,
    task_handle: JoinHandle<()>,
}

// Domain model (read-only from UI's perspective)
struct RepoModel {
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    repo_criteria: RepoCriteria,
    correlated_rx: watch::Receiver<Arc<CorrelatedData>>,
}

// UI state per repo
struct RepoUiState {
    table_view: TableView,
    table_state: TableState,
    selected_selectable_idx: Option<usize>,
    multi_selected: BTreeSet<usize>,
    has_unseen_changes: bool,
    show_providers: bool,
}
```

## Command Execution

Commands (create worktree, archive session, etc.) still run in the foreground since they need to update UI mode (e.g. show loading state, switch to new tab). After completion, they poke `refresh_trigger` to get fresh data rather than calling `refresh_all`.

## Migration Path

This is a significant refactor. Incremental approach:

1. Split `DataStore` into `ProviderData` + `CorrelatedData` + `TableView` (no async changes yet)
2. Make provider traits `Send + Sync`, registry uses `Arc`
3. Move refresh into background task with watch channels
4. Remove `refresh_all`, update event loop

## Future Considerations

- **Tearing**: If partial reads ever cause issues, persistent data structures or RCU can be introduced. The `Arc<ProviderData>` snapshot model already prevents tearing within a single observation.
- **Per-provider concurrency**: Individual providers within a repo already run via `join!`. If needed, they could become truly independent tasks with their own watch channels, but batch-per-repo is simpler for now.
- **Web UI**: Layer 2's watch channel is the natural subscription point. A web server could subscribe alongside the TUI.
