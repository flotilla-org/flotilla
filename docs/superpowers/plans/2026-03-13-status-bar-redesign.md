# Status Bar Redesign Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Redesign the TUI status bar into adaptive `status / keys / task` sections with clickable key labels, dismissable errors, a hideable keys section, and a Zellij-inspired ribbon style.

**Architecture:** Extract status-bar-specific layout and hit-testing into a focused module that builds a render model from app state, then render that model from `ui.rs` as chevron-separated ribbons and route clicks through dedicated bottom-row hit areas in `key_handlers.rs`. Keep dismissal UI-local, keep keyboard shortcuts as the source of truth, and drive the whole change test-first with pure builder tests plus interaction and snapshot coverage.

**Tech Stack:** Rust, ratatui, crossterm mouse events, insta snapshot tests

---

## File Structure

- Create: `crates/flotilla-tui/src/status_bar.rs`
  Purpose: status-bar view model, section builders, width-allocation logic, ribbon styling helpers, spinner state, and status-bar hit-target types.
- Modify: `crates/flotilla-tui/src/lib.rs`
  Purpose: export the new `status_bar` module.
- Modify: `crates/flotilla-tui/src/ui.rs`
  Purpose: replace early-return status-bar rendering with a builder-driven segmented renderer.
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
  Purpose: add status-bar visibility and dedicated hit-test storage.
- Modify: `crates/flotilla-tui/src/app/mod.rs`
  Purpose: add UI-local dismissal state/helpers and any status-bar action helpers shared between keyboard and mouse.
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
  Purpose: add `K` handling and route bottom-row clicks through status-bar hit areas.
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
  Purpose: add focused app-level helpers for mouse/key behavior tests if needed.
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
  Purpose: extend the snapshot harness for status-bar test setup.
- Modify: `crates/flotilla-tui/tests/snapshots.rs`
  Purpose: add status-bar rendering snapshots for wide/narrow/error/task/hidden-key states.
- Modify: `docs/keybindings.md`
  Purpose: document the new `K` toggle and clickable status-bar affordances.

## Chunk 1: Status-Bar Model and State Scaffolding

### Task 1: Define the status-bar model through failing pure tests

**Files:**
- Create: `crates/flotilla-tui/src/status_bar.rs`
- Modify: `crates/flotilla-tui/src/lib.rs`

- [ ] **Step 1: Write failing unit tests for width priority and section visibility**

Add tests in `crates/flotilla-tui/src/status_bar.rs` for:

```rust
#[test]
fn hides_keys_before_truncating_task_or_status() {
    let model = StatusBarModel::build(StatusBarInput {
        width: 48,
        keys_visible: true,
        status: StatusSection::plain("Connected"),
        task: Some(TaskSection::new("Refreshing repository...", 0)),
        keys: vec![
            KeyChip::new("enter", "open", StatusBarAction::OpenSelected),
            KeyChip::new("/", "search", StatusBarAction::StartSearch),
            KeyChip::new("q", "quit", StatusBarAction::Quit),
        ],
    });

    assert!(model.visible_keys.len() < 3);
    assert!(model.task_text.contains("Refreshing"));
    assert!(model.status_text.contains("Connected"));
}

#[test]
fn hidden_keys_remove_middle_section_entirely() {
    let model = StatusBarModel::build(StatusBarInput {
        width: 80,
        keys_visible: false,
        status: StatusSection::plain("Ready"),
        task: None,
        keys: vec![KeyChip::new("q", "quit", StatusBarAction::Quit)],
    });

    assert!(model.visible_keys.is_empty());
}

#[test]
fn key_ribbons_use_chevron_separators_when_multiple_actions_are_visible() {
    let model = StatusBarModel::build(StatusBarInput {
        width: 80,
        keys_visible: true,
        status: StatusSection::plain("Ready"),
        task: None,
        keys: vec![
            KeyChip::new("/", "SEARCH", StatusBarAction::StartSearch),
            KeyChip::new("n", "NEW", StatusBarAction::NewBranch),
        ],
    });

    assert!(model.keys_text.contains(""));
}
```

- [ ] **Step 2: Run the new unit tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked status_bar::tests`

Expected: compile or assertion failures because `StatusBarModel`, `StatusBarInput`, and related types do not exist yet.

- [ ] **Step 3: Add the minimal status-bar module and exports**

Create the first version of `crates/flotilla-tui/src/status_bar.rs` and export it from `crates/flotilla-tui/src/lib.rs`:

```rust
pub mod status_bar;
```

Start with minimal builder types:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusBarAction {
    OpenSelected,
    StartSearch,
    Quit,
    ToggleKeys,
    OpenHelp,
    Refresh,
    NewBranch,
    OpenMenu,
    ClearError(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChip {
    pub key: String,
    pub label: String,
    pub action: StatusBarAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSection {
    pub description: String,
    pub spinner_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusSection {
    Plain(String),
    Error { id: usize, text: String },
}
```

Also add ribbon rendering constants in the same module:

```rust
pub const CHEVRON_SEPARATOR: &str = "";
```

- [ ] **Step 4: Implement the minimal builder logic to satisfy the pure tests**

Implement a `StatusBarModel::build(...)` path that:

- preserves `status` unless the width is extremely small
- drops or truncates visible key chips first
- keeps task text visible until after keys have been reduced
- removes the middle section entirely when `keys_visible == false`
- emits compact key-first ribbon text using `` separators for adjacent key actions

Keep the first implementation simple. Refine only after tests are green.

- [ ] **Step 5: Re-run the pure tests and verify they pass**

Run: `cargo test -p flotilla-tui --locked status_bar::tests`

Expected: PASS

- [ ] **Step 6: Commit the scaffolding**

```bash
git add crates/flotilla-tui/src/status_bar.rs crates/flotilla-tui/src/lib.rs
git commit -m "feat: add status bar layout model scaffolding"
```

### Task 2: Add app/UI state for key visibility, dismissals, and hit targets

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write failing tests for UI state defaults and dismissal behavior**

Add tests covering:

```rust
#[test]
fn ui_state_defaults_to_showing_status_bar_keys() {
    let ui = UiState::new(&[]);
    assert!(ui.status_bar.show_keys);
}

#[test]
fn dismissing_status_message_hides_only_that_message() {
    let mut app = test_app_with_status("rate limit exceeded");
    let id = app.visible_status_items()[0].id();

    app.dismiss_status_item(id);

    assert!(app.visible_status_items().is_empty());
}
```

- [ ] **Step 2: Run the targeted tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked app::ui_state::tests`

Expected: compile failures because the new `status_bar` UI state and dismissal helpers do not exist yet.

- [ ] **Step 3: Add focused status-bar state to `UiState` and layout hit areas**

In `crates/flotilla-tui/src/app/ui_state.rs`, add small dedicated structs:

```rust
#[derive(Default)]
pub struct StatusBarLayout {
    pub area: Rect,
    pub key_targets: Vec<StatusBarTarget>,
    pub dismiss_targets: Vec<StatusBarTarget>,
}

pub struct StatusBarUiState {
    pub show_keys: bool,
    pub dismissed_status_ids: HashSet<usize>,
}
```

Keep these separate from `tab_areas`.

- [ ] **Step 4: Add minimal app helpers for visible status items and dismissal**

In `crates/flotilla-tui/src/app/mod.rs`, add small helpers such as:

```rust
pub fn dismiss_status_item(&mut self, id: usize) {
    self.ui.status_bar.dismissed_status_ids.insert(id);
}
```

Also add a helper that converts `model.status_message` and peer-host problems into visible status items with stable IDs for the current render.

- [ ] **Step 5: Re-run the targeted tests and verify they pass**

Run: `cargo test -p flotilla-tui --locked app::ui_state::tests`

Expected: PASS

- [ ] **Step 6: Commit the state changes**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "feat: add status bar ui state and dismissals"
```

## Chunk 2: Mouse and Keyboard Interaction

### Task 3: Drive clickable chips and dismiss controls from failing app tests

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write failing interaction tests for key-chip clicks and dismiss clicks**

Add app-level tests along these lines:

```rust
#[test]
fn clicking_search_chip_enters_issue_search_mode() {
    let mut app = test_app();
    app.ui.layout.status_bar.key_targets = vec![StatusBarTarget::new(
        Rect::new(10, 29, 10, 1),
        StatusBarAction::StartSearch,
    )];

    app.handle_mouse(left_click(12, 29));

    assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
}

#[test]
fn clicking_dismiss_target_hides_visible_error() {
    let mut app = test_app_with_status("boom");
    app.ui.layout.status_bar.dismiss_targets = vec![StatusBarTarget::new(
        Rect::new(20, 29, 1, 1),
        StatusBarAction::ClearError(0),
    )];

    app.handle_mouse(left_click(20, 29));

    assert!(app.visible_status_items().is_empty());
}
```

- [ ] **Step 2: Run the interaction tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked key_handlers`

Expected: compile or assertion failures because `handle_mouse` does not consult status-bar targets yet.

- [ ] **Step 3: Add `K` handling in normal mode**

In `crates/flotilla-tui/src/app/key_handlers.rs`, add:

```rust
KeyCode::Char('K') => {
    self.ui.status_bar.show_keys = !self.ui.status_bar.show_keys;
}
```

- [ ] **Step 4: Route bottom-row mouse clicks through status-bar targets before table handling**

Add a focused helper in `key_handlers.rs`:

```rust
fn handle_status_bar_mouse(&mut self, mouse: MouseEvent) -> bool
```

Behavior:

- if a key target contains the click, dispatch its `StatusBarAction`
- if a dismiss target contains the click, dismiss the item
- return `true` if the click was handled

Call this before row-selection logic.

- [ ] **Step 5: Re-run the interaction tests and verify they pass**

Run: `cargo test -p flotilla-tui --locked key_handlers`

Expected: PASS

- [ ] **Step 6: Commit the interaction layer**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/test_support.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "feat: add status bar mouse interactions"
```

## Chunk 3: Rendering, Snapshots, and Documentation

### Task 4: Replace the renderer through failing snapshots

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

- [ ] **Step 1: Add failing snapshots for the redesigned states**

Add snapshot tests for:

- wide normal bar with visible chips
- narrow width where keys compress first
- hidden keys section
- active task section
- dismissable host error
- dismissable generic error
- Zellij-style chevron-separated keys section

Use the harness to express those states directly, for example:

```rust
#[test]
fn status_bar_hidden_keys() {
    let mut harness = TestHarness::single_repo("my-project");
    harness.ui.status_bar.show_keys = false;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 2: Run the snapshot tests to verify they fail**

Run: `cargo test -p flotilla-tui --locked snapshots::status_bar`

Expected: snapshot failures because the renderer still emits the old single-line text.

- [ ] **Step 3: Refactor `render_status_bar` to build and render segmented content**

In `crates/flotilla-tui/src/ui.rs`:

- replace early returns with one builder pass
- build `status`, `keys`, and `task` sections from current app state
- render visually separated ribbon-style key chips with `` separators
- render compact dismiss affordances for errors
- store clickable rects in `ui.layout.status_bar`

Keep the rendering one-row and avoid unrelated refactors elsewhere in `ui.rs`.

- [ ] **Step 3a: Match the approved visual treatment**

Apply the styling direction from the design doc:

- left section: white on black
- middle keys section: darker inverted ribbons
- right section: white on black
- emphasized key token inside each key ribbon

Do not spend time on broad terminal fallback support in this task. Assume reasonable glyph support and defer fallback work.

- [ ] **Step 4: Add spinner/task formatting with minimal state**

Use a simple deterministic spinner source in `status_bar.rs`:

```rust
const SPINNER_FRAMES: &[&str] = &["-", "\\", "|", "/"];
```

If there is no existing tick counter available in render state, use a placeholder first and add the smallest viable integration needed for stable tests.

- [ ] **Step 5: Re-run the snapshot tests and accept only the intended updates**

Run: `cargo test -p flotilla-tui --locked snapshots::status_bar`

Expected: PASS with updated snapshots matching the new segmented bar.

- [ ] **Step 6: Commit the renderer changes**

```bash
git add crates/flotilla-tui/src/ui.rs crates/flotilla-tui/tests/support/mod.rs crates/flotilla-tui/tests/snapshots.rs crates/flotilla-tui/tests/snapshots
git commit -m "feat: redesign tui status bar"
```

### Task 5: Document and verify the final behavior

**Files:**
- Modify: `docs/keybindings.md`

- [ ] **Step 1: Add a failing documentation-oriented assertion if helpful**

If there is an existing docs or help-text test, extend it first. Otherwise skip directly to the docs edit.

- [ ] **Step 2: Update keybindings and help references**

Document:

- `K` toggles status-bar keys
- visible status-bar labels are clickable
- errors in the status bar can be dismissed
- the keys section uses compact ribbon-style hints

- [ ] **Step 3: Run the targeted package test suite**

Run: `cargo test -p flotilla-tui --locked`

If running inside the restricted Codex sandbox and socket/native build issues appear, use:

Run: `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Expected: PASS

- [ ] **Step 4: Run formatting**

Run: `cargo fmt --check`

Expected: PASS

- [ ] **Step 5: Run clippy for the touched crate or workspace**

Run: `cargo clippy --all-targets --locked -- -D warnings`

Expected: PASS

- [ ] **Step 6: Commit docs and verification fixes**

```bash
git add docs/keybindings.md
git commit -m "docs: document status bar controls"
```

## Open Issue to Resolve During Execution

- No unresolved key collision remains for the keys toggle; use uppercase `K` as the default and keep lowercase `k` navigation unchanged until `#218` keybinding work lands.
- Nerd Font chevrons are the explicit first-pass target. Terminal fallback behavior is intentionally deferred unless a small targeted workaround is needed for a known broken environment.
