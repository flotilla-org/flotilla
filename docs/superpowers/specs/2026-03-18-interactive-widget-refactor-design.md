# Interactive Widget Refactor

Refactor the TUI input handling and rendering from a centralised dispatch model into self-contained interactive widgets that own their state, key handling, and rendering.

## Problem

`key_handlers.rs` (2125 lines) and `ui.rs` (1650 lines) are monolithic. Adding a new UI mode requires touching `dispatch_action` (SelectNext, SelectPrev, Confirm, Dismiss arms), `handle_key`, `handle_mouse`, and a `render_*` function — six or more sites scattered across two large files. The `UiMode` enum carries all mode state inline, and `FocusTarget` mediates a cross-product dispatch (Action x FocusTarget) that grows quadratically.

## Inspiration

Codex's TUI uses a `BottomPaneView` trait with a view stack. Each modal owns its state and key handling. Widgets communicate app-level effects through an async `AppEvent` channel. Flotilla adapts this pattern for its synchronous event loop, replacing the channel with a context struct.

## Design

### The `InteractiveWidget` Trait

Every interactive UI element implements this trait. Each widget owns its state and handles its own input and rendering.

```rust
pub trait InteractiveWidget {
    /// Handle a resolved Action (from the keymap).
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome;

    /// Handle a raw key that wasn't resolved to an Action.
    /// Used for text input passthrough. Default: Ignored.
    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        let _ = (key, ctx);
        Outcome::Ignored
    }

    /// Handle mouse events. Default: Ignored.
    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        let _ = (mouse, ctx);
        Outcome::Ignored
    }

    /// Render into the given area. Takes `&mut self` so widgets can store
    /// layout metadata (click targets, hit-test regions) during rendering.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext);

    /// Which keymap mode applies to this widget.
    fn mode_id(&self) -> ModeId;

    /// Whether this widget captures all raw keys (skipping keymap resolution).
    /// Pure text-input widgets (BranchInput, IssueSearch) return true so that
    /// shared bindings like `?` → ToggleHelp or `j` → SelectNext don't
    /// intercept characters the user intends to type.
    /// When true, the dispatcher skips keymap resolution and sends all keys
    /// directly to `handle_raw_key`, except Esc and Enter which are always
    /// resolved through the keymap so dismiss/confirm still works.
    ///
    /// Hybrid widgets that combine text input with list navigation
    /// (CommandPalette, FilePicker) should return false and instead handle
    /// both `handle_action` (for resolved navigation actions like SelectNext)
    /// and `handle_raw_key` (for unresolved text input characters).
    fn captures_raw_keys(&self) -> bool {
        false
    }
}
```

Key decisions:
- `render` takes `&mut self` so widgets can store their own click targets and hit-test regions during rendering. This replaces the centralised `LayoutAreas` struct.
- `captures_raw_keys` solves the text-input keymap bypass problem for pure text-input widgets. Today, `resolve_action` has hardcoded bypasses for BranchInput, IssueSearch, CommandPalette, and FilePicker. With the trait, pure text-input widgets (`BranchInputWidget`, `IssueSearchWidget`) return `true` from `captures_raw_keys`, and the dispatcher skips keymap resolution for those keys. Hybrid widgets (`CommandPaletteWidget`, `FilePickerWidget`) that need both text input and list navigation return `false` — they handle resolved actions (e.g. `SelectNext`/`SelectPrev`) via `handle_action` and unresolved text characters via `handle_raw_key`. Their keymap modes are configured with only the navigation bindings they need (arrows, j/k for FilePicker), so other keys fall through to `handle_raw_key` for text input.

### Outcome Enum

Handlers return an `Outcome` that tells the app loop what to do with the widget stack.

```rust
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
```

Transition semantics are explicit in the return value. No implicit combinations to reason about:

- **Table presses `.`**: table's handler returns `Push(ActionMenuWidget::new(...))`
- **Action menu Esc**: returns `Finished`, app pops it
- **Action menu selects "Remove Checkout"**: returns `Swap(DeleteConfirmWidget::new(...))`
- **Delete confirm Enter**: sends command via context, returns `Finished`

**Toggle pattern** (e.g. `?` for help): `ToggleHelp` is a shared keymap binding that reaches different widgets with opposite semantics. When `HelpWidget` is on top, it handles `ToggleHelp` by returning `Finished` (pop). When `BaseView` handles it (no help widget above), it returns `Push(HelpWidget::new())`. The top-down routing ensures the correct widget handles it first.

### Context Structs

`WidgetContext` is the boundary between widgets and app state. Widgets read app state and signal side effects through it. Widget state lives on `&mut self`, not on the context.

```rust
pub struct WidgetContext<'a> {
    // Read access
    pub model: &'a TuiModel,
    pub keymap: &'a Keymap,
    pub config: &'a ConfigStore,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    pub target_host: Option<&'a HostName>,

    // Write access — side effects
    pub commands: &'a mut CommandQueue,

    // Signals — widgets set these, app loop reads and applies after handling
    pub should_quit: bool,
    pub pending_cancel: Option<u64>,
}
```

`RenderContext` is a read-only subset for rendering:

```rust
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
}
```

Widget handlers are synchronous. Async work (provider refreshes, command execution) flows through `CommandQueue` as it does today.

**What stays on the App layer**: Some actions don't belong to any widget:

- **`CycleTheme`**: mutates `&mut Theme` which is app-owned. The app handles this before dispatching to the stack.
- **`Refresh`**: triggers async daemon dispatch (currently in `run.rs`). Stays in the event loop, dispatched via `tokio::spawn`.
- **Ctrl-Z suspend/resume**: terminal lifecycle, stays in the event loop.
- **Coalesced scroll**: event batching logic, stays in the event loop — synthesised scroll events are dispatched into the widget stack normally.

These are handled by the app/event-loop layer before or after the widget stack dispatch, not via additional `Outcome` variants.

### Widget Composition: Focus Stack

The app holds a focus stack of widgets. The base layer is always present; modals push on top.

```
App.widget_stack: Vec<Box<dyn InteractiveWidget>>
  [0] BaseView (always present, never popped)
        ├── TabBar        (mouse-only)
        ├── StatusBar     (mouse-only)
        ├── WorkItemTable (focusable)
        └── PreviewPanel  (focusable, future)
  [1] ActionMenuWidget   (modal, pushed when opened)
  [2] ...more modals
```

**Input routing** — top-down. The topmost widget gets first crack at every event:

```rust
fn handle_key(&mut self, key: KeyEvent) {
    let top = self.widget_stack.last().expect("stack never empty");
    let action = if top.captures_raw_keys() {
        // Pure text-input mode: only resolve Esc/Enter through keymap
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.resolve_action(key),
            _ => None,
        }
    } else {
        self.resolve_action(key) // uses top widget's mode_id()
    };

    // App-level actions that don't belong to any widget
    if let Some(action) = action {
        match action {
            Action::CycleTheme => { self.cycle_theme(); return; }
            _ => {}
        }
    }

    let mut ctx = self.build_widget_context();

    for i in (0..self.widget_stack.len()).rev() {
        let outcome = if let Some(action) = action {
            self.widget_stack[i].handle_action(action, &mut ctx)
        } else {
            self.widget_stack[i].handle_raw_key(key, &mut ctx)
        };

        if !matches!(outcome, Outcome::Ignored) {
            self.apply_outcome(i, outcome);
            break;
        }
    }

    self.apply_context_signals(ctx);
}
```

The loop only calls `apply_outcome` (which mutates the stack) when the outcome is not `Ignored`, and breaks immediately after. This ensures the stack is never modified while iteration continues.

This supports global keys (like tab switching) working even with a modal open — if the modal returns `Ignored`, the event falls through to the base layer.

**Rendering** — bottom-up. Base layer renders first, modals overlay:

```rust
fn render(&mut self, frame: &mut Frame) {
    let ctx = self.build_render_context();
    self.widget_stack[0].render(frame, content_area, &ctx);
    for widget in &mut self.widget_stack[1..] {
        widget.render(frame, frame.area(), &ctx);
    }
}
```

Note: `render` takes `&mut self` on both the app and widget level, since widgets store hit-test metadata during rendering.

**Mouse fallthrough**: Modal widgets do area-based hit testing against their stored layout regions. Clicks outside the modal's area return `Outcome::Ignored`, falling through to `BaseView`, which routes to its children (status bar targets, tab bar, table). This matches the current `handle_status_bar_mouse` behaviour where status bar clicks work regardless of active mode.

### BaseView: The Composite Base Layer

`BaseView` is an `InteractiveWidget` that manages children and internal focus:

- **TabBar**: top strip, mouse-only (click to switch repos, drag to reorder)
- **StatusBar**: bottom strip, mouse-only (click to dismiss errors, trigger actions)
- **WorkItemTable**: main content, focusable (keyboard navigation, action dispatch)
- **PreviewPanel**: side/bottom panel, focusable in future (Tab to toggle focus)

`BaseView` delegates `handle_action` to whichever child currently has focus. Mouse events route based on hit position. Rendering delegates to each child with appropriate sub-areas.

Internal focus toggling (table ↔ preview) is a local concern of `BaseView` — the stack framework doesn't need to know about it.

**Config/EventLog view**: The current `UiMode::Config` replaces the table view entirely with an event log. This is handled by `BaseView` swapping its active content child: when the flotilla tab is selected, `BaseView` renders and routes input to an `EventLogWidget` instead of `WorkItemTable`. This is an internal concern of `BaseView` (like the table ↔ preview focus toggle), not a separate stack entry. The `EventLogWidget` reports `ModeId::Config` from its `mode_id()` so Config-mode key bindings apply.

**Tab drag-to-reorder**: Currently managed in `run.rs` with `DragState`. This moves into `TabBar`, which stores its own drag state and handles `MouseDown`/`Drag`/`MouseUp` sequences. On drop, it signals tab reorder through `WidgetContext` (e.g., via `commands` or a dedicated signal). `BaseView` routes drag events to `TabBar` when the initial click was in the tab area.

### Concrete Widgets

**Base layer children:**

| Widget | State | File |
|--------|-------|------|
| `BaseView` | sub-focus, children | `widgets/base_view.rs` |
| `TabBar` | click targets, drag state | `widgets/tab_bar.rs` |
| `StatusBar` | click targets, show_keys, dismissed IDs | `widgets/status_bar.rs` |
| `WorkItemTable` | selection, multi-select, scroll | `widgets/work_item_table.rs` |
| `PreviewPanel` | scroll position | `widgets/preview_panel.rs` |
| `EventLogWidget` | selected, count, filter | `widgets/event_log.rs` |

**Modal widgets (pushed onto stack):**

| Widget | State (currently inlined in UiMode) | File |
|--------|--------------------------------------|------|
| `ActionMenuWidget` | `items: Vec<Intent>, index: usize` | `widgets/action_menu.rs` |
| `BranchInputWidget` | `input, kind, pending_issue_ids` | `widgets/branch_input.rs` |
| `IssueSearchWidget` | `input: Input` | `widgets/issue_search.rs` |
| `FilePickerWidget` | `input, dir_entries, selected` | `widgets/file_picker.rs` |
| `DeleteConfirmWidget` | `info, loading, terminal_keys, identity, remote_host` | `widgets/delete_confirm.rs` |
| `CloseConfirmWidget` | `id, title, identity, command` | `widgets/close_confirm.rs` |
| `CommandPaletteWidget` | `input, entries, selected, scroll_top` | `widgets/command_palette.rs` |
| `HelpWidget` | `scroll: u16` | `widgets/help.rs` |

### Intent Resolution

`intent.resolve()` currently takes `&App` to construct commands (accessing `model`, `target_host`, `config`, template commands, etc.). This must be refactored so widgets can resolve intents without a reference to `App`.

Two approaches, to be decided during implementation:

1. **Refactor `intent.resolve` to take `&WidgetContext`** (or a subset struct like `IntentContext` with the fields it actually reads). This is the cleaner long-term answer.
2. **Keep `intent.resolve` on `App` and have the app resolve before pushing the modal**. E.g., `WorkItemTable` returns `Outcome::Push(ActionMenuWidget::new(items, resolved_commands))` where the commands are pre-resolved. Less clean but lower-risk for migration.

Approach 1 is preferred. The fields `intent.resolve` accesses are: `model.active_repo_identity()`, `model.active_repo_root()`, `model.active().providers`, `ui.target_host`, `config` (for template commands), and command-building helpers (`repo_command`, `targeted_command`, `item_host_repo_command`). These can be exposed through `WidgetContext` or a focused `IntentContext` struct. The command-building helpers are thin wrappers that construct `Command` from a `CommandAction` plus routing metadata — they can become free functions or methods on the context.

### What Disappears

- **`UiMode` enum**: replaced by the widget stack. The "mode" is whichever widget is on top.
- **`FocusTarget` enum**: each widget handles its own actions.
- **`key_handlers.rs`**: dissolves into per-widget `handle_action` implementations.
- **`ui.rs`**: dissolves into per-widget `render` implementations.
- **`LayoutAreas`**: replaced by per-widget stored layout metadata (each widget stores its own click targets during `render`).

### What Stays

- **`ModeId`** and **`Keymap`**: unchanged. Each widget reports its `mode_id()` for keymap resolution.
- **`Action` enum**: unchanged.
- **`Intent` enum**: stays, but `resolve` signature changes (see Intent Resolution above).
- **`App` struct**: becomes thinner — owns widget stack, model, config, keymap, theme, command queue. Methods become thin stack walkers. App-level concerns (theme cycling, refresh dispatch, Ctrl-Z) are handled before/after the stack.
- **`RepoUiState`**: stays, but loses mode-specific fields. Owned by `WorkItemTable` or `BaseView`.
- **`run.rs` event loop**: stays but simplifies. Scroll coalescing, Ctrl-Z, and async refresh dispatch remain. Tab click routing and drag-to-reorder move into `TabBar`. Event log filter clicking moves into `EventLogWidget`.

## Migration Strategy

Extract one widget at a time, edges first, working inward. Each step is independently compilable and testable.

1. **Introduce the trait**: add `widgets/mod.rs` with `InteractiveWidget`, `Outcome`, `WidgetContext`, `RenderContext`.
2. **Extract `HelpWidget`**: simplest modal — just scroll state, two key handlers (SelectNext/Prev for scrolling, Dismiss/ToggleHelp for closing), one render function. Proves the trait and stack mechanism work. Note: `ToggleHelp` is handled by both `HelpWidget` (returns `Finished`) and the base layer (returns `Push`) — first widget to demonstrate the toggle pattern.
3. **Extract `ActionMenuWidget`**: navigation + confirm + dismiss + rendering. First widget that uses `Swap` (menu → delete confirm). Requires starting the `intent.resolve` refactoring — either refactor to take `WidgetContext`/`IntentContext`, or pre-resolve intents when building the menu.
4. **Extract confirm dialogs**: `DeleteConfirmWidget`, `CloseConfirmWidget`. Simple confirm/dismiss with command dispatch via context.
5. **Extract text-input widgets**: `BranchInputWidget`, `IssueSearchWidget`. First widgets returning `true` from `captures_raw_keys`. Validates the text-input keymap bypass mechanism.
6. **Extract hybrid input widgets**: `CommandPaletteWidget`, `FilePickerWidget`. These combine text input with list navigation — they return `false` from `captures_raw_keys` and use both `handle_action` (for navigation) and `handle_raw_key` (for text). Validates the dual-method approach.
7. **Extract `BaseView` + children** (sub-steps):
    - 7a. **Extract `WorkItemTable`**: absorb table navigation from `navigation.rs` (`select_next`, `select_prev`, `selected_work_item`, multi-select toggle). Absorb table rendering from `ui.rs`.
    - 7b. **Extract `TabBar`**: absorb tab navigation (`next_tab`, `prev_tab`, `switch_tab`, `move_tab`) and tab click/drag from `run.rs`. Absorb tab bar rendering.
    - 7c. **Extract `StatusBar`**: absorb status bar mouse handling and rendering.
    - 7d. **Extract `PreviewPanel`**: absorb preview rendering.
    - 7e. **Extract `EventLogWidget`**: absorb event log rendering and Config-mode navigation. Absorb event log filter click handling from `run.rs`.
    - 7f. **Compose `BaseView`**: wire children together with internal focus routing and layout delegation.
8. **Remove old scaffolding**: delete `UiMode`, `FocusTarget`, `key_handlers.rs`, `ui.rs`, `LayoutAreas`, `navigation.rs`.

At each step, the old `dispatch_action` match arms shrink — extracted modes are handled by the widget stack, unextracted modes still go through the old path. A bridge in `App::handle_key` checks "is there a widget on the stack for this?" before falling back to the legacy path. This bridge disappears at step 8.

### Test migration

Tests move alongside their widget. Each widget file includes its own `#[cfg(test)] mod tests`. The existing `test_support` and `test_builders` modules continue to provide `stub_app()` etc., extended with helpers for constructing `WidgetContext` in isolation. Widget-level tests become simpler — no need to set up a full `App`, just the widget struct and a test context:

```rust
#[test]
fn action_menu_select_next_advances_index() {
    let mut widget = ActionMenuWidget::new(vec![Intent::CreateWorkspace, Intent::RemoveCheckout]);
    let mut ctx = test_widget_context();
    let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
    assert_eq!(widget.index, 1);
    assert!(matches!(outcome, Outcome::Consumed));
}
```

## File Structure After Migration

```
crates/flotilla-tui/src/
  widgets/
    mod.rs                — trait, Outcome, WidgetContext, RenderContext
    base_view.rs          — BaseView composite
    work_item_table.rs    — table selection, actions, rendering
    preview_panel.rs      — preview content rendering
    tab_bar.rs            — tab click/drag routing, rendering
    status_bar.rs         — status targets, rendering
    event_log.rs          — event log navigation, filter, rendering
    action_menu.rs        — intent menu
    branch_input.rs       — branch name input
    issue_search.rs       — issue search input
    file_picker.rs        — directory browser
    delete_confirm.rs     — checkout deletion confirmation
    close_confirm.rs      — PR close confirmation
    command_palette.rs    — command palette
    help.rs               — help overlay
  app/
    mod.rs                — App struct, stack walking, model updates
    executor.rs           — unchanged
    intent.rs             — resolve signature updated to take IntentContext/WidgetContext
    test_support.rs       — extended with widget test helpers
    test_builders.rs      — unchanged
    ui_state.rs           — slimmed: RepoUiState, UiState (no UiMode/FocusTarget)
  run.rs                  — event loop (scroll coalescing, Ctrl-Z, async dispatch)
  keymap.rs               — unchanged
  palette.rs              — unchanged
  theme.rs                — unchanged
  event.rs                — unchanged
  event_log.rs            — unchanged
```
