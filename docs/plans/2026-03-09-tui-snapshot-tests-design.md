# TUI Rendering Snapshot Tests Design

**Issue**: #96
**Date**: 2026-03-09

## Goal

Add snapshot tests for `crates/flotilla-tui/src/ui.rs` (0% coverage) using ratatui's `TestBackend` and the `insta` crate for snapshot management.

## Architecture

A reusable `TestHarness` in `crates/flotilla-tui/tests/test_fixtures.rs` handles terminal setup, fixture construction, and buffer capture. Test cases live in `crates/flotilla-tui/tests/snapshots.rs`.

### TestHarness

Wraps a ratatui `Terminal<TestBackend>` at a fixed size (120x30). Provides:

- Builder methods to configure `TuiModel` — add repos, populate with checkouts/PRs/issues/sessions
- Ability to set `UiMode` and other `UiState` fields
- `render_to_string()` — calls `ui::render()` and converts the buffer to a string for `insta::assert_snapshot!`

### Test Cases (moderate scope)

| Test | Exercises |
|------|-----------|
| `empty_state` | No repos, renders without panic |
| `single_repo_empty_table` | One repo, no work items |
| `single_repo_with_items` | Checkouts, PRs, issues in table |
| `tab_bar_multiple_repos` | Multiple repos show correct tab names |
| `status_bar_normal` | Normal mode status hints |
| `status_bar_with_error` | Error message displayed |
| `help_screen` | Help mode renders keybindings |
| `action_menu` | ActionMenu mode with sample intents |
| `config_screen` | Config mode with provider statuses |
| `selected_item_preview` | Preview panel shows selected item details |

### Dependencies

Add `insta` to `crates/flotilla-tui/Cargo.toml` under `[dev-dependencies]`.

### Snapshot Files

Stored in `crates/flotilla-tui/tests/snapshots/snapshots/` (insta's default path convention). Committed to the repo.

### Key Decisions

- **Fixed terminal size** (120x30) for deterministic output
- **Plain `insta`** — no `glob` feature, each test is an explicit function
- **Buffer-to-string** via ratatui `Buffer` cell iteration for plain-text snapshots
- **Integration tests** in `tests/` rather than inline in `ui.rs` (already 1094 lines)
