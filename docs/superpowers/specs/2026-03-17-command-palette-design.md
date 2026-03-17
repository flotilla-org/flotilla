# Command Palette — Design Spec (Phase 1)

**Issue:** #332
**Date:** 2026-03-17

## Goal

Add a command palette activated by `/` in Normal mode. The palette lists available commands with prefix filtering, arrow navigation, Tab to fill, and Enter to execute. The `search` command takes inline arguments: `/search term1 term2` applies the filter directly on Enter.

Phase 2 (#401) adds more commands with arguments, tab-completion on argument values, and noun-verb structured commands like `checkout <path> remove`.

## Visual Layout

The status bar is the input line. When the palette opens, the status bar stays in place and a fixed-height completion area appears above it, overlaying the content area. The popup is always 9 rows: 1 status bar (with input) + 8 completion slots.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                            (content area)                                │
│                                                                          │
│/ re█              ⏎ RUN  TAB FILL  ESC CLOSE      ◫ auto  @local        │  ← status bar (input on left, keys + indicators on right)
│  refresh           refresh active repo                              r    │  ← completion rows (8 slots, fixed)
│  add repo          track a repository                                    │
│                                                                          │  ← empty slots show background
│                                                                          │
│                                                                          │
│                                                                          │
│                                                                          │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

- **Status bar row** (top of popup, at its normal screen position): `/ ` prefix + input text + cursor on the left; key chips (RUN, FILL, CLOSE) and mode indicators in the middle/right.
- **Completion area** (below status bar): 8 rows, always present. Three columns — name (left), description (middle, muted), key hint (right, accent). Selected row highlighted. Empty rows show background.
- Rendered over content using `Clear` + draw.

### Normal Mode Status Bar

In Normal mode (palette closed), the status bar shows `/ for commands` as the status text on the left, replacing the previously blank area:

```
/ for commands       ⏎ OPEN  . MENU  n NEW  ? HELP  q QUIT      ◫ auto  @local
```

## Interaction

| Input | Behavior |
|-------|----------|
| `/` in Normal mode | Open palette, show all commands |
| `//` in Normal mode | Open palette with `search ` pre-filled (shortcut) |
| Type characters | Prefix-filter the completion list |
| Up/Down arrows | Navigate completion list (wrap around) |
| Tab / Right arrow | Fill selected command name into input (don't execute) |
| Enter | Execute: dispatch the command with any arguments |
| Esc | Close palette, return to Normal |

## Commands (Phase 1)

Each entry has a name, description, and optional key hint.

| Name | Description | Key | Behavior on Enter |
|------|-------------|-----|-------------------|
| search | filter items in view | / | Takes inline args: `/search term` applies filter directly |
| refresh | refresh active repo | r | Immediate dispatch |
| branch | create a new branch | n | Immediate dispatch (opens branch input) |
| help | show key bindings | ? | Immediate dispatch |
| quit | exit flotilla | q | Immediate dispatch |
| layout | cycle view layout | l | Immediate dispatch |
| host | cycle target host | h | Immediate dispatch |
| theme | cycle color theme | | Immediate dispatch |
| providers | show provider health | | Immediate dispatch |
| debug | show debug panel | | Immediate dispatch |
| actions | open context menu | . | Immediate dispatch |
| add repo | track a repository | | Immediate dispatch (opens file picker) |
| select | toggle multi-select | space | Immediate dispatch |
| keys | toggle key hints | K | Immediate dispatch |

Most commands dispatch an existing `Action` variant. The `search` command is special: it parses everything after `search ` as the search query and applies it directly as the item filter, bypassing the IssueSearch mode.

## Filtering

Case-insensitive prefix match on the command name. The full list shows on open (no filter text). As the user types, non-matching entries are hidden but the 8-row area stays fixed. If the filter empties the list, all 8 rows are blank.

Ordering: fixed display order (as listed above — most common first). Later: frecency-based ranking.

## Execution

- **Tab/Right**: fills the selected command's name into the input (e.g., `search `), with a trailing space. The cursor is placed after the space so the user can type arguments. The completion list updates to reflect the new input.
- **Enter**: closes the palette and dispatches. For most commands, this dispatches the corresponding `Action`. For `search`, it extracts the text after `search ` and applies it as the item search filter.

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
    name: &'static str,
    description: &'static str,
    key_hint: Option<&'static str>,
    action: Action,
}
```

Phase 2 will generalize `action` to a `PaletteEntryKind` enum supporting both actions and argument-bearing commands.

### Additions to existing enums

- `ModeId::CommandPalette`
- `FocusTarget::CommandPalette`
- `Action::OpenCommandPalette` (replaces `OpenIssueSearch` in the `/` binding)

## Status Bar Integration

The palette input is embedded in the status bar — not a separate row. When the palette is open:

- **Left side**: `/ ` prefix + input text + cursor (where the status text normally is)
- **Key chips**: change to RUN (Enter), FILL (Tab), CLOSE (Esc)
- **Mode indicators**: stay as normal (layout icon, host)

When the palette is closed (Normal mode):
- **Left side**: `/ for commands` as the status text

## Key Binding Changes

- `/` in Normal mode → `OpenCommandPalette` (was `OpenIssueSearch`)
- `//` handling: when palette is open and input is empty, typing `/` fills `search ` into the input.

## Rendering

The palette overlay is a fixed 9-row area: the status bar at its normal position (top of the popup), plus 8 completion rows directly below it, overlaying the content area. The status bar renders with the input embedded on the left. The completion area uses `Clear` to erase underlying content before drawing.

The completion list:
- Name column: left-aligned, normal weight.
- Description column: left-aligned, muted color.
- Key hint column: right-aligned, accent color.
- Selected row: bold, distinct background (`theme.action_highlight`).
- Empty rows: bar background color.

## Scope

**Phase 1 (this spec):** palette UI, prefix filtering, Tab fill, Enter execute, `search` with inline args, `//` shortcut, fixed-height overlay, status bar integration.

**Phase 2 (#401):** more commands with arguments (`branch <name>`, `layout <name>`, `host <name>`, `checkout <path> remove`, `cr <id> close`, etc.), tab-completion on argument values, noun-verb structured commands, intent dispatch, CLI unification.
