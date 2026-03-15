# Batch A Interaction Foundation

**Date:** 2026-03-14

## Problem

Flotilla's TUI currently defines keyboard behavior directly in mode-specific match arms in
`crates/flotilla-tui/src/app/key_handlers.rs`.
That structure mixes three different concerns:

- physical keys such as `j`, `Esc`, `Enter`, and `?`
- logical user actions such as "move selection", "confirm", and "dismiss"
- focus-specific behavior such as table navigation, help scrolling, and menu selection

As a result, the same key means different things only because `UiMode` happens to be different, and shared concepts like dismiss or confirm are reimplemented several times. That blocks the next keybinding step: a user-configurable keymap cannot map one logical action to multiple contexts if the app still treats every context as a separate key handler.

Batch A is the internal foundation for fixing that. It should establish focus-aware action dispatch without yet introducing TOML config or user-visible keybinding customization.

## Goals

- Introduce an explicit focus model for TUI interaction.
- Introduce an explicit action layer between raw key events and app behavior.
- Centralize shared navigation, confirm, dismiss, and tab actions behind one dispatcher.
- Normalize obvious mode inconsistencies while making the interaction model simpler.
- Preserve existing specialized text-input editing behavior where centralization would not yet pay off.
- Make the follow-on configurable keybinding work a smaller, local change instead of another structural refactor.

## Non-Goals

- Loading keybindings from TOML.
- Adding `crokey` or any other key-parsing dependency.
- Auto-generating help text from the keymap.
- Reworking mouse handling into the same action system in this batch.
- Replacing `Intent` with a single combined enum.
- Changing the available Normal-mode shortcuts beyond the normalization described here.
- Redesigning status-bar shortcut labels or other UI presentation.

## Design Overview

This batch adds one layer of indirection:

```text
KeyEvent
  -> default Rust keymap
  -> Action
  -> dispatch_action(Action)
  -> behavior chosen by current FocusTarget
```

`UiMode` remains the source of rendering state and mode-specific data, but it stops being the primary place where shared navigation semantics are defined.

Batch A deliberately does not centralize everything. Text-entry modes such as branch input and issue search still need mode-specific handling for character insertion, backspace, and submission payload creation. The new action layer should absorb the shared parts first and leave raw text editing in the existing helpers until the configurable keymap work needs more.

## Focus Model

Add a new `FocusTarget` enum in
`crates/flotilla-tui/src/app/ui_state.rs`:

```rust
pub enum FocusTarget {
    WorkItemTable,
    EventLog,
    HelpText,
    ActionMenu,
    BranchInput,
    IssueSearchInput,
    FilePickerList,
    DeleteConfirmDialog,
    CloseConfirmDialog,
}
```

Add `UiMode::focus_target(&self) -> FocusTarget`, or an equivalent `App::current_focus()` helper, that derives focus from the current mode.

The mapping is straightforward:

- `UiMode::Normal` -> `WorkItemTable`
- `UiMode::Config` -> `EventLog`
- `UiMode::Help` -> `HelpText`
- `UiMode::ActionMenu { .. }` -> `ActionMenu`
- `UiMode::BranchInput { .. }` -> `BranchInput`
- `UiMode::IssueSearch { .. }` -> `IssueSearchInput`
- `UiMode::FilePicker { .. }` -> `FilePickerList`
- `UiMode::DeleteConfirm { .. }` -> `DeleteConfirmDialog`
- `UiMode::CloseConfirm { .. }` -> `CloseConfirmDialog`

This batch assumes a single focused widget at a time. Split focus or nested focus is out of scope.

## Action Layer

Add an `Action` enum representing logical user intent at the UI interaction layer. It should include:

```rust
pub enum Action {
    SelectNext,
    SelectPrev,
    Confirm,
    Dismiss,
    Quit,
    Refresh,
    PrevTab,
    NextTab,
    MoveTabLeft,
    MoveTabRight,
    ToggleHelp,
    ToggleMultiSelect,
    ToggleProviders,
    ToggleDebug,
    CycleHost,
    CycleLayout,
    OpenActionMenu,
    OpenBranchInput,
    OpenIssueSearch,
    OpenFilePicker,
    Dispatch(Intent),
}
```

`Action` is intentionally broader than `Intent`.

- `Intent` remains the domain-operation layer for work-item actions such as remove checkout, open change request, create workspace, or generate branch name.
- `Action` is the input/UI layer for navigation, mode entry, dismissal, tab movement, and dispatching domain operations.

The relationship should be one-way:

```text
KeyEvent -> Action -> maybe Dispatch(Intent) -> existing intent resolution
```

That keeps domain operations isolated from keyboard policy while avoiding a larger rewrite of `Intent`.

## Default Keymap

Batch A uses a Rust-defined default keymap only. It should be implemented as a small helper in
`crates/flotilla-tui/src/app/key_handlers.rs`,
for example:

```rust
fn resolve_action(&self, key: KeyEvent) -> Option<Action>
```

The resolver may use the current mode when needed. That is still compatible with the design goal: shared key meaning lives in one place instead of being reimplemented in separate handlers.

The keymap should encode the current intended defaults, not merely preserve the old handler layout. The important point is that shared key meaning now lives in one place.

Expected mappings for this batch:

- `j`, `Down` -> `SelectNext`
- `k`, `Up` -> `SelectPrev`
- `Enter` -> `Confirm`
- `Esc` -> `Dismiss`
- `q` -> `Quit` in `UiMode::Normal`, `Dismiss` in non-text secondary contexts
- `r` -> `Refresh`
- `[` -> `PrevTab`
- `]` -> `NextTab`
- `{` -> `MoveTabLeft`
- `}` -> `MoveTabRight`
- `?` -> `ToggleHelp`
- `Space` -> `ToggleMultiSelect`
- `h` -> `CycleHost`
- `l` -> `CycleLayout`
- `.` -> `OpenActionMenu`
- `n` -> `OpenBranchInput`
- `/` -> `OpenIssueSearch`
- `a` -> `OpenFilePicker`
- `c` -> `ToggleProviders`
- `D` -> `ToggleDebug`
- `d` -> `Dispatch(Intent::RemoveCheckout)`
- `p` -> `Dispatch(Intent::OpenChangeRequest)`

Mode-specific helpers may still intercept keys before default action resolution when they are true text-editing input. Examples:

- branch input in manual editing mode
- issue search input while typing
- file picker path input

That exception should be narrow. The point is to avoid forcing character-entry code through an unfinished abstraction.

The main purpose of the mode-aware `q` rule is normalization:

- in the main table view, `q` quits
- in secondary non-text views such as help, config, menus, and confirmation dialogs, `q` dismisses
- in text-entry modes, `q` remains ordinary input text

## Dispatch Model

Add a single `dispatch_action(&mut self, action: Action)` method in
`crates/flotilla-tui/src/app/key_handlers.rs`.

This method should use `FocusTarget` for shared actions and direct routing for global actions.

Examples:

- `SelectNext`
  - `WorkItemTable` -> `select_next()`
  - `EventLog` -> advance config log selection
  - `HelpText` -> scroll help down
  - `ActionMenu` -> move highlighted menu item down
  - `FilePickerList` -> advance picker selection
- `SelectPrev`
  - symmetric reverse behavior for the same targets
- `Confirm`
  - `WorkItemTable` -> `action_enter()`
  - `ActionMenu` -> `execute_menu_action()`
  - `DeleteConfirmDialog` -> confirm deletion if not loading
  - `CloseConfirmDialog` -> confirm close action
  - `BranchInput` -> submit current branch value when not generating
  - `IssueSearchInput` -> submit current search query
  - `FilePickerList` -> preserve existing picker activation behavior
- `Dismiss`
  - overlay/input targets -> return to `UiMode::Normal`, with any existing cleanup required by that mode
  - `WorkItemTable` -> Normal-mode dismiss cascade
  - `EventLog` -> return to `UiMode::Normal`
- `PrevTab`, `NextTab`, `MoveTabLeft`, `MoveTabRight`
  - route through existing tab helpers
- `Dispatch(Intent)`
  - route through `dispatch_if_available()` or equivalent

Specialized mode helpers should remain available where action dispatch needs to reuse existing logic rather than duplicate it. For example, `Confirm` for issue search can call a dedicated helper that already knows how to queue the `SearchIssues` command and persist `active_search_query`.

## Behavior Normalization

Batch A should intentionally normalize a few current inconsistencies.

### Dismiss

`Dismiss` becomes the universal back/cancel action.

In overlay or temporary interaction modes, dismiss returns to `UiMode::Normal`:

- help
- action menu
- branch input
- issue search
- file picker
- delete confirm
- close confirm

In `UiMode::Normal`, dismiss keeps the existing cascading priority:

1. If an in-flight command is cancelable, queue cancellation.
2. Else if an active issue search exists, clear it.
3. Else if provider attribution is visible, hide it.
4. Else if multi-select is active, clear it.
5. Else quit the app.

In `UiMode::Config`, dismiss should return to `UiMode::Normal` instead of quitting the app. Config should behave like a view, not like a separate root state.

### Quit

`Quit` remains explicit and should always quit immediately. This gives the app a consistent escape hatch even after `Dismiss` is normalized to mean "back" in Config and overlay contexts.

### ToggleHelp

Help toggling should be modeled as an action rather than a one-off early return. From `Normal`, it opens help. From `Help`, it returns to `Normal` and resets help scroll. In other modes, it should do nothing.

### Confirm

`Confirm` should be the shared "activate" action, but this batch should not over-abstract confirm flows that still need mode-local payload assembly. The architectural requirement is that the logical action is shared; the underlying implementation can still call specialized helpers.

## Integration Boundaries

The new action layer should not absorb responsibilities that belong elsewhere.

- Table navigation stays in
  `crates/flotilla-tui/src/app/navigation.rs`.
- Work-item operations stay in `Intent` resolution.
- Text mutation for `tui_input::Input` values stays in the existing per-mode helpers for now.
- Mouse handling remains mostly as-is in this batch. It may continue to call existing methods directly rather than dispatching through `Action`.

This preserves clear file boundaries:

- `ui_state.rs`: mode and focus state definitions
- `key_handlers.rs`: key-to-action resolution and action dispatch
- `navigation.rs`: table and tab movement primitives
- `mod.rs`: app-level convenience helpers used by dispatch

## Error Handling and Invariants

The refactor should preserve these invariants:

- Missing selection remains a no-op for actions that depend on a selected work item.
- `DeleteConfirm { loading: true }` must not confirm deletion.
- Branch input in `Generating` mode must ignore confirm and character-entry actions until generation completes.
- Issue-search dismissal must continue clearing the active query through the existing clear helper.
- Action-menu execution may replace the current mode with another mode, and dispatch must not blindly reset back to `Normal` afterward.
- Tab movement operations remain no-ops when movement is invalid.

Batch A should avoid introducing a partially-centralized state where both the action dispatcher and leftover mode match arms independently interpret the same shared key. If a shared key is resolved to `Action`, its behavior should be owned in one place.

## Testing Strategy

This work should be test-first and should shift tests toward the new boundaries rather than only reasserting the old ones.

### Unit Tests

Add focused tests for:

- `FocusTarget` derivation from every `UiMode`
- key-to-action resolution for shared keys
- `ToggleHelp` behavior in Normal, Help, and non-help modes
- `Dismiss` behavior in Config, Normal, and overlay/input modes
- `SelectNext` / `SelectPrev` dispatch for table, help, event log, action menu, and file picker
- `Confirm` dispatch for work item table, menu, delete confirm, close confirm, branch input, and issue search
- `Dispatch(Intent)` routing for available vs unavailable actions

### Regression Tests

Keep a smaller set of end-to-end `handle_key()` tests that verify the top-level wiring still works for:

- opening and closing help
- Normal-mode dismiss cascade ordering
- delete confirm respecting loading state
- action menu execution preserving a newly-entered mode
- branch input generating mode ignoring input

### Scope Discipline

Do not add tests yet for:

- TOML parsing
- user-configurable overrides
- auto-generated help content

Those belong to the later keybinding batch that builds on this foundation.

## Files Likely Involved

| File | Change |
|------|--------|
| `crates/flotilla-tui/src/app/ui_state.rs` | Add `FocusTarget` and focus derivation |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Add `Action`, default key resolution, and `dispatch_action()`; trim duplicated shared-key logic |
| `crates/flotilla-tui/src/app/navigation.rs` | Reuse existing navigation helpers from action dispatch; minor helper extraction if needed |
| `crates/flotilla-tui/src/app/mod.rs` | Keep or add app helpers used by dispatch, especially dismiss/search helpers |

## Implementation Sequence

The intended implementation order is:

1. Add `FocusTarget` and tests for mode-to-focus mapping.
2. Add `Action` and tests for key resolution.
3. Add `dispatch_action()` for shared actions.
4. Convert help and config handling to the new action path.
5. Convert action menu, confirm dialogs, and file-picker navigation to the new action path.
6. Convert branch input and issue search to use the shared actions only where it improves clarity without over-abstracting text entry.
7. Remove now-redundant per-mode key logic.

This keeps the refactor incremental while ensuring the end state has a single owner for shared interaction semantics.
