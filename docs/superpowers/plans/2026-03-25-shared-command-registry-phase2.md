# Shared Command Registry Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the shared command registry into the TUI — noun-verb palette with position-aware completions, intent adapter, command echo, key binding changes.

**Architecture:** The palette becomes a mini-CLI parser using `flotilla-commands` types. User text is tokenized with `shlex`, parsed through `parse_noun_command` / `parse_host_command`, resolved via `Resolved`, and dispatched with ambient context (repo from active tab, host from `HostResolution`). Intents become thin adapters that produce command token strings. A new status bar section shows command echo and pre-fill preview.

**Tech Stack:** Rust, clap (existing), shlex (new), ratatui (existing), flotilla-commands (existing), flotilla-tui (existing)

**Spec:** `docs/superpowers/specs/2026-03-25-shared-command-registry-phase2-design.md`

**CI gates:** `cargo +nightly-2026-03-12 fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/flotilla-commands/src/lib.rs` | Modify | Add `parse_noun_command`, `parse_host_command` re-exports |
| `crates/flotilla-commands/src/parse.rs` | Create | `parse_noun_command()`, `parse_host_command()` convenience functions |
| `crates/flotilla-tui/Cargo.toml` | Modify | Add `shlex` dependency |
| `crates/flotilla-tui/src/palette.rs` | Modify | Overhaul: argument-bearing entries, palette-local command definitions, `target` instead of `host` |
| `crates/flotilla-tui/src/widgets/command_palette.rs` | Modify | Overhaul: shlex tokenization, noun-verb parsing, position-aware completions, pre-fill support, context-aware filtering |
| `crates/flotilla-tui/src/app/ui_state.rs` | Modify | Add `command_echo: Option<String>` field to `UiState` |
| `crates/flotilla-tui/src/binding_table.rs` | Modify | Key binding changes: `?` → contextual palette, `h` → help, remove Shared `?` |
| `crates/flotilla-tui/src/keymap.rs` | Modify | Add `OpenContextualPalette` action variant |
| `crates/flotilla-tui/src/widgets/repo_page.rs` | Modify | Handle `OpenContextualPalette` (pre-fill from selection), `Esc` clears selection |
| `crates/flotilla-tui/src/widgets/overview_page.rs` | Modify | Handle `OpenContextualPalette` (empty, no selection on overview) |
| `crates/flotilla-tui/src/widgets/status_bar_widget.rs` | Modify | Render command echo section, pre-fill preview |
| `crates/flotilla-tui/src/app/intent.rs` | Modify | Add `to_command_tokens()` for convertible intents |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Modify | Wire command echo on intent dispatch, clear echo on key press |

---

## Task 1: Parse convenience functions in flotilla-commands

**Files:**
- Create: `crates/flotilla-commands/src/parse.rs`
- Modify: `crates/flotilla-commands/src/lib.rs`

- [ ] **Step 1: Write failing tests for parse_noun_command**

In `crates/flotilla-commands/src/parse.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::CommandAction;

    #[test]
    fn parse_cr_open() {
        let noun = parse_noun_command(&["cr", "#42", "open"]).unwrap();
        let resolved = noun.resolve().unwrap();
        assert!(matches!(resolved, Resolved::NeedsContext { ref command, .. }
            if matches!(command.action, CommandAction::OpenChangeRequest { .. })));
    }

    #[test]
    fn parse_unknown_noun_errors() {
        assert!(parse_noun_command(&["bogus", "verb"]).is_err());
    }

    #[test]
    fn parse_host_routed_command() {
        let resolved = parse_host_command(&["host", "feta", "cr", "#42", "open"]).unwrap();
        match resolved {
            Resolved::NeedsContext { ref command, .. } => {
                assert!(command.host.is_some());
            }
            _ => panic!("expected NeedsContext"),
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-commands parse --locked`
Expected: FAIL — `parse_noun_command` and `parse_host_command` not found

- [ ] **Step 3: Implement parse_noun_command and parse_host_command**

In `crates/flotilla-commands/src/parse.rs`:

```rust
use clap::{Command as ClapCommand, FromArgMatches, Subcommand};

use crate::commands::host::HostNounPartial;
use crate::noun::NounCommand;
use crate::resolved::{Refinable, Resolved};

/// Parse a token sequence as a noun-verb command.
/// Tokens should NOT include a binary name — e.g. `["cr", "#42", "open"]`.
pub fn parse_noun_command(tokens: &[&str]) -> Result<NounCommand, String> {
    let cmd = <NounCommand as Subcommand>::augment_subcommands(
        ClapCommand::new("flotilla").no_binary_name(true),
    );
    let matches = cmd.try_get_matches_from(tokens).map_err(|e| e.to_string())?;
    <NounCommand as FromArgMatches>::from_arg_matches(&matches).map_err(|e| e.to_string())
}

/// Parse a token sequence starting with "host" through two-stage host routing.
/// Tokens should include "host" as the first element — e.g. `["host", "feta", "cr", "#42", "open"]`.
/// Clap uses the first element as argv[0] (ignored), so "host" serves as the binary name.
pub fn parse_host_command(tokens: &[&str]) -> Result<Resolved, String> {
    let partial = HostNounPartial::try_parse_from(tokens).map_err(|e| e.to_string())?;
    partial.refine()?.resolve()
}
```

Note: `HostNounPartial` implements `clap::Parser`, so `try_parse_from` works directly. The first token (`"host"`) is consumed by clap as argv[0] — do NOT prepend another binary name. This matches how the CLI dispatches: `SubCommand::Host(partial) => partial.refine()?.resolve()`.

- [ ] **Step 4: Add module and re-exports to lib.rs**

In `crates/flotilla-commands/src/lib.rs`, add:
```rust
pub mod parse;
pub use parse::{parse_host_command, parse_noun_command};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-commands parse --locked`
Expected: PASS

- [ ] **Step 6: Run full CI checks**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-commands/src/parse.rs crates/flotilla-commands/src/lib.rs
git commit -m "feat: add parse_noun_command and parse_host_command convenience functions"
```

---

## Task 2: Add shlex dependency and OpenContextualPalette action

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`
- Modify: `crates/flotilla-tui/src/keymap.rs`

- [ ] **Step 1: Add shlex dependency**

In `crates/flotilla-tui/Cargo.toml`, add to `[dependencies]`:
```toml
shlex = "1"
```

- [ ] **Step 2: Add OpenContextualPalette to Action enum**

In `crates/flotilla-tui/src/keymap.rs`, add `OpenContextualPalette` variant to the `Action` enum (after `OpenCommandPalette`). Also add it to `from_config_str`, `as_config_str`, and `description` methods. It should NOT be `is_global()` — it needs the widget stack for selection context.

- [ ] **Step 3: Build to verify**

Run: `cargo build --locked`
Expected: PASS (with warnings about unused variant — that's fine)

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml crates/flotilla-tui/src/keymap.rs
git commit -m "chore: add shlex dependency and OpenContextualPalette action"
```

---

## Task 3: Key binding changes

**Files:**
- Modify: `crates/flotilla-tui/src/binding_table.rs`
- Modify: `crates/flotilla-tui/src/palette.rs`

- [ ] **Step 1: Update binding table**

In `crates/flotilla-tui/src/binding_table.rs`, change the `BINDINGS` array:

| Change | Line | From | To |
|--------|------|------|----|
| Remove Shared `?` | 108 | `b(BindingModeId::Shared, "?", Action::ToggleHelp)` | Delete line |
| Normal `?` | 115 | `h(BindingModeId::Normal, "?", Action::ToggleHelp, "Help")` | `h(BindingModeId::Normal, "?", Action::OpenContextualPalette, "Ctx")` |
| Normal `h` | 123 | `b(BindingModeId::Normal, "h", Action::CycleHost)` | `h(BindingModeId::Normal, "h", Action::ToggleHelp, "Help")` |
| Help `?` | 143 | `h(BindingModeId::Help, "?", Action::ToggleHelp, "Close")` | `h(BindingModeId::Help, "h", Action::ToggleHelp, "Close")` |

- [ ] **Step 2: Update palette entries**

In `crates/flotilla-tui/src/palette.rs`, update `all_entries()`:
- `help` entry: change `key_hint: Some("?")` to `key_hint: Some("h")`
- `host` entry: change `name: "host"` to `name: "target"`, `description: "cycle target host"` to `description: "set provisioning target"`, `key_hint: Some("h")` to `key_hint: None`
- `layout` entry: change `key_hint: Some("l")` to `key_hint: Some("l")` (unchanged) — but description should say "set view layout" instead of "cycle view layout"

Also update the test `all_entries_returns_expected_count` if entry count changes.

- [ ] **Step 2b: Update help_sections and mode indicators**

In `crates/flotilla-tui/src/keymap.rs`, update `help_sections()` (~line 347): remove `CycleHost` from the "General" section, add `OpenContextualPalette` to the "Actions" section.

In `crates/flotilla-tui/src/widgets/status_bar_widget.rs`, find the host mode indicator click target (~line 275) which currently triggers `KeyCode::Char('h')`. After rebinding, `h` maps to `ToggleHelp`, not `CycleHost`. Either remove the click target or change it to open the palette pre-filled with `target `.

- [ ] **Step 3: Fix binding table tests**

Update any tests in `binding_table.rs` that reference `"?"` → `ToggleHelp` bindings (lines 524-525, 623-624). These test fixtures need to match the new binding layout.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/binding_table.rs crates/flotilla-tui/src/palette.rs
git commit -m "feat: rebind ? to contextual palette, h to help, rename host to target"
```

---

## Task 4: command_echo field and status bar rendering

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/widgets/status_bar_widget.rs`

- [ ] **Step 1: Add command_echo to UiState**

In `crates/flotilla-tui/src/app/ui_state.rs`, add to `UiState` struct:
```rust
pub command_echo: Option<String>,
```

Initialize to `None` in the constructor / `Default` impl.

- [ ] **Step 2: Update status bar rendering**

In `crates/flotilla-tui/src/widgets/status_bar_widget.rs`, modify `render_bespoke()` to add a command echo section at the left of the status bar, before key hints. Read `command_echo` from the render context (it will need to be threaded through — check how `status_message` currently reaches the renderer and follow the same pattern).

The command echo section renders:
- If `command_echo` is `Some(text)`: render the text in a dim style
- If `command_echo` is `None`: render nothing (zero width)

This is a rendering change — the exact layout adjustment depends on how `render_bespoke` currently allocates horizontal space. Follow the existing pattern for the status text section.

- [ ] **Step 3: Build and verify**

Run: `cargo build --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/widgets/status_bar_widget.rs
git commit -m "feat: add command_echo field to UiState and render in status bar"
```

---

## Task 5: Palette overhaul — argument-bearing palette-local commands

**Files:**
- Modify: `crates/flotilla-tui/src/palette.rs`

- [ ] **Step 1: Write tests for argument-bearing palette-local commands**

Add tests in `crates/flotilla-tui/src/palette.rs`:

```rust
#[test]
fn parse_layout_command() {
    let result = parse_palette_local("layout zoom");
    assert!(matches!(result, Some(PaletteLocalResult::SetLayout("zoom"))));
}

#[test]
fn parse_target_command() {
    let result = parse_palette_local("target feta");
    assert!(matches!(result, Some(PaletteLocalResult::SetTarget("feta"))));
}

#[test]
fn parse_search_command() {
    let result = parse_palette_local("search bug fix");
    assert!(matches!(result, Some(PaletteLocalResult::Search("bug fix"))));
}

#[test]
fn parse_unknown_returns_none() {
    let result = parse_palette_local("cr #42 open");
    assert!(result.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui palette --locked`
Expected: FAIL — `parse_palette_local` not found

- [ ] **Step 3: Implement PaletteLocalResult and parse_palette_local**

In `crates/flotilla-tui/src/palette.rs`, add:

```rust
#[derive(Debug, PartialEq)]
pub enum PaletteLocalResult<'a> {
    Action(Action),           // no-arg command (refresh, quit, help, etc.)
    SetLayout(&'a str),       // layout <name>
    SetTheme(&'a str),        // theme <name>
    SetTarget(&'a str),       // target <name>
    Search(&'a str),          // search <query>
}

/// Try to parse input as a palette-local command. Returns None if not a local command.
pub fn parse_palette_local(input: &str) -> Option<PaletteLocalResult<'_>> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    let arg = rest.trim();
    match cmd {
        "layout" if !arg.is_empty() => Some(PaletteLocalResult::SetLayout(arg)),
        "theme" if !arg.is_empty() => Some(PaletteLocalResult::SetTheme(arg)),
        "target" if !arg.is_empty() => Some(PaletteLocalResult::SetTarget(arg)),
        // "search" with trailing content → search command; bare "search" falls through to no-arg lookup
        "search" if input.starts_with("search ") => Some(PaletteLocalResult::Search(arg)),
        _ => {
            // Check no-arg palette entries
            let entries = all_entries();
            entries.iter().find(|e| e.name == cmd && arg.is_empty()).map(|e| PaletteLocalResult::Action(e.action))
        }
    }
}
```

Also add completion helpers:

```rust
pub const LAYOUT_VALUES: &[&str] = &["auto", "zoom", "right", "below"];
pub const TARGET_LOCAL: &str = "local";

/// Get completions for palette-local commands at the current input position.
pub fn palette_local_completions(input: &str) -> Vec<&'static str> {
    let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
    if rest.is_empty() && !input.ends_with(' ') {
        // Completing the command name itself — handled by filter_entries
        return vec![];
    }
    match cmd {
        "layout" => LAYOUT_VALUES.iter().filter(|v| v.starts_with(rest.trim())).copied().collect(),
        "theme" => vec![], // theme completions would come from config — defer
        _ => vec![],
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui palette --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/palette.rs
git commit -m "feat: palette-local command parsing with argument support"
```

---

## Task 6: Palette widget — noun-verb parsing and dispatch

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`

This is the largest task. The palette widget's `confirm()` method changes from "look up entry by name" to "tokenize → try palette-local → try noun-verb → resolve → dispatch."

- [ ] **Step 1: Write test for palette noun-verb parsing**

Add a test module in `crates/flotilla-tui/src/widgets/command_palette.rs` (or a separate test file) that verifies the parse pipeline. Since the palette widget needs `WidgetContext`, unit tests should test the parsing function directly (extracted as a free function), not the full widget.

Create a helper function `parse_palette_input` that the widget and tests both use:

```rust
/// Parse palette input text into a Resolved command or a PaletteLocalResult.
pub fn parse_palette_input(input: &str) -> Result<PaletteParseResult, String> {
    // 1. Try palette-local
    if let Some(local) = palette::parse_palette_local(input) {
        return Ok(PaletteParseResult::Local(local));
    }
    // 2. Tokenize with shlex
    let tokens = shlex::split(input).ok_or_else(|| "unclosed quote".to_string())?;
    let token_refs: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
    if token_refs.is_empty() {
        return Err("empty command".into());
    }
    // 3. Route: host → parse_host_command, else → parse_noun_command → resolve
    if token_refs[0] == "host" {
        flotilla_commands::parse_host_command(&token_refs).map(PaletteParseResult::Resolved)
    } else {
        let noun = flotilla_commands::parse_noun_command(&token_refs)?;
        noun.resolve().map(PaletteParseResult::Resolved)
    }
}
```

- [ ] **Step 2: Wire parse_palette_input into confirm()**

Replace the current `confirm()` method body. The new flow:
1. Get input text
2. Call `parse_palette_input`
3. If `PaletteLocalResult`: dispatch the local action (set layout, set target, search, or fire action)
4. If `Resolved`: call `tui_dispatch` (context injection + push to command queue)
5. If error: set `status_message` with the error text
6. Return `Outcome::Finished` on success

The `tui_dispatch` logic (repo context from active tab, host resolution) should be a function on `WidgetContext` or passed as context. Follow the pattern the widget already uses for `ctx.commands.push()`.

- [ ] **Step 3: Add context-aware filtering**

Check `ctx.is_config` (overview tab indicator). When true, filter out repo-scoped nouns from completions. When the user confirms a repo-scoped command on the overview tab, return an error.

The filtering applies in the completion list — the widget needs to know whether to show `cr`, `checkout`, etc. as completions. The simplest approach: maintain a `has_repo_context: bool` field on the widget, set from `ctx.is_config` at construction time.

- [ ] **Step 4: Build and test manually**

Run: `cargo build --locked && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/widgets/command_palette.rs
git commit -m "feat: palette noun-verb parsing with shlex tokenization and dispatch"
```

---

## Task 7: Position-aware completions

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`

- [ ] **Step 1: Implement position-aware completion**

The current palette filters entries by prefix. The new palette needs to show different completions based on parse position. Add a function:

```rust
fn palette_completions(input: &str, model: &TuiModel, has_repo_context: bool) -> Vec<CompletionItem> {
    let tokens = shlex::split(input).unwrap_or_default();
    let at_space = input.ends_with(' ');

    // No tokens or partial first token → complete noun names + palette-local commands
    if tokens.is_empty() || (tokens.len() == 1 && !at_space) {
        let prefix = tokens.first().map(|s| s.as_str()).unwrap_or("");
        return complete_top_level(prefix, has_repo_context);
    }

    let first = tokens[0].as_str();

    // Check palette-local completions
    if let Some(completions) = complete_palette_local(first, input) {
        return completions;
    }

    // Noun completions — delegate to clap Command tree + model data
    complete_noun_verb(&tokens, at_space, model, has_repo_context)
}
```

The `complete_noun_verb` function uses the Phase 1 completion engine (`flotilla_commands::complete::complete()`) for verb/flag completions, and model data for subject completions. It needs to detect the parse position (noun entered → show subjects, noun+subject → show verbs).

Subject completion sources (from model):
- `checkout` → branch names from `model.active().providers.checkouts` values
- `cr` → CR IDs from `model.active().providers.change_requests` keys
- `issue` → issue IDs from `model.active().providers.issues` keys
- `agent` → session keys from `model.active().providers.cloud_agents` keys
- `workspace` → workspace refs from `model.active().providers.workspaces` keys
- `repo` → `RepoIdentity.path` from `model.repos` keys
- `host` → host names from `model.hosts` keys

- [ ] **Step 1b: Write tests for context-aware filtering**

```rust
#[test]
fn overview_tab_excludes_repo_scoped_nouns() {
    let completions = palette_completions("", &model, false); // has_repo_context=false
    let names: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(names.contains(&"host"));
    assert!(names.contains(&"repo"));
    assert!(names.contains(&"layout"));
    assert!(!names.contains(&"cr"));
    assert!(!names.contains(&"checkout"));
}

#[test]
fn repo_tab_includes_all_nouns() {
    let completions = palette_completions("", &model, true);
    let names: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
    assert!(names.contains(&"cr"));
    assert!(names.contains(&"checkout"));
}
```

- [ ] **Step 2: Wire completions into the widget's render and filtering**

Replace the current `filter_entries` call with `palette_completions`. The completion list updates on every keystroke. The rendering code that shows filtered entries needs to show `CompletionItem` values instead of `PaletteEntry` references.

- [ ] **Step 3: Add Tab behavior**

Tab should accept the current completion: append the completion value + space to input, update cursor position, refresh completions.

- [ ] **Step 4: Build and test**

Run: `cargo build --locked && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/widgets/command_palette.rs
git commit -m "feat: position-aware completions from model data and clap tree"
```

---

## Task 8: Contextual palette (pre-fill from selection)

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs`
- Modify: `crates/flotilla-tui/src/widgets/overview_page.rs`

- [ ] **Step 1: Write pre-fill mapping function**

Add to `crates/flotilla-tui/src/widgets/command_palette.rs`:

```rust
/// Map a work item to a palette pre-fill string.
pub fn palette_prefill(item: &WorkItem) -> Option<String> {
    if let Some(cr_key) = &item.change_request_key {
        return Some(format!("cr {} ", cr_key));
    }
    if let Some(branch) = &item.branch {
        if item.checkout_key().is_some() {
            return Some(format!("checkout {} ", branch));
        }
    }
    if let Some(issue_key) = item.issue_keys.first() {
        return Some(format!("issue {} ", issue_key));
    }
    if let Some(session_key) = &item.session_key {
        return Some(format!("agent {} ", session_key));
    }
    if let Some(ws_ref) = item.workspace_refs.first() {
        return Some(format!("workspace {} ", ws_ref));
    }
    None
}
```

- [ ] **Step 1b: Write test for pre-fill priority**

```rust
#[test]
fn prefill_prefers_cr_over_checkout() {
    let mut item = WorkItem::default();
    item.change_request_key = Some("#42".into());
    item.branch = Some("feat".into());
    let prefill = palette_prefill(&item);
    assert_eq!(prefill, Some("cr #42 ".into()));
}
```

- [ ] **Step 2: Handle OpenContextualPalette in repo_page**

In `crates/flotilla-tui/src/widgets/repo_page.rs`, handle `Action::OpenContextualPalette`:
- Get selected work item from table
- Call `palette_prefill(item)` to get pre-fill text
- Create `CommandPaletteWidget` with pre-filled input (use `with_state` constructor or add a `with_prefill` method)
- Push the widget

If no item is selected, open empty palette (same as `/`).

- [ ] **Step 3: Handle OpenContextualPalette in overview_page**

In `crates/flotilla-tui/src/widgets/overview_page.rs`, handle `Action::OpenContextualPalette`:
- Open empty palette (overview has no work items to pre-fill from)

- [ ] **Step 4: Handle Esc to clear selection in repo_page**

In `crates/flotilla-tui/src/widgets/repo_page.rs`, modify the dismiss chain (around line 273-274, before the quit fallback):
- If table has a selected row, clear the selection and return `Outcome::Consumed`
- This goes after "clear multi-selection" and before "quit"

Check `WorkItemTable` for a method to clear selection — if none exists, add `pub fn clear_selection(&mut self)` that sets `selected_selectable_idx = None` and `table_state.select(None)`.

- [ ] **Step 5: Build and test**

Run: `cargo build --locked && cargo test --workspace --locked`

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/widgets/command_palette.rs crates/flotilla-tui/src/widgets/repo_page.rs crates/flotilla-tui/src/widgets/overview_page.rs crates/flotilla-tui/src/widgets/work_item_table.rs
git commit -m "feat: contextual palette with pre-fill from selection, Esc clears selection"
```

---

## Task 9: Intent adapter — to_command_tokens

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`

- [ ] **Step 1: Write tests for to_command_tokens**

In `crates/flotilla-tui/src/app/intent/tests.rs` (or add a test module), test each convertible intent:

```rust
#[test]
fn open_cr_produces_tokens() {
    let item = work_item_with_cr_key("#42");
    let tokens = Intent::OpenChangeRequest.to_command_tokens(&item);
    assert_eq!(tokens, Some(vec!["cr".into(), "#42".into(), "open".into()]));
}

#[test]
fn remove_checkout_returns_none() {
    let item = work_item_with_branch("my-feature");
    assert!(Intent::RemoveCheckout.to_command_tokens(&item).is_none());
}
```

Test all 11 intents per the spec's conversion table.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui intent --locked`
Expected: FAIL

- [ ] **Step 3: Implement to_command_tokens**

In `crates/flotilla-tui/src/app/intent.rs`, add:

```rust
/// Build a command token vector from a work item, or None if this intent
/// cannot be expressed as a simple noun-verb command.
pub fn to_command_tokens(&self, item: &WorkItem) -> Option<Vec<String>> {
    match self {
        Intent::OpenChangeRequest => {
            let id = item.change_request_key.as_ref()?;
            Some(vec!["cr".into(), id.clone(), "open".into()])
        }
        Intent::CloseChangeRequest => {
            let id = item.change_request_key.as_ref()?;
            Some(vec!["cr".into(), id.clone(), "close".into()])
        }
        Intent::OpenIssue => {
            let id = item.issue_keys.first()?;
            Some(vec!["issue".into(), id.clone(), "open".into()])
        }
        Intent::ArchiveSession => {
            let key = item.session_key.as_ref()?;
            Some(vec!["agent".into(), key.clone(), "archive".into()])
        }
        Intent::TeleportSession => {
            let key = item.session_key.as_ref()?;
            let mut tokens = vec!["agent".into(), key.clone(), "teleport".into()];
            if let Some(branch) = &item.branch {
                tokens.extend(["--branch".into(), branch.clone()]);
            }
            Some(tokens)
        }
        Intent::SwitchToWorkspace => {
            let ws_ref = item.workspace_refs.first()?;
            Some(vec!["workspace".into(), ws_ref.clone(), "select".into()])
        }
        Intent::CreateCheckout => {
            let branch = item.branch.as_ref()?;
            let mut tokens = vec!["checkout".into(), "create".into(), "--branch".into(), branch.clone()];
            if item.kind == WorkItemKind::RemoteBranch || item.kind == WorkItemKind::ChangeRequest {
                // tracking existing branch, not fresh
            } else {
                tokens.push("--fresh".into());
            }
            Some(tokens)
        }
        Intent::GenerateBranchName => {
            if item.issue_keys.is_empty() {
                return None;
            }
            let ids = item.issue_keys.join(",");
            Some(vec!["issue".into(), ids, "suggest-branch".into()])
        }
        // Non-convertible
        Intent::RemoveCheckout | Intent::CreateWorkspace | Intent::LinkIssuesToChangeRequest => None,
    }
}
```

Note: `to_command_tokens` takes only `&WorkItem`, not `&App` — it doesn't need app context because host routing is handled by `HostResolution` in the resolve step.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui intent --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/intent.rs
git commit -m "feat: intent to_command_tokens for convertible intents"
```

---

## Task 10: Command echo on key-binding actions

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` (or wherever `dispatch_if_available` / `resolve_and_push` lives)
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Set command_echo when intent dispatches**

Find where `dispatch_if_available` or `resolve_and_push` is called (in `key_handlers.rs` or `app/mod.rs`). After a convertible intent's tokens are produced, join them with spaces and set `app.ui.command_echo`:

```rust
if let Some(tokens) = intent.to_command_tokens(item) {
    app.ui.command_echo = Some(tokens.join(" "));
}
```

- [ ] **Step 2: Clear command_echo on key press**

In the main key handling path (before action dispatch), clear the echo:

```rust
app.ui.command_echo = None;
```

This ensures echo is visible only until the next key press.

- [ ] **Step 3: Add pre-fill preview to status bar**

In the status bar rendering (or wherever the command echo section is rendered from Task 4), when `command_echo` is `None` and there's a selected work item, show the pre-fill preview: `/cr #42 ?` with `?` highlighted.

This requires the status bar renderer to have access to the selected work item's pre-fill. Thread this through the render context — compute `palette_prefill` from the selected item and pass it to the status bar.

- [ ] **Step 4: Build and test**

Run: `cargo build --locked && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/widgets/status_bar_widget.rs
git commit -m "feat: command echo on key-binding actions, pre-fill preview in status bar"
```

---

## Task 11: TUI dispatch — resolve ambient context

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs` (or create a dispatch helper module)

- [ ] **Step 1: Implement fill_repo_sentinels**

The CLI has `inject_repo_context` in `src/main.rs:376` that fills SENTINEL `RepoSelector::Query("")` fields. The TUI needs an equivalent that takes a `RepoSelector` directly instead of `&Cli`. Add to the dispatch module:

```rust
/// Fill SENTINEL empty RepoSelector::Query("") fields in a CommandAction with a real repo selector.
/// Mirrors inject_repo_context from main.rs but takes a RepoSelector directly.
fn fill_repo_sentinels(action: &mut CommandAction, repo: RepoSelector) {
    match action {
        CommandAction::Checkout { repo: r, .. } if *r == RepoSelector::Query(String::new()) => *r = repo,
        CommandAction::SearchIssues { repo: r, .. } if *r == RepoSelector::Query(String::new()) => *r = repo,
        _ => {}
    }
}
```

- [ ] **Step 2: Write tests for tui_dispatch**

```rust
#[test]
fn dispatch_ready_command_pushes_directly() {
    let cmd = Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } };
    let mut queue = CommandQueue::default();
    let result = tui_dispatch(Resolved::Ready(cmd.clone()), None, &dispatch_ctx(false, None));
    assert!(result.is_ok());
    // command should be in queue unchanged
}

#[test]
fn dispatch_needs_repo_on_overview_tab_errors() {
    let cmd = Command { host: None, context_repo: None,
        action: CommandAction::Checkout { repo: RepoSelector::Query("".into()), target: CheckoutTarget::Branch("feat".into()), issue_ids: vec![] } };
    let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Required, host: HostResolution::ProvisioningTarget };
    let result = tui_dispatch(resolved, None, &dispatch_ctx(true, None)); // is_config=true → overview
    assert!(result.is_err());
}

#[test]
fn dispatch_needs_repo_fills_sentinels() {
    let cmd = Command { host: None, context_repo: None,
        action: CommandAction::Checkout { repo: RepoSelector::Query("".into()), target: CheckoutTarget::Branch("feat".into()), issue_ids: vec![] } };
    let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Required, host: HostResolution::ProvisioningTarget };
    let result = tui_dispatch(resolved, None, &dispatch_ctx(false, Some("org/repo")));
    assert!(result.is_ok());
    // command.context_repo should be set, sentinel should be filled
}

#[test]
fn dispatch_inferred_sets_context_when_available() {
    let cmd = Command { host: None, context_repo: None,
        action: CommandAction::OpenChangeRequest { id: "#42".into() } };
    let resolved = Resolved::NeedsContext { command: cmd, repo: RepoContext::Inferred, host: HostResolution::ProviderHost };
    let result = tui_dispatch(resolved, None, &dispatch_ctx(false, Some("org/repo")));
    assert!(result.is_ok());
    // command.context_repo should be Some
}
```

- [ ] **Step 3: Implement tui_dispatch and resolve_host**

Implement per the spec's pseudocode, using real TUI types. This function lives in the palette module or a new `dispatch.rs` helper. It needs access to `WidgetContext` fields: `is_config`, `active_repo`, `target_host`, and the model for `item_execution_host`.

- [ ] **Step 3: Wire into palette confirm**

The palette's `confirm()` method calls `tui_dispatch` after parsing and resolving. Errors are shown as status messages.

- [ ] **Step 4: Run full CI**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/widgets/command_palette.rs
git commit -m "feat: tui_dispatch with repo context from active tab and host resolution"
```

---

## Task 12: Final integration and cleanup

**Files:**
- Various — integration testing and cleanup

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Fix any warnings.

- [ ] **Step 3: Run format check**

Run: `cargo +nightly-2026-03-12 fmt --check`
Fix any formatting issues with `cargo +nightly-2026-03-12 fmt`.

- [ ] **Step 4: Manual smoke test (if possible)**

Run `cargo run` and test:
- `/` opens empty palette, shows nouns + palette-local commands
- Typing `cr ` shows CR ID completions (if any CRs exist)
- `?` on a selected work item opens pre-filled palette
- `h` toggles help
- `Esc` clears selection when nothing else is dismissable
- Status bar shows command echo on `p` (open PR) key binding
- `layout zoom` sets layout
- `target local` sets provisioning target

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: integration cleanup for palette phase 2"
```

---

## Dependency Order

```
Task 1 (parse functions)
    ↓
Task 2 (shlex + action variant)
    ↓
Task 3 (key bindings)  ←  independent of Task 1
    ↓
Task 4 (command_echo + status bar)  ←  independent of Task 1-3
    ↓
Task 5 (palette-local commands)
    ↓
Task 6 (palette noun-verb parsing)  ←  depends on Task 1, 2, 5
    ↓
Task 7 (position-aware completions)  ←  depends on Task 6
    ↓
Task 8 (contextual palette + Esc)  ←  depends on Task 3, 6
    ↓
Task 9 (intent adapter)  ←  independent of Task 6-8
    ↓
Task 10 (command echo)  ←  depends on Task 4, 9
    ↓
Task 11 (TUI dispatch)  ←  depends on Task 6, 9
    ↓
Task 12 (integration)  ←  depends on all
```

Tasks that can run in parallel:
- Tasks 1, 3, 4, 9 are independent of each other
- Tasks 2 and 5 are independent of each other
