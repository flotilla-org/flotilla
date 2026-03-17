# Command Palette — Design Spec (Phase 1)

**Issue:** #332
**Date:** 2026-03-17

## Goal

Add a command palette activated by `/` in Normal mode. Phase 1 covers no-argument commands only: the palette lists available actions, supports prefix filtering, arrow navigation, and Enter to execute. `//` remains a direct shortcut to item search for vi users.

Phase 2 (follow-up issue) adds commands with arguments, tab-completion on argument values, and noun-verb structured commands like `checkout <path> remove`.

## Visual Layout

Bottom-anchored overlay rendered over the content area:

```
┌─────────────────────────────────────────────────┐
│                  (content area)                  │
│                                                  │
│  search             filter items in view    /    │  ← completions (up to 8 rows)
│  refresh            refresh active repo     r    │
│  new branch         create a new branch     n    │
│  help               show key bindings       ?    │
│  ...                                             │
├─────────────────────────────────────────────────┤
│/ re█                                             │  ← input row (replaces status bar)
└─────────────────────────────────────────────────┘
```

- **Input row** (bottom): `/ ` prefix, then `tui_input::Input` text, cursor visible.
- **Completion list** (above input): up to 8 rows. Three columns — name (left), description (middle, muted), key hint (right, accent). Selected row highlighted.
- Rendered over content using `Clear` + draw, same pattern as existing popups.

## Interaction

| Input | Behavior |
|-------|----------|
| `/` in Normal mode | Open palette, show all commands |
| `//` in Normal mode | Open item search directly (existing behavior) |
| Type characters | Prefix-filter the command list |
| Up/Down arrows | Navigate completion list (wrap around) |
| Enter | Execute selected command, close palette |
| Tab | Execute selected command, close palette (alias) |
| Esc | Close palette, return to Normal |

## Commands (Phase 1 — no arguments)

Each entry has a name, description, and optional key hint. All are immediate-dispatch — no argument input required.

| Name | Description | Key | Dispatches |
|------|-------------|-----|------------|
| search | filter items in view | / | `OpenIssueSearch` |
| refresh | refresh active repo | r | `Refresh` |
| branch | create a new branch | n | `OpenBranchInput` |
| help | show key bindings | ? | `ToggleHelp` |
| quit | exit flotilla | q | `Quit` |
| layout | cycle view layout | l | `CycleLayout` |
| host | cycle target host | h | `CycleHost` |
| theme | cycle color theme | | `CycleTheme` |
| providers | show provider health | | `ToggleProviders` |
| debug | show debug panel | | `ToggleDebug` |
| actions | open context menu | . | `OpenActionMenu` |
| add repo | track a repository | | `OpenFilePicker` |
| select | toggle multi-select | space | `ToggleMultiSelect` |
| keys | toggle key hints | K | `ToggleStatusBarKeys` |

These map directly to existing `Action` enum variants. No intents in phase 1 — they require argument-driven dispatch (phase 2).

## Filtering

Case-insensitive prefix match on the command name. The full list shows on open (no filter text). As the user types, non-matching entries are hidden. If the filter empties the list, show "no matches" in muted text.

Ordering: fixed display order (as listed above — most common first). Later: frecency-based ranking.

## Execution

Confirming an entry closes the palette (returns to Normal mode) and then dispatches the corresponding `Action` through the existing `dispatch_action` path.

## New Types

### `UiMode::CommandPalette`

```rust
CommandPalette {
    input: Input,
    entries: Vec<PaletteEntry>,
    selected: usize,
    scroll_top: usize,
}
```

### `PaletteEntry`

```rust
struct PaletteEntry {
    name: String,
    description: String,
    key_hint: Option<String>,
    action: Action,
}
```

Phase 2 will generalize `action` to a `PaletteEntryKind` enum supporting both actions and argument-bearing commands.

### Additions to existing enums

- `ModeId::CommandPalette`
- `FocusTarget::CommandPalette`
- `Action::OpenCommandPalette` (replaces `OpenIssueSearch` in the `/` binding)

## Status Bar

When in `CommandPalette` mode, the status bar row becomes the input line. Renders `/ ` prefix followed by the input text with cursor. No key chips — the completion list above is self-explanatory.

## Key Binding Changes

- `/` in Normal mode → `OpenCommandPalette` (was `OpenIssueSearch`)
- `//` handling: when palette is open and input is empty, typing `/` dispatches `OpenIssueSearch` and closes the palette.

## Rendering

The palette overlay occupies the bottom N+1 rows of the frame (1 input row + up to 8 completion rows). It renders after the main content, using `Clear` to erase the underlying area before drawing.

The completion list is a simple table:
- Name column: left-aligned, normal weight. Prefix match highlighted in bold.
- Description column: left-aligned, muted color.
- Key hint column: right-aligned, accent color (same as key hints elsewhere).

Selected row uses the active selection style (bold, distinct background).

## Scope

**Phase 1 (this spec):** palette UI, no-arg commands, prefix filtering, arrow navigation, `//` search shortcut.

**Phase 2 (follow-up issue):** commands with arguments (`branch <name>`, `layout <name>`, `host <name>`, `checkout <path> remove`, `cr <id> close`, etc.), tab-completion on argument values, noun-verb structured commands, intent dispatch. This also covers unifying CLI subcommands with TUI commands for reuse across interfaces (web, MCP).
