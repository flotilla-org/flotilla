# Child Widget Trait and Global Action Classification

Make BaseView's children implement `InteractiveWidget` so they handle their own actions and rendering uniformly. Classify actions as global vs focus-routed so globals bypass the widget stack.

## Problem

BaseView's children (TabBar, StatusBarWidget, WorkItemTable, PreviewPanel, EventLogWidget) are plain structs with bespoke method signatures. BaseView manually routes actions to the right child with a `match action { ... }` cross-product — the same pattern the original refactor was meant to eliminate. Children can't be swapped, composed, or treated uniformly.

Additionally, some actions are truly global (tab switching, theme cycling, layout cycling) but currently flow through the widget stack where BaseView returns `Ignored` and they fall through to App's `dispatch_action`. This is needlessly indirect.

## Design

### Global vs focus-routed actions

Actions are classified by whether they're handled globally (by App, regardless of focus) or routed to the focused widget.

```rust
impl Action {
    pub fn is_global(&self) -> bool {
        matches!(self,
            Action::PrevTab | Action::NextTab |
            Action::MoveTabLeft | Action::MoveTabRight |
            Action::CycleTheme | Action::CycleLayout | Action::CycleHost |
            Action::ToggleDebug | Action::ToggleStatusBarKeys |
            Action::Refresh
        )
    }
}
```

In `handle_key`, global actions are handled by App before the widget stack dispatch:

```rust
if let Some(action) = action {
    if action.is_global() {
        self.handle_global_action(action);
        return;
    }
}
// ... then widget stack dispatch ...
```

`handle_global_action` absorbs:
- Tab navigation from `dispatch_action` (`PrevTab`, `NextTab`, `MoveTabLeft`, `MoveTabRight`)
- Toggle/cycle actions from BaseView's `handle_action` and `dispatch_action` (`CycleTheme`, `CycleLayout`, `CycleHost`, `ToggleDebug`, `ToggleStatusBarKeys`)
- `Refresh` from BaseView's `handle_action` (currently constructs a refresh command — moves to App)

`dispatch_action` shrinks to only focus-dependent actions that need `&mut App`: `Confirm` (action_enter), `OpenActionMenu`, `OpenFilePicker`, `Dispatch(intent)`.

### Children implement InteractiveWidget

All BaseView children implement the `InteractiveWidget` trait. For children where some trait methods don't apply, the defaults return `Ignored`.

**WorkItemTable:**
- `handle_action`: SelectNext, SelectPrev, ToggleMultiSelect, OpenBranchInput, OpenIssueSearch (note: also sets `*ctx.mode = UiMode::IssueSearch` — the remaining UiMode bridge), OpenCommandPalette, ToggleHelp (returns Push)
- `handle_mouse`: left click (select row), right click (select row), scroll (select_next/prev)
- `render`: table rendering (already implemented)
- `mode_id`: ModeId::Normal

**EventLogWidget:**
- `handle_action`: SelectNext, SelectPrev
- `handle_mouse`: filter click
- `render`: config screen rendering (already implemented)
- `mode_id`: ModeId::Config (informational — not used for keymap resolution since EventLog is never a stack widget; Config-mode keymap resolution works via the existing `resolve_action` fallback from `UiMode`)

**TabBar:**
- `handle_action`: returns Ignored for everything (tab actions are global)
- `handle_mouse`: tab click (→ AppAction), drag initiation
- `render`: tab bar rendering (already implemented)
- `mode_id`: ModeId::Normal

**StatusBarWidget:**
- `handle_action`: returns Ignored for everything (ToggleStatusBarKeys is global)
- `handle_mouse`: click targets (→ AppAction::StatusBarKeyPress/ClearError)
- `render`: status bar rendering (already implemented, needs `active_widget_mode` and `active_widget_data` from RenderContext)
- `mode_id`: ModeId::Normal

**PreviewPanel:**
- `handle_action`: returns Ignored (no actions yet)
- `handle_mouse`: returns Ignored (no mouse yet)
- `render`: preview rendering (already implemented)
- `mode_id`: ModeId::Normal

### BaseView: two-phase dispatch

BaseView's `handle_action` uses a two-phase pattern: delegate to the focused child first, then handle cross-cutting concerns if the child returned `Ignored`.

```rust
fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
    // Phase 1: delegate to focused child
    let outcome = match self.active_child(ctx) {
        ActiveChild::Table => self.table.handle_action(action, ctx),
        ActiveChild::EventLog => self.event_log.handle_action(action, ctx),
    };
    if !matches!(outcome, Outcome::Ignored) {
        return outcome;
    }

    // Phase 2: cross-cutting actions handled by BaseView
    match action {
        Action::Dismiss => self.dismiss(ctx),   // cascade: cancel → clear search → ... → quit
        Action::Quit => { ctx.app_actions.push(AppAction::Quit); Outcome::Consumed }
        // Actions needing &mut App — fall through
        Action::Confirm | Action::OpenActionMenu | Action::OpenFilePicker | Action::Dispatch(_) => Outcome::Ignored,
        _ => Outcome::Ignored,
    }
}
```

The Dismiss cascade spans multiple children's state (in-flight commands, search query, providers, multi-select) — it stays on BaseView because it's a cross-cutting concern, not a table concern.

`ToggleProviders` flips `rui.show_providers` which affects table rendering. It could live on either BaseView or WorkItemTable — both have access through `WidgetContext`. Keep it on WorkItemTable since it's table display state.

`Quit` stays on BaseView (not WorkItemTable) — application lifecycle is not a table concern.

### BaseView: mouse routing

BaseView's `handle_mouse` routes based on hit position, delegating to children:

```rust
fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
    // Check children in visual order
    if in_tab_bar_area(mouse) { return self.tab_bar.handle_mouse(mouse, ctx); }
    if in_status_bar_area(mouse) { return self.status_bar.handle_mouse(mouse, ctx); }
    if in_content_area(mouse) {
        // Double-click detection stays on BaseView (cross-child concern)
        if is_double_click(mouse) {
            ctx.app_actions.push(AppAction::ActionEnter);
            return Outcome::Consumed;
        }
        return match self.active_child(ctx) {
            ActiveChild::Table => self.table.handle_mouse(mouse, ctx),
            ActiveChild::EventLog => self.event_log.handle_mouse(mouse, ctx),
        };
    }
    Outcome::Ignored
}
```

### Render signatures

Children move from bespoke render signatures to `InteractiveWidget::render(&mut self, frame, area, &mut RenderContext)`.

`RenderContext` still carries:
- `&mut UiState` — temporary bridge for table state, layout areas
- `active_widget_mode: Option<ModeId>` — needed by StatusBarWidget to show correct key hints
- `active_widget_data: WidgetStatusData` — needed by StatusBarWidget for branch input / command palette text

For TabBar, which currently takes `drag_active: bool`: BaseView sets a `pub drag_active: bool` field on `self.tab_bar` before calling `self.tab_bar.render(...)`. This is a pre-render poke, not a RenderContext field — it's TabBar-specific state.

### Tab state ownership (future)

The tab bar currently reads `model.active_repo` and `model.repo_order` — it doesn't own which tab is selected. This is a known smell deferred to Stage 2 (per-tab content tree) when TabBar becomes the authority for tab state.

## What changes

1. Add `Action::is_global()` classification
2. Add `handle_global_action` on App, move global handling from `dispatch_action` and `BaseView::handle_action`
3. Pre-dispatch globals in `handle_key` before widget stack
4. Implement `InteractiveWidget` on WorkItemTable, EventLogWidget, TabBar, StatusBarWidget, PreviewPanel
5. Update render signatures from bespoke to `InteractiveWidget::render`
6. Change BaseView to two-phase dispatch (delegate to child, then cross-cutting)
7. Move mouse handling logic from BaseView into each child (except double-click detection and drag state)
8. Slim `dispatch_action` to only focus-dependent actions needing `&mut App`
