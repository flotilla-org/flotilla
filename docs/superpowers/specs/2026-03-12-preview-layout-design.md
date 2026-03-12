# Dynamic Preview Layout and Visibility

**Issue:** #220
**Date:** 2026-03-12

## Problem

The TUI currently renders the preview panel to the right of the table at all times. That works well on wide terminals, but it wastes space on tall or narrow terminals where a bottom preview would preserve more useful width for the table. There is also no way to temporarily hide the preview when scanning the work item list.

The requested behavior has three parts:

- Automatically choose a good preview position based on terminal geometry
- Let the user explicitly override that choice with a keybinding cycle
- Let the user hide or show the preview entirely without losing the position preference

## Scope

This change is intentionally localized to the TUI. It should avoid a large `ui.rs` refactor for now and instead add a small amount of state plus a focused rendering decision in the existing layout code.

The preference is app-wide, not per repository tab.

## State Model

Add a single app-level preview preference to `UiState` with two fields:

- `position_mode: Auto | Right | Below`
- `visible: bool`

`position_mode` captures the user's chosen policy:

- `Auto` means resolve the layout from the current terminal size
- `Right` means always render the preview on the right
- `Below` means always render the preview below the table

`visible` controls whether the preview region is rendered at all. When it is `false`, the table uses the full content area.

This state belongs at the app level rather than in per-repo UI state because preview placement is a display preference for the whole TUI, not part of any repo-specific workflow.

## Persistence

Persist the explicit preview preference and visibility flag in config:

- `position_mode`
- `visible`

`Auto` persists as the mode value itself, not as a resolved side. On startup, if the saved mode is `Auto`, the layout is re-evaluated from the current terminal size. On terminal resize, `Auto` re-resolves, while explicit `Right` and `Below` remain fixed.

This matches the desired hybrid behavior: user choices persist, but automatic layout remains dynamic.

## Layout Resolution

`render_content` remains the main layout decision point in `crates/flotilla-tui/src/ui.rs`.

Rendering rules:

- If preview is hidden, render only the unified table in the full content area
- If preview is visible and the resolved position is `Right`, render the existing horizontal split
- If preview is visible and the resolved position is `Below`, render a vertical split with table on top and preview below
- If the debug panel is enabled, keep the existing preview/debug split inside the preview region regardless of whether that region is right-side or bottom-aligned

## Auto Heuristic

Automatic layout resolution uses a two-stage heuristic:

1. Check whether each candidate layout satisfies minimum usable sizes for both the table and preview
2. If both candidates are viable, use terminal aspect ratio as a tiebreaker

This avoids blindly switching based on aspect ratio alone when one orientation would make either pane unusably small.

### Candidate Evaluation

Define constants in `ui.rs` for:

- minimum table width
- minimum preview width
- minimum table height
- minimum preview height
- aspect-ratio threshold

The resolver evaluates two candidates:

- `Right`: table width and preview width must each remain above their minimums
- `Below`: table height and preview height must each remain above their minimums

Decision order:

- If only one candidate is viable, use it
- If both are viable, use aspect ratio as the tiebreaker
- If neither is ideal, fall back to `Right` to preserve current behavior

The exact thresholds are implementation details and can be tuned later without changing the state model.

## Input and User Feedback

Add two normal-mode keybindings:

- `v` cycles preview position mode: `auto -> right -> below -> auto`
- `P` toggles preview visibility

User feedback should appear in the two places already used for discoverability:

- normal-mode status bar
- help overlay

The status bar should display the current preview state compactly, for example:

- `preview:auto`
- `preview:right`
- `preview:below`
- `preview:hidden`

When hidden, the status bar should prioritize the hidden state over the saved position so the visible result is immediately clear.

## Testing Strategy

This should be implemented test-first.

### Logic Tests

Add focused tests for:

- cycling `position_mode`
- toggling preview visibility
- resolving `Auto` to `Right`
- resolving `Auto` to `Below`
- preserving explicit `Right` and `Below` regardless of terminal size

These tests should use fixed terminal dimensions rather than reading the real terminal.

### Snapshot Tests

Extend the TUI snapshot coverage with fixed-size harnesses for:

- preview shown on the right
- preview shown below on a tall or narrow terminal
- preview hidden
- updated help text
- updated status bar text

The existing test harness already supports fixed terminal sizes and is a good fit for deterministic rendering tests.

## Files Involved

| File | Change |
|------|--------|
| `crates/flotilla-tui/src/app/ui_state.rs` | Add preview preference types/state |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Add `v` and `P` handlers |
| `crates/flotilla-tui/src/ui.rs` | Resolve preview layout, render hidden/right/below states, update status/help text |
| `crates/flotilla-tui/tests/support/mod.rs` | Add harness helpers for preview state and terminal geometry |
| `crates/flotilla-tui/tests/snapshots.rs` | Add failing and then passing snapshot coverage |
| `docs/keybindings.md` | Document new keybindings |
| TUI config persistence files/modules | Persist preview mode and visibility if config is already loaded through them |

## Out of Scope

- Large-scale `ui.rs` refactoring
- Per-repository preview preferences
- Mouse controls for preview position or visibility
- Persisting the resolved `Auto` side instead of the mode
- Any redesign of preview content itself
