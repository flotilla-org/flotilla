# TUI Overlay Layout Hardening Design

**Problem:** The in-progress interactive widget refactor left overlay rendering split between `ui.rs` and widget implementations. That makes fixed-height overlays vulnerable to drawing past the bottom of the frame when layout assumptions diverge or the terminal is shorter than expected.

**Goal:** Centralize the clamped layout arithmetic for top-anchored overlays and add render smoke tests that fail if any widget path writes outside the frame.

## Approach

Introduce a small helper in `crates/flotilla-tui/src/ui_helpers.rs` that computes a safe overlay layout for the command palette style of UI:
- a status/input row anchored near the bottom of the frame
- a results area directly beneath that row
- a visible row count clamped against the available frame height

Keep ownership unchanged for now:
- `ui.rs` remains the render owner for the command palette overlay
- `CommandPaletteWidget` remains an event/state widget only
- centered popup widgets continue using the existing popup helpers

## Testing

Add no-panic render tests that exercise the widget render paths on a cramped terminal:
- command palette via `UiMode::CommandPalette`
- command palette via widget stack
- action menu
- branch input
- close confirm
- delete confirm
- file picker
- help

These tests are intended to catch bottom-edge and small-terminal overflows early, without requiring snapshot changes.
