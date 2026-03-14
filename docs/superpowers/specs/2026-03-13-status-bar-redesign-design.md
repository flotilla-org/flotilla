# TUI Status Bar Redesign

**Date:** 2026-03-13

## Problem

The current TUI status bar is a single early-return text line. Global errors replace everything else, there are no click targets, and the bar does not adapt when terminal width is tight. That makes it hard to keep status, discoverability, and running-task feedback visible at the same time.

The requested redesign keeps the bar visible but restructures it into sections:

- `status / errors`
- `keys`
- `task`

It also needs mouse support, adaptive truncation, and the ability to hide only the keys section.

## Goals

- Keep the status bar visible at all times in normal operation.
- Split the row into stable logical sections instead of a single mutually-exclusive message.
- Make visible key labels clickable.
- Allow the keys section to be shown or hidden with keyboard controls.
- Make errors dismissable from the bar.
- Adapt gracefully to narrow widths with deterministic truncation priority.
- Adopt a Zellij-inspired ribbon presentation with strong visual separation between adjacent actions and sections.

## Non-Goals

- Multi-row status bar layouts.
- A full widget framework for bottom-row UI.
- Persisting dismissed errors across app restarts.
- Changing daemon-side error production or peer-host state.
- General-purpose mouse support for every possible shortcut.
- Solving low-capability terminal fallback comprehensively in this change. Fallbacks can be added later.

## Visual Direction

The status bar should borrow the successful visual language of Zellij's compact/status bar rather than looking like plain inline prose.

The first implementation should use:

- Nerd Font chevron separators (``) between adjacent ribbons
- white-on-black styling for the left `status / errors` section
- white-on-black styling for the right `task` section
- a darker inverted ribbon treatment for the middle `keys` section
- emphasized key tokens inside action labels, with the key itself highlighted more strongly than the action text

Example shape, not literal final copy:

```text
 ERRORS  / SEARCH  n NEW  K KEYS  Refreshing repo...
```

The important point is not strict imitation of Zellij's modes, but reuse of its dense ribbon look:

- short key-first labels
- strong chevron boundaries
- clear section contrast
- less sentence-like hint text

Low-capability terminal fallbacks are deferred for now. The immediate target is terminals with reasonable glyph support. A fallback path can be added later if needed.

## Layout Model

The status bar remains one terminal row and is always rendered.

It is divided into three logical sections:

### Left: Status / Errors

This section is the highest priority and should remain visible longest under width pressure.

It shows:

- dismissable generic `status_message` errors
- dismissable peer-host problems
- normal compact status text when there are no active errors

The first implementation should prioritize rendering currently-active issues rather than building a historical log. Dismissal is local UI state: dismissing an error hides it from the bar until a new occurrence arrives.

### Middle: Keys

This section shows action labels as Zellij-style ribbons rather than prose or flat chips, so the click targets are obvious.

Each visible label is clickable and dispatches the same app behavior as the corresponding keyboard shortcut where that mapping is clean and deterministic.

The default presentation should collapse actions to compact key-first labels rather than long hints. For example:

- `⏎ OPEN`
- `/ SEARCH`
- `n NEW`
- `K KEYS`

The exact copy can be tuned during implementation, but the desired direction is compact and glanceable.

This section can be hidden independently of the rest of the bar. The intended keyboard behavior is:

- `K` toggles the keys section
- `?` opens help as today and remains the fallback discoverability path when keys are hidden

### Right: Task

This section shows the active in-flight command for the current repo, including a spinner and compact summary. If multiple commands are running for the active repo, it may show the first plus a count.

This section is lower priority than status but higher priority than keys only in terms of information value; however the user explicitly wants width compression order to be:

1. `keys`
2. `task`
3. `status`

That means `keys` should shrink or disappear first, `task` should truncate next, and `status` should truncate last.

## Adaptive Width Rules

The renderer should stop using early returns for status-bar content. Instead, it should gather all candidate content first, then allocate width according to fixed priorities.

Expected behavior:

- Wide terminals show all three sections.
- Moderate widths start truncating or dropping `keys`.
- Narrower widths truncate `task`.
- Only very narrow widths truncate `status / errors`.

Within the `keys` section, width pressure should first collapse the number of visible ribbons before degrading the higher-priority left and right sections. This follows the same philosophy Zellij uses for compacting visible affordances while keeping the bar legible.

Implementation detail is flexible, but the algorithm should be deterministic and testable from pure inputs.

## Interaction Model

## Clickable Key Labels

The visible key labels themselves are the click targets. They should be rendered as clearly separated chips/segments rather than undifferentiated prose.

In this design, "segment" means a ribbon with chevron boundaries, not just whitespace-separated tokens.

Clicks should dispatch the same actions as keyboard shortcuts for the supported normal-mode actions shown in the bar. The status bar should not invent separate mouse-only actions for those items.

## Error Dismissal

Errors in the left section are dismissable from the status bar.

First-pass scope:

- peer-host warnings are dismissable
- generic `status_message` errors are dismissable

Dismissal should not mutate daemon state. It only hides the current message from the UI. If a new host problem or a new status error is produced later, it can appear again.

## Hidden Keys State

The hidden state applies only to the `keys` section, not the whole status bar.

There is no requirement for a mouse-only reopen affordance in the bar itself. Keyboard controls (`K`, `?`) are sufficient for the first pass.

## State and Hit Testing

The redesign should introduce a small status-bar view model that serves as the single source of truth for:

- rendered segments for each section
- width allocation decisions
- click target metadata
- dismiss target metadata

This avoids splitting logic between render-time string assembly and ad hoc mouse coordinate checks.

`UiState.layout` should gain dedicated status-bar hit areas instead of reusing `tab_areas`. Bottom-row hit testing is a separate concern from the tab bar and should remain isolated.

## Mode Behavior

Normal mode should use the full segmented status bar design.

Other modes can continue to override content where needed, but they should reuse the segmented renderer where practical instead of replacing the whole bottom row with unrelated logic. The important constraint is that the redesign should not regress mode-specific guidance such as search input, confirm dialogs, branch input, or help.

## Testing Strategy

This work should be implemented test-first.

### Unit Tests

Add focused tests for:

- section visibility when keys are shown vs hidden
- width allocation and truncation priority
- task truncation behavior
- status preservation under narrow widths
- generated click target metadata for visible key chips
- generated dismiss targets for visible errors

These tests should target the status-bar builder/model rather than relying only on snapshots.

### Interaction Tests

Add mouse-focused tests for:

- clicking a visible key chip dispatches the expected action
- clicking an error dismiss target hides the message
- clicking empty status-bar space is a no-op

### Snapshot Tests

Add or update snapshots for:

- normal wide layout with all sections visible
- narrower layout where keys are reduced first
- hidden keys section
- active task with spinner
- host error visible with dismiss affordance
- generic error visible with dismiss affordance
- mixed state with status plus task
- Zellij-style chevron rendering in the middle keys section
- expected compact key-first labels

## Files Likely Involved

| File | Change |
|------|--------|
| `crates/flotilla-tui/src/ui.rs` | Replace early-return status rendering with segmented status-bar builder and renderer |
| `crates/flotilla-tui/src/app/ui_state.rs` | Add status-bar layout/hit-test state and keys visibility state |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Add status-bar mouse handling and `k` toggle |
| `crates/flotilla-tui/src/app/mod.rs` | Add any app-level helpers/state needed for dismissable status items |
| `crates/flotilla-tui/tests/support/mod.rs` | Extend test harness support for status-bar scenarios |
| `crates/flotilla-tui/tests/snapshots.rs` | Add snapshot coverage for new status-bar states |
| `docs/keybindings.md` | Document `K` and any revised status-bar affordances |

## Recommended Implementation Direction

Use a small dedicated status-bar view model rather than keeping all behavior inline in `render_status_bar`.

That approach is the best fit for the requested combination of:

- adaptive width behavior
- persistent sections
- click targets
- dismissable errors
- testable rendering rules
- Zellij-inspired ribbon rendering without importing Zellij's mode model

It adds a modest amount of structure without turning the bottom row into a full component system.
