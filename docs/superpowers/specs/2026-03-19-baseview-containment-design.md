# BaseView Containment Fix

Fix the widget containment structure so BaseView owns its children, restores WorkItemTable as a proper component, and absorbs ui::render().

## Problem

The widget refactor left BaseView as a hollow shell — `struct BaseView;` with no fields. Its children (TabBar, StatusBarWidget, EventLogWidget, PreviewPanel) live as separate fields on `App` and are threaded through `RenderContext` to reach `BaseView::render()`, which just calls `ui::render()` as a pass-through. `WorkItemTable` was absorbed into BaseView rather than composed, so the table has no independent identity. `RenderContext` became a kitchen sink carrying `&mut UiState`, child refs, widget mode data, and status bar data.

## Design

### BaseView owns its children

```rust
pub struct BaseView {
    pub tab_bar: TabBar,
    pub status_bar: StatusBarWidget,
    pub table: WorkItemTable,
    pub preview: PreviewPanel,
    pub event_log: EventLogWidget,
}
```

These fields are removed from `App`. BaseView is constructed in `App::new()` and placed at `widget_stack[0]`.

### WorkItemTable restored

WorkItemTable is restored as a struct with its own methods — at minimum `select_next`, `select_prev`, `toggle_multi_select`, and table rendering. BaseView delegates to it for table-related actions and rendering.

WorkItemTable does not need to implement `InteractiveWidget` — it's a child of BaseView, not a stack-level widget. BaseView routes actions to it internally.

### RenderContext slimmed

```rust
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    pub ui: &'a mut UiState,
    pub active_widget_mode: Option<ModeId>,
    pub active_widget_data: WidgetStatusData,
}
```

Child refs (`&mut TabBar`, `&mut StatusBarWidget`, etc.) are removed — BaseView accesses its children through `self`. `UiState` stays on `RenderContext` as a temporary bridge until `RepoUiState` moves onto WorkItemTable directly. `active_widget_mode` and `active_widget_data` stay because they describe the topmost widget on the stack (which could be a modal above BaseView) and the status bar needs them to display correct key hints.

### BaseView::render() does layout

BaseView::render() absorbs the layout orchestration from `ui::render()`:

1. Compute three-row layout: tab bar (1 row top), content (flexible middle), status bar (1 row bottom)
2. Render `self.tab_bar` into top chunk
3. Render content into middle chunk — either config screen (via `self.event_log`) or repo view (via `self.table` + `self.preview`)
4. Handle command palette status bar offset if active
5. Render `self.status_bar` into bottom chunk, passing `ctx.active_widget_mode` and `ctx.active_widget_data` so it shows the correct key hints for whatever modal is on top

The table rendering helpers (`render_unified_table`, row builders, provider table) move from `ui.rs` into either `WorkItemTable::render()` or shared helpers. `ui.rs` can be deleted or reduced to shared utility functions.

### BaseView::handle_action() delegates

BaseView handles two categories of actions:

**Handled directly by BaseView** (table and UI state):
- `SelectNext` / `SelectPrev` → delegates to `self.table.select_next(ctx)` in Normal mode, `self.event_log.select_next()` in Config mode
- `ToggleMultiSelect` → `self.table.toggle_multi_select(ctx)`
- `Dismiss` cascade (cancel → clear search → clear providers → clear multi-select → quit)
- `ToggleHelp` → `Outcome::Push(HelpWidget::new())`
- `OpenBranchInput` → `Outcome::Push(BranchInputWidget::new(...))`
- `OpenIssueSearch` → `Outcome::Push(IssueSearchWidget::new())`
- `OpenCommandPalette` → `Outcome::Push(CommandPaletteWidget::new())`
- `ToggleProviders`, `Quit` → `AppAction` signals
- Config-mode `Dismiss` → set `*ctx.mode = UiMode::Normal`

**Falls through to App `dispatch_action`** (needs `&mut App`):
- `Confirm` → `action_enter()` resolves intents against `&App`
- `OpenActionMenu` → `open_action_menu()` calls `intent.resolve(item, &App)` to build menu entries
- `OpenFilePicker` → `open_file_picker_from_active_repo_parent()` reads filesystem
- `Dispatch(intent)` → `dispatch_if_available()` resolves intents against `&App`
- `PrevTab` / `NextTab` / `MoveTabLeft` / `MoveTabRight` → tab navigation mutates `model.repo_order` and `model.active_repo`
- `CycleTheme`, `CycleLayout`, `CycleHost`, `ToggleDebug`, `ToggleStatusBarKeys` → `AppAction` signals (already handled, but listed for completeness)

These actions return `Outcome::Ignored` from BaseView. The bridge in `handle_key` falls through to `dispatch_action` on App. This boundary exists because `intent.resolve` and tab navigation need `&mut App` context that `WidgetContext` doesn't expose. Moving these into BaseView would require expanding WidgetContext significantly — that's a worthwhile follow-up but out of scope here.

### BaseView::handle_mouse() absorbs mouse routing

Mouse handling for the base layer moves from `run.rs` and `key_handlers.rs` into `BaseView::handle_mouse()`:

- Hit-test against `self.tab_bar` stored areas → delegate tab clicks, return `TabBarAction`
- Hit-test against `self.status_bar` stored areas → delegate status bar clicks
- Hit-test against table area → delegate to `self.table` (click to select, scroll)
- Event log filter click → delegate to `self.event_log.handle_click()`
- Everything else → `Outcome::Ignored`

**Actions that still need `&mut App`:**
- Double-click on table row → needs `action_enter()` (intent resolution). BaseView returns `AppAction::ActionEnter` (new variant) and the app processes it.
- Right-click on table row → needs `open_action_menu()`. BaseView returns `AppAction::OpenActionMenu` (already exists).
- Status bar `KeyPress` action → recursively calls `handle_key()`. BaseView returns `AppAction::StatusBarKeyPress { code, modifiers }` (new variant) and the app calls `handle_key`.
- Tab switching → mutates `model.repo_order`, `model.active_repo`, `ui.mode`, `ui.drag`. These are `AppAction::SwitchToTab(i)`, `AppAction::SwitchToConfig`, `AppAction::TabDragSwap { from, to }`, etc. (new variants).
- Tab drop → calls `config.save_tab_order()`. `AppAction::SaveTabOrder`.

The `run.rs` tab click/drag handling becomes App-level `AppAction` processing rather than inline code.

### WidgetContext additions

To support BaseView handling more actions, `WidgetContext` gains:

```rust
pub drag: &'a mut DragState,  // for tab drag state
```

Tab reordering during drag requires mutable access to `model.repo_order` and `model.active_repo`. These are handled via `AppAction` rather than direct mutation, keeping WidgetContext's model access read-only.

### check_infinite_scroll interaction

The post-dispatch `check_infinite_scroll()` hook runs on App after `SelectNext`/`SelectPrev`. It reads `self.ui.repo_ui` (which was just mutated by BaseView through WidgetContext) and mutates `self.model.repos` (to set `issue_fetch_pending`). This is safe because the `mem::take` pattern restores the widget stack before `check_infinite_scroll` runs, and `repo_ui` changes from WidgetContext are already written back to `self.ui.repo_ui` at that point.

### run.rs simplification

After BaseView absorbs mouse routing:
- Tab click/drag handling moves from inline `run.rs` code to `AppAction` processing
- Event log filter click moves into BaseView
- `render_frame()` stays as the `mem::take` pattern for rendering through the widget stack
- `run.rs` retains: event loop, scroll coalescing, Ctrl-Z, async refresh dispatch, command processing, `AppAction` processing for mouse-originated actions

### External updates via Arc (future pattern)

If a BaseView child ever needs poking from outside (e.g., a future dashboard widget receiving live data), the child's state can be promoted to `Arc<Mutex<T>>` shared between BaseView and the external source. Immediate-mode full redraw means the next frame picks up the update naturally. Nothing needs this today.

## Staging (this PR)

This is a single focused refactor:

1. Restore `WorkItemTable` struct with table methods and rendering
2. Make BaseView own all children (TabBar, StatusBar, WorkItemTable, PreviewPanel, EventLog)
3. Remove children from App
4. Absorb `ui::render()` into `BaseView::render()` with layout orchestration
5. Move mouse routing from `run.rs` and `key_handlers.rs` into `BaseView::handle_mouse()`, using `AppAction` for operations needing `&mut App`
6. Expand BaseView action handling (Config mode navigation, more modal opens)
7. Slim RenderContext (remove child refs, keep ui/widget mode)
8. Delete or reduce `ui.rs`

## Future: Per-tab content (Stage 2)

A natural evolution where BaseView becomes Screen, and the content area becomes a tab-switched container:

```
Screen
  ├── TabBar
  ├── StatusBar
  └── TabContent (switches on active tab)
        Flotilla → OverviewPage (providers, hosts, event log)
        Repo(i)  → RepoPage (WorkItemTable, PreviewPanel)
        Future   → AgentPage, DashboardPage, ...
```

This depends on the tab system maturing and is out of scope for this PR.
