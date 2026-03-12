# Preview Layout Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add dynamic preview positioning, explicit preview mode cycling, preview hide/show, and persisted app-wide preview preferences to the TUI.

**Architecture:** Keep the change localized to the TUI by adding a small app-level preview preference state to `UiState`, resolving the effective preview layout inside `ui.rs`, and wiring config persistence through `ConfigStore` and `App::new`. Test the layout policy directly and verify rendering with snapshot tests at fixed terminal sizes.

**Tech Stack:** Rust, ratatui, crossterm, serde/toml config loading, insta snapshot tests

**Spec:** `docs/superpowers/specs/2026-03-12-preview-layout-design.md`

---

## Chunk 1: Preview State and Layout Policy

### Task 1: Add preview mode state to `UiState`

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Test: `crates/flotilla-tui/src/app/ui_state.rs`

- [ ] **Step 1: Write the failing state tests**

Add tests for:

```rust
#[test]
fn preview_position_mode_cycles_auto_right_below_auto() {}

#[test]
fn preview_visibility_toggle_flips_boolean() {}

#[test]
fn ui_state_defaults_to_visible_auto_preview() {}
```

These should assert:
- default mode is `Auto`
- default visibility is `true`
- one cycle advances `Auto -> Right -> Below -> Auto`
- toggling visibility flips the stored flag without affecting the mode

- [ ] **Step 2: Run the focused tests to verify they fail**

Run:

```bash
cargo test -p flotilla-tui preview_position_mode_cycles_auto_right_below_auto ui_state_defaults_to_visible_auto_preview preview_visibility_toggle_flips_boolean
```

Expected: FAIL because the preview state and helpers do not exist yet.

- [ ] **Step 3: Add preview state and helpers**

In `crates/flotilla-tui/src/app/ui_state.rs`:
- add `PreviewPositionMode` enum with `Auto`, `Right`, `Below`
- add an app-level `PreviewState`/`PreviewPreferences` struct with `position_mode` and `visible`
- add default implementations so new `UiState` instances start in `Auto` and visible
- add small helper methods such as `cycle_preview_position_mode()` and `toggle_preview_visibility()`

Keep this state on `UiState`, not `RepoUiState`.

- [ ] **Step 4: Re-run the focused tests**

Run the same command as Step 2.

Expected: PASS

### Task 2: Add testable preview layout resolution logic

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`
- Test: `crates/flotilla-tui/src/ui.rs` or `crates/flotilla-tui/tests/snapshots.rs` if the module already keeps policy tests there

- [ ] **Step 1: Write the failing layout-policy tests**

Add direct tests for a resolver function with fixed terminal dimensions:

```rust
#[test]
fn auto_preview_prefers_right_when_wide_layout_meets_minimums() {}

#[test]
fn auto_preview_prefers_below_when_only_vertical_layout_meets_minimums() {}

#[test]
fn explicit_right_mode_ignores_terminal_shape() {}

#[test]
fn explicit_below_mode_ignores_terminal_shape() {}
```

These tests should cover:
- `Auto` chooses `Right` on a wide terminal
- `Auto` chooses `Below` on a tall or narrow terminal
- explicit modes bypass auto resolution
- the hidden state is separate from the resolved position

- [ ] **Step 2: Run the focused resolver tests to verify they fail**

Run:

```bash
cargo test -p flotilla-tui auto_preview_prefers_right_when_wide_layout_meets_minimums auto_preview_prefers_below_when_only_vertical_layout_meets_minimums explicit_right_mode_ignores_terminal_shape explicit_below_mode_ignores_terminal_shape
```

Expected: FAIL because the resolver does not exist yet.

- [ ] **Step 3: Implement the resolver in `ui.rs`**

In `crates/flotilla-tui/src/ui.rs`:
- add constants for minimum table width, minimum preview width, minimum table height, minimum preview height, and aspect-ratio threshold
- add a small internal resolver that accepts content `Rect` plus `PreviewPositionMode`
- return an effective layout enum such as `Right` or `Below`
- use the decision order from the spec:
  - if one candidate meets minimums and the other does not, choose the viable one
  - if both meet minimums, use aspect ratio as the tiebreaker
  - if neither meets minimums, fall back to `Right`

- [ ] **Step 4: Re-run the focused resolver tests**

Run the same command as Step 2.

Expected: PASS

- [ ] **Step 5: Commit Chunk 1**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/ui.rs
git commit -m "feat: add preview layout state and resolver"
```

## Chunk 2: Rendering and Interaction

### Task 3: Wire the preview state into normal-mode key handling

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Test: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Write the failing key-handler tests**

Add tests for:

```rust
#[test]
fn v_cycles_preview_position_mode_in_normal_mode() {}

#[test]
fn uppercase_p_toggles_preview_visibility_in_normal_mode() {}
```

Assert that:
- `v` changes `Auto -> Right -> Below -> Auto`
- `P` flips visibility
- neither key changes `UiMode`

- [ ] **Step 2: Run the focused key-handler tests to verify they fail**

Run:

```bash
cargo test -p flotilla-tui v_cycles_preview_position_mode_in_normal_mode uppercase_p_toggles_preview_visibility_in_normal_mode
```

Expected: FAIL because the key handlers do not map those keys yet.

- [ ] **Step 3: Implement the key handlers**

In `crates/flotilla-tui/src/app/key_handlers.rs`:
- add `KeyCode::Char('v')` to cycle preview position mode
- add `KeyCode::Char('P')` to toggle preview visibility
- keep the behavior limited to `UiMode::Normal`

- [ ] **Step 4: Re-run the focused key-handler tests**

Run the same command as Step 2.

Expected: PASS

### Task 4: Update rendering for hidden, right, and below preview states

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
- Modify: `crates/flotilla-tui/tests/snapshots.rs`
- Test: `crates/flotilla-tui/tests/snapshots/*.snap`

- [ ] **Step 1: Write the failing snapshot tests**

In `crates/flotilla-tui/tests/support/mod.rs`:
- add harness helpers to set preview mode/visibility
- add a `with_width()` helper so tests can force narrow layouts as well as tall ones

In `crates/flotilla-tui/tests/snapshots.rs`, add snapshots for:
- visible preview on the right
- visible preview below on a tall or narrow terminal
- hidden preview
- updated help text
- updated status bar text showing preview state

- [ ] **Step 2: Run the snapshot tests to verify they fail**

Run:

```bash
cargo test -p flotilla-tui --test snapshots
```

Expected: FAIL with missing or changed snapshots.

- [ ] **Step 3: Implement rendering and status/help text**

In `crates/flotilla-tui/src/ui.rs`:
- update `render_content` so hidden preview renders the table full-width/full-height
- when visible, resolve the effective preview position and split horizontally or vertically
- keep the existing preview/debug split inside the preview region
- update the normal-mode status bar to surface compact preview state text
- update the help overlay with `v` and `P`

- [ ] **Step 4: Accept the new snapshots after reviewing them**

Run:

```bash
cargo test -p flotilla-tui --test snapshots
```

Expected: PASS after updating the approved snapshot files.

- [ ] **Step 5: Commit Chunk 2**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/ui.rs crates/flotilla-tui/tests/support/mod.rs crates/flotilla-tui/tests/snapshots.rs crates/flotilla-tui/tests/snapshots
git commit -m "feat: add preview layout controls to the tui"
```

## Chunk 3: Persisted Preferences and Docs

### Task 5: Persist preview preferences in global config

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Test: `crates/flotilla-core/src/config.rs`

- [ ] **Step 1: Write the failing config tests**

In `crates/flotilla-core/src/config.rs`, add tests that:
- missing preview config falls back to visible `Auto`
- valid preview config parses `auto`, `right`, `below`
- visibility flag parses and defaults correctly

If the final implementation adds explicit save helpers, also add tests that writing the preview config creates the base directory and preserves unrelated config fields.

- [ ] **Step 2: Run the focused config tests to verify they fail**

Run:

```bash
cargo test -p flotilla-core load_config_missing_or_invalid_returns_defaults load_config_parses_preview_preferences
```

Expected: FAIL because preview config fields do not exist yet.

- [ ] **Step 3: Implement config structs and app initialization**

In `crates/flotilla-core/src/config.rs`:
- extend `FlotillaConfig` with a UI section for preview preferences
- add serde-backed types for preview mode and visibility
- keep defaults aligned with `UiState` defaults
- add load/save helpers if needed for user-triggered updates

In `crates/flotilla-tui/src/app/mod.rs`:
- initialize `UiState` from loaded config when constructing `App`
- persist preview preference changes after `v` and `P` actions, using `ConfigStore`

Keep persistence app-wide in the global config file rather than per-repo files.

- [ ] **Step 4: Re-run the focused config tests**

Run the same command as Step 2.

Expected: PASS

### Task 6: Update user-facing docs and run final verification

**Files:**
- Modify: `docs/keybindings.md`
- Verify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Verify: `crates/flotilla-tui/src/ui.rs`
- Verify: `crates/flotilla-core/src/config.rs`

- [ ] **Step 1: Update docs**

In `docs/keybindings.md`:
- add `v` for preview mode cycle
- add `P` for preview show/hide
- describe the behavior consistently with the help overlay

- [ ] **Step 2: Run formatting**

Run:

```bash
cargo fmt --check
```

Expected: PASS

- [ ] **Step 3: Run targeted tests**

Run:

```bash
cargo test -p flotilla-tui
cargo test -p flotilla-core config
```

Expected: PASS

- [ ] **Step 4: Run workspace verification**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 5: Commit Chunk 3**

```bash
git add crates/flotilla-core/src/config.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/ui_state.rs docs/keybindings.md
git commit -m "feat: persist preview layout preferences"
```
