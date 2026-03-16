# Configurable Key Bindings Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace hardcoded key bindings with a configurable keymap system that loads from `config.toml` and auto-generates the help screen.

**Architecture:** A `Keymap` struct holds shared and per-mode key bindings as `HashMap<KeyCombination, Action>`. The `App` constructor builds the keymap from programmatic defaults merged with user overrides from `[ui.keys]` in `config.toml`. `resolve_action` delegates to `Keymap::resolve()`. The help screen renders from keymap contents instead of hardcoded text.

**Tech Stack:** `crokey 1.4` (key combo parsing/display/serde, compatible with project's crossterm 0.29), `serde` for config deserialization.

**Design departures from spec:**
- Key bindings are stored in `config.toml` under `[ui.keys]` rather than a separate `keybindings.toml` (simpler, user preference).
- Help screen action descriptions use static strings rather than dynamic provider labels (e.g. "Remove checkout" instead of "Remove {noun}"). Provider label customisation is rare and can be added later if needed.

**Implementation note:** Verify `crokey::key!` macro works with special characters (`?`, `{`, `}`). If it doesn't support these, construct `KeyCombination` from `KeyEvent` manually instead.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/flotilla-tui/Cargo.toml` | Modify | Add `crokey = "1.4"` dependency |
| `crates/flotilla-tui/src/keymap.rs` | Create | `Action` enum, `ModeId` enum, `Keymap` struct, defaults, resolve, config merge, help generation |
| `crates/flotilla-tui/src/lib.rs` | Modify | Add `pub mod keymap;` |
| `crates/flotilla-core/src/config.rs` | Modify | Add `KeysConfig` (raw string maps) to `UiConfig` |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Modify | Replace `Action` enum and `resolve_action` hardcoded match with `Keymap::resolve()` |
| `crates/flotilla-tui/src/app/mod.rs` | Modify | Store `Keymap` in `App`, build from config in constructor |
| `crates/flotilla-tui/src/ui.rs` | Modify | Replace hardcoded `render_help` with keymap-driven help text |

---

## Chunk 1: Keymap Module Foundation

### Task 1: Add crokey dependency

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`
- Modify: `crates/flotilla-tui/src/lib.rs`

- [ ] **Step 1: Add crokey to Cargo.toml**

Add `crokey = "1.4"` to the `[dependencies]` section of `crates/flotilla-tui/Cargo.toml`, after `catppuccin`:

```toml
crokey = "1.4"
```

- [ ] **Step 2: Create empty keymap module**

Create `crates/flotilla-tui/src/keymap.rs` with a placeholder:

```rust
//! Configurable key binding system.
//!
//! `Keymap` holds shared and per-mode key bindings. The `App` constructor
//! builds a keymap from programmatic defaults merged with user overrides
//! from `[ui.keys]` in `config.toml`.

#[cfg(test)]
mod tests {}
```

- [ ] **Step 3: Register module in lib.rs**

Add `pub mod keymap;` to `crates/flotilla-tui/src/lib.rs` (after `pub mod event_log;`).

- [ ] **Step 4: Verify it compiles**

Run: `cargo build -p flotilla-tui`
Expected: compiles successfully, crokey resolves.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml crates/flotilla-tui/src/lib.rs crates/flotilla-tui/src/keymap.rs
git commit -m "chore: add crokey dependency and empty keymap module"
```

---

### Task 2: Define Action enum in keymap module

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`

The `Action` enum currently lives as a private enum in `key_handlers.rs`. Move it to `keymap.rs`, make it public, and add string conversion methods for config support. Keep `Dispatch(Intent)` for dispatch ergonomics.

- [ ] **Step 1: Write tests for Action string round-trip**

Add to `keymap.rs`:

```rust
use super::app::intent::Intent;

/// A resolved UI action. Shared navigation actions (`SelectNext`, `Confirm`, etc.)
/// dispatch based on the current `FocusTarget`. Intent-wrapping variants route
/// through the existing `Intent::resolve()` pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SelectNext,
    SelectPrev,
    Confirm,
    Dismiss,
    Quit,
    Refresh,
    PrevTab,
    NextTab,
    MoveTabLeft,
    MoveTabRight,
    ToggleHelp,
    ToggleMultiSelect,
    ToggleProviders,
    ToggleDebug,
    ToggleStatusBarKeys,
    CycleHost,
    CycleLayout,
    CycleTheme,
    OpenActionMenu,
    OpenBranchInput,
    OpenIssueSearch,
    OpenFilePicker,
    Dispatch(Intent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_config_str_round_trips_for_all_non_intent_actions() {
        let actions = [
            ("select_next", Action::SelectNext),
            ("select_prev", Action::SelectPrev),
            ("confirm", Action::Confirm),
            ("dismiss", Action::Dismiss),
            ("quit", Action::Quit),
            ("refresh", Action::Refresh),
            ("prev_tab", Action::PrevTab),
            ("next_tab", Action::NextTab),
            ("move_tab_left", Action::MoveTabLeft),
            ("move_tab_right", Action::MoveTabRight),
            ("toggle_help", Action::ToggleHelp),
            ("toggle_multi_select", Action::ToggleMultiSelect),
            ("toggle_providers", Action::ToggleProviders),
            ("toggle_debug", Action::ToggleDebug),
            ("toggle_status_bar_keys", Action::ToggleStatusBarKeys),
            ("cycle_host", Action::CycleHost),
            ("cycle_layout", Action::CycleLayout),
            ("cycle_theme", Action::CycleTheme),
            ("open_action_menu", Action::OpenActionMenu),
            ("open_branch_input", Action::OpenBranchInput),
            ("open_issue_search", Action::OpenIssueSearch),
            ("open_file_picker", Action::OpenFilePicker),
        ];
        for (name, action) in &actions {
            assert_eq!(Action::from_config_str(name), Some(*action), "from_config_str failed for {name}");
            assert_eq!(action.as_config_str(), *name, "as_config_str failed for {action:?}");
        }
    }

    #[test]
    fn action_config_str_round_trips_for_intent_actions() {
        let actions = [
            ("switch_to_workspace", Action::Dispatch(Intent::SwitchToWorkspace)),
            ("create_workspace", Action::Dispatch(Intent::CreateWorkspace)),
            ("remove_checkout", Action::Dispatch(Intent::RemoveCheckout)),
            ("create_checkout", Action::Dispatch(Intent::CreateCheckout)),
            ("generate_branch_name", Action::Dispatch(Intent::GenerateBranchName)),
            ("open_change_request", Action::Dispatch(Intent::OpenChangeRequest)),
            ("open_issue", Action::Dispatch(Intent::OpenIssue)),
            ("link_issues_to_change_request", Action::Dispatch(Intent::LinkIssuesToChangeRequest)),
            ("teleport_session", Action::Dispatch(Intent::TeleportSession)),
            ("archive_session", Action::Dispatch(Intent::ArchiveSession)),
            ("close_change_request", Action::Dispatch(Intent::CloseChangeRequest)),
        ];
        for (name, action) in &actions {
            assert_eq!(Action::from_config_str(name), Some(*action), "from_config_str failed for {name}");
            assert_eq!(action.as_config_str(), *name, "as_config_str failed for {action:?}");
        }
    }

    #[test]
    fn action_from_config_str_returns_none_for_unknown() {
        assert_eq!(Action::from_config_str("nonexistent_action"), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-tui keymap::tests --no-run`
Expected: compile error — `Action::from_config_str` and `as_config_str` don't exist yet.

- [ ] **Step 3: Implement Action string conversion**

Add to `keymap.rs` (the `impl Action` block):

```rust
impl Action {
    /// Parse an action name from config TOML. Returns `None` for unrecognised names.
    pub fn from_config_str(s: &str) -> Option<Self> {
        Some(match s {
            "select_next" => Action::SelectNext,
            "select_prev" => Action::SelectPrev,
            "confirm" => Action::Confirm,
            "dismiss" => Action::Dismiss,
            "quit" => Action::Quit,
            "refresh" => Action::Refresh,
            "prev_tab" => Action::PrevTab,
            "next_tab" => Action::NextTab,
            "move_tab_left" => Action::MoveTabLeft,
            "move_tab_right" => Action::MoveTabRight,
            "toggle_help" => Action::ToggleHelp,
            "toggle_multi_select" => Action::ToggleMultiSelect,
            "toggle_providers" => Action::ToggleProviders,
            "toggle_debug" => Action::ToggleDebug,
            "toggle_status_bar_keys" => Action::ToggleStatusBarKeys,
            "cycle_host" => Action::CycleHost,
            "cycle_layout" => Action::CycleLayout,
            "cycle_theme" => Action::CycleTheme,
            "open_action_menu" => Action::OpenActionMenu,
            "open_branch_input" => Action::OpenBranchInput,
            "open_issue_search" => Action::OpenIssueSearch,
            "open_file_picker" => Action::OpenFilePicker,
            "switch_to_workspace" => Action::Dispatch(Intent::SwitchToWorkspace),
            "create_workspace" => Action::Dispatch(Intent::CreateWorkspace),
            "remove_checkout" => Action::Dispatch(Intent::RemoveCheckout),
            "create_checkout" => Action::Dispatch(Intent::CreateCheckout),
            "generate_branch_name" => Action::Dispatch(Intent::GenerateBranchName),
            "open_change_request" => Action::Dispatch(Intent::OpenChangeRequest),
            "open_issue" => Action::Dispatch(Intent::OpenIssue),
            "link_issues_to_change_request" => Action::Dispatch(Intent::LinkIssuesToChangeRequest),
            "teleport_session" => Action::Dispatch(Intent::TeleportSession),
            "archive_session" => Action::Dispatch(Intent::ArchiveSession),
            "close_change_request" => Action::Dispatch(Intent::CloseChangeRequest),
            _ => return None,
        })
    }

    /// Stable config name for this action.
    pub fn as_config_str(&self) -> &'static str {
        match self {
            Action::SelectNext => "select_next",
            Action::SelectPrev => "select_prev",
            Action::Confirm => "confirm",
            Action::Dismiss => "dismiss",
            Action::Quit => "quit",
            Action::Refresh => "refresh",
            Action::PrevTab => "prev_tab",
            Action::NextTab => "next_tab",
            Action::MoveTabLeft => "move_tab_left",
            Action::MoveTabRight => "move_tab_right",
            Action::ToggleHelp => "toggle_help",
            Action::ToggleMultiSelect => "toggle_multi_select",
            Action::ToggleProviders => "toggle_providers",
            Action::ToggleDebug => "toggle_debug",
            Action::ToggleStatusBarKeys => "toggle_status_bar_keys",
            Action::CycleHost => "cycle_host",
            Action::CycleLayout => "cycle_layout",
            Action::CycleTheme => "cycle_theme",
            Action::OpenActionMenu => "open_action_menu",
            Action::OpenBranchInput => "open_branch_input",
            Action::OpenIssueSearch => "open_issue_search",
            Action::OpenFilePicker => "open_file_picker",
            Action::Dispatch(Intent::SwitchToWorkspace) => "switch_to_workspace",
            Action::Dispatch(Intent::CreateWorkspace) => "create_workspace",
            Action::Dispatch(Intent::RemoveCheckout) => "remove_checkout",
            Action::Dispatch(Intent::CreateCheckout) => "create_checkout",
            Action::Dispatch(Intent::GenerateBranchName) => "generate_branch_name",
            Action::Dispatch(Intent::OpenChangeRequest) => "open_change_request",
            Action::Dispatch(Intent::OpenIssue) => "open_issue",
            Action::Dispatch(Intent::LinkIssuesToChangeRequest) => "link_issues_to_change_request",
            Action::Dispatch(Intent::TeleportSession) => "teleport_session",
            Action::Dispatch(Intent::ArchiveSession) => "archive_session",
            Action::Dispatch(Intent::CloseChangeRequest) => "close_change_request",
        }
    }

    /// Human-readable description for help screen.
    pub fn description(&self) -> &'static str {
        match self {
            Action::SelectNext => "Navigate down",
            Action::SelectPrev => "Navigate up",
            Action::Confirm => "Confirm / execute",
            Action::Dismiss => "Cancel / dismiss / quit",
            Action::Quit => "Quit",
            Action::Refresh => "Refresh data",
            Action::PrevTab => "Previous repo tab",
            Action::NextTab => "Next repo tab",
            Action::MoveTabLeft => "Move tab left",
            Action::MoveTabRight => "Move tab right",
            Action::ToggleHelp => "Toggle help",
            Action::ToggleMultiSelect => "Toggle selection",
            Action::ToggleProviders => "Toggle providers panel",
            Action::ToggleDebug => "Toggle debug panel",
            Action::ToggleStatusBarKeys => "Toggle status bar keys",
            Action::CycleHost => "Cycle target host",
            Action::CycleLayout => "Cycle layout",
            Action::CycleTheme => "Cycle theme",
            Action::OpenActionMenu => "Action menu",
            Action::OpenBranchInput => "New branch",
            Action::OpenIssueSearch => "Search issues",
            Action::OpenFilePicker => "Add repository",
            Action::Dispatch(Intent::SwitchToWorkspace) => "Switch to workspace",
            Action::Dispatch(Intent::CreateWorkspace) => "Create workspace",
            Action::Dispatch(Intent::RemoveCheckout) => "Remove checkout",
            Action::Dispatch(Intent::CreateCheckout) => "Create checkout",
            Action::Dispatch(Intent::GenerateBranchName) => "Generate branch name",
            Action::Dispatch(Intent::OpenChangeRequest) => "Open PR in browser",
            Action::Dispatch(Intent::OpenIssue) => "Open issue in browser",
            Action::Dispatch(Intent::LinkIssuesToChangeRequest) => "Link issues to PR",
            Action::Dispatch(Intent::TeleportSession) => "Teleport session",
            Action::Dispatch(Intent::ArchiveSession) => "Archive session",
            Action::Dispatch(Intent::CloseChangeRequest) => "Close PR",
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui keymap::tests`
Expected: all 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/keymap.rs
git commit -m "feat: define Action enum with config string conversion in keymap module"
```

---

### Task 3: Define ModeId and Keymap struct with defaults

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`

The `Keymap` struct holds shared bindings and per-mode bindings. `ModeId` identifies the configurable modes (text input modes stay hardcoded). `Keymap::defaults()` produces bindings matching the current hardcoded behaviour.

- [ ] **Step 1: Write tests for Keymap defaults and resolve**

Add to the `tests` module in `keymap.rs`:

```rust
use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn shifted(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

#[test]
fn defaults_resolve_shared_navigation() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('j'))), Some(Action::SelectNext));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Down)), Some(Action::SelectNext));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('k'))), Some(Action::SelectPrev));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Up)), Some(Action::SelectPrev));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Enter)), Some(Action::Confirm));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Esc)), Some(Action::Dismiss));
}

#[test]
fn shared_bindings_work_across_modes() {
    let keymap = Keymap::defaults();
    // j/k are shared — should work in Help, Config, ActionMenu
    assert_eq!(keymap.resolve(ModeId::Help, key(KeyCode::Char('j'))), Some(Action::SelectNext));
    assert_eq!(keymap.resolve(ModeId::Config, key(KeyCode::Char('k'))), Some(Action::SelectPrev));
    assert_eq!(keymap.resolve(ModeId::ActionMenu, key(KeyCode::Enter)), Some(Action::Confirm));
}

#[test]
fn normal_mode_specific_bindings() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('.'))), Some(Action::OpenActionMenu));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('d'))), Some(Action::Dispatch(Intent::RemoveCheckout)));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('p'))), Some(Action::Dispatch(Intent::OpenChangeRequest)));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('r'))), Some(Action::Refresh));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char(' '))), Some(Action::ToggleMultiSelect));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('n'))), Some(Action::OpenBranchInput));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('/'))), Some(Action::OpenIssueSearch));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('a'))), Some(Action::OpenFilePicker));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('c'))), Some(Action::ToggleProviders));
    assert_eq!(keymap.resolve(ModeId::Normal, shifted(KeyCode::Char('D'))), Some(Action::ToggleDebug));
    assert_eq!(keymap.resolve(ModeId::Normal, shifted(KeyCode::Char('T'))), Some(Action::CycleTheme));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('h'))), Some(Action::CycleHost));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('l'))), Some(Action::CycleLayout));
}

#[test]
fn mode_specific_overrides_shared() {
    let keymap = Keymap::defaults();
    // 'q' in Normal is Quit, but in Help/Config/ActionMenu/DeleteConfirm/CloseConfirm it's Dismiss
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(keymap.resolve(ModeId::Help, key(KeyCode::Char('q'))), Some(Action::Dismiss));
    assert_eq!(keymap.resolve(ModeId::Config, key(KeyCode::Char('q'))), Some(Action::Dismiss));
    assert_eq!(keymap.resolve(ModeId::ActionMenu, key(KeyCode::Char('q'))), Some(Action::Dismiss));
    assert_eq!(keymap.resolve(ModeId::DeleteConfirm, key(KeyCode::Char('q'))), Some(Action::Dismiss));
    assert_eq!(keymap.resolve(ModeId::CloseConfirm, key(KeyCode::Char('q'))), Some(Action::Dismiss));
}

#[test]
fn tab_switching_in_normal_and_config() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('['))), Some(Action::PrevTab));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char(']'))), Some(Action::NextTab));
    assert_eq!(keymap.resolve(ModeId::Config, key(KeyCode::Char('['))), Some(Action::PrevTab));
    assert_eq!(keymap.resolve(ModeId::Config, key(KeyCode::Char(']'))), Some(Action::NextTab));
}

#[test]
fn delete_confirm_has_y_n_bindings() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::DeleteConfirm, key(KeyCode::Char('y'))), Some(Action::Confirm));
    assert_eq!(keymap.resolve(ModeId::DeleteConfirm, key(KeyCode::Char('n'))), Some(Action::Dismiss));
}

#[test]
fn close_confirm_has_y_n_bindings() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::CloseConfirm, key(KeyCode::Char('y'))), Some(Action::Confirm));
    assert_eq!(keymap.resolve(ModeId::CloseConfirm, key(KeyCode::Char('n'))), Some(Action::Dismiss));
}

#[test]
fn toggle_status_bar_keys_is_shared_across_modes() {
    let keymap = Keymap::defaults();
    // K is a shared binding — works in all modes (text input modes bypass the keymap)
    assert_eq!(keymap.resolve(ModeId::Normal, shifted(KeyCode::Char('K'))), Some(Action::ToggleStatusBarKeys));
    assert_eq!(keymap.resolve(ModeId::Help, shifted(KeyCode::Char('K'))), Some(Action::ToggleStatusBarKeys));
    assert_eq!(keymap.resolve(ModeId::Config, shifted(KeyCode::Char('K'))), Some(Action::ToggleStatusBarKeys));
    assert_eq!(keymap.resolve(ModeId::ActionMenu, shifted(KeyCode::Char('K'))), Some(Action::ToggleStatusBarKeys));
}

#[test]
fn help_mode_toggle_with_question_mark() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::Help, key(KeyCode::Char('?'))), Some(Action::ToggleHelp));
}

#[test]
fn unbound_key_returns_none() {
    let keymap = Keymap::defaults();
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('z'))), None);
}

#[test]
fn file_picker_falls_through_to_shared_but_resolve_action_bypasses_keymap() {
    // FilePicker is handled by an early return in resolve_action (not the keymap).
    // At the keymap level, shared bindings still apply — but resolve_action never
    // reaches the keymap for FilePicker mode. This test just documents that the
    // keymap itself has no mode-specific FilePicker bindings.
    let keymap = Keymap::defaults();
    // Shared 'j' -> SelectNext still resolves at keymap level
    assert_eq!(keymap.resolve(ModeId::FilePicker, key(KeyCode::Char('j'))), Some(Action::SelectNext));
    // But '?' (ToggleHelp) would also resolve — which is why resolve_action
    // bypasses the keymap entirely for FilePicker.
    assert_eq!(keymap.resolve(ModeId::FilePicker, key(KeyCode::Char('?'))), Some(Action::ToggleHelp));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui keymap::tests --no-run`
Expected: compile error — `Keymap`, `ModeId` don't exist yet.

- [ ] **Step 3: Implement ModeId and Keymap**

Add to `keymap.rs` (before `impl Action`):

```rust
use std::collections::HashMap;

use crokey::KeyCombination;
use crossterm::event::KeyEvent;

/// Identifies a UI mode for key binding configuration.
///
/// Text input modes (BranchInput, IssueSearch) are excluded — they forward
/// unrecognised keys to `tui_input` and only intercept Esc/Enter, which is
/// handled separately in `handle_key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModeId {
    Normal,
    Help,
    Config,
    ActionMenu,
    DeleteConfirm,
    CloseConfirm,
    /// File picker navigation (j/k/Enter/Esc only — text input handled separately).
    FilePicker,
    /// Branch name input (Esc/Enter only — text input handled separately).
    BranchInput,
    /// Issue search input (Esc/Enter only — text input handled separately).
    IssueSearch,
}

/// Configurable key binding map with shared and per-mode layers.
///
/// Resolution order: mode-specific bindings checked first, then shared.
/// Text input modes bypass the keymap entirely (handled in `handle_key`).
pub struct Keymap {
    shared: HashMap<KeyCombination, Action>,
    modes: HashMap<ModeId, HashMap<KeyCombination, Action>>,
}

impl Keymap {
    /// Look up an action for the given mode and key event.
    ///
    /// Checks mode-specific bindings first, then falls back to shared bindings.
    pub fn resolve(&self, mode: ModeId, key: KeyEvent) -> Option<Action> {
        let combo = KeyCombination::from(key);
        self.modes
            .get(&mode)
            .and_then(|bindings| bindings.get(&combo))
            .or_else(|| self.shared.get(&combo))
            .copied()
    }

    /// Programmatic defaults matching the current hardcoded key bindings.
    pub fn defaults() -> Self {
        use crossterm::event::KeyCode;
        use crokey::key;

        let mut shared: HashMap<KeyCombination, Action> = HashMap::new();
        let mut modes: HashMap<ModeId, HashMap<KeyCombination, Action>> = HashMap::new();

        // ── Shared bindings ──
        shared.insert(key!(j), Action::SelectNext);
        shared.insert(key!(Down), Action::SelectNext);
        shared.insert(key!(k), Action::SelectPrev);
        shared.insert(key!(Up), Action::SelectPrev);
        shared.insert(key!(Enter), Action::Confirm);
        shared.insert(key!(Esc), Action::Dismiss);
        shared.insert(key!('?'), Action::ToggleHelp);
        // K works in all non-text-input modes (text input modes are early-returned
        // in resolve_action so they never reach the keymap).
        shared.insert(key!(K), Action::ToggleStatusBarKeys);

        // ── Normal mode ──
        let normal = modes.entry(ModeId::Normal).or_default();
        normal.insert(key!(q), Action::Quit);
        normal.insert(key!(r), Action::Refresh);
        normal.insert(key!('['), Action::PrevTab);
        normal.insert(key!(']'), Action::NextTab);
        normal.insert(key!('{'), Action::MoveTabLeft);
        normal.insert(key!('}'), Action::MoveTabRight);
        normal.insert(key!(Space), Action::ToggleMultiSelect);
        normal.insert(key!(h), Action::CycleHost);
        normal.insert(key!(l), Action::CycleLayout);
        normal.insert(key!(T), Action::CycleTheme);
        normal.insert(key!('.'), Action::OpenActionMenu);
        normal.insert(key!(n), Action::OpenBranchInput);
        normal.insert(key!('/'), Action::OpenIssueSearch);
        normal.insert(key!(a), Action::OpenFilePicker);
        normal.insert(key!(c), Action::ToggleProviders);
        normal.insert(key!(D), Action::ToggleDebug);
        normal.insert(key!(d), Action::Dispatch(Intent::RemoveCheckout));
        normal.insert(key!(p), Action::Dispatch(Intent::OpenChangeRequest));

        // ── Config mode ──
        let config = modes.entry(ModeId::Config).or_default();
        config.insert(key!(q), Action::Dismiss);
        config.insert(key!('['), Action::PrevTab);
        config.insert(key!(']'), Action::NextTab);

        // ── Help mode ──
        let help = modes.entry(ModeId::Help).or_default();
        help.insert(key!(q), Action::Dismiss);

        // ── Action menu mode ──
        let action_menu = modes.entry(ModeId::ActionMenu).or_default();
        action_menu.insert(key!(q), Action::Dismiss);

        // ── Delete confirm mode ──
        let delete_confirm = modes.entry(ModeId::DeleteConfirm).or_default();
        delete_confirm.insert(key!(y), Action::Confirm);
        delete_confirm.insert(key!(n), Action::Dismiss);
        delete_confirm.insert(key!(q), Action::Dismiss);

        // ── Close confirm mode ──
        let close_confirm = modes.entry(ModeId::CloseConfirm).or_default();
        close_confirm.insert(key!(y), Action::Confirm);
        close_confirm.insert(key!(n), Action::Dismiss);
        close_confirm.insert(key!(q), Action::Dismiss);

        Self { shared, modes }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui keymap::tests`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/keymap.rs
git commit -m "feat: Keymap struct with ModeId, shared/per-mode defaults, and resolve"
```

---

## Chunk 2: Config Types and Merge

### Task 4: Add KeysConfig to flotilla-core config

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`

Add raw string maps for key binding overrides. The TUI parses these into `Keymap` entries using crokey. The core crate stays agnostic to key types.

- [ ] **Step 1: Write test for KeysConfig deserialization**

Add to the `tests` module in `config.rs`:

```rust
#[test]
fn keys_config_deserializes_from_toml() {
    let toml = r#"
[ui.keys.shared]
"ctrl-r" = "refresh"
"g" = "select_next"

[ui.keys.normal]
"x" = "quit"
"#;
    let config: FlotillaConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.ui.keys.shared.get("ctrl-r"), Some(&"refresh".to_string()));
    assert_eq!(config.ui.keys.shared.get("g"), Some(&"select_next".to_string()));
    assert_eq!(config.ui.keys.normal.get("x"), Some(&"quit".to_string()));
}

#[test]
fn keys_config_defaults_to_empty() {
    let config: FlotillaConfig = toml::from_str("").unwrap();
    assert!(config.ui.keys.shared.is_empty());
    assert!(config.ui.keys.normal.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core config::tests::keys_config`
Expected: compile error — `ui.keys` field doesn't exist.

- [ ] **Step 3: Add KeysConfig struct and wire into UiConfig**

In `crates/flotilla-core/src/config.rs`, add after `PreviewConfig`:

```rust
/// Raw key binding overrides from config.toml.
///
/// Keys are key combo strings (parsed by `crokey` in the TUI crate).
/// Values are action names (parsed by `Action::from_config_str`).
/// Empty maps mean "use defaults".
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KeysConfig {
    #[serde(default)]
    pub shared: HashMap<String, String>,
    #[serde(default)]
    pub normal: HashMap<String, String>,
    #[serde(default)]
    pub help: HashMap<String, String>,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub action_menu: HashMap<String, String>,
    #[serde(default)]
    pub delete_confirm: HashMap<String, String>,
    #[serde(default)]
    pub close_confirm: HashMap<String, String>,
}
```

Add `keys` field to `UiConfig`:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UiConfig {
    #[serde(default)]
    pub preview: PreviewConfig,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub keys: KeysConfig,
}
```

Ensure `HashMap` is imported (should already be in scope).

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core config::tests::keys_config`
Expected: both tests pass.

- [ ] **Step 5: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass (adding a defaulted field is backwards-compatible).

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/config.rs
git commit -m "feat: add KeysConfig to UiConfig for key binding overrides"
```

---

### Task 5: Config merge — build Keymap from KeysConfig

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`

Add `Keymap::from_config()` that starts from defaults and applies user overrides. Invalid key strings or action names are logged as warnings and skipped.

- [ ] **Step 1: Write tests for config merge**

Add to the `tests` module in `keymap.rs`:

```rust
use flotilla_core::config::KeysConfig;

#[test]
fn from_config_overrides_shared_binding() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("g".into(), "select_next".into());

    let keymap = Keymap::from_config(&keys);
    // 'g' is now select_next
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('g'))), Some(Action::SelectNext));
    // original 'j' still works
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('j'))), Some(Action::SelectNext));
}

#[test]
fn from_config_overrides_mode_binding() {
    let mut keys = KeysConfig::default();
    keys.normal.insert("x".into(), "quit".into());

    let keymap = Keymap::from_config(&keys);
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('x'))), Some(Action::Quit));
    // original 'q' still works
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('q'))), Some(Action::Quit));
}

#[test]
fn from_config_skips_invalid_key_string() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("NOT_A_VALID_KEY!!!".into(), "quit".into());

    // Should not panic, just skip the invalid entry
    let keymap = Keymap::from_config(&keys);
    // Defaults still work
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('q'))), Some(Action::Quit));
}

#[test]
fn from_config_skips_invalid_action_name() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("g".into(), "nonexistent_action".into());

    let keymap = Keymap::from_config(&keys);
    // 'g' should not be bound
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('g'))), None);
}

#[test]
fn from_config_empty_uses_defaults() {
    let keys = KeysConfig::default();
    let keymap = Keymap::from_config(&keys);
    // Spot-check a few defaults
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('j'))), Some(Action::SelectNext));
    assert_eq!(keymap.resolve(ModeId::Normal, key(KeyCode::Char('q'))), Some(Action::Quit));
    assert_eq!(keymap.resolve(ModeId::DeleteConfirm, key(KeyCode::Char('y'))), Some(Action::Confirm));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui keymap::tests::from_config --no-run`
Expected: compile error — `Keymap::from_config` doesn't exist.

- [ ] **Step 3: Implement from_config**

Add to `keymap.rs`:

```rust
use flotilla_core::config::KeysConfig;

impl Keymap {
    /// Build a keymap from defaults merged with user config overrides.
    ///
    /// Invalid key strings or action names are logged and skipped.
    pub fn from_config(config: &KeysConfig) -> Self {
        let mut keymap = Self::defaults();

        let mode_configs: &[(&HashMap<String, String>, ModeId)] = &[
            (&config.normal, ModeId::Normal),
            (&config.help, ModeId::Help),
            (&config.config, ModeId::Config),
            (&config.action_menu, ModeId::ActionMenu),
            (&config.delete_confirm, ModeId::DeleteConfirm),
            (&config.close_confirm, ModeId::CloseConfirm),
        ];

        // Apply shared overrides
        for (key_str, action_str) in &config.shared {
            match Self::parse_binding(key_str, action_str) {
                Some((combo, action)) => {
                    keymap.shared.insert(combo, action);
                }
                None => {
                    tracing::warn!(key = %key_str, action = %action_str, "skipping invalid shared key binding");
                }
            }
        }

        // Apply per-mode overrides
        for (entries, mode) in mode_configs {
            for (key_str, action_str) in *entries {
                match Self::parse_binding(key_str, action_str) {
                    Some((combo, action)) => {
                        keymap.modes.entry(*mode).or_default().insert(combo, action);
                    }
                    None => {
                        tracing::warn!(key = %key_str, action = %action_str, ?mode, "skipping invalid key binding");
                    }
                }
            }
        }

        keymap
    }

    fn parse_binding(key_str: &str, action_str: &str) -> Option<(KeyCombination, Action)> {
        let combo: KeyCombination = key_str.parse().ok()?;
        let action = Action::from_config_str(action_str)?;
        Some((combo, action))
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-tui keymap::tests`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/keymap.rs
git commit -m "feat: Keymap::from_config merges user overrides with defaults"
```

---

## Chunk 3: Wire Keymap Into App

### Task 6: Store Keymap in App and build from config

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`

Add a `keymap` field to `App` and build it from config in the constructor.

- [ ] **Step 1: Add keymap field to App struct**

In `crates/flotilla-tui/src/app/mod.rs`, add import:

```rust
use crate::keymap::Keymap;
```

Add field to `App` struct (after `theme`):

```rust
pub keymap: Keymap,
```

- [ ] **Step 2: Build keymap in constructor**

In `App::new()`, after the line that loads the theme/layout config, build the keymap:

```rust
let keymap = Keymap::from_config(&loaded_config.ui.keys);
```

Add `keymap` to the `Self { ... }` return.

- [ ] **Step 3: Verify it compiles and tests pass**

Run: `cargo test --locked`
Expected: all tests pass. Test builders may need `keymap: Keymap::defaults()` added to `stub_app` — check and fix.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/test_support.rs
git commit -m "feat: store Keymap in App, build from config in constructor"
```

---

### Task 7: Add ModeId conversion from UiMode

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`

Add a method to convert `UiMode` to `ModeId` for keymap resolution. This is needed by `resolve_action`.

- [ ] **Step 1: Write test**

Add to `keymap.rs` tests:

```rust
use crate::app::ui_state::UiMode;

#[test]
fn mode_id_from_ui_mode() {
    assert_eq!(ModeId::from(&UiMode::Normal), ModeId::Normal);
    assert_eq!(ModeId::from(&UiMode::Help), ModeId::Help);
    assert_eq!(ModeId::from(&UiMode::Config), ModeId::Config);
    assert_eq!(
        ModeId::from(&UiMode::ActionMenu { items: vec![], index: 0 }),
        ModeId::ActionMenu
    );
    assert_eq!(
        ModeId::from(&UiMode::BranchInput {
            input: tui_input::Input::default(),
            kind: crate::app::BranchInputKind::Manual,
            pending_issue_ids: vec![]
        }),
        ModeId::BranchInput
    );
    assert_eq!(
        ModeId::from(&UiMode::IssueSearch { input: tui_input::Input::default() }),
        ModeId::IssueSearch
    );
}
```

- [ ] **Step 2: Implement From<&UiMode> for ModeId**

Add to `keymap.rs`:

```rust
use crate::app::ui_state::UiMode;

impl From<&UiMode> for ModeId {
    fn from(mode: &UiMode) -> Self {
        match mode {
            UiMode::Normal => ModeId::Normal,
            UiMode::Help => ModeId::Help,
            UiMode::Config => ModeId::Config,
            UiMode::ActionMenu { .. } => ModeId::ActionMenu,
            UiMode::BranchInput { .. } => ModeId::BranchInput,
            UiMode::FilePicker { .. } => ModeId::FilePicker,
            UiMode::DeleteConfirm { .. } => ModeId::DeleteConfirm,
            UiMode::CloseConfirm { .. } => ModeId::CloseConfirm,
            UiMode::IssueSearch { .. } => ModeId::IssueSearch,
        }
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-tui keymap::tests::mode_id`
Expected: passes.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/keymap.rs
git commit -m "feat: ModeId conversion from UiMode"
```

---

### Task 8: Replace resolve_action with Keymap-based resolution

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`

This is the core integration step. Replace the hardcoded match in `resolve_action` with `self.keymap.resolve()`. Keep the text input mode early returns. Remove the old private `Action` enum (now in `keymap.rs`). Fold `handle_delete_confirm_key` and `handle_close_confirm_key` fallthrough logic (the `y`/`n` keys) into the keymap defaults (already done in Task 3). Move the `K` shortcut from the special-case in `handle_key` into the keymap (already done in Task 3).

- [ ] **Step 1: Update imports in key_handlers.rs**

Replace the `Action` enum definition at the top of key_handlers.rs with an import:

```rust
use crate::keymap::{Action, ModeId};
```

Remove the old `Action` enum (lines 14-38 approximately). Keep all other imports.

- [ ] **Step 2: Rewrite resolve_action to use keymap**

Replace `resolve_action` with:

```rust
fn resolve_action(&self, key: KeyEvent) -> Option<Action> {
    let mode_id = ModeId::from(&self.ui.mode);

    // Text input modes: only Esc and Enter are intercepted.
    // All other keys pass through to tui_input in handle_key.
    match mode_id {
        ModeId::BranchInput | ModeId::IssueSearch => {
            return match key.code {
                KeyCode::Esc => Some(Action::Dismiss),
                KeyCode::Enter => Some(Action::Confirm),
                _ => None,
            };
        }
        // FilePicker has both a text input and a navigation list.
        // Only intercept navigation keys; everything else goes to tui_input.
        ModeId::FilePicker => {
            return match key.code {
                KeyCode::Char('j') | KeyCode::Down => Some(Action::SelectNext),
                KeyCode::Char('k') | KeyCode::Up => Some(Action::SelectPrev),
                KeyCode::Esc => Some(Action::Dismiss),
                KeyCode::Enter => Some(Action::Confirm),
                _ => None,
            };
        }
        _ => {}
    }

    self.keymap.resolve(mode_id, key)
}
```

- [ ] **Step 3: Remove special-case K handling from handle_key**

In `handle_key`, remove the `K` shortcut block (lines 351-360 approximately). The keymap now handles `K` → `ToggleStatusBarKeys` as a normal mode binding.

- [ ] **Step 4: Add ToggleStatusBarKeys to dispatch_action**

In `dispatch_action`, add a handler for `ToggleStatusBarKeys`:

```rust
Action::ToggleStatusBarKeys => {
    self.ui.status_bar.show_keys = !self.ui.status_bar.show_keys;
}
```

- [ ] **Step 5: Remove handle_delete_confirm_key and handle_close_confirm_key**

These methods handled `y`/`Enter` → confirm and `Esc`/`n` → dismiss for the confirm dialogs. The keymap now handles `y` → `Confirm` and `n` → `Dismiss` for `DeleteConfirm`/`CloseConfirm` modes, and the existing `dispatch_action` for `Confirm`/`Dismiss` already handles these focus targets.

Remove `handle_delete_confirm_key` and `handle_close_confirm_key` methods.

Update `handle_key` to remove the fallthrough calls to these methods:

```rust
pub fn handle_key(&mut self, key: KeyEvent) {
    if let Some(action) = self.resolve_action(key) {
        self.dispatch_action(action);
        return;
    }

    // Unresolved keys in text input modes pass through to tui_input
    match self.ui.mode {
        UiMode::FilePicker { .. } => self.handle_file_picker_key(key),
        UiMode::BranchInput { .. } => self.handle_branch_input_key(key),
        UiMode::IssueSearch { .. } => self.handle_issue_search_key(key),
        _ => {}
    }
}
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo test --locked`
Expected: all tests pass. The existing tests in `key_handlers.rs` exercise the same behaviour — the keymap defaults match the old hardcoded bindings.

If tests fail, check:
- Whether tests reference the old private `Action` type (update to `crate::keymap::Action`)
- Whether any test assertions depended on the precise dispatch path

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "refactor: replace hardcoded key dispatch with Keymap-based resolution"
```

---

## Chunk 4: Auto-Generated Help Screen

### Task 9: Generate help text from Keymap

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

Replace the hardcoded help text with a method on `Keymap` that produces help lines. Keep the static icon/column reference sections (these don't depend on key bindings) and generate the key binding sections dynamically.

- [ ] **Step 1: Add help generation method to Keymap**

Add to `keymap.rs`:

```rust
/// A key binding entry for help display.
#[derive(Debug, Clone)]
pub struct HelpBinding {
    pub key_display: String,
    pub description: &'static str,
}

/// A section of help text for display.
#[derive(Debug, Clone)]
pub struct HelpSection {
    pub title: &'static str,
    pub bindings: Vec<HelpBinding>,
}

impl Keymap {
    /// Build help sections from the active keymap for Normal mode.
    ///
    /// Groups bindings by category with human-readable key names.
    /// Actions bound to multiple keys are combined (e.g. "j / ↓").
    pub fn help_sections(&self) -> Vec<HelpSection> {
        // Collect Normal mode effective bindings (mode-specific + shared)
        let mut action_keys: HashMap<Action, Vec<String>> = HashMap::new();

        // Shared bindings first
        for (combo, action) in &self.shared {
            action_keys.entry(*action).or_default().push(combo.to_string());
        }

        // Normal mode bindings override
        if let Some(normal) = self.modes.get(&ModeId::Normal) {
            for (combo, action) in normal {
                action_keys.entry(*action).or_default().push(combo.to_string());
            }
        }

        // Sort keys within each action for stable display
        for keys in action_keys.values_mut() {
            keys.sort();
            keys.dedup();
        }

        // Define the category groupings and their display order
        let navigation = Self::help_section("Navigation", &action_keys, &[
            Action::SelectNext,
            Action::SelectPrev,
        ]);
        let actions = Self::help_section("Actions", &action_keys, &[
            Action::Confirm,
            Action::OpenActionMenu,
            Action::OpenBranchInput,
            Action::Dispatch(Intent::RemoveCheckout),
            Action::Dispatch(Intent::OpenChangeRequest),
            Action::OpenIssueSearch,
            Action::OpenFilePicker,
            Action::CycleLayout,
            Action::Refresh,
            Action::ToggleStatusBarKeys,
        ]);
        let multi_select = Self::help_section("Multi-select (issues)", &action_keys, &[
            Action::ToggleMultiSelect,
        ]);
        let repos = Self::help_section("Repos", &action_keys, &[
            Action::PrevTab,
            Action::NextTab,
            Action::MoveTabLeft,
            Action::MoveTabRight,
        ]);
        let general = Self::help_section("General", &action_keys, &[
            Action::ToggleDebug,
            Action::CycleTheme,
            Action::CycleHost,
            Action::ToggleHelp,
            Action::Dismiss,
            Action::Quit,
        ]);

        vec![navigation, actions, multi_select, repos, general]
    }

    fn help_section(
        title: &'static str,
        action_keys: &HashMap<Action, Vec<String>>,
        actions: &[Action],
    ) -> HelpSection {
        let bindings = actions
            .iter()
            .filter_map(|action| {
                let keys = action_keys.get(action)?;
                Some(HelpBinding {
                    key_display: keys.join(" / "),
                    description: action.description(),
                })
            })
            .collect();
        HelpSection { title, bindings }
    }
}
```

Note: `Action` needs `Hash` for the HashMap. Add `Hash` to the derive on `Action`. Since `Intent` doesn't derive `Hash`, implement it manually:

```rust
impl std::hash::Hash for Action {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let Action::Dispatch(intent) = self {
            std::mem::discriminant(intent).hash(state);
        }
    }
}
```

- [ ] **Step 2: Write a test for help_sections**

Add to `keymap.rs` tests:

```rust
#[test]
fn help_sections_include_all_categories() {
    let keymap = Keymap::defaults();
    let sections = keymap.help_sections();
    let titles: Vec<&str> = sections.iter().map(|s| s.title).collect();
    assert_eq!(titles, vec!["Navigation", "Actions", "Multi-select (issues)", "Repos", "General"]);
}

#[test]
fn help_sections_navigation_has_bindings() {
    let keymap = Keymap::defaults();
    let sections = keymap.help_sections();
    let nav = &sections[0];
    assert_eq!(nav.title, "Navigation");
    assert!(!nav.bindings.is_empty());
    // SelectNext should have at least j and Down
    let select_next = &nav.bindings[0];
    assert!(select_next.key_display.contains("j"), "expected j in key_display: {}", select_next.key_display);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-tui keymap::tests::help`
Expected: passes.

- [ ] **Step 4: Update render_help in ui.rs**

In `crates/flotilla-tui/src/ui.rs`, modify `render_help` to use keymap help sections for the key binding portion while keeping the static icon/column sections.

Replace the `help_text` construction in `render_help` (the function receives `&Keymap` as an additional parameter — update the call site in `render_ui` to pass it):

```rust
fn render_help(model: &TuiModel, ui: &mut UiState, theme: &Theme, keymap: &Keymap, frame: &mut Frame) {
    if !matches!(ui.mode, UiMode::Help) {
        return;
    }

    let area = ui_helpers::popup_area(frame.area(), 60, 85);
    frame.render_widget(Clear, area);

    let mut help_text = vec![
        Line::from(Span::styled("Item Icons", Style::default().bold())),
        Line::from("  ●  Checkout with workspace    ○  Checkout (no workspace)"),
        Line::from("  ▶  Running session            ◆  Idle session"),
        Line::from("  ⊙  Pull request               ◇  Issue"),
        Line::from("  ⊶  Remote branch"),
        Line::from(""),
        Line::from(Span::styled("Column Indicators", Style::default().bold())),
        Line::from("  WT: ◆ main  ✓ checked out"),
        Line::from("  WS: ● has workspace  2/3/… multiple"),
        Line::from("  PR: ✓ merged  ✗ closed"),
        Line::from("  Git: ? untracked  M modified  ↑ ahead  ↓ behind"),
        Line::from(""),
    ];

    // Dynamic sections from keymap
    for section in keymap.help_sections() {
        help_text.push(Line::from(Span::styled(section.title, Style::default().bold())));
        for binding in &section.bindings {
            help_text.push(Line::from(format!("  {:18}{}", binding.key_display, binding.description)));
        }
        help_text.push(Line::from(""));
    }

    // Extra non-keybinding hints
    help_text.push(Line::from(Span::styled("Mouse", Style::default().bold())));
    help_text.push(Line::from("  Click            Select item"));
    help_text.push(Line::from("  Double-click     Open workspace"));
    help_text.push(Line::from("  Right-click      Action menu"));
    help_text.push(Line::from("  Scroll wheel     Navigate list"));
    help_text.push(Line::from("  Drag tab         Reorder tabs"));

    let total_lines = help_text.len() as u16;
    let inner_height = area.height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(inner_height);
    ui.help_scroll = ui.help_scroll.min(max_scroll);
    let scroll = ui.help_scroll;

    let has_more_below = scroll < max_scroll;
    let has_more_above = scroll > 0;
    let title = match (has_more_above, has_more_below) {
        (true, true) => " Help ↑↓ ",
        (false, true) => " Help ↓ ",
        (true, false) => " Help ↑ ",
        (false, false) => " Help ",
    };

    let paragraph = Paragraph::new(help_text)
        .block(Block::bordered().style(theme.block_style()).title(title))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}
```

Update the `render_help` call site in `render_ui` (or wherever `render_help` is called) to pass `keymap`. This likely means the top-level `render_ui` function needs a `&Keymap` parameter, which flows from the `App` struct.

- [ ] **Step 5: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 6: Run clippy and format**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt`

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-tui/src/keymap.rs crates/flotilla-tui/src/ui.rs crates/flotilla-tui/src/app/mod.rs
git commit -m "feat: auto-generate help screen from active keymap bindings"
```

---

## Chunk 5: Final Validation

### Task 10: End-to-end validation

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Format**

Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 4: Manual smoke test with custom config**

Create a temporary config override and verify it takes effect:

1. Add to `~/.config/flotilla/config.toml`:
   ```toml
   [ui.keys.shared]
   "g" = "select_next"
   ```
2. Run flotilla, verify `g` navigates down.
3. Open help (`?`), verify `g` appears in navigation section.
4. Remove the test config entry.

- [ ] **Step 5: Commit any final fixes**
