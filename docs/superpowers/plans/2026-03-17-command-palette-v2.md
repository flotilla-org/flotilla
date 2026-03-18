# Command Palette v2 — Status Bar Integration

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rework the command palette so the input lives in the status bar, completions grow downward in a fixed 8-row area, Tab fills instead of executing, and `/search term` applies filters directly.

**Architecture:** The existing `UiMode::CommandPalette` and `palette.rs` stay. The rendering changes from a bottom-anchored overlay (input at bottom, completions above) to a top-anchored overlay (status bar with input at top, 8 completion rows below). The status bar content function provides the input as the status text and adjusts key chips. Tab/Right fills the selected command name. The `search` command gets special handling: Enter with `/search term` applies the filter directly instead of dispatching `OpenIssueSearch`.

**Tech Stack:** Rust, ratatui, tui_input

**Spec:** `docs/superpowers/specs/2026-03-17-command-palette-design.md`

**Existing implementation:** The basic palette already works — `UiMode::CommandPalette`, `palette.rs`, key handling, rendering, and tests are all in place. This plan covers the delta to match the revised spec.

---

## Summary of Changes

| Area | Current | Target |
|------|---------|--------|
| Status bar (Normal) | empty status text | `/ for commands` |
| Input location | separate row at bottom of overlay | embedded in status bar (left side) |
| Completions direction | above input (upward) | below status bar (downward) |
| Popup height | variable (shrinks with fewer matches) | fixed 9 rows (1 status + 8 completion) |
| Tab key | executes selected command | fills command name into input |
| `//` shortcut | switches to IssueSearch mode | fills `search ` into input |
| `/search term` + Enter | N/A | applies filter directly |
| Key chips in palette | none | RUN, FILL, CLOSE + mode indicators |

---

## Chunk 1: Status bar and rendering changes

### Task 1: Normal mode status text — `/ for commands`

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:368`

- [ ] **Step 1: Change the default Normal mode status text**

In `status_bar_content`, the Normal mode fallback (line 368) currently returns `StatusSection::plain("")`. Change to:

```rust
                StatusSection::plain("/ for commands")
```

- [ ] **Step 2: Run tests, update snapshots**

Run: `cargo test --workspace --locked`

Many snapshot tests will change (all Normal mode snapshots now show "/ for commands"). Accept them:

Run: `INSTA_UPDATE=always cargo test -p flotilla-tui --locked --test snapshots`

- [ ] **Step 3: Commit**

```
feat: show "/ for commands" in Normal mode status bar (#332)
```

---

### Task 2: Status bar content when palette is open

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:450-452`

The status bar in CommandPalette mode needs to show the input on the left and palette-specific key chips.

- [ ] **Step 1: Update CommandPalette status bar content**

Replace the current empty `StatusBarContent` for `CommandPalette` (line 450-452) with:

```rust
        UiMode::CommandPalette { ref input, .. } => {
            let status_text = format!("/ {}", input.value());
            StatusBarContent {
                status: StatusSection::plain(&status_text),
                keys: vec![
                    key_chip(ENTER_KEY_GLYPH, "Run", KeyCode::Enter),
                    key_chip("TAB", "Fill", KeyCode::Tab),
                    key_chip("esc", "Close", KeyCode::Esc),
                ],
                task: None,
                mode_indicators: normal_mode_indicators(ui),
            }
        }
```

- [ ] **Step 2: Run build**

Run: `cargo build`

- [ ] **Step 3: Commit**

```
feat: palette input and key chips in status bar (#332)
```

---

### Task 3: Rewrite `render_command_palette` — fixed-height, completions below status bar

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:1257-1322`

The overlay now renders 8 completion rows directly below the status bar (which is at the bottom of the frame). The status bar itself handles the input rendering via `status_bar_content`. The overlay only draws the completion area.

- [ ] **Step 1: Rewrite `render_command_palette`**

Replace the entire function with:

```rust
fn render_command_palette(ui: &UiState, theme: &Theme, frame: &mut Frame) {
    let UiMode::CommandPalette { ref input, ref entries, selected, scroll_top } = ui.mode else {
        return;
    };

    let frame_area = frame.area();
    if frame_area.height < (MAX_PALETTE_ROWS as u16) + 2 {
        return;
    }

    // Completion area: 8 rows directly above the status bar (bottom row).
    // Status bar is at frame_area.y + frame_area.height - 1.
    let completions_y = frame_area.y + frame_area.height - 1 - MAX_PALETTE_ROWS as u16;
    let area = Rect::new(frame_area.x, completions_y, frame_area.width, MAX_PALETTE_ROWS as u16);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bar_bg)), area);

    let filtered: Vec<&crate::palette::PaletteEntry> = crate::palette::filter_entries(entries, input.value());
    let name_width = filtered.iter().map(|e| e.name.len()).max().unwrap_or(0).min(20);
    let hint_width: u16 = 7;

    for (i, entry) in filtered.iter().skip(scroll_top).take(MAX_PALETTE_ROWS).enumerate() {
        let row_y = area.y + i as u16;
        let is_selected = scroll_top + i == selected;

        let row_style = if is_selected {
            Style::default().bg(theme.action_highlight).add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.bar_bg)
        };

        let row_area = Rect::new(area.x, row_y, area.width, 1);
        frame.render_widget(Block::default().style(row_style), row_area);

        let name_span = Span::styled(format!("  {:<width$}", entry.name, width = name_width), row_style.fg(theme.text));
        let desc_span = Span::styled(format!("  {}", entry.description), row_style.fg(theme.muted));

        let line = Line::from(vec![name_span, desc_span]);
        frame.render_widget(Paragraph::new(line), Rect::new(area.x, row_y, area.width.saturating_sub(hint_width), 1));

        let hint_text = entry.key_hint.unwrap_or("");
        if !hint_text.is_empty() {
            let hint_span = Span::styled(format!(" {} ", hint_text), row_style.fg(theme.key_hint));
            let hint_x = area.x + area.width.saturating_sub(hint_width);
            frame.render_widget(Paragraph::new(Line::from(hint_span)), Rect::new(hint_x, row_y, hint_width, 1));
        }
    }

    // Set cursor position in the status bar input area.
    // The status bar renders "/ {input}" — cursor is at x=2+visual_cursor on the status bar row.
    let status_bar_y = frame_area.y + frame_area.height - 1;
    let cursor_x = frame_area.x + 2 + input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, status_bar_y));
}
```

Key differences from the current version:
- No input row rendered (status bar handles it)
- Completion area is fixed at `MAX_PALETTE_ROWS` rows, always the same height
- Completions are above the status bar (growing upward from it), not below
- Empty rows just show `bar_bg` background (the `Clear` + `Block` fill handles this)
- Cursor positioned on the status bar row, not a separate input row

- [ ] **Step 2: Run build and tests**

Run: `cargo build && cargo test --workspace --locked`

- [ ] **Step 3: Commit**

```
feat: fixed-height palette completions above status bar (#332)
```

---

## Chunk 2: Key handling changes

### Task 4: Tab/Right fills command name instead of executing

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:34-39, 220-230`

- [ ] **Step 1: Add Right arrow to resolve_action**

In `resolve_action`, the `CommandPalette` match (line 34-39), add `KeyCode::Right` alongside `Tab`:

```rust
            ModeId::CommandPalette => {
                return match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    KeyCode::Tab | KeyCode::Right => Some(Action::SelectNext), // reuse SelectNext as "fill" signal — we'll intercept in dispatch
                    KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Down => Some(Action::SelectNext),
                    _ => None,
                };
            }
```

Wait — we need a new action for "fill" since Tab and Down both currently map to SelectNext/Confirm. Let me reconsider.

Actually, the simplest approach: handle Tab/Right directly in the `handle_key` passthrough section (where unresolved keys go), not through the action system. This avoids adding a new Action variant.

Instead, change `resolve_action` to only intercept Esc, Enter, Up, Down:

```rust
            ModeId::CommandPalette => {
                return match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Down => Some(Action::SelectNext),
                    _ => None,
                };
            }
```

Then in the `handle_key` text input passthrough for `CommandPalette`, handle Tab/Right before passing to tui_input:

```rust
            UiMode::CommandPalette { ref mut input, ref entries, ref mut selected, ref mut scroll_top, .. } => {
                // Tab / Right arrow: fill selected command name
                if matches!(key.code, KeyCode::Tab | KeyCode::Right) {
                    let filtered = crate::palette::filter_entries(entries, input.value());
                    if let Some(entry) = filtered.get(*selected) {
                        let filled = format!("{} ", entry.name);
                        *input = Input::from(filled.as_str());
                        *selected = 0;
                        *scroll_top = 0;
                    }
                    return;
                }

                input.handle_event(&crossterm::event::Event::Key(key));
                // // shortcut: typing / when input is empty fills "search "
                if input.value() == "/" {
                    *input = Input::from("search ");
                    *selected = 0;
                    *scroll_top = 0;
                    return;
                }
                *selected = 0;
                *scroll_top = 0;
            }
```

- [ ] **Step 2: Update the `//` shortcut**

Note the change above: `//` now fills `search ` into the input instead of switching to `IssueSearch` mode. This keeps the user in the palette.

- [ ] **Step 3: Run build and tests**

Run: `cargo build && cargo test --workspace --locked`

The `double_slash_opens_issue_search` test will fail — update it:

```rust
    #[test]
    fn double_slash_fills_search() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
        app.handle_key(key(KeyCode::Char('/')));
        // Should still be in CommandPalette with "search " filled in
        if let UiMode::CommandPalette { ref input, .. } = app.ui.mode {
            assert_eq!(input.value(), "search ");
        } else {
            panic!("expected CommandPalette");
        }
    }
```

- [ ] **Step 4: Add test for Tab fill**

```rust
    #[test]
    fn command_palette_tab_fills_command_name() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // First entry is "search" — Tab should fill it
        app.handle_key(key(KeyCode::Tab));
        if let UiMode::CommandPalette { ref input, selected, .. } = app.ui.mode {
            assert_eq!(input.value(), "search ");
            assert_eq!(selected, 0); // selection resets
        } else {
            panic!("expected CommandPalette");
        }
    }
```

- [ ] **Step 5: Commit**

```
feat: Tab/Right fills command name, // fills search (#332)
```

---

### Task 5: `/search term` + Enter applies filter directly

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:220-230` (Confirm handler for CommandPalette)

- [ ] **Step 1: Update Confirm handler to parse `/search term`**

In `dispatch_action`, the `FocusTarget::CommandPalette` arm of `Action::Confirm` (around line 220), change to check if the input starts with a known command that takes args:

```rust
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, ref entries, selected, .. } = self.ui.mode {
                        let text = input.value().to_string();

                        // Check for "search " prefix — apply filter directly
                        if let Some(query) = text.strip_prefix("search ") {
                            let query = query.trim().to_string();
                            self.ui.mode = UiMode::Normal;
                            if !query.is_empty() {
                                self.active_ui_mut().active_search_query = Some(query);
                            }
                            return;
                        }

                        // Otherwise dispatch the selected entry's action
                        let filtered = crate::palette::filter_entries(entries, &text);
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

- [ ] **Step 2: Update the `command_palette_enter_dispatches_action` test**

The existing test presses Enter on the first entry ("search") which dispatches `OpenIssueSearch`. With the new behavior, pressing Enter when the input is empty and "search" is selected should still dispatch the action (since the input doesn't start with "search "). The test should still pass. Verify:

Run: `cargo test -p flotilla-tui --lib command_palette_enter`

- [ ] **Step 3: Add test for `/search term` execution**

```rust
    #[test]
    fn command_palette_search_with_args_applies_filter() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // Type "search auth"
        for c in "search auth".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.active_ui().active_search_query.as_deref(), Some("auth"));
    }
```

- [ ] **Step 4: Add test for empty search term**

```rust
    #[test]
    fn command_palette_search_empty_term_clears() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // Type "search " (no term)
        for c in "search ".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.active_ui().active_search_query, None);
    }
```

- [ ] **Step 5: Run all tests**

Run: `cargo test --workspace --locked`

- [ ] **Step 6: Commit**

```
feat: /search term applies filter directly from palette (#332)
```

---

## Chunk 3: Snapshots and CI

### Task 6: Update snapshot tests

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`
- Modify: `crates/flotilla-tui/tests/snapshots/*.snap`

- [ ] **Step 1: Update all snapshots**

Run: `INSTA_UPDATE=always cargo test -p flotilla-tui --locked --test snapshots`

Review the updated snapshots to verify:
- Normal mode snapshots show "/ for commands" in status bar
- Palette snapshots show input in status bar row with key chips, completions below, fixed 8-row height

- [ ] **Step 2: Verify all tests pass**

Run: `cargo test --workspace --locked`

- [ ] **Step 3: Commit**

```
test: update snapshots for palette v2 layout (#332)
```

---

### Task 7: CI gates

- [ ] **Step 1: Run fmt**

Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 3: Run full test suite**

Run: `cargo test --workspace --locked`

- [ ] **Step 4: Commit any fixups**

```
chore: fmt + clippy fixes (#332)
```
