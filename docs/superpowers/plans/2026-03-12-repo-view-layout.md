# RepoViewLayout Refactor Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify `PreviewPositionMode` + `visible: bool` into a single `RepoViewLayout` enum with variants `Auto | Zoom | Right | Below`, cycled with `l`.

**Architecture:** Replace the two-field `PreviewState` struct with a single `RepoViewLayout` enum on `UiState`. The `Zoom` variant replaces the old `visible = false` state. Config persistence simplifies from two fields to one `layout` field. The `v` and `P` key bindings collapse into `l`.

**Tech Stack:** Rust, ratatui, serde/toml for config

**Note:** `UiState` already has a `layout: LayoutAreas` field (mouse hit-testing areas), so the new field is named `view_layout`.

---

## Chunk 1: Atomic type + consumer migration

This is a single atomic change across all crates — the old types are deleted and all consumers updated in one commit so the workspace never breaks.

### Task 1: Replace config types in flotilla-core

**Files:**
- Modify: `crates/flotilla-core/src/config.rs:58-86` (config structs + `default_true`)
- Modify: `crates/flotilla-core/src/config.rs:282-313` (`save_preview_preferences` → `save_layout`)
- Modify: `crates/flotilla-core/src/config.rs:570-630` (config tests)

- [ ] **Step 1: Replace `PreviewPositionModeConfig` with `RepoViewLayoutConfig`**

```rust
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepoViewLayoutConfig {
    #[default]
    Auto,
    Zoom,
    Right,
    Below,
}
```

- [ ] **Step 2: Replace `PreviewConfig` struct**

Remove the `visible` field and `default_true()` helper. Replace with:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreviewConfig {
    #[serde(default)]
    pub layout: RepoViewLayoutConfig,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            layout: RepoViewLayoutConfig::default(),
        }
    }
}
```

Delete the `default_true()` function (lines 84-86) — it was only used for the `visible` field.

- [ ] **Step 3: Replace `save_preview_preferences` with `save_layout`**

```rust
pub fn save_layout(&self, layout: RepoViewLayoutConfig) {
    let path = self.base.join("config.toml");
    let mut config = self.load_config();
    config.ui.preview.layout = layout;

    if let Err(err) = std::fs::create_dir_all(&self.base) {
        tracing::warn!(path = %self.base.display(), err = %err, "failed to create config dir");
        return;
    }

    let content = match toml::to_string_pretty(&config) {
        Ok(content) => content,
        Err(err) => {
            tracing::warn!(path = %path.display(), err = %err, "failed to serialize config");
            return;
        }
    };

    if let Err(err) = std::fs::write(&path, content) {
        tracing::warn!(path = %path.display(), err = %err, "failed to write config");
        return;
    }

    if let Some(cached) = self.global_config.get() {
        *cached.lock().expect("config cache mutex poisoned") = config;
    }
}
```

- [ ] **Step 4: Replace the three config tests**

```rust
#[test]
fn load_config_parses_layout() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[ui.preview]\nlayout = \"zoom\"\n",
    )
    .unwrap();

    let store = ConfigStore::with_base(dir.path());
    let cfg = store.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Zoom);
}

#[test]
fn save_layout_writes_global_config() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[vcs.git.checkouts]\nprovider = \"worktree\"\n",
    )
    .unwrap();

    let store = ConfigStore::with_base(dir.path());
    store.save_layout(RepoViewLayoutConfig::Right);

    let reloaded = ConfigStore::with_base(dir.path());
    let cfg = reloaded.load_config();
    assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
}

#[test]
fn save_layout_updates_same_store_cache() {
    let dir = tempdir().unwrap();
    let store = ConfigStore::with_base(dir.path());

    assert_eq!(
        store.load_config().ui.preview.layout,
        RepoViewLayoutConfig::Auto
    );

    store.save_layout(RepoViewLayoutConfig::Below);

    let cfg = store.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Below);
}
```

- [ ] **Step 5: Verify flotilla-core compiles and tests pass**

Run: `cargo test -p flotilla-core --locked`

---

### Task 2: Replace UI types in ui_state.rs

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:67-89` (enum + struct definitions)
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:219-264` (UiState fields + methods)
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:330-394` (tests)
- Modify: `crates/flotilla-tui/src/app/mod.rs:28-31` (pub use exports)

- [ ] **Step 1: Delete old types, add `RepoViewLayout`**

Delete `PreviewPositionMode` enum (lines 67-73), `PreviewState` struct (lines 75-79), and its `Default` impl (lines 81-88). Add:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepoViewLayout {
    #[default]
    Auto,
    Zoom,
    Right,
    Below,
}
```

- [ ] **Step 2: Update `UiState`**

Replace `pub preview: PreviewState` (line 222) with `pub view_layout: RepoViewLayout`.

In `UiState::new()` (line 240): replace `preview: PreviewState::default()` with `view_layout: RepoViewLayout::default()`.

- [ ] **Step 3: Replace methods**

Delete `cycle_preview_position_mode()` and `toggle_preview_visibility()`. Add:

```rust
pub fn cycle_layout(&mut self) {
    self.view_layout = match self.view_layout {
        RepoViewLayout::Auto => RepoViewLayout::Zoom,
        RepoViewLayout::Zoom => RepoViewLayout::Right,
        RepoViewLayout::Right => RepoViewLayout::Below,
        RepoViewLayout::Below => RepoViewLayout::Auto,
    };
}
```

- [ ] **Step 4: Update `pub use` in `app/mod.rs`**

Line 28-31: replace `PreviewPositionMode, PreviewState` with `RepoViewLayout`.

- [ ] **Step 5: Replace tests**

Replace `new_with_empty_paths` assertions (lines 334-336): change `state.preview.visible` / `state.preview.position_mode` to `state.view_layout == RepoViewLayout::Auto`.

Delete the three old tests (`ui_state_defaults_to_visible_auto_preview`, `preview_position_mode_cycles_auto_right_below_auto`, `preview_visibility_toggle_flips_boolean`). Replace with:

```rust
#[test]
fn ui_state_defaults_to_auto_layout() {
    let state = UiState::new(&[]);
    assert_eq!(state.view_layout, RepoViewLayout::Auto);
}

#[test]
fn layout_cycles_auto_zoom_right_below_auto() {
    let mut state = UiState::new(&[]);

    state.cycle_layout();
    assert_eq!(state.view_layout, RepoViewLayout::Zoom);

    state.cycle_layout();
    assert_eq!(state.view_layout, RepoViewLayout::Right);

    state.cycle_layout();
    assert_eq!(state.view_layout, RepoViewLayout::Below);

    state.cycle_layout();
    assert_eq!(state.view_layout, RepoViewLayout::Auto);
}
```

---

### Task 3: Update App config bridge in mod.rs

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs:18` (import)
- Modify: `crates/flotilla-tui/src/app/mod.rs:185-213` (App::new + persist method)
- Modify: `crates/flotilla-tui/src/app/mod.rs:668-711` (tests)

- [ ] **Step 1: Update import**

Line 18: replace `PreviewPositionModeConfig` with `RepoViewLayoutConfig`.

- [ ] **Step 2: Update `App::new()` config loading**

Replace the preview loading block (lines 187-193) with:

```rust
ui.view_layout = match loaded_config.ui.preview.layout {
    RepoViewLayoutConfig::Auto => RepoViewLayout::Auto,
    RepoViewLayoutConfig::Zoom => RepoViewLayout::Zoom,
    RepoViewLayoutConfig::Right => RepoViewLayout::Right,
    RepoViewLayoutConfig::Below => RepoViewLayout::Below,
};
```

(Delete the `ui.preview.visible = preview.visible;` line.)

- [ ] **Step 3: Replace `persist_preview_preferences` with `persist_layout`**

```rust
pub fn persist_layout(&self) {
    let layout = match self.ui.view_layout {
        RepoViewLayout::Auto => RepoViewLayoutConfig::Auto,
        RepoViewLayout::Zoom => RepoViewLayoutConfig::Zoom,
        RepoViewLayout::Right => RepoViewLayoutConfig::Right,
        RepoViewLayout::Below => RepoViewLayoutConfig::Below,
    };
    self.config.save_layout(layout);
}
```

- [ ] **Step 4: Replace the two App config tests**

```rust
#[test]
fn app_new_loads_layout_from_config() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[ui.preview]\nlayout = \"below\"\n",
    )
    .unwrap();

    let daemon: Arc<dyn DaemonHandle> = Arc::new(TestDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let app = App::new(
        daemon,
        vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())],
        config,
    );

    assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
}

#[test]
fn persist_layout_writes_current_ui_state() {
    let dir = tempdir().unwrap();
    let daemon: Arc<dyn DaemonHandle> = Arc::new(TestDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let mut app = App::new(
        daemon,
        vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())],
        config,
    );

    app.ui.view_layout = RepoViewLayout::Right;
    app.persist_layout();

    let reloaded = ConfigStore::with_base(dir.path());
    let cfg = reloaded.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
}
```

---

### Task 4: Update key bindings — replace `v`/`P` with `l`

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:101-108` (key handler)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:1377-1412` (tests)

- [ ] **Step 1: Replace key handler arms**

Replace the `v` and `P` arms (lines 101-108) with:

```rust
KeyCode::Char('l') => {
    self.ui.cycle_layout();
    self.persist_layout();
}
```

- [ ] **Step 2: Replace the two key handler tests**

Delete `v_cycles_preview_position_mode_in_normal_mode` and `uppercase_p_toggles_preview_visibility_in_normal_mode`. Replace with:

```rust
#[test]
fn l_cycles_layout_in_normal_mode() {
    let mut app = stub_app();
    assert_eq!(app.ui.view_layout, super::super::RepoViewLayout::Auto);

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, super::super::RepoViewLayout::Zoom);
    assert!(matches!(app.ui.mode, UiMode::Normal));

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, super::super::RepoViewLayout::Right);
    assert!(matches!(app.ui.mode, UiMode::Normal));

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, super::super::RepoViewLayout::Below);
    assert!(matches!(app.ui.mode, UiMode::Normal));

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, super::super::RepoViewLayout::Auto);
    assert!(matches!(app.ui.mode, UiMode::Normal));
}
```

---

### Task 5: Update rendering — resolve layout + status bar + help

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:18` (import: `PreviewPositionMode` → `RepoViewLayout`)
- Modify: `crates/flotilla-tui/src/ui.rs:46-84` (ResolvedPreviewPosition, resolve functions)
- Modify: `crates/flotilla-tui/src/ui.rs:280-326` (status bar)
- Modify: `crates/flotilla-tui/src/ui.rs:336-366` (render_content)
- Modify: `crates/flotilla-tui/src/ui.rs:1045-1046` (help text)
- Modify: `crates/flotilla-tui/src/ui.rs:1088-1098` (`preview_status_text` → `layout_status_text`)
- Modify: `crates/flotilla-tui/src/ui.rs:1316-1346` (resolution tests)

- [ ] **Step 1: Update import**

Line 18: replace `PreviewPositionMode` with `RepoViewLayout` in the `use crate::app::` import.

- [ ] **Step 2: Update `resolve_preview_position`**

Change return type to `Option<ResolvedPreviewPosition>`. `Zoom` returns `None`:

```rust
fn resolve_preview_position(area: Rect, layout: RepoViewLayout) -> Option<ResolvedPreviewPosition> {
    match layout {
        RepoViewLayout::Right => Some(ResolvedPreviewPosition::Right),
        RepoViewLayout::Below => Some(ResolvedPreviewPosition::Below),
        RepoViewLayout::Auto => Some(resolve_auto_preview_position(area)),
        RepoViewLayout::Zoom => None,
    }
}
```

- [ ] **Step 3: Update `render_content`**

Replace the visibility check + position resolution (lines 342-365) with:

```rust
fn render_content(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    if ui.mode.is_config() {
        render_config_screen(model, ui, frame, area);
        return;
    }

    let Some(position) = resolve_preview_position(area, ui.view_layout) else {
        render_unified_table(model, ui, frame, area);
        return;
    };

    let chunks = match position {
        ResolvedPreviewPosition::Right => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(100 - PREVIEW_SPLIT_RIGHT_PERCENT),
                Constraint::Percentage(PREVIEW_SPLIT_RIGHT_PERCENT),
            ])
            .split(area),
        ResolvedPreviewPosition::Below => Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(100 - PREVIEW_SPLIT_BELOW_PERCENT),
                Constraint::Percentage(PREVIEW_SPLIT_BELOW_PERCENT),
            ])
            .split(area),
    };

    render_unified_table(model, ui, frame, chunks[0]);
    render_preview(model, ui, frame, chunks[1]);
}
```

- [ ] **Step 4: Replace `preview_status_text` with `layout_status_text`**

```rust
fn layout_status_text(ui: &UiState) -> &'static str {
    match ui.view_layout {
        RepoViewLayout::Auto => "Layout(l): auto",
        RepoViewLayout::Zoom => "Layout(l): zoom",
        RepoViewLayout::Right => "Layout(l): right",
        RepoViewLayout::Below => "Layout(l): below",
    }
}
```

- [ ] **Step 5: Update status bar**

In `render_status_bar`:
- Line 280: rename `preview_status` to `layout_status` and call `layout_status_text(ui)`.
- Lines 301, 304, 308: replace `{preview_status}` with `{layout_status}`.
- Line 323: replace `s.push_str("  v:preview  P:hide");` with nothing (remove the line — the layout hint is already in the `layout_status` string).
- Line 325: replace `preview_status` with `layout_status`.

- [ ] **Step 6: Update help text**

Lines 1045-1046: replace the two lines with a single line:

```rust
Line::from("  l                Cycle layout (auto/zoom/right/below)"),
```

- [ ] **Step 7: Replace resolution tests**

Update the test import (line 1316): `PreviewPositionMode` → `RepoViewLayout`.

Delete the four old tests. Replace with:

```rust
#[test]
fn auto_layout_prefers_right_when_wide() {
    let position =
        resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Auto);
    assert_eq!(position, Some(ResolvedPreviewPosition::Right));
}

#[test]
fn auto_layout_prefers_below_when_tall() {
    let position =
        resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Auto);
    assert_eq!(position, Some(ResolvedPreviewPosition::Below));
}

#[test]
fn explicit_right_layout() {
    let position =
        resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Right);
    assert_eq!(position, Some(ResolvedPreviewPosition::Right));
}

#[test]
fn explicit_below_layout() {
    let position =
        resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Below);
    assert_eq!(position, Some(ResolvedPreviewPosition::Below));
}

#[test]
fn zoom_layout_returns_none() {
    let position =
        resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Zoom);
    assert_eq!(position, None);
}
```

---

### Task 6: Update snapshot tests

**Files:**
- Modify: `crates/flotilla-tui/tests/support/mod.rs:14,95-103`
- Modify: `crates/flotilla-tui/tests/snapshots.rs:5,140-188`

- [ ] **Step 1: Update test helpers**

In `support/mod.rs`:
- Line 14: replace `PreviewPositionMode` import with `RepoViewLayout`.
- Replace `with_preview_mode()` and `with_preview_visible()` (lines 95-103) with:

```rust
pub fn with_layout(mut self, layout: RepoViewLayout) -> Self {
    self.ui.view_layout = layout;
    self
}
```

- [ ] **Step 2: Update snapshot test code**

In `snapshots.rs`:
- Line 5: replace `PreviewPositionMode` import with `RepoViewLayout`.
- `selected_item_preview_below` (line 156): change `.with_preview_mode(PreviewPositionMode::Below)` to `.with_layout(RepoViewLayout::Below)`.
- `hidden_preview_uses_full_content_area` (line 164): rename to `zoom_layout_uses_full_content_area`, change `.with_preview_visible(false)` to `.with_layout(RepoViewLayout::Zoom)`.
- `status_bar_preview_state` (line 183): rename to `status_bar_layout_state`, change `.with_preview_mode(PreviewPositionMode::Below)` to `.with_layout(RepoViewLayout::Below)`.

---

### Task 7: Verify and commit atomically

- [ ] **Step 1: Run full test suite**

```bash
cargo test --workspace --locked
```

This will fail for snapshot tests because the status bar text changed. That's expected.

- [ ] **Step 2: Update snapshots**

```bash
cargo insta test --workspace --accept --unreferenced=delete
```

Many snapshots will update — every Normal-mode status bar will change from `preview:auto` / `v:preview  P:hide` to `Layout(l): auto`. Renamed tests will produce new snap files; old ones (`snapshots__hidden_preview_uses_full_content_area.snap`, `snapshots__status_bar_preview_state.snap`) will be deleted by `--unreferenced=delete`.

- [ ] **Step 3: Run clippy and fmt**

```bash
cargo fmt && cargo clippy --all-targets --locked -- -D warnings
```

- [ ] **Step 4: Verify no stale references**

Search for: `PreviewPositionMode`, `PreviewState`, `preview.position_mode`, `preview.visible`, `toggle_preview_visibility`, `cycle_preview_position_mode`, `save_preview_preferences`, `persist_preview_preferences`, `default_true`, `with_preview_mode`, `with_preview_visible`.

- [ ] **Step 5: Run full test suite again**

```bash
cargo test --workspace --locked
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: unify PreviewPositionMode + visible into RepoViewLayout enum

Replace the two-field PreviewState (position_mode + visible) with a single
RepoViewLayout enum: Auto, Zoom, Right, Below. Cycle with 'l' key instead
of 'v'/'P'. Status bar shows Layout(l): {variant}. Config simplifies to a
single layout field."
```
