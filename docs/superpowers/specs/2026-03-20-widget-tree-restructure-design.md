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

**RepoPage** owns its `WorkItemTable`, `PreviewPanel`, and all per-repo UI state (selection, multi-select, pending actions, layout). One instance per repo tab. No state swapping. Layout becomes per-repo (currently global on `UiState`) — `CycleLayout` becomes a page-scoped action rather than a global one.

**OverviewPage** composes three child widgets — `ProvidersWidget`, `HostsWidget`, `EventLogWidget` — in a two-column layout: providers and hosts stacked on the left, event log on the right. `ProvidersWidget` shows aggregate provider status across all repos. `HostsWidget` shows connected hosts and their health.

### State Ownership

Hard line: widgets own all their UI state. Daemon-sourced data is shared and read-only.

#### Shared<T>

A newtype wrapping an `AtomicU64` generation counter and a `Mutex<T>`:

```rust
struct Shared<T> {
    generation: AtomicU64,
    data: Mutex<T>,
}

impl<T> Shared<T> {
    fn read(&self) -> MutexGuard<T> { ... }
    fn changed(&self, since: &mut u64) -> Option<MutexGuard<T>> { ... }
    fn generation(&self) -> u64 { self.generation.load(Ordering::Acquire) }
    fn mutate(&self, f: impl FnOnce(&mut T)) { ... }
}
```

The generation counter is an `AtomicU64` outside the mutex, so `generation()` is lock-free and `changed()` returns `MutexGuard<T>` directly (not `MutexGuard<Versioned<T>>`).

`changed(since)` is the primary query: "did this change since I last looked?" It checks the atomic generation, and only if it advanced does it lock the mutex, update the caller's stored generation, and return the data. One atomic load in the common (unchanged) case. No manual bookkeeping.

`mutate` locks the mutex, applies the closure, and bumps the atomic generation.

#### Data Distribution

Each owning widget holds a `Shared<T>` handle to exactly the data it needs. No monolithic shared model. Child widgets do not hold `Shared` handles — the owning widget reconciles in one place and pushes derived data down to children via direct field mutation or method arguments.

```rust
// Event loop owns the write side
struct AppData {
    repos: HashMap<RepoIdentity, Shared<RepoData>>,
    provider_statuses: Shared<ProviderStatuses>,
    hosts: Shared<HostsData>,
}

// RepoPage holds its repo's handle — sole reader
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

When a snapshot arrives, the event loop calls `mutate` on the specific repo's handle. On render, `RepoPage` calls `changed(&mut self.last_seen_generation)` — if its data changed, it reconciles (rebuilds grouped items, preserves selection by identity, prunes stale multi-select) and pushes the results into `WorkItemTable` and `PreviewPanel`. If not, it just draws. A change to hosts does not force any `RepoPage` to reconcile.

The `Mutex` is uncontended in practice (single-threaded event loop) but provides correct ownership semantics in Rust. Because only the owning widget calls `changed()`/`read()`, there is no risk of sibling widgets deadlocking on the same handle during a render pass.

#### Reconciliation Hierarchy

`RepoPage` is the sole reader of `Shared<RepoData>`. Its children (`WorkItemTable`, `PreviewPanel`) never touch the `Shared` handle. On reconciliation, `RepoPage`:

1. Rebuilds `GroupedWorkItems` from the new data
2. Calls `self.table.update_items(grouped_items)` — the table preserves selection by identity and prunes stale multi-select
3. The preview panel reads the selected item from the table on each render — no explicit push needed

Similarly, `OverviewPage` is the sole reader of its `Shared` handles and pushes derived data to `ProvidersWidget`, `HostsWidget`, and `EventLogWidget`.

#### Widget-Owned State

`WorkItemTable` owns its selection directly:

```rust
struct WorkItemTable {
    table_state: TableState,
    selected_identity: Option<WorkItemIdentity>,
    grouped_items: GroupedWorkItems,
}
```

Selection is tracked by identity, resolved to an index only for rendering. On `update_items()`, it looks up `selected_identity` in the new data.

#### Commands Flow Out

Widgets mutate daemon state by pushing `ProtoCommand`s onto the command queue (via `WidgetContext`). The event loop sends them to the daemon.

#### WidgetContext (New)

The current `WidgetContext` carries `&TuiModel`, `&mut HashMap<RepoIdentity, RepoUiState>`, `&mut UiMode`, and more. In the new design it slims down — widgets own their state, and the mode is implicit in which widget/modal is active:

```rust
struct WidgetContext<'a> {
    commands: &'a mut CommandQueue,
    app_actions: &'a mut Vec<AppAction>,
    keymap: &'a Keymap,
}
```

`app_actions` remains the channel for widgets to request things the widget tree cannot handle alone (e.g. `SaveTabOrder`, `ClearStatusError`). Actions that were previously app-global but are structurally resolved by the new tree (`SwitchToRepo`, `SwitchToConfig`, `OpenFilePicker`) become methods on `Tabs` or `Screen` rather than `AppAction` variants.

#### UiMode Elimination

`UiMode` disappears. Today it distinguishes Normal, Config, and IssueSearch modes. In the new design:

- Normal vs Config → which `TabPage` is active (RepoPage vs OverviewPage)
- IssueSearch → a modal or command-palette state, not a mode

#### InteractiveWidget Trait

The trait signature is unchanged. Only the context types (`WidgetContext`, `RenderContext`) change as described above. The `as_any`/`as_any_mut` methods remain for downcast access (used by `Screen` for animation checks).

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

Widgets that need daemon-sourced data read their `Shared<T>` handles (see Reconciliation Hierarchy above). `StatusBar` is a special case — it needs in-flight commands, the active widget mode, and error/status messages. `Screen` passes these via `RenderContext` and direct method arguments, since StatusBar is a leaf widget that doesn't own `Shared` handles.

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

### Status Bar (Future)

The status bar should behave the same regardless of which page is active. Key hints should be context-sensitive to what's available (e.g. commands requiring a selection should not appear when nothing is selected) rather than switching wholesale based on page type. This is orthogonal to the widget restructure and can be addressed separately. Consider whether the work item table should always have a default cursor position so selection-dependent commands are always available.

## Migration Sequence

Each step leaves the app working.

1. **Introduce `Shared<T>`.** The newtype with `changed()`, `read()`, `mutate()`, `generation()`. Small, testable in isolation.

2. **Create `Screen` widget (thin wrapper).** `Screen` wraps the existing `BaseView` and modal stack. Delegates everything — no behavior change. This establishes the new root without rewiring dispatch.

3. **Move global action resolution into `Screen`.** Extract global action dispatch from `App`/`BaseView` into `Screen`. `Screen` resolves globals first, delegates the rest to `BaseView`.

4. **Move modal stack into `Screen`.** `Screen` owns the modal stack and handles `Push`/`Finished`/`Swap` outcomes. `BaseView` no longer manages modals.

5. **Create `Tabs` widget.** Absorbs `TabBar` rendering, owns `Vec<TabPage>`, routes input and render to the active page.

6. **Create `RepoPage`.** Absorbs `WorkItemTable`, `PreviewPanel`, and per-repo UI state from `RepoUiState`. Holds `Shared<RepoData>`. One instance per repo tab.

7. **Create `OverviewPage`.** Splits the current `EventLogWidget` into `ProvidersWidget`, `HostsWidget`, and a slimmed `EventLogWidget`, composed in the two-column layout.

8. **Delete `BaseView`.** Everything it did now lives in `Screen`, `Tabs`, `RepoPage`, or `OverviewPage`.

9. **Remove `RepoUiState` from `UiState`.** It is now owned by `RepoPage` instances. `UiState` shrinks or disappears.
