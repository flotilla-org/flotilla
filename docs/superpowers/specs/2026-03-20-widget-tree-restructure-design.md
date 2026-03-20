# Widget Tree Restructure

## Problem

`BaseView` is a god-widget. It owns all base-layer children (TabBar, StatusBar, WorkItemTable, PreviewPanel, EventLogWidget), handles layout, mode switching, mouse routing, two-phase action dispatch, and drag state. Tab content is determined by checking `UiMode` rather than being structurally owned by tabs. Per-repo UI state lives in an external `HashMap<RepoIdentity, RepoUiState>` and gets swapped in and out based on the active tab.

This structure makes it hard to add new tab types, compose widgets differently, or reason about state ownership. As we add more views (agent dashboards, settings editors), the problem compounds.

## Design

### Widget Tree

```
Screen
├── Tabs
│   ├── TabPage { label: "Flotilla", content: OverviewPage }
│   │   └── OverviewPage
│   │       ├── ProvidersWidget
│   │       ├── HostsWidget
│   │       └── EventLogWidget
│   ├── TabPage { label: "my-repo", content: RepoPage }
│   │   └── RepoPage
│   │       ├── WorkItemTable
│   │       └── PreviewPanel
│   └── [+] (handled by Tabs directly, not a TabPage)
├── StatusBar
└── ModalStack (Vec<Box<dyn InteractiveWidget>>)
```

**Screen** is the root `InteractiveWidget`. It owns `Tabs`, `StatusBar`, and the modal stack. It resolves global actions (quit, refresh, tab switch) before anything reaches the widget tree.

**Tabs** owns a `Vec<TabPage>` and an active index. It renders the tab bar strip (absorbing the current `TabBar` widget) and delegates content rendering and input to the active page's content widget. Tab drag-reorder lives here.

**TabPage** is a struct, not a trait:

```rust
struct TabPage {
    label: TabLabel,
    content: Box<dyn InteractiveWidget>,
}
```

**RepoPage** owns its `WorkItemTable`, `PreviewPanel`, and all per-repo UI state (selection, multi-select, pending actions, layout preference). One instance per repo tab. No state swapping.

**OverviewPage** composes three child widgets — `ProvidersWidget`, `HostsWidget`, `EventLogWidget` — in a two-column layout: providers and hosts stacked on the left, event log on the right.

### State Ownership

Hard line: widgets own all their UI state. Daemon-sourced data is shared and read-only.

#### Shared<T>

A newtype wrapping `Arc<Mutex<Versioned<T>>>` with ergonomic accessors:

```rust
struct Versioned<T> {
    generation: u64,
    data: T,
}

struct Shared<T> {
    inner: Arc<Mutex<Versioned<T>>>,
}

impl<T> Shared<T> {
    fn read(&self) -> MutexGuard<Versioned<T>> { ... }
    fn changed(&self, since: &mut u64) -> Option<MutexGuard<T>> { ... }
    fn mutate(&self, f: impl FnOnce(&mut T)) { ... }
}
```

`changed(since)` is the primary query: "did this change since I last looked?" It compares generations, updates the caller's stored generation, and returns the data only if it advanced. One lock, one comparison, no manual bookkeeping.

`mutate` applies a closure and bumps the generation automatically.

#### Data Distribution

Each widget holds `Shared<T>` handles to exactly the data it needs. No monolithic shared model.

```rust
// Event loop owns the write side
struct AppData {
    repos: HashMap<RepoIdentity, Shared<RepoData>>,
    provider_statuses: Shared<ProviderStatuses>,
    hosts: Shared<HostsData>,
}

// RepoPage holds its repo's handle
struct RepoPage {
    repo_data: Shared<RepoData>,
    table: WorkItemTable,
    preview: PreviewPanel,
    multi_selected: HashSet<WorkItemIdentity>,
    pending_actions: HashMap<WorkItemIdentity, PendingAction>,
    layout: RepoViewLayout,
    last_seen_generation: u64,
}

// OverviewPage holds the handles it needs
struct OverviewPage {
    provider_statuses: Shared<ProviderStatuses>,
    hosts: Shared<HostsData>,
}
```

When a snapshot arrives, the event loop calls `mutate` on the specific repo's handle. On render, `RepoPage` calls `changed(&mut self.last_seen_generation)` — if its data changed, it reconciles (rebuilds grouped items, preserves selection by identity, prunes stale multi-select). If not, it just draws. A change to hosts does not force any `RepoPage` to reconcile.

The `Mutex` is uncontended in practice (single-threaded event loop) but provides correct ownership semantics in Rust.

#### Widget-Owned State

`WorkItemTable` owns its selection directly:

```rust
struct WorkItemTable {
    table_state: TableState,
    selected_identity: Option<WorkItemIdentity>,
    grouped_items: GroupedWorkItems,
}
```

Selection is tracked by identity, resolved to an index only for rendering. On reconciliation, it looks up `selected_identity` in the new data.

#### Commands Flow Out

Widgets mutate daemon state by pushing `ProtoCommand`s onto the command queue (via `WidgetContext`). The event loop sends them to the daemon. This is unchanged from today.

### Input Dispatch

Three phases, in order:

**Phase 1 — Global actions.** `Screen` resolves keymap into a `GlobalAction` enum (quit, refresh, tab switch, tab reorder, help). If matched, consumed immediately. Widgets never see it.

**Phase 2 — Modal dispatch.** If the modal stack is non-empty, the top modal gets the event exclusively. Returns `Consumed`, `Ignored`, `Finished` (pop), `Push`, or `Swap`. If `Ignored`, the event is dropped — modals trap input.

**Phase 3 — Page dispatch.** `Tabs` delegates to the active `TabPage`'s content widget. The content widget handles internally (e.g. `RepoPage` routes to `WorkItemTable`). If the child returns `Ignored`, the page handles page-level actions (layout toggle, multi-select). `Outcome::Push` from any widget pushes onto `Screen`'s modal stack.

Mouse routing follows the same phases: `Screen` hit-tests tab bar vs content vs status bar and delegates accordingly.

### Rendering Pipeline

Top-down, immediate mode. Widgets read their `Shared<T>` handles for data and own their UI state directly.

```
Screen::render()
├── self.tabs.render()
│   ├── render tab bar strip
│   └── active_page.content.render()
│       ├── RepoPage: table + preview (layout split)
│       └── OverviewPage: providers + hosts + event log
├── self.status_bar.render()
└── for modal in &mut self.modal_stack:
        modal.render()
```

`RenderContext` slims down — no longer carries `&mut UiState` or per-repo state:

```rust
struct RenderContext<'a> {
    theme: &'a Theme,
    keymap: &'a Keymap,
    in_flight: &'a HashMap<u64, InFlightCommand>,
    active_widget_mode: ModeId,
    active_widget_data: WidgetStatusData,
}
```

### Widget Lifecycle

**Tab creation.** When a repo is added, the event loop creates a `Shared<RepoData>` handle, constructs a `RepoPage` with it, wraps it in a `TabPage`, and pushes it into `Tabs.pages`.

**Tab removal.** `Tabs` drops the `TabPage`. The `RepoPage` and all its UI state are gone. The `Shared<RepoData>` handle's refcount drops; if the event loop also drops its side, the data is freed.

**OverviewPage.** Created once at startup. Lives for the lifetime of the app.

**Modals.** Created on demand via `Outcome::Push`, destroyed on `Finished`. Stack lives on `Screen`.

**Tab reordering.** Reorders `Vec<TabPage>` — no state reconstruction.

### Modals

Modals remain at `Screen` level (app-scoped). The current set stays as-is:

| Modal | Purpose |
|-------|---------|
| ActionMenu | Available actions for selected item(s) |
| Help | Key binding reference |
| BranchInput | Text input for new branch name |
| DeleteConfirm | Checkout deletion confirmation |
| CloseConfirm | PR close confirmation |
| CommandPalette | Fuzzy-filter command picker |
| FilePicker | Filesystem browser for adding repos |
| IssueSearch | Issue search query input |

Several of these are candidates for folding into the command palette in future (BranchInput, IssueSearch, DeleteConfirm, CloseConfirm), but that is out of scope.

## Migration Sequence

Each step leaves the app working.

1. **Introduce `Shared<T>` and `Versioned<T>`.** The newtype with `changed()`, `read()`, `mutate()`. Small, testable in isolation.

2. **Create `Screen` widget.** Takes over from `App` as the root `InteractiveWidget`. Owns modal stack, `StatusBar`, and a placeholder for `Tabs`. Global action split happens here.

3. **Create `Tabs` widget.** Absorbs `TabBar` rendering, owns `Vec<TabPage>`, routes input and render to the active page.

4. **Create `RepoPage`.** Absorbs `WorkItemTable`, `PreviewPanel`, and per-repo UI state from `RepoUiState`. Holds `Shared<RepoData>`. One instance per repo tab.

5. **Create `OverviewPage`.** Splits the current `EventLogWidget` into `ProvidersWidget`, `HostsWidget`, and a slimmed `EventLogWidget`, composed in the two-column layout.

6. **Delete `BaseView`.** Everything it did now lives in `Screen`, `Tabs`, `RepoPage`, or `OverviewPage`.

7. **Remove `RepoUiState` from `UiState`.** It is now owned by `RepoPage` instances. `UiState` shrinks or disappears.
