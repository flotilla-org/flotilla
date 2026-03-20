# BaseView Containment Fix Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make BaseView own its children (TabBar, StatusBar, WorkItemTable, PreviewPanel, EventLog), slim RenderContext, and absorb ui::render() — fixing the hollow-shell problem.

**Architecture:** Move child widget fields from App into BaseView. BaseView::render() does layout orchestration directly. WorkItemTable is restored as a component. RenderContext drops child refs. Mouse routing moves from run.rs/key_handlers.rs into BaseView.

**Tech Stack:** Rust, ratatui, crossterm

**Spec:** `docs/superpowers/specs/2026-03-19-baseview-containment-design.md`

---

## Task 1: Restore WorkItemTable as a component

**Files:**
- Create: `crates/flotilla-tui/src/widgets/work_item_table.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — add module

Extract table-specific logic from BaseView into WorkItemTable. It does NOT implement `InteractiveWidget` — it's a child of BaseView with direct methods.

- [ ] **Step 1: Create WorkItemTable struct**

```rust
pub struct WorkItemTable;

impl WorkItemTable {
    pub fn new() -> Self { Self }

    pub fn select_next(&self, ctx: &mut WidgetContext) { ... }
    pub fn select_prev(&self, ctx: &mut WidgetContext) { ... }
    pub fn toggle_multi_select(&self, ctx: &mut WidgetContext) { ... }
}
```

Move the `select_next`, `select_prev`, `toggle_multi_select` method bodies from `BaseView` into WorkItemTable. Find these as static methods on BaseView in `base_view.rs`.

- [ ] **Step 2: Move table rendering**

Move `render_unified_table`, `build_header_row`, `build_item_row`, `render_repo_providers`, and the provider helper functions from `ui.rs` into `work_item_table.rs`. Find these by searching for the function names in `ui.rs`. Add a `render` method on WorkItemTable:

```rust
pub fn render(&self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) { ... }
```

The existing BaseView tests serve as the regression suite — running them after this move verifies identical behavior.

- [ ] **Step 3: Update BaseView to delegate to self.table**

Add `table: WorkItemTable` field to BaseView. Change BaseView's `select_next`, `select_prev`, `toggle_multi_select` calls to delegate to `self.table`.

- [ ] **Step 4: Register module, verify compilation**

Add `pub mod work_item_table;` to `widgets/mod.rs`.

Run: `cargo build -p flotilla-tui --locked`

- [ ] **Step 5: Run tests and commit**

Run: `cargo test -p flotilla-tui --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

```
refactor: restore WorkItemTable as BaseView child component
```

---

## Task 2: Move children from App into BaseView

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — add child fields
- Modify: `crates/flotilla-tui/src/app/mod.rs` — remove child fields from App
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — slim RenderContext
- Modify: `crates/flotilla-tui/src/run.rs` — update render_frame
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — update handle_mouse, dispatch_action

This is the core containment change. BaseView gains fields:

```rust
pub struct BaseView {
    pub tab_bar: TabBar,
    pub status_bar: StatusBarWidget,
    pub table: WorkItemTable,
    pub preview: PreviewPanel,
    pub event_log: EventLogWidget,
}
```

- [ ] **Step 1: Add fields to BaseView, remove from App**

In `base_view.rs`, add the five child fields. Update `BaseView::new()` to construct them.

In `app/mod.rs`, remove `tab_bar`, `status_bar_widget`, `event_log_widget`, `preview_panel` from the `App` struct and `App::new()`.

Update `App::new()` to pass child widgets into `BaseView::new(...)` when constructing widget_stack[0].

- [ ] **Step 2: Slim RenderContext**

In `widgets/mod.rs`, remove the child refs from `RenderContext`:

```rust
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub ui: &'a mut UiState,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    pub active_widget_mode: Option<ModeId>,
    pub active_widget_data: WidgetStatusData,
}
```

Remove: `tab_bar`, `status_bar_widget`, `event_log_widget`, `preview_panel`.

- [ ] **Step 3: Update render_frame in run.rs**

`render_frame` no longer needs to pass child refs into `RenderContext`. Update the context construction to drop the child fields.

- [ ] **Step 4: Update BaseView::render() to use self instead of ctx**

BaseView::render() currently delegates to `ui::render()` passing children from ctx. Change it to pass `&mut self.tab_bar`, `&mut self.status_bar`, etc. from self.

For now, keep calling `ui::render()` but pass children from self instead of from ctx. This is a bridge step.

- [ ] **Step 5: Update handle_mouse and dispatch_action in key_handlers.rs**

`handle_status_bar_mouse` currently accesses `self.status_bar_widget`. After the move, the status bar lives on BaseView. Use the `mem::take` pattern (same as `handle_key`) to avoid borrow conflicts:

```rust
fn handle_status_bar_mouse(&mut self, mouse: MouseEvent) -> bool {
    let mut stack = std::mem::take(&mut self.widget_stack);
    let base = stack[0].as_any_mut()
        .downcast_mut::<crate::widgets::base_view::BaseView>()
        .expect("widget_stack[0] is always BaseView");
    let result = base.status_bar.handle_click(mouse.column, mouse.row);
    // ... process result ...
    self.widget_stack = stack;
    result_bool
}
```

Similarly for `dispatch_action` accessing event_log_widget for Config-mode navigation — use `mem::take` then downcast.

**Note:** These downcasts are temporary bridge code. Task 4 moves mouse routing into BaseView::handle_mouse(), and Task 5 moves Config-mode navigation into BaseView::handle_action(), eliminating all downcasts.

- [ ] **Step 6: Update run.rs mouse handling**

Tab click/drag handling in `run.rs` accesses `app.tab_bar` and `app.event_log_widget`. Use the same `mem::take` + downcast pattern:

```rust
let mut stack = std::mem::take(&mut app.widget_stack);
let base = stack[0].as_any_mut()
    .downcast_mut::<crate::widgets::base_view::BaseView>()
    .expect("base view");
// ... use base.tab_bar, base.event_log ...
app.widget_stack = stack;
```

- [ ] **Step 7: Update test harness**

`TestHarness` in `tests/support/mod.rs` constructs `tab_bar`, `status_bar_widget`, etc. as App fields. Update to construct them inside BaseView instead.

Tests in `key_handlers.rs` that access `app.event_log_widget` or `app.status_bar_widget` need updating to downcast through the stack.

- [ ] **Step 8: Verify AppAction processing**

Check `process_app_actions` in `app/mod.rs`. Some `AppAction` variants like `ToggleProviders` and `ToggleMultiSelect` also exist as direct actions in BaseView::handle_action. Verify there's no double-toggle — BaseView should handle these directly and NOT also push an AppAction. If both paths exist, remove the AppAction path since BaseView handles it.

- [ ] **Step 9: Run tests and commit**

Run: `cargo +nightly-2026-03-12 fmt && cargo test -p flotilla-tui --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

```
refactor: move child widgets from App into BaseView
```

---

## Task 3: Absorb ui::render() into BaseView::render()

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — inline layout orchestration
- Modify: `crates/flotilla-tui/src/ui.rs` — remove render() and render_content()
- Modify: `crates/flotilla-tui/src/widgets/work_item_table.rs` — may need additional render helpers

Move the layout computation and rendering orchestration from `ui::render()` directly into `BaseView::render()`.

- [ ] **Step 1: Inline ui::render() into BaseView::render()**

The current `ui::render()` does:
1. Three-row layout: tab bar, content, status bar
2. Render tab bar
3. Render content (config screen or repo view)
4. Status bar offset for command palette
5. Render status bar
6. Render command palette overlay (from UiMode — but this is now on the widget stack)

Move this logic into `BaseView::render()`, replacing the delegation to `ui::render()`. Use `self.tab_bar`, `self.status_bar`, `self.table`, `self.preview`, `self.event_log` directly.

- [ ] **Step 2: Move render_content() into BaseView**

`render_content()` switches between config screen and repo view:
- Config mode → `self.event_log.render_config_screen(...)`
- Normal mode → split between table and preview, then `self.table.render(...)` and `self.preview.render(...)`

Move this as a private method on BaseView.

- [ ] **Step 3: Clean up ui.rs**

After moving render() and render_content(), check what remains in ui.rs. If only utility functions remain (like `active_rui`, `selected_work_item`), either move them to appropriate locations or keep ui.rs as a slim helpers module.

If ui.rs is empty or near-empty, delete it and remove `pub mod ui;` from `lib.rs`.

- [ ] **Step 4: Verify overlay rendering handled by widget stack**

Confirm that overlay rendering (command palette, branch input popup, file picker) is handled by the widget stack, not by `ui::render()`. In the current code, `ui::render()` no longer calls `render_command_palette()` etc. — these were already removed when the widgets' `render()` methods were implemented. No removal should be needed. If any calls remain, remove them.

Note: BaseView::render() will need to destructure `RenderContext` to pass individual params to `self.table.render(...)` and `self.preview.render(...)`, since those methods take raw params rather than a context struct.

- [ ] **Step 5: Run all tests including snapshots**

Run: `cargo +nightly-2026-03-12 fmt && cargo test -p flotilla-tui --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

Snapshot tests must pass without changes (rendering output should be identical).

```
refactor: absorb ui::render() into BaseView, remove ui.rs orchestration
```

---

## Task 4: Move mouse routing into BaseView

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — implement handle_mouse
- Modify: `crates/flotilla-tui/src/run.rs` — remove inline tab click/drag handling
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — remove handle_status_bar_mouse, simplify handle_mouse
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — add new AppAction variants

Move mouse handling from `run.rs` and `key_handlers.rs` into `BaseView::handle_mouse()`.

- [ ] **Step 1: Add BaseView mouse state and new AppAction variants**

Add mouse-related state to BaseView:
```rust
pub struct BaseView {
    // ... existing children ...
    table_area: Rect,           // stored from last render, for click hit-testing
    double_click: DoubleClickState,  // move from UiState
    drag: DragState,            // move from UiState
}
```

Move `DoubleClickState` and `DragState` from `UiState` into `BaseView` since they're exclusively used by the base layer.

Move `row_at_mouse` from `App` (navigation.rs) into `WorkItemTable` or BaseView, adapted to use the stored `table_area` and `WidgetContext` for repo UI state.

Add to `AppAction` enum:
```rust
ActionEnter,                    // double-click on table row
StatusBarKeyPress { code: KeyCode, modifiers: KeyModifiers },
SwitchToConfig,
SwitchToRepo(usize),
StartTabDrag { repo_index: usize, start_x: u16 },
TabDragMove { column: u16, row: u16 },
TabDragDrop,
SaveTabOrder,
OpenFilePicker,                 // tab bar [+] click
```

- [ ] **Step 2: Implement BaseView::handle_mouse()**

BaseView checks hits in order:
1. `self.tab_bar` — click/drag areas → return `Consumed` + push AppAction
2. `self.event_log` — filter area click → return `Consumed`
3. `self.status_bar` — click targets → return `Consumed` + push AppAction for KeyPress
4. Table area — click to select, double-click → `Consumed` + AppAction::ActionEnter, right-click → `Consumed` + AppAction::OpenActionMenu, scroll → `Consumed`
5. Nothing hit → `Ignored`

Read the current mouse handling in `run.rs` (lines 115-174) and `key_handlers.rs` (lines 187-270) to understand the full logic.

- [ ] **Step 3: Process new AppActions in App**

In `app/mod.rs` `process_app_actions()`, handle the new variants:
- `ActionEnter` → call `self.action_enter()`
- `StatusBarKeyPress` → call `self.handle_key(KeyEvent::new(code, modifiers))`
- `SwitchToConfig` → `self.dismiss_modals(); self.ui.mode = UiMode::Config;`
- `SwitchToRepo(i)` → `self.dismiss_modals(); self.switch_tab(i);`
- `StartTabDrag` / `TabDragMove` / `TabDragDrop` / `SaveTabOrder` → tab drag state management

- [ ] **Step 4: Simplify run.rs**

Remove the inline tab click/drag/drop handling from `run.rs`. All mouse events go through `app.handle_mouse(m)`. The event log filter click bypass is also removed (BaseView handles it).

```rust
Event::Mouse(m) => {
    app.handle_mouse(m);
}
```

Coalesced scroll still applies — synthetic scroll events are dispatched normally.

- [ ] **Step 5: Simplify handle_mouse in key_handlers.rs**

Remove `handle_status_bar_mouse` and `dispatch_status_bar_action`. The legacy table/gear-icon mouse handling is now in BaseView. handle_mouse becomes just the widget stack dispatch with modal barrier, plus the post-dispatch AppAction processing.

After processing mouse events through the widget stack, call `check_infinite_scroll()` when the action involved selection changes (scroll events). Currently, mouse scroll bypasses `check_infinite_scroll` — this is a pre-existing gap that we fix here. Either check unconditionally after mouse dispatch (cheap — it just reads selection position) or add a selection-changed flag.

- [ ] **Step 6: Write tests for BaseView mouse handling**

Add tests verifying BaseView::handle_mouse returns correct AppAction variants:
- Click on table row → Consumed (selection updated)
- Double-click on table row → Consumed + AppAction::ActionEnter
- Right-click on table row → Consumed + AppAction::OpenActionMenu
- Scroll → Consumed (selection changes)
- Click on status bar target → Consumed + AppAction::StatusBarKeyPress
- Click on tab → Consumed + AppAction::SwitchToRepo/SwitchToConfig
- Click outside all areas → Ignored

- [ ] **Step 7: Run all tests**

Run: `cargo +nightly-2026-03-12 fmt && cargo test -p flotilla-tui --locked && cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

```
refactor: move mouse routing into BaseView::handle_mouse()
```

---

## Task 5: Expand BaseView action handling

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — handle Config mode
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — slim dispatch_action

Move Config-mode navigation and dismiss into BaseView, reducing the legacy fallback.

- [ ] **Step 1: Handle Config-mode actions in BaseView**

BaseView::handle_action currently returns `Ignored` for everything when mode is not Normal. Change it to also handle Config mode:

- `SelectNext` in Config → `self.event_log.select_next()`
- `SelectPrev` in Config → `self.event_log.select_prev()`
- `Dismiss` in Config → `*ctx.mode = UiMode::Normal`
- Tab switching in Config → `Outcome::Ignored` (still needs App)

- [ ] **Step 2: Slim dispatch_action**

Remove `SelectNext`/`SelectPrev` Config-mode handling and Config-mode `Dismiss` from `dispatch_action` in key_handlers.rs. These are now handled by BaseView.

- [ ] **Step 3: Run tests and commit**

```
refactor: expand BaseView to handle Config mode navigation
```

---

## Task 6: Clean up dead code

**Files:**
- Modify: various — remove dead functions, unused imports
- Possibly delete: `crates/flotilla-tui/src/ui.rs` if empty

- [ ] **Step 1: Check what remains in ui.rs**

If `render()` and `render_content()` were absorbed, check if anything else remains. Utility functions like `active_rui`, `selected_work_item` may have moved. If ui.rs is empty or only has shared helpers, either consolidate into `ui_helpers.rs` or delete.

- [ ] **Step 2: Remove dead functions in navigation.rs**

Check if any functions are only used by tests. Remove `#[allow(dead_code)]` annotations and let the compiler flag truly dead code.

- [ ] **Step 3: Remove dead functions in key_handlers.rs**

After mouse routing and Config-mode handling moved to BaseView, check what's left in `dispatch_action` and whether any private methods are now dead.

- [ ] **Step 4: Clean up unused imports across all modified files**

- [ ] **Step 5: Final verification**

Run: `cargo +nightly-2026-03-12 fmt && cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

All snapshot tests must pass without changes.

```
refactor: remove dead code from BaseView containment fix
```
