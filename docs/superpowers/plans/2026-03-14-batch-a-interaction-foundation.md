# Batch A Interaction Foundation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the internal interaction foundation for Batch A by introducing `FocusTarget`, `Action`, centralized key-to-action resolution, and focus-aware action dispatch without adding TOML keybinding config.

**Architecture:** Keep `UiMode` as the rendering and state container, but derive a focused widget from it and route shared actions through a single dispatcher. Preserve existing `Intent`-based work-item operations and existing text-entry mutation helpers, while moving shared navigation, confirm, dismiss, help, and tab behavior out of per-mode key match arms.

**Tech Stack:** Rust, `crossterm` key events, `tui_input`, existing `Intent` resolution, ratatui TUI app state, crate-local unit tests in `flotilla-tui`.

---

## File Structure

- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
  - Add `FocusTarget` and mode-to-focus derivation plus unit tests.
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
  - Add `Action`, default key resolution, `dispatch_action()`, and focused regression tests.
- Modify: `crates/flotilla-tui/src/app/navigation.rs`
  - Reuse or extract small helpers for focus-routed list/menu navigation if current helpers are too table-specific.
- Modify: `crates/flotilla-tui/src/app/file_picker.rs`
  - Reuse file-picker activation and selection helpers from the shared action dispatcher without duplicating logic.
- Modify: `crates/flotilla-tui/src/app/mod.rs`
  - Add or expose small app helpers needed by dismiss/confirm dispatch.

## Chunk 1: Focus Model Foundation

### Task 1: Add failing tests for `FocusTarget` coverage

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Test: `crates/flotilla-tui/src/app/ui_state.rs`

- [ ] **Step 1: Write the failing tests**

Add tests covering:
- `UiMode::Normal` maps to `FocusTarget::WorkItemTable`
- `UiMode::Config` maps to `FocusTarget::EventLog`
- help, menu, branch input, issue search, file picker, delete confirm, and close confirm each map to the expected focus target

- [ ] **Step 2: Run the targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked ui_state::tests::focus_target -- --nocapture`
Expected: FAIL because `FocusTarget` and the mapping helper do not exist yet.

- [ ] **Step 3: Implement the minimal focus model**

Add:
- a new `FocusTarget` enum in `ui_state.rs`
- a `UiMode::focus_target(&self) -> FocusTarget` helper
- any derives needed for test assertions

- [ ] **Step 4: Run the targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked ui_state::tests::focus_target -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs
git commit -m "refactor: add focus target model"
```

## Chunk 2: Action Resolution Layer

### Task 2: Add failing tests for key-to-action resolution

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Write the failing tests**

Add tests covering:
- `j` / `Down` resolve to `Action::SelectNext`
- `k` / `Up` resolve to `Action::SelectPrev`
- `Enter` resolves to `Action::Confirm`
- `Esc` resolves to `Action::Dismiss`
- `?` resolves to `Action::ToggleHelp`
- `q` resolves to `Action::Quit` in `UiMode::Normal`
- `q` resolves to `Action::Dismiss` in `UiMode::Config` and `UiMode::Help`
- `q` is not intercepted in text-entry modes such as manual branch input
- `d` and `p` resolve to `Action::Dispatch(...)`

- [ ] **Step 2: Run the targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::resolve_action -- --nocapture`
Expected: FAIL because `Action` and `resolve_action()` do not exist yet.

- [ ] **Step 3: Implement the minimal action enum and resolver**

Add:
- a new `Action` enum in `key_handlers.rs`
- `resolve_action(&self, key: KeyEvent) -> Option<Action>`
- mode-aware `q` handling as specified in the design
- explicit non-interception for manual text-entry cases that should keep raw character input

- [ ] **Step 4: Run the targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::resolve_action -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "refactor: add key action resolution"
```

### Task 3: Add failing tests for top-level help, quit, and dismiss normalization

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Write the failing tests**

Cover:
- `ToggleHelp` opens help from Normal and closes help back to Normal while resetting scroll
- `Dismiss` in Config returns to Normal instead of quitting
- `Dismiss` in Normal preserves the existing cascade order
- `Quit` from Normal still quits immediately

- [ ] **Step 2: Run the targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::dismiss -- --nocapture`
Expected: FAIL because shared action dispatch is not wired yet.

- [ ] **Step 3: Implement minimal shared behavior helpers**

Add:
- a `dispatch_action()` entry point for `ToggleHelp`, `Dismiss`, and `Quit`
- a small helper for the Normal-mode dismiss cascade if that makes the code easier to test
- top-level `handle_key()` wiring that prefers resolved shared actions before falling back to raw mode-specific input handling

- [ ] **Step 4: Run the targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::dismiss -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "refactor: normalize dismiss and help actions"
```

## Chunk 3: Focus-Aware Dispatch

### Task 4: Add failing tests for focus-routed navigation actions

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/navigation.rs`
- Modify: `crates/flotilla-tui/src/app/file_picker.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`
- Test: `crates/flotilla-tui/src/app/file_picker.rs`

- [ ] **Step 1: Write the failing tests**

Add tests proving:
- `SelectNext` / `SelectPrev` move the work-item table selection in Normal mode
- the same actions scroll help text in Help mode
- the same actions move event-log selection in Config mode
- the same actions move the highlighted action-menu item
- the same actions move file-picker selection without duplicating file-picker behavior

- [ ] **Step 2: Run the targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::select_next -- --nocapture`
Expected: FAIL because navigation still lives in separate mode-specific handlers.

- [ ] **Step 3: Implement minimal focus-routed navigation**

Update `dispatch_action()` so `SelectNext` and `SelectPrev` route by `FocusTarget`, reusing existing helpers where possible. If current helpers are too specialized, extract small focused helpers instead of copying logic inline.

- [ ] **Step 4: Run the targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::select_next -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/navigation.rs crates/flotilla-tui/src/app/file_picker.rs
git commit -m "refactor: route shared navigation by focus target"
```

### Task 5: Add failing tests for focus-routed confirm and dismiss actions

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/file_picker.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`
- Test: `crates/flotilla-tui/src/app/file_picker.rs`

- [ ] **Step 1: Write the failing tests**

Cover:
- `Confirm` in Normal mode triggers `action_enter()`
- `Confirm` in ActionMenu executes the highlighted action and preserves a newly-entered replacement mode
- `Confirm` in `DeleteConfirm` respects the loading guard
- `Confirm` in `CloseConfirm` queues the close command
- `Confirm` in manual branch input submits the current branch
- `Confirm` in issue search dispatches search and stores the active query
- `Confirm` in file picker activates the selected entry
- `Dismiss` closes menu, file picker, branch input, issue search, delete confirm, and close confirm

- [ ] **Step 2: Run the targeted tests to verify failure**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::confirm -- --nocapture`
Expected: FAIL because confirm and dismiss are still handled per mode.

- [ ] **Step 3: Implement minimal focus-routed confirm and dismiss**

Update `dispatch_action()` so shared confirm and dismiss behavior routes by `FocusTarget`, but keep mode-specific helper functions for payload assembly and raw `tui_input` editing.

Concrete implementation targets:
- reuse `execute_menu_action()`
- reuse or extract delete/close confirm submit helpers
- reuse or extract branch-input submit and issue-search submit helpers
- expose file-picker activation as a helper callable from shared dispatch

- [ ] **Step 4: Run the targeted tests to verify pass**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests::confirm -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/file_picker.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "refactor: route confirm and dismiss by focus target"
```

### Task 6: Remove redundant per-mode key logic and run focused verification

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Possibly modify: `crates/flotilla-tui/src/app/file_picker.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`
- Test: `crates/flotilla-tui/src/app/file_picker.rs`
- Test: `crates/flotilla-tui/src/app/ui_state.rs`

- [ ] **Step 1: Write any final regression tests for leftover edge cases**

Add or tighten tests for:
- branch input generating mode ignores raw input and confirm
- issue-search dismiss still clears active query
- action-menu execution does not get overwritten by an unconditional reset to Normal
- `q` still types literal text in manual text-entry modes

- [ ] **Step 2: Run the targeted tests to verify current failure or coverage gap**

Run: `cargo test -p flotilla-tui --locked key_handlers::tests -- --nocapture`
Expected: FAIL or reveal uncovered edge cases before cleanup is complete.

- [ ] **Step 3: Delete or simplify redundant match-arm logic**

Remove now-obsolete shared-key handling from:
- the top-level `handle_key()` match on `UiMode`
- help-specific `j/k/Esc/?` handling
- config-specific `j/k/Esc/q` duplication
- action-menu `Esc` / `Enter` / `j` / `k` duplication
- confirm-dialog shared-key duplication

Keep only the true text-editing and mode-local data mutation that the shared dispatcher does not own yet.

- [ ] **Step 4: Run the focused TUI test suite**

Run: `cargo test -p flotilla-tui --locked ui_state::tests::focus_target -- --nocapture`
Expected: PASS.

Run: `cargo test -p flotilla-tui --locked key_handlers::tests -- --nocapture`
Expected: PASS.

Run: `cargo test -p flotilla-tui --locked file_picker::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the package test suite**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/navigation.rs crates/flotilla-tui/src/app/file_picker.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "refactor: centralize tui interaction dispatch"
```

## Notes For Execution

- Do not add TOML loading, `crokey`, or auto-generated help in this plan.
- Prefer extracting small helper methods over adding another large switch statement.
- Keep `Intent` intact; only `Action::Dispatch(Intent)` should bridge into it.
- If a test name in the commands above ends up slightly different, preserve the same scope and intent rather than broadening the command to the whole workspace too early.

Plan complete and saved to `docs/superpowers/plans/2026-03-14-batch-a-interaction-foundation.md`. Ready to execute?
