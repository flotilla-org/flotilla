# Command Palette (Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `/`-activated command palette that lists no-arg commands with prefix filtering, arrow navigation, and Enter to execute.

**Architecture:** New `UiMode::CommandPalette` variant holds input state, filtered entries, and selection. A `PaletteEntry` struct pairs a name/description/key-hint with an `Action`. The palette renders as a bottom-anchored overlay (input row + up to 8 completion rows) drawn over the content area. Filtering, navigation, and dispatch reuse existing `Action`/`dispatch_action` paths.

**Tech Stack:** Rust, ratatui, tui_input, crokey

**Spec:** `docs/superpowers/specs/2026-03-17-command-palette-design.md`

---

## Chunk 1: Data types, key binding, mode wiring

### Task 1: Add `PaletteEntry` and palette builder

**Files:**
- Create: `crates/flotilla-tui/src/palette.rs`
- Modify: `crates/flotilla-tui/src/lib.rs` (add `pub mod palette;`)

This module owns the command list and filtering logic, independent of UI.

- [ ] **Step 1: Write test for `palette_entries` returning all commands**

```rust
// crates/flotilla-tui/src/palette.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_entries_returns_expected_count() {
        let entries = all_entries();
        assert_eq!(entries.len(), 14);
        assert_eq!(entries[0].name, "search");
        assert_eq!(entries[entries.len() - 1].name, "keys");
    }
}
```

- [ ] **Step 2: Implement `PaletteEntry` and `all_entries`**

```rust
// crates/flotilla-tui/src/palette.rs
use crate::keymap::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub key_hint: Option<&'static str>,
    pub action: Action,
}

pub fn all_entries() -> Vec<PaletteEntry> {
    vec![
        PaletteEntry { name: "search", description: "filter items in view", key_hint: Some("/"), action: Action::OpenIssueSearch },
        PaletteEntry { name: "refresh", description: "refresh active repo", key_hint: Some("r"), action: Action::Refresh },
        PaletteEntry { name: "branch", description: "create a new branch", key_hint: Some("n"), action: Action::OpenBranchInput },
        PaletteEntry { name: "help", description: "show key bindings", key_hint: Some("?"), action: Action::ToggleHelp },
        PaletteEntry { name: "quit", description: "exit flotilla", key_hint: Some("q"), action: Action::Quit },
        PaletteEntry { name: "layout", description: "cycle view layout", key_hint: Some("l"), action: Action::CycleLayout },
        PaletteEntry { name: "host", description: "cycle target host", key_hint: Some("h"), action: Action::CycleHost },
        PaletteEntry { name: "theme", description: "cycle color theme", key_hint: None, action: Action::CycleTheme },
        PaletteEntry { name: "providers", description: "show provider health", key_hint: None, action: Action::ToggleProviders },
        PaletteEntry { name: "debug", description: "show debug panel", key_hint: None, action: Action::ToggleDebug },
        PaletteEntry { name: "actions", description: "open context menu", key_hint: Some("."), action: Action::OpenActionMenu },
        PaletteEntry { name: "add repo", description: "track a repository", key_hint: None, action: Action::OpenFilePicker },
        PaletteEntry { name: "select", description: "toggle multi-select", key_hint: Some("space"), action: Action::ToggleMultiSelect },
        PaletteEntry { name: "keys", description: "toggle key hints", key_hint: Some("K"), action: Action::ToggleStatusBarKeys },
    ]
}
```

- [ ] **Step 3: Write test for prefix filtering**

```rust
    #[test]
    fn filter_by_prefix() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "re");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "refresh");
    }

    #[test]
    fn filter_empty_returns_all() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "");
        assert_eq!(filtered.len(), entries.len());
    }

    #[test]
    fn filter_case_insensitive() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "HELP");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "zzz");
        assert!(filtered.is_empty());
    }
```

- [ ] **Step 4: Implement `filter_entries`**

```rust
pub fn filter_entries<'a>(entries: &'a [PaletteEntry], prefix: &str) -> Vec<&'a PaletteEntry> {
    if prefix.is_empty() {
        return entries.iter().collect();
    }
    let lower = prefix.to_lowercase();
    entries.iter().filter(|e| e.name.to_lowercase().starts_with(&lower)).collect()
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-tui --lib palette`

- [ ] **Step 6: Commit**

```
feat: add palette entry registry with prefix filtering (#332)
```

---

### Task 2: Add `UiMode::CommandPalette`, `FocusTarget`, `ModeId`

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:32-100`
- Modify: `crates/flotilla-tui/src/keymap.rs:204-231`

- [ ] **Step 1: Add `CommandPalette` variant to `UiMode`**

In `crates/flotilla-tui/src/app/ui_state.rs`, after `IssueSearch` (line 67), add:

```rust
    CommandPalette {
        input: Input,
        entries: Vec<crate::palette::PaletteEntry>,
        selected: usize,
        scroll_top: usize,
    },
```

- [ ] **Step 2: Add `CommandPalette` to `FocusTarget`**

In `crates/flotilla-tui/src/app/ui_state.rs`, add variant to `FocusTarget` enum (after line 80):

```rust
    CommandPalette,
```

- [ ] **Step 3: Add `focus_target` mapping**

In `crates/flotilla-tui/src/app/ui_state.rs`, `focus_target()` method (line 88-100), add arm:

```rust
            UiMode::CommandPalette { .. } => FocusTarget::CommandPalette,
```

- [ ] **Step 4: Add `ModeId::CommandPalette`**

In `crates/flotilla-tui/src/keymap.rs`, `ModeId` enum (line 204-215), add variant. Then add arm to `From<&UiMode>` impl (line 217-231):

```rust
            UiMode::CommandPalette { .. } => ModeId::CommandPalette,
```

- [ ] **Step 5: Fix all match exhaustiveness errors**

Multiple existing `match` arms on `UiMode`, `FocusTarget`, and `ModeId` will fail to compile. Fix each:

- `crates/flotilla-tui/src/app/ui_state.rs` — tests constructing all UiMode variants (around lines 349, 395) — add `CommandPalette` test entries.
- `crates/flotilla-tui/src/app/key_handlers.rs` — `dispatch_action` Dismiss arm, mouse handler blocks for overlay modes.
- `crates/flotilla-tui/src/ui.rs` — `status_bar_content` match.
- `crates/flotilla-tui/src/keymap.rs` — `build_default_keymap`, `from_config_str` call sites, mode iteration in tests.

For each, add the minimal arm needed (e.g. `UiMode::CommandPalette { .. } => { ... }`). Details in later tasks — for now, just make it compile.

- [ ] **Step 6: Run build and tests**

Run: `cargo build && cargo test --workspace --locked`

- [ ] **Step 7: Commit**

```
feat: add CommandPalette to UiMode, FocusTarget, ModeId (#332)
```

---

### Task 3: Add `Action::OpenCommandPalette` and rebind `/`

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs:19-43, 59-98, 103-141, 144-182, 285-295`
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:53-263`

- [ ] **Step 1: Add `OpenCommandPalette` to `Action` enum**

In `crates/flotilla-tui/src/keymap.rs`, after `OpenFilePicker` (line 41):

```rust
    OpenCommandPalette,
```

- [ ] **Step 2: Add config string mapping**

In `from_config_str` (line 59-98), add:
```rust
            "open_command_palette" => Action::OpenCommandPalette,
```

In `as_config_str` (line 103-141), add:
```rust
            Action::OpenCommandPalette => "open_command_palette",
```

In `description` (line 144-182), add:
```rust
            Action::OpenCommandPalette => "Open command palette",
```

- [ ] **Step 3: Rebind `/` to `OpenCommandPalette`**

In `build_default_keymap` (around line 290), change:
```rust
// was: normal.insert(kc(KeyCode::Char('/'), KeyModifiers::NONE), Action::OpenIssueSearch);
normal.insert(kc(KeyCode::Char('/'), KeyModifiers::NONE), Action::OpenCommandPalette);
```

- [ ] **Step 4: Handle `OpenCommandPalette` in `dispatch_action`**

In `crates/flotilla-tui/src/app/key_handlers.rs`, `dispatch_action` method, after the `OpenFilePicker` arm (around line 262), add:

```rust
            Action::OpenCommandPalette => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.ui.mode = UiMode::CommandPalette {
                        input: Input::default(),
                        entries: crate::palette::all_entries(),
                        selected: 0,
                        scroll_top: 0,
                    };
                }
            }
```

- [ ] **Step 5: Run build and tests**

Run: `cargo build && cargo test --workspace --locked`

Fix any test that asserts `/` maps to `OpenIssueSearch` — update to `OpenCommandPalette`.

- [ ] **Step 6: Commit**

```
feat: add OpenCommandPalette action, rebind / (#332)
```

---

### Task 4: Key handling for CommandPalette mode

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:21-51, 53-307`

- [ ] **Step 1: Add `CommandPalette` to `resolve_action` text input modes**

In `crates/flotilla-tui/src/app/key_handlers.rs`, `resolve_action` (line 21-51), add a case alongside BranchInput/IssueSearch:

```rust
            ModeId::CommandPalette => {
                return match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter | KeyCode::Tab => Some(Action::Confirm),
                    KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Down => Some(Action::SelectNext),
                    _ => None,
                };
            }
```

- [ ] **Step 2: Handle `Confirm` for `FocusTarget::CommandPalette`**

In `dispatch_action`, the `Action::Confirm` arm dispatches based on focus target. Add a case for `FocusTarget::CommandPalette` (alongside `DeleteConfirmDialog`, `CloseConfirmDialog`, etc.):

```rust
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, ref entries, selected, .. } = self.ui.mode {
                        let filtered = crate::palette::filter_entries(entries, input.value());
                        if let Some(entry) = filtered.get(selected) {
                            let action = entry.action;
                            self.ui.mode = UiMode::Normal;
                            self.dispatch_action(action);
                            return;
                        }
                    }
                    self.ui.mode = UiMode::Normal;
                }
```

- [ ] **Step 3: Handle `SelectNext`/`SelectPrev` for CommandPalette**

In `dispatch_action`, the `SelectNext` arm (around line 55) matches on focus target. Add:

```rust
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, ref entries, ref mut selected, ref mut scroll_top, .. } = self.ui.mode {
                        let count = crate::palette::filter_entries(entries, input.value()).len();
                        if count > 0 {
                            *selected = (*selected + 1) % count;
                            let max_visible = 8usize;
                            if *selected >= *scroll_top + max_visible {
                                *scroll_top = selected.saturating_sub(max_visible - 1);
                            } else if *selected < *scroll_top {
                                *scroll_top = *selected;
                            }
                        }
                    }
                }
```

And for `SelectPrev`:

```rust
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, ref entries, ref mut selected, ref mut scroll_top, .. } = self.ui.mode {
                        let count = crate::palette::filter_entries(entries, input.value()).len();
                        if count > 0 {
                            *selected = if *selected == 0 { count - 1 } else { *selected - 1 };
                            let max_visible = 8usize;
                            if *selected >= *scroll_top + max_visible {
                                *scroll_top = selected.saturating_sub(max_visible - 1);
                            } else if *selected < *scroll_top {
                                *scroll_top = *selected;
                            }
                        }
                    }
                }
```

- [ ] **Step 4: Handle `Dismiss` for CommandPalette**

In the `Dismiss` match (around line 264), add alongside other overlay modes:

```rust
                FocusTarget::CommandPalette => {
                    self.ui.mode = UiMode::Normal;
                }
```

- [ ] **Step 5: Handle text input passthrough in `handle_key`**

In the `handle_key` method (around line 309-322), after the action dispatch, the method falls through to pass unresolved keys to `tui_input` for text-input modes. Add `CommandPalette` to that passthrough, and reset `selected` to 0 when text changes:

```rust
            UiMode::CommandPalette { ref mut input, ref mut selected, ref mut scroll_top, .. } => {
                input.handle_event(&crossterm::event::Event::Key(key));
                *selected = 0;
                *scroll_top = 0;
            }
```

- [ ] **Step 6: Handle `//` → search shortcut**

In the text input passthrough for `CommandPalette`, after handling the key event, check if the input value is `/`:

```rust
            UiMode::CommandPalette { ref mut input, ref mut selected, ref mut scroll_top, .. } => {
                input.handle_event(&crossterm::event::Event::Key(key));
                // // shortcut: typing / when input is empty opens search
                if input.value() == "/" {
                    self.ui.mode = UiMode::IssueSearch { input: Input::default() };
                    return;
                }
                *selected = 0;
                *scroll_top = 0;
            }
```

- [ ] **Step 7: Block mouse events in CommandPalette mode**

In the `handle_mouse` method, add `CommandPalette` to the overlay mode early-return block (alongside `DeleteConfirm`, `CloseConfirm`, `BranchInput`, etc.):

```rust
            | UiMode::CommandPalette { .. }
```

- [ ] **Step 8: Run build and tests**

Run: `cargo build && cargo test --workspace --locked`

- [ ] **Step 9: Commit**

```
feat: command palette key handling — navigation, filtering, dispatch (#332)
```

---

## Chunk 2: Rendering and snapshots

### Task 5: Render the command palette overlay

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:96-118, 352-450`

- [ ] **Step 1: Add `render_command_palette` call to main `render` function**

In `crates/flotilla-tui/src/ui.rs`, in the `render` function (line 96-118), add after line 117:

```rust
    render_command_palette(ui, theme, frame);
```

- [ ] **Step 2: Add `CommandPalette` arm to `status_bar_content`**

In `status_bar_content` (around line 352-450), add before the `Help` arm:

```rust
        UiMode::CommandPalette { .. } => StatusBarContent {
            status: StatusSection::plain(""),
            keys: vec![],
            task: None,
            mode_indicators: vec![],
        },
```

The status bar row will be replaced by the palette input row, so we render empty content for it.

- [ ] **Step 3: Implement `render_command_palette`**

```rust
const MAX_PALETTE_ROWS: usize = 8;

fn render_command_palette(ui: &UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::CommandPalette { ref input, ref entries, selected, scroll_top } = ui.mode else {
        return;
    };

    let filtered: Vec<&crate::palette::PaletteEntry> = crate::palette::filter_entries(entries, input.value());
    let visible_count = filtered.len().min(MAX_PALETTE_ROWS);
    let total_height = visible_count as u16 + 1; // completions + input row

    let frame_area = frame.area();
    if frame_area.height < total_height + 2 {
        return; // not enough space
    }

    // Bottom-anchored area
    let area = Rect::new(frame_area.x, frame_area.y + frame_area.height - total_height, frame_area.width, total_height);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

    // ── Completion rows (top portion of area) ──
    let completions_area = Rect::new(area.x, area.y, area.width, visible_count as u16);

    // Compute column widths
    let name_width = filtered.iter().map(|e| e.name.len()).max().unwrap_or(0).min(20);
    let hint_width = 5; // e.g. " / " with padding

    for (i, entry) in filtered.iter().skip(scroll_top).take(MAX_PALETTE_ROWS).enumerate() {
        let row_y = completions_area.y + i as u16;
        let is_selected = scroll_top + i == selected;

        let row_style = if is_selected {
            Style::default().bg(theme.action_highlight).bold()
        } else {
            Style::default().bg(theme.bar_bg)
        };

        // Clear the row
        let row_area = Rect::new(area.x, row_y, area.width, 1);
        frame.render_widget(Block::default().style(row_style), row_area);

        // Name column
        let name_span = Span::styled(
            format!("  {:<width$}", entry.name, width = name_width),
            row_style.fg(theme.text),
        );

        // Description column
        let desc_span = Span::styled(
            format!("  {}", entry.description),
            row_style.fg(theme.muted),
        );

        // Key hint column (right-aligned)
        let hint_text = entry.key_hint.unwrap_or("");
        let hint_span = Span::styled(
            format!(" {} ", hint_text),
            row_style.fg(theme.key_hint),
        );

        let line = Line::from(vec![name_span, desc_span]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, row_y, area.width.saturating_sub(hint_width), 1));

        // Render hint right-aligned
        if !hint_text.is_empty() {
            let hint_x = area.x + area.width.saturating_sub(hint_width);
            frame.render_widget(Paragraph::new(Line::from(hint_span)), Rect::new(hint_x, row_y, hint_width, 1));
        }
    }

    // ── Input row (bottom of area) ──
    let input_y = area.y + visible_count as u16;
    let input_area = Rect::new(area.x, input_y, area.width, 1);
    let input_style = Style::default().fg(theme.input_text).bg(theme.bar_bg);
    let display = format!("/ {}", input.value());
    frame.render_widget(Paragraph::new(display).style(input_style), input_area);

    // Position cursor after "/ " + input cursor position
    let cursor_x = area.x + 2 + input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, input_y));
}
```

- [ ] **Step 4: Run build**

Run: `cargo build`

- [ ] **Step 5: Commit**

```
feat: render command palette overlay (#332)
```

---

### Task 6: Snapshot tests

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

- [ ] **Step 1: Add snapshot test for command palette with no filter**

```rust
#[test]
fn command_palette_open() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::CommandPalette {
        input: Input::default(),
        entries: flotilla_tui::palette::all_entries(),
        selected: 0,
        scroll_top: 0,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 2: Add snapshot test with filter text**

```rust
#[test]
fn command_palette_filtered() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::CommandPalette {
        input: Input::from("he"),
        entries: flotilla_tui::palette::all_entries(),
        selected: 0,
        scroll_top: 0,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 3: Add snapshot test with selection on non-first item**

```rust
#[test]
fn command_palette_selection() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::CommandPalette {
        input: Input::default(),
        entries: flotilla_tui::palette::all_entries(),
        selected: 3,
        scroll_top: 0,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 4: Run tests and accept snapshots**

Run: `cargo test -p flotilla-tui --test snapshots`
Then: `cargo insta accept` (or manually move `.snap.new` → `.snap`)

- [ ] **Step 5: Verify all workspace tests pass**

Run: `cargo test --workspace --locked`

- [ ] **Step 6: Commit**

```
test: command palette snapshot tests (#332)
```

---

### Task 7: Unit tests for key handling

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` (test module)

- [ ] **Step 1: Test that `/` opens the palette**

```rust
    #[test]
    fn slash_opens_command_palette() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
    }
```

- [ ] **Step 2: Test that `//` transitions to search**

```rust
    #[test]
    fn double_slash_opens_issue_search() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
    }
```

- [ ] **Step 3: Test that Enter dispatches selected action**

```rust
    #[test]
    fn command_palette_enter_dispatches_action() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // First entry is "search" which dispatches OpenIssueSearch
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
    }
```

- [ ] **Step 4: Test that Esc dismisses**

```rust
    #[test]
    fn command_palette_esc_dismisses() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }
```

- [ ] **Step 5: Test arrow navigation wraps**

```rust
    #[test]
    fn command_palette_arrow_navigation_wraps() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // Down from 0 → 1
        app.handle_key(key(KeyCode::Down));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected CommandPalette");
        }
        // Up from 1 → 0
        app.handle_key(key(KeyCode::Up));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected CommandPalette");
        }
        // Up from 0 → wraps to last
        app.handle_key(key(KeyCode::Up));
        if let UiMode::CommandPalette { selected, entries, .. } = &app.ui.mode {
            assert_eq!(*selected, entries.len() - 1);
        } else {
            panic!("expected CommandPalette");
        }
    }
```

- [ ] **Step 6: Test typing resets selection to 0**

```rust
    #[test]
    fn command_palette_typing_resets_selection() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        // Now type a char — selection resets
        app.handle_key(key(KeyCode::Char('h')));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected CommandPalette");
        }
    }
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p flotilla-tui --lib`

- [ ] **Step 8: Commit**

```
test: command palette key handling unit tests (#332)
```

---

### Task 8: CI gates and cleanup

- [ ] **Step 1: Run fmt**

Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Fix any warnings.

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace --locked`

- [ ] **Step 4: Commit any fixups**

```
chore: fmt + clippy fixes (#332)
```
