# Child Widget Trait Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make BaseView's children implement `InteractiveWidget` uniformly, classify actions as global vs focus-routed, and change BaseView to a two-phase focus router.

**Architecture:** Add `Action::is_global()` for pre-dispatch. Children implement `InteractiveWidget` with uniform render/handle signatures. BaseView delegates to focused child first, handles cross-cutting concerns (Dismiss, Quit) on Ignored. Mouse routing delegates to hit-tested children.

**Tech Stack:** Rust, ratatui, crossterm

**Spec:** `docs/superpowers/specs/2026-03-19-child-widget-trait-design.md`

---

## Task 1: Add global action classification and pre-dispatch

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs` — add `is_global()` to Action
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — add `handle_global_action`, pre-dispatch in `handle_key`
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — remove global action handling

Move global actions out of the widget stack path into App-level pre-dispatch.

- [ ] **Step 1: Add `is_global()` to Action**

In `crates/flotilla-tui/src/keymap.rs`, add to `impl Action`:
```rust
pub fn is_global(&self) -> bool {
    matches!(self,
        Action::PrevTab | Action::NextTab |
        Action::MoveTabLeft | Action::MoveTabRight |
        Action::CycleTheme | Action::CycleLayout | Action::CycleHost |
        Action::ToggleDebug | Action::ToggleStatusBarKeys |
        Action::Refresh
    )
}
```

- [ ] **Step 2: Add `handle_global_action` on App**

In `key_handlers.rs`, add a method that absorbs global action handling from both `dispatch_action` and `BaseView::handle_action`:

```rust
fn handle_global_action(&mut self, action: Action) {
    match action {
        Action::PrevTab => self.prev_tab(),
        Action::NextTab => self.next_tab(),
        Action::MoveTabLeft => { if !self.ui.mode.is_config() && self.move_tab(-1) { self.config.save_tab_order(&self.persisted_tab_order_paths()); } }
        Action::MoveTabRight => { if !self.ui.mode.is_config() && self.move_tab(1) { self.config.save_tab_order(&self.persisted_tab_order_paths()); } }
        Action::Refresh => { /* move refresh command construction from BaseView */ }
        // CycleTheme, CycleLayout, CycleHost, ToggleDebug, ToggleStatusBarKeys
        // are already handled via AppAction — just process them directly here
        _ => {}
    }
}
```

Read `BaseView::handle_action` and `dispatch_action` to find all global action handling and consolidate into this method.

- [ ] **Step 3: Pre-dispatch globals in `handle_key`**

In `handle_key`, after resolving the action but before the widget stack dispatch loop, add:

```rust
if let Some(action) = action {
    if action.is_global() {
        self.handle_global_action(action);
        return;
    }
}
```

- [ ] **Step 4: Remove global handling from BaseView and dispatch_action**

Remove `CycleTheme`, `CycleLayout`, `CycleHost`, `ToggleDebug`, `ToggleStatusBarKeys`, `Refresh` handling from `BaseView::handle_action`. Remove tab navigation from `dispatch_action`. These now return `Ignored` from BaseView (or never reach it).

- [ ] **Step 5: Verify and commit**

Run: `cargo +nightly-2026-03-12 fmt && cargo test -p flotilla-tui --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

```
refactor: classify global actions and pre-dispatch before widget stack
```

---

## Task 2: Implement InteractiveWidget on WorkItemTable

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/work_item_table.rs` — implement trait
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — delegate to self.table

WorkItemTable is the primary focused child for Normal mode. It handles selection, multi-select, modal opening, and providers toggle.

- [ ] **Step 1: Implement InteractiveWidget on WorkItemTable**

Add `impl InteractiveWidget for WorkItemTable`. Move action handling from BaseView into WorkItemTable::handle_action:

- `SelectNext` → `self.select_next(ctx); Outcome::Consumed`
- `SelectPrev` → `self.select_prev(ctx); Outcome::Consumed`
- `ToggleMultiSelect` → `self.toggle_multi_select(ctx); Outcome::Consumed`
- `ToggleProviders` → toggle `show_providers` on active repo UI → `Outcome::Consumed`
- `ToggleHelp` → `Outcome::Push(Box::new(HelpWidget::new()))`
- `OpenBranchInput` → push BranchInputWidget + set UiMode bridge → `Outcome::Push(...)`
- `OpenIssueSearch` → push IssueSearchWidget + set UiMode bridge → `Outcome::Push(...)`
- `OpenCommandPalette` → push CommandPaletteWidget + set UiMode bridge → `Outcome::Push(...)`
- Everything else → `Outcome::Ignored`

`handle_mouse`: Move table click/scroll from BaseView. Left click selects row, right click selects row (AppAction::OpenActionMenu pushed by BaseView after), scroll calls select_next/prev. Note: double-click detection stays on BaseView.

`render`: Already implemented — update signature to match `InteractiveWidget::render(&mut self, frame, area, &mut RenderContext)`. Access model/ui/theme from `ctx` instead of individual params.

`mode_id`: `ModeId::Normal`

`as_any` / `as_any_mut`: Implement (required by trait).

- [ ] **Step 2: Update BaseView to delegate to self.table**

In BaseView::handle_action, replace the inlined table action handling with delegation:
```rust
// Phase 1: delegate to focused child
let outcome = match self.active_child(ctx) {
    ActiveChild::Table => self.table.handle_action(action, ctx),
    ActiveChild::EventLog => self.event_log.handle_action(action, ctx),
};
if !matches!(outcome, Outcome::Ignored) { return outcome; }

// Phase 2: cross-cutting (Dismiss cascade, Quit)
```

- [ ] **Step 3: Verify and commit**

```
refactor: implement InteractiveWidget on WorkItemTable
```

---

## Task 3: Implement InteractiveWidget on EventLogWidget

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/event_log.rs` — implement trait
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — remove event log action handling

EventLogWidget is the focused child for Config mode.

- [ ] **Step 1: Implement InteractiveWidget on EventLogWidget**

`handle_action`:
- `SelectNext` → `self.select_next(); Outcome::Consumed`
- `SelectPrev` → `self.select_prev(); Outcome::Consumed`
- Everything else → `Outcome::Ignored`

Note: `Dismiss` (return to Normal from Config) stays on BaseView — it's a mode transition, not an event log concern.

`handle_mouse`: Move filter click handling into `handle_mouse`. Currently `handle_click(x, y) -> bool` — wrap it to return `Outcome`.

`render`: Update signature to `InteractiveWidget::render`. The config screen rendering (`render_config_screen`) becomes the `render` implementation.

`mode_id`: `ModeId::Config`

- [ ] **Step 2: Update BaseView delegation**

BaseView's Config-mode `SelectNext`/`SelectPrev` handling is replaced by the focused-child delegation (which routes to EventLog in Config mode).

- [ ] **Step 3: Verify and commit**

```
refactor: implement InteractiveWidget on EventLogWidget
```

---

## Task 4: Implement InteractiveWidget on TabBar, StatusBarWidget, PreviewPanel

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/tab_bar.rs`
- Modify: `crates/flotilla-tui/src/widgets/status_bar_widget.rs`
- Modify: `crates/flotilla-tui/src/widgets/preview_panel.rs`

These are simpler — mostly rendering + mouse, no keyboard actions.

- [ ] **Step 1: Implement InteractiveWidget on TabBar**

`handle_action`: returns `Ignored` for everything (tab actions are global).
`handle_mouse`: wrap existing `handle_click` to return Outcome + AppAction. Move drag initiation logic.
`render`: update signature. BaseView sets `self.tab_bar.drag_active = self.drag.active` before calling render.
`mode_id`: `ModeId::Normal`

- [ ] **Step 2: Implement InteractiveWidget on StatusBarWidget**

`handle_action`: returns `Ignored`.
`handle_mouse`: wrap existing `handle_click` to return Outcome + AppAction.
`render`: update signature. Needs `active_widget_mode` and `active_widget_data` from RenderContext.
`mode_id`: `ModeId::Normal`

- [ ] **Step 3: Implement InteractiveWidget on PreviewPanel**

`handle_action`: returns `Ignored`.
`handle_mouse`: returns `Ignored`.
`render`: update signature.
`mode_id`: `ModeId::Normal`

- [ ] **Step 4: Update BaseView mouse routing to delegate to children**

Replace BaseView's inlined mouse handling with delegation:
```rust
if in_tab_bar_area { return self.tab_bar.handle_mouse(mouse, ctx); }
if in_status_bar_area { return self.status_bar.handle_mouse(mouse, ctx); }
if in_content_area { return self.table.handle_mouse(mouse, ctx); /* or event_log */ }
```

Keep double-click detection and drag state management on BaseView.

- [ ] **Step 5: Update BaseView::render to call children via trait**

Replace bespoke render calls with uniform `InteractiveWidget::render` calls on each child, passing the appropriate sub-area.

- [ ] **Step 6: Verify and commit**

```
refactor: implement InteractiveWidget on TabBar, StatusBar, PreviewPanel
```

---

## Task 5: Clean up BaseView and dispatch_action

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/base_view.rs` — slim to focus router + cross-cutting
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — slim dispatch_action

- [ ] **Step 1: Slim BaseView::handle_action**

After tasks 2-4, BaseView should only handle:
- Two-phase delegation (focused child first)
- Cross-cutting: Dismiss cascade, Quit
- Fallthrough: Confirm, OpenActionMenu, OpenFilePicker, Dispatch(intent) → Ignored

Remove all action handling that moved to children.

- [ ] **Step 2: Slim dispatch_action**

After global pre-dispatch (Task 1) and child widget handling (Tasks 2-4), dispatch_action should only contain:
- `Confirm` → `self.action_enter()`
- `OpenActionMenu` → `self.open_action_menu()`
- `OpenFilePicker` → `self.open_file_picker_from_active_repo_parent()`
- `Dispatch(intent)` → `self.dispatch_if_available(intent)`
- Catch-all no-op for anything else

- [ ] **Step 3: Remove dead code**

Check for unused imports, dead functions, stale comments across all modified files.

- [ ] **Step 4: Verify all tests**

Run: `cargo +nightly-2026-03-12 fmt && cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`

All snapshot tests must pass unchanged.

```
refactor: slim BaseView and dispatch_action after child widget migration
```
