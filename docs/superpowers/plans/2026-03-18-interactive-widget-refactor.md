# Interactive Widget Refactor Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the monolithic `key_handlers.rs` and `ui.rs` with self-contained `InteractiveWidget` implementations that own their state, input handling, and rendering.

**Architecture:** A focus stack of widgets on `App`, routed top-down for input and bottom-up for rendering. Each widget implements the `InteractiveWidget` trait with `handle_action`, `handle_raw_key`, `handle_mouse`, and `render`. A `WidgetContext` struct provides state access; `Outcome` enum controls stack transitions.

**Tech Stack:** Rust, ratatui, crossterm, tui_input

**Spec:** `docs/superpowers/specs/2026-03-18-interactive-widget-refactor-design.md`

---

## Task 1: Introduce the `InteractiveWidget` trait and supporting types

**Files:**
- Create: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/lib.rs` — add `pub mod widgets;`

This task defines the trait, `Outcome`, `WidgetContext`, and `RenderContext`. No behavioural changes yet.

- [ ] **Step 1: Create `widgets/mod.rs` with trait and types**

```rust
// crates/flotilla-tui/src/widgets/mod.rs

use std::collections::HashMap;

use crossterm::event::{KeyEvent, MouseEvent};
use flotilla_protocol::HostName;
use ratatui::{layout::Rect, Frame};

use flotilla_protocol::{HostName, RepoIdentity};
use ratatui::{layout::Rect, Frame};

use crate::{
    app::{CommandQueue, InFlightCommand, RepoUiState, TuiModel},
    keymap::{Action, Keymap, ModeId},
    theme::Theme,
};
use flotilla_core::config::ConfigStore;

/// Result of handling an action or key event.
pub enum Outcome {
    /// Event handled, nothing else to do.
    Consumed,
    /// Event not handled — try the next widget down the stack.
    Ignored,
    /// This widget is done, pop it from the stack.
    Finished,
    /// Push a new widget on top of this one.
    Push(Box<dyn InteractiveWidget>),
    /// Pop this widget and push a replacement.
    Swap(Box<dyn InteractiveWidget>),
}

/// Read/write access to app state for widget handlers.
pub struct WidgetContext<'a> {
    // Read access
    pub model: &'a TuiModel,
    pub keymap: &'a Keymap,
    pub config: &'a ConfigStore,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    pub target_host: Option<&'a HostName>,
    pub active_repo: usize,
    pub repo_order: &'a [RepoIdentity],

    // Write access
    pub commands: &'a mut CommandQueue,
    pub repo_ui: &'a mut HashMap<RepoIdentity, RepoUiState>,

    // Signals
    pub should_quit: bool,
    pub pending_cancel: Option<u64>,
}

/// Read-only context for rendering.
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
}

/// Every interactive UI element implements this trait.
pub trait InteractiveWidget {
    /// Handle a resolved Action (from the keymap).
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome;

    /// Handle a raw key that wasn't resolved to an Action.
    fn handle_raw_key(&mut self, _key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    /// Handle mouse events.
    fn handle_mouse(&mut self, _mouse: MouseEvent, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    /// Render into the given area. Takes `&mut self` so widgets can store
    /// layout metadata (click targets) during rendering.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext);

    /// Which keymap mode applies to this widget.
    fn mode_id(&self) -> ModeId;

    /// Whether this widget captures all raw keys (skipping keymap resolution).
    /// Only Esc and Enter are still resolved when true.
    fn captures_raw_keys(&self) -> bool {
        false
    }
}
```

- [ ] **Step 2: Register the module**

Add `pub mod widgets;` to `crates/flotilla-tui/src/lib.rs`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-tui --locked`
Expected: compiles with no errors (the module is defined but nothing uses it yet).

- [ ] **Step 4: Commit**

```
feat: introduce InteractiveWidget trait and supporting types
```

---

## Task 2: Add widget test helpers

**Files:**
- Modify: `crates/flotilla-tui/src/app/test_support.rs` — add `test_widget_context` helper
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — add test support re-export

Test helpers for constructing `WidgetContext` in isolation, so widget tests don't need a full `App`.

- [ ] **Step 1: Add `test_widget_context` helper to `test_support.rs`**

Add a function that builds a `WidgetContext` from a `stub_app()`, suitable for widget unit tests. Since `WidgetContext` borrows fields from different owners, the simplest approach is a helper struct that owns the data and can produce a context by reference:

```rust
/// Test harness that owns app state and can produce a `WidgetContext`.
pub struct TestWidgetHarness {
    pub model: TuiModel,
    pub keymap: Keymap,
    pub config: Arc<ConfigStore>,
    pub in_flight: HashMap<u64, InFlightCommand>,
    pub commands: CommandQueue,
    pub repo_ui: HashMap<RepoIdentity, RepoUiState>,
    pub target_host: Option<HostName>,
}

impl TestWidgetHarness {
    pub fn new() -> Self {
        let app = stub_app();
        let repo_ui = app.ui.repo_ui;
        Self {
            model: app.model,
            keymap: app.keymap,
            config: app.config,
            in_flight: app.in_flight,
            commands: CommandQueue::default(),
            repo_ui,
            target_host: None,
        }
    }

    pub fn ctx(&mut self) -> WidgetContext<'_> {
        WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &*self.config,
            in_flight: &self.in_flight,
            target_host: self.target_host.as_ref(),
            active_repo: self.model.active_repo,
            repo_order: &self.model.repo_order,
            commands: &mut self.commands,
            repo_ui: &mut self.repo_ui,
            should_quit: false,
            pending_cancel: None,
        }
    }
}
```

- [ ] **Step 2: Write a test that constructs the harness**

```rust
#[test]
fn test_widget_harness_builds_context() {
    let mut harness = TestWidgetHarness::new();
    let ctx = harness.ctx();
    assert!(!ctx.should_quit);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-tui --locked test_widget_harness`
Expected: PASS

- [ ] **Step 4: Commit**

```
feat: add TestWidgetHarness for widget unit tests
```

---

## Task 3: Extract `HelpWidget`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/help.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — add `mod help; pub use help::HelpWidget;`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — remove Help handling from `dispatch_action`
- Modify: `crates/flotilla-tui/src/ui.rs` — remove `render_help`
- Modify: `crates/flotilla-tui/src/app/mod.rs` — add widget stack field, bridge logic

This is the simplest modal: scroll state, SelectNext/Prev/Dismiss/ToggleHelp handling, one render function. It proves the trait, stack mechanism, and bridge work.

- [ ] **Step 1: Write failing tests for HelpWidget**

Create `crates/flotilla-tui/src/widgets/help.rs` with tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::TestWidgetHarness;
    use crate::keymap::{Action, ModeId};
    use crate::widgets::Outcome;

    #[test]
    fn mode_id_is_help() {
        let widget = HelpWidget::new();
        assert_eq!(widget.mode_id(), ModeId::Help);
    }

    #[test]
    fn select_next_increments_scroll() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::SelectNext, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 1);
    }

    #[test]
    fn select_prev_decrements_scroll() {
        let mut widget = HelpWidget::new();
        widget.scroll = 5;
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::SelectPrev, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 4);
    }

    #[test]
    fn select_prev_at_zero_stays() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::SelectPrev, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 0);
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::Dismiss, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn toggle_help_returns_finished() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::ToggleHelp, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let outcome = widget.handle_action(Action::Refresh, &mut harness.ctx());
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked help`
Expected: compilation errors — `HelpWidget` not defined yet.

- [ ] **Step 3: Implement `HelpWidget` struct and `InteractiveWidget`**

```rust
use ratatui::{layout::Rect, Frame};

use crate::{
    keymap::{Action, ModeId},
    widgets::{InteractiveWidget, Outcome, RenderContext, WidgetContext},
};

pub struct HelpWidget {
    pub scroll: u16,
}

impl HelpWidget {
    pub fn new() -> Self {
        Self { scroll: 0 }
    }
}

impl InteractiveWidget for HelpWidget {
    fn handle_action(&mut self, action: Action, _ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.scroll = self.scroll.saturating_add(1);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Dismiss | Action::ToggleHelp => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext) {
        // Move render_help logic here from ui.rs.
        // Copy the body of render_help(), replacing:
        //   - ui.help_scroll → self.scroll
        //   - keymap parameter → ctx.keymap
        //   - theme parameter → ctx.theme
        //   - Remove the UiMode::Help guard (we only render when on the stack)
        todo!("move render_help body here")
    }

    fn mode_id(&self) -> ModeId {
        ModeId::Help
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --locked help`
Expected: all 7 HelpWidget tests pass.

- [ ] **Step 5: Move `render_help` body into `HelpWidget::render`**

Copy the body of `render_help` from `ui.rs` (lines 1168-1228) into `HelpWidget::render`, adapting references:
- `ui.help_scroll` → `self.scroll`
- `keymap` param → `ctx.keymap`
- `theme` param → `ctx.theme`
- Remove the `if !matches!(ui.mode, UiMode::Help)` guard
- Clamp logic for scroll stays, using `self.scroll` directly

- [ ] **Step 6: Add widget stack to `App` and bridge logic**

In `crates/flotilla-tui/src/app/mod.rs`, add to the `App` struct:

```rust
pub widget_stack: Vec<Box<dyn crate::widgets::InteractiveWidget>>,
```

Initialise as empty `vec![]` in `App::new()`.

In `App::handle_key` (in `key_handlers.rs`), add widget stack dispatch **before** the existing logic.

**Borrow checker pattern:** The widget stack and the context both need mutable borrows from `App`. To avoid conflicting borrows, we `std::mem::take` the widget stack out of `self` before building the context, iterate over the taken stack, then put it back. This is the central mechanism — every call site that dispatches through the stack must use this pattern.

```rust
pub fn handle_key(&mut self, key: KeyEvent) {
    // ── Widget stack dispatch (bridge) ──
    if !self.widget_stack.is_empty() {
        // Step 1: Peek top widget's mode_id and captures_raw_keys BEFORE taking the stack
        let captures_raw = self.widget_stack.last().expect("checked non-empty").captures_raw_keys();
        let mode_id = self.widget_stack.last().expect("checked non-empty").mode_id();

        // Step 2: Resolve action via keymap (borrows self.keymap only)
        let action = if captures_raw {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.resolve_action(key),
                _ => None,
            }
        } else {
            self.keymap.resolve(mode_id, crokey::KeyCombination::from(key))
        };

        // Step 3: Take stack out of self to avoid borrow conflicts
        let mut stack = std::mem::take(&mut self.widget_stack);

        // Step 4: Build context from remaining self fields
        let mut ctx = self.build_widget_context();

        // Step 5: Iterate over taken stack
        let mut outcome_action: Option<(usize, Outcome)> = None;
        for i in (0..stack.len()).rev() {
            let outcome = if let Some(action) = action {
                stack[i].handle_action(action, &mut ctx)
            } else {
                stack[i].handle_raw_key(key, &mut ctx)
            };

            if !matches!(outcome, Outcome::Ignored) {
                outcome_action = Some((i, outcome));
                break;
            }
        }

        // Step 6: Put stack back and apply outcome
        self.widget_stack = stack;
        if let Some((index, outcome)) = outcome_action {
            self.apply_outcome(index, outcome);
        }

        self.apply_context_signals(ctx);
        return; // Widget stack handled it, skip legacy path
    }

    // ── Legacy path (existing code) ──
    if let Some(action) = self.resolve_action(key) {
        // ...existing dispatch_action logic...
    }
    // ...rest of existing handle_key...
}
```

Add helper methods on `App`:

```rust
fn build_widget_context(&mut self) -> WidgetContext<'_> {
    WidgetContext {
        model: &self.model,
        keymap: &self.keymap,
        config: &*self.config,
        in_flight: &self.in_flight,
        target_host: self.ui.target_host.as_ref(),
        active_repo: self.model.active_repo,
        repo_order: &self.model.repo_order,
        commands: &mut self.proto_commands,
        repo_ui: &mut self.ui.repo_ui,
        should_quit: false,
        pending_cancel: None,
    }
}

fn apply_outcome(&mut self, index: usize, outcome: Outcome) {
    match outcome {
        Outcome::Consumed | Outcome::Ignored => {}
        Outcome::Finished => { self.widget_stack.remove(index); }
        Outcome::Push(widget) => { self.widget_stack.push(widget); }
        Outcome::Swap(widget) => {
            // Insert at the same position so stack ordering is preserved
            self.widget_stack.remove(index);
            self.widget_stack.insert(index, widget);
        }
    }
}

fn apply_context_signals(&mut self, ctx: WidgetContext) {
    if ctx.should_quit {
        self.should_quit = true;
    }
    if ctx.pending_cancel.is_some() {
        self.pending_cancel = ctx.pending_cancel;
    }
}
```

- [ ] **Step 7: Wire up `ToggleHelp` to push `HelpWidget`**

In `dispatch_action`, change the `ToggleHelp` handler for Normal mode to push onto the widget stack instead of setting `UiMode::Help`:

```rust
Action::ToggleHelp => match self.ui.mode {
    UiMode::Normal => {
        self.widget_stack.push(Box::new(crate::widgets::HelpWidget::new()));
    }
    UiMode::Help => {
        // Legacy path — keep for now until all UiMode::Help references are removed
        self.ui.mode = UiMode::Normal;
        self.ui.help_scroll = 0;
    }
    _ => {}
},
```

- [ ] **Step 8: Update rendering to use widget stack**

In `crates/flotilla-tui/src/ui.rs`, in the `render()` function, remove the `render_help(ui, theme, keymap, frame);` call. Instead, in `run.rs`, after the existing `terminal.draw(...)` call, render widget stack overlays. Alternatively, pass the widget stack into `ui::render` and iterate there.

The simplest bridge: in `run.rs`, change the draw call:

```rust
terminal.draw(|f| {
    ui::render(&app.model, &mut app.ui, &app.in_flight, &app.theme, &app.keymap, f);
    // Render widget stack overlays
    let area = f.area(); // Capture area before mutable borrow
    let ctx = RenderContext {
        model: &app.model,
        theme: &app.theme,
        keymap: &app.keymap,
        in_flight: &app.in_flight,
    };
    for widget in &mut app.widget_stack {
        widget.render(f, area, &ctx);
    }
})?;
```

Remove the `render_help` call from `ui::render()`.

- [ ] **Step 9: Remove old Help-mode handling from `dispatch_action`**

Remove the `FocusTarget::HelpText` arms from `SelectNext`, `SelectPrev`, and `Dismiss` in `dispatch_action`. Remove the `UiMode::Help` arm from `ToggleHelp` (it's now handled by the widget stack — `HelpWidget` returns `Finished` for `ToggleHelp`). Remove `render_help` from `ui.rs`.

- [ ] **Step 10: Update existing tests**

Existing tests in `key_handlers.rs` that test help behaviour (`question_mark_toggles_help_from_normal`, `question_mark_toggles_help_back_to_normal`, `esc_in_help_returns_to_normal`, `help_q_returns_to_normal_and_resets_scroll`, `dispatch_action_select_next_scrolls_help`) should now verify via the widget stack. Some can be moved to `widgets/help.rs`. Others that test integration (pressing `?` key pushes/pops) stay in `key_handlers.rs` but assert on `widget_stack` state instead of `UiMode`.

- [ ] **Step 11: Run full test suite**

Run: `cargo test -p flotilla-tui --locked`
Expected: all tests pass.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 12: Commit**

```
refactor: extract HelpWidget as first InteractiveWidget implementation
```

---

## Task 4: Extract `ActionMenuWidget`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/action_menu.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` — add module
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` — remove ActionMenu handling
- Modify: `crates/flotilla-tui/src/ui.rs` — remove `render_action_menu`

The action menu has navigation (SelectNext/Prev), confirm (executes selected intent and returns Finished or Swap), dismiss, and rendering. This is the first widget that uses `Swap` — when the selected intent is `RemoveCheckout`, the menu swaps to a `DeleteConfirmWidget` (added in Task 5).

**Intent resolution prerequisite:** `intent.resolve(item, &App)` currently takes `&App`. Before building the widget, extract the command-building helpers from `App` into free functions that take context fields:

- [ ] **Step 1: Extract command-building helpers from `App`**

In `crates/flotilla-tui/src/app/intent.rs`, the `resolve` method calls `app.repo_command(action)`, `app.targeted_command(action)`, `app.item_host_repo_command(action, item)`, `app.provider_repo_command(action, item)`, and `app.local_template_commands()`. These are thin wrappers in `crates/flotilla-tui/src/app/mod.rs` that construct `Command` from `CommandAction` plus routing metadata (active repo identity, target host, item host).

Create an `IntentContext` struct (or extend `WidgetContext`) with the fields these helpers need (`active_repo_identity`, `active_repo_root`, `target_host`, `config`), and refactor `intent.resolve` to take `&IntentContext` instead of `&App`. Move the command-building helpers to be methods on `IntentContext` or free functions.

- [ ] **Step 2: Run tests to verify the refactor compiles and passes**

Run: `cargo test -p flotilla-tui --locked`

- [ ] **Step 3: Commit the intent resolution refactor**

```
refactor: extract intent resolution from App dependency
```

- [ ] **Step 4: Write failing tests for ActionMenuWidget**

Tests for: `mode_id`, `select_next` advances index, `select_next` stays at end, `select_prev` decrements, `select_prev` stays at zero, `dismiss` returns Finished, `confirm` returns Finished (for simple intents), unhandled action returns Ignored. Write these in `crates/flotilla-tui/src/widgets/action_menu.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked action_menu`

- [ ] **Step 3: Implement `ActionMenuWidget`**

Struct holds `items: Vec<Intent>`, `index: usize`, `selected_item: WorkItem` (clone of the selected work item when the menu was opened), and a stored `menu_area: Rect` for hit testing. Implement `InteractiveWidget`.

For `Confirm`: the widget needs to resolve the intent into a command and push it via `ctx.commands`. Since `intent.resolve` currently takes `&App`, the bridge approach is to have the widget hold pre-resolved data or to refactor the command-building helpers to work with `WidgetContext`. Start with the simplest approach: extract command-building helpers (`repo_command`, `targeted_command`, etc.) as free functions that take `WidgetContext` fields, and refactor `intent.resolve` to use them. If this is too large, defer full resolution to the caller and have the menu return the selected `Intent` + `WorkItem` via a new outcome or signal.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Move `render_action_menu` body into widget**

Copy from `ui.rs` lines 1008-1029, adapting state references.

- [ ] **Step 6: Wire up — `open_action_menu` pushes widget onto stack**

Modify `open_action_menu` in `key_handlers.rs` to push an `ActionMenuWidget` instead of setting `UiMode::ActionMenu`.

- [ ] **Step 7: Move mouse handling**

Move `handle_menu_mouse` logic into `ActionMenuWidget::handle_mouse`. The widget stores its rendered `menu_area` from `render()` and uses it for hit testing.

- [ ] **Step 8: Remove legacy ActionMenu handling**

Remove `FocusTarget::ActionMenu` arms from `dispatch_action`, remove `handle_menu_mouse` from `key_handlers.rs`, remove `render_action_menu` from `ui.rs`.

- [ ] **Step 9: Update and migrate tests**

- [ ] **Step 10: Run full test suite + clippy**

- [ ] **Step 11: Commit**

```
refactor: extract ActionMenuWidget
```

---

## Task 5: Extract `DeleteConfirmWidget` and `CloseConfirmWidget`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/delete_confirm.rs`
- Create: `crates/flotilla-tui/src/widgets/close_confirm.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

Simple confirm/dismiss widgets with command dispatch via context.

- [ ] **Step 1: Write failing tests for `DeleteConfirmWidget`**

Tests: confirm pushes `RemoveCheckout` command and returns Finished, confirm while loading is Consumed (no command), dismiss returns Finished, `y` key maps to confirm (via keymap), mode_id is `DeleteConfirm`.

- [ ] **Step 2: Implement `DeleteConfirmWidget`**

Struct holds: `info: Option<CheckoutStatus>`, `loading: bool`, `terminal_keys`, `identity`, `remote_host`. On confirm (when not loading and info is Some), push `RemoveCheckout` command via `ctx.commands`, return Finished. On dismiss, return Finished without pushing.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Move `render_delete_confirm` body into widget**

- [ ] **Step 5: Write failing tests for `CloseConfirmWidget`**

- [ ] **Step 6: Implement `CloseConfirmWidget`**

Struct holds: `id`, `title`, `identity`, `command`. On confirm, push the held command via `ctx.commands`, return Finished.

- [ ] **Step 7: Run tests, verify pass**

- [ ] **Step 8: Move `render_close_confirm` body into widget**

- [ ] **Step 9: Wire up — push widgets from legacy code**

Where `resolve_and_push` currently sets `UiMode::DeleteConfirm`, push `DeleteConfirmWidget` onto the stack instead. Similarly for `CloseConfirm`. Update `ActionMenuWidget`'s confirm to `Swap(DeleteConfirmWidget::new(...))` for RemoveCheckout.

- [ ] **Step 10: Remove legacy confirm handling from `dispatch_action`**

- [ ] **Step 11: Update and migrate tests**

- [ ] **Step 12: Run full test suite + clippy**

- [ ] **Step 13: Commit**

```
refactor: extract DeleteConfirmWidget and CloseConfirmWidget
```

---

## Task 6: Extract `BranchInputWidget` and `IssueSearchWidget`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/branch_input.rs`
- Create: `crates/flotilla-tui/src/widgets/issue_search.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

First widgets returning `captures_raw_keys() = true`. All non-Esc/Enter keys go to `handle_raw_key` for tui_input passthrough.

- [ ] **Step 1: Write failing tests for `BranchInputWidget`**

Tests: `captures_raw_keys` returns true, `mode_id` is `BranchInput`, confirm with non-empty input pushes Checkout command and returns Finished, confirm with empty input returns Finished (no command), confirm while Generating is Consumed, dismiss returns Finished, raw key 'q' appends to input.

- [ ] **Step 2: Implement `BranchInputWidget`**

Struct holds `input: Input`, `kind: BranchInputKind`, `pending_issue_ids: Vec<(String, String)>`. On confirm (Manual, non-empty): build `Checkout` command, push via context, return Finished. `handle_raw_key` delegates to `input.handle_event()`.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Move `render_input_popup` body (BranchInput rendering) into widget**

- [ ] **Step 5: Write failing tests for `IssueSearchWidget`**

Tests: `captures_raw_keys` returns true, confirm with query pushes SearchIssues command, empty confirm returns Finished, dismiss clears search and returns Finished.

- [ ] **Step 6: Implement `IssueSearchWidget`**

- [ ] **Step 7: Run tests, verify pass**

- [ ] **Step 8: Move IssueSearch rendering (part of `render_input_popup`) into widget**

Note: `render_input_popup` currently handles both BranchInput and IssueSearch (they share a similar popup layout). Either duplicate the popup frame code in both widgets or extract a shared helper function.

- [ ] **Step 9: Wire up — push widgets from legacy code**

- [ ] **Step 10: Remove legacy handling**

- [ ] **Step 11: Update and migrate tests**

- [ ] **Step 12: Run full test suite + clippy**

- [ ] **Step 13: Commit**

```
refactor: extract BranchInputWidget and IssueSearchWidget
```

---

## Task 7: Extract `CommandPaletteWidget` and `FilePickerWidget`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/command_palette.rs`
- Create: `crates/flotilla-tui/src/widgets/file_picker.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/file_picker.rs` — absorb into widget
- Modify: `crates/flotilla-tui/src/ui.rs`

Hybrid widgets: `captures_raw_keys() = false`. They handle both resolved actions (SelectNext/Prev via keymap) and raw keys (text input characters) via `handle_raw_key`.

- [ ] **Step 1: Write failing tests for `CommandPaletteWidget`**

Tests: `captures_raw_keys` returns false, `mode_id` is `CommandPalette`, SelectNext wraps around, SelectPrev wraps, confirm with selected entry dispatches its action, confirm with "search query" dispatches SearchIssues, Tab fills selected entry name, Backspace on empty returns Finished, typing `/` fills "search ".

- [ ] **Step 2: Implement `CommandPaletteWidget`**

Struct holds `input: Input`, `entries: &'static [PaletteEntry]`, `selected: usize`, `scroll_top: usize`. `handle_action` handles SelectNext/SelectPrev/Confirm/Dismiss. `handle_raw_key` handles Tab, Backspace-on-empty, `/`-shortcut, and delegates other keys to `input.handle_event()`.

Note: `Confirm` currently calls `self.dispatch_action(action)` for palette entries that resolve to other actions. In the widget model, the widget can return `Outcome::Consumed` and signal via context, or the widget can return the resolved action for the app to re-dispatch. The simplest approach: for action-dispatching palette entries, return `Outcome::Finished` and set a signal or use a post-confirm callback. Alternatively, have the widget store the resolved action and the app re-dispatches after popping. Decide during implementation — the key constraint is that the palette must be able to trigger actions like `OpenBranchInput`, `OpenIssueSearch`, etc.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Move `render_command_palette` body into widget**

- [ ] **Step 5: Write failing tests for `FilePickerWidget`**

Tests: SelectNext/Prev navigate list, confirm on git repo pushes TrackRepoPath command, confirm on directory navigates into it, Tab fills selected entry name into input, text input refreshes directory listing.

- [ ] **Step 6: Implement `FilePickerWidget`**

Absorb logic from `crates/flotilla-tui/src/app/file_picker.rs` (select_next, select_prev, handle_file_picker_key, activate_dir_entry, refresh_dir_listing, etc.) and from `ui.rs` (`render_file_picker`). The widget owns all file picker state.

Note: `refresh_dir_listing` currently calls `std::fs::read_dir` — it can remain a method on the widget struct.

- [ ] **Step 7: Run tests, verify pass**

- [ ] **Step 8: Move `render_file_picker` body and mouse handling into widget**

- [ ] **Step 9: Wire up, remove legacy handling**

- [ ] **Step 10: Delete `crates/flotilla-tui/src/app/file_picker.rs`**

- [ ] **Step 11: Update and migrate tests**

- [ ] **Step 12: Run full test suite + clippy**

- [ ] **Step 13: Commit**

```
refactor: extract CommandPaletteWidget and FilePickerWidget
```

---

## Task 8: Extract `WorkItemTable`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/work_item_table.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/navigation.rs` — absorb table navigation
- Modify: `crates/flotilla-tui/src/ui.rs`

The largest extraction — table selection, multi-select, action dispatch (OpenActionMenu, OpenBranchInput, etc.), and `render_unified_table` + `render_preview`.

- [ ] **Step 1: Write failing tests**

Tests for: SelectNext/Prev advance selection, ToggleMultiSelect toggles, OpenActionMenu returns Push(ActionMenuWidget), OpenBranchInput returns Push(BranchInputWidget), Dispatch(Intent::RemoveCheckout) returns Push(DeleteConfirmWidget) or Swap, Dismiss cascades (cancel → clear search → clear providers → clear multi-select → quit), double-click detection.

- [ ] **Step 2: Implement `WorkItemTable`**

The widget needs access to `RepoUiState` (selection, multi_selected, table_state, etc.). Since `RepoUiState` is per-repo and lives in `UiState.repo_ui`, the widget either:
- Holds a reference/index to the active repo's state, or
- Receives the state through `WidgetContext`

The simplest approach: `WidgetContext` gains a method or field to access the active `RepoUiState`. The widget holds transient state (double-click tracking) and delegates persistent selection state to `RepoUiState` through the context.

Absorb from `navigation.rs`: `select_next`, `select_prev`, `row_at_mouse`, `toggle_multi_select` (table-specific functions). Tab navigation functions (`next_tab`, `prev_tab`, `move_tab`, `switch_tab`) stay out — they belong to `BaseView` or `TabBar`.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Move table and preview rendering into widget**

Move `render_unified_table` and `render_preview` / `render_preview_content` from `ui.rs`.

- [ ] **Step 5: Move table mouse handling**

The normal-mode mouse handling from `handle_mouse` (left click → select + double-click, right click → open menu, scroll) moves into the widget.

- [ ] **Step 6: Wire up, remove legacy handling**

- [ ] **Step 7: Update and migrate tests**

- [ ] **Step 8: Run full test suite + clippy**

- [ ] **Step 9: Commit**

```
refactor: extract WorkItemTable widget
```

---

## Task 9: Extract `TabBar` and `StatusBar`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/tab_bar.rs`
- Create: `crates/flotilla-tui/src/widgets/status_bar_widget.rs` (distinct from existing `status_bar.rs` module which has the model/layout types)
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/run.rs` — remove tab click routing and drag logic
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Write failing tests for `TabBar`**

Tests: click on repo tab switches active repo, click on flotilla tab switches to config, click on gear toggles providers, click on add opens file picker (returns Push), drag reorder swaps tabs.

- [ ] **Step 2: Implement `TabBar`**

Absorb tab click routing from `run.rs` (lines 118-170) and drag logic (lines 171-206). Absorb tab navigation from `navigation.rs` (`next_tab`, `prev_tab`, `switch_tab`, `move_tab`). Store `DragState` as widget state. Store click target areas from rendering.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Move `render_tab_bar` into widget**

- [ ] **Step 5: Write failing tests for `StatusBar`**

Tests: click on dismiss target hides error, click on key target dispatches action.

- [ ] **Step 6: Implement StatusBar widget**

Absorb `handle_status_bar_mouse` from `key_handlers.rs` and `dispatch_status_bar_action`. Store click targets from rendering.

- [ ] **Step 7: Move `render_status_bar` into widget**

- [ ] **Step 8: Wire up, remove legacy handling from `run.rs` and `key_handlers.rs`**

- [ ] **Step 9: Run full test suite + clippy**

- [ ] **Step 10: Commit**

```
refactor: extract TabBar and StatusBar widgets
```

---

## Task 10: Extract `EventLogWidget` and `PreviewPanel`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/event_log.rs`
- Create: `crates/flotilla-tui/src/widgets/preview_panel.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/src/run.rs` — remove event log filter click

- [ ] **Step 1: Write failing tests for `EventLogWidget`**

Tests: SelectNext advances selection, SelectPrev decrements, at-end stays, at-zero stays, no selection jumps to last, mode_id is Config, filter click cycles level.

- [ ] **Step 2: Implement `EventLogWidget`**

Struct holds: `selected: Option<usize>`, `count: usize`, `filter: tracing::Level`. Absorb event log navigation from the Config-mode arms of `dispatch_action`. Absorb `render_event_log` and `render_config_screen` from `ui.rs`. Absorb filter click handling from `run.rs`.

- [ ] **Step 3: Run tests, verify pass**

- [ ] **Step 4: Implement `PreviewPanel`**

Minimal widget wrapping `render_preview` and `render_preview_content`. Currently not focusable — just rendering delegation.

- [ ] **Step 5: Move rendering**

- [ ] **Step 6: Run full test suite + clippy**

- [ ] **Step 7: Commit**

```
refactor: extract EventLogWidget and PreviewPanel
```

---

## Task 11: Compose `BaseView`

**Files:**
- Create: `crates/flotilla-tui/src/widgets/base_view.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs` — `BaseView` is always `widget_stack[0]`
- Modify: `crates/flotilla-tui/src/run.rs` — simplify event loop
- Modify: `crates/flotilla-tui/src/ui.rs` — remove remaining rendering (content layout, provider panel, debug panel)

- [ ] **Step 1: Implement `BaseView`**

`BaseView` holds: `table: WorkItemTable`, `preview: PreviewPanel`, `tab_bar: TabBar`, `status_bar: StatusBarWidget`, `event_log: EventLogWidget`, `active_child: ActiveChild` (enum: Table, EventLog).

`handle_action`: delegates to the active child. Also handles actions that switch active child (tab navigation switching between Config and Normal).

`handle_mouse`: routes based on hit position — checks tab bar area, status bar area, then delegates to active content child.

`render`: computes layout (tab bar top, content middle, status bar bottom), delegates to each child with appropriate sub-area. Absorbs the remaining layout logic from `ui::render` (the top-level `render` function that splits into tab bar / content / status bar chunks).

- [ ] **Step 2: Initialise `BaseView` as `widget_stack[0]` in `App::new()`**

- [ ] **Step 3: Route rendering through the widget stack**

Replace the `ui::render` call in `run.rs` with widget stack rendering:

```rust
terminal.draw(|f| {
    let ctx = RenderContext { model: &app.model, theme: &app.theme, keymap: &app.keymap, in_flight: &app.in_flight };
    let area = f.area();
    for widget in &mut app.widget_stack {
        widget.render(f, area, &ctx);
    }
})?;
```

- [ ] **Step 4: Remove the legacy path from `App::handle_key`**

The bridge added in Task 3 (`if !self.widget_stack.is_empty()`) becomes the only path. Remove the legacy `dispatch_action` call and the old `handle_key` body.

- [ ] **Step 5: Run full test suite + clippy**

- [ ] **Step 6: Commit**

```
refactor: compose BaseView as root widget
```

---

## Task 12: Remove old scaffolding

**Files:**
- Delete: `crates/flotilla-tui/src/app/key_handlers.rs`
- Delete: `crates/flotilla-tui/src/app/navigation.rs`
- Delete: `crates/flotilla-tui/src/app/file_picker.rs` (should already be deleted in Task 7)
- Delete: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs` — remove `UiMode`, `FocusTarget`, `LayoutAreas`
- Modify: `crates/flotilla-tui/src/app/mod.rs` — remove module declarations, clean up imports
- Modify: `crates/flotilla-tui/src/lib.rs` — remove `pub mod ui;` if fully absorbed

- [ ] **Step 1: Audit `ui.rs` for remaining non-widget content**

Before deleting, check for shared helper functions, layout utilities, and constants that haven't been absorbed by specific widgets. Move any remaining shared code to `crates/flotilla-tui/src/ui_helpers.rs` (which already exists) or a new `widgets/helpers.rs`. Constants like `HIGHLIGHT_SYMBOL`, popup sizing helpers, and `resolve_preview_position` may need a shared home.

- [ ] **Step 2: Remove `UiMode` and `FocusTarget`**

Delete the enums and their `impl` blocks from `ui_state.rs`. Update any remaining references. `ModeId` in `keymap.rs` stays (it's the keymap concept) but the `From<&UiMode>` impl is removed.

- [ ] **Step 2: Remove `LayoutAreas`**

Each widget now stores its own click targets. Remove the centralised struct.

- [ ] **Step 3: Delete empty files**

Remove `key_handlers.rs`, `navigation.rs`, `ui.rs` if they're now empty or fully absorbed.

- [ ] **Step 4: Clean up module declarations and imports**

- [ ] **Step 5: Run full test suite**

Run: `cargo test --workspace --locked`

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 7: Run fmt check**

Run: `cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 8: Commit**

```
refactor: remove UiMode, FocusTarget, key_handlers.rs, ui.rs — widget refactor complete
```
