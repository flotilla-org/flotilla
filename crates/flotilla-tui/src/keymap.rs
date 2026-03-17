// Keymap module: configurable key bindings for the TUI.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use flotilla_core::config::KeysConfig;

use crate::app::{intent::Intent, ui_state::UiMode};

/// An action that can be triggered by a key binding.
///
/// Most variants correspond to UI-level operations (navigation, mode transitions).
/// `Dispatch(Intent)` wraps an `Intent` for actions that go through the executor pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    OpenCommandPalette,
    Dispatch(Intent),
}

impl Hash for Action {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        if let Action::Dispatch(intent) = self {
            std::mem::discriminant(intent).hash(state);
        }
    }
}

impl Action {
    /// Parse an action from its snake_case config string representation.
    ///
    /// Intent-wrapping actions use the intent name directly (e.g. "remove_checkout"
    /// maps to `Action::Dispatch(Intent::RemoveCheckout)`).
    pub fn from_config_str(s: &str) -> Option<Action> {
        let action = match s {
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
            "open_command_palette" => Action::OpenCommandPalette,
            // Intent-wrapping actions
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
        };
        Some(action)
    }

    /// Convert the action to its snake_case config string representation.
    ///
    /// This is the inverse of `from_config_str`.
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
            Action::OpenCommandPalette => "open_command_palette",
            Action::Dispatch(intent) => match intent {
                Intent::SwitchToWorkspace => "switch_to_workspace",
                Intent::CreateWorkspace => "create_workspace",
                Intent::RemoveCheckout => "remove_checkout",
                Intent::CreateCheckout => "create_checkout",
                Intent::GenerateBranchName => "generate_branch_name",
                Intent::OpenChangeRequest => "open_change_request",
                Intent::OpenIssue => "open_issue",
                Intent::LinkIssuesToChangeRequest => "link_issues_to_change_request",
                Intent::TeleportSession => "teleport_session",
                Intent::ArchiveSession => "archive_session",
                Intent::CloseChangeRequest => "close_change_request",
            },
        }
    }

    /// Human-readable description of the action, suitable for help screen display.
    pub fn description(&self) -> &'static str {
        match self {
            Action::SelectNext => "Move selection down",
            Action::SelectPrev => "Move selection up",
            Action::Confirm => "Confirm / execute",
            Action::Dismiss => "Dismiss / go back",
            Action::Quit => "Quit the application",
            Action::Refresh => "Refresh all providers",
            Action::PrevTab => "Switch to previous tab",
            Action::NextTab => "Switch to next tab",
            Action::MoveTabLeft => "Move current tab left",
            Action::MoveTabRight => "Move current tab right",
            Action::ToggleHelp => "Toggle help screen",
            Action::ToggleMultiSelect => "Toggle multi-select",
            Action::ToggleProviders => "Toggle provider config",
            Action::ToggleDebug => "Toggle debug panel",
            Action::ToggleStatusBarKeys => "Toggle status bar key hints",
            Action::CycleHost => "Cycle host filter",
            Action::CycleLayout => "Cycle layout",
            Action::CycleTheme => "Cycle colour theme",
            Action::OpenActionMenu => "Open action menu",
            Action::OpenBranchInput => "New branch input",
            Action::OpenIssueSearch => "Search issues",
            Action::OpenFilePicker => "Open file picker",
            Action::OpenCommandPalette => "Open command palette",
            Action::Dispatch(intent) => match intent {
                Intent::SwitchToWorkspace => "Switch to workspace",
                Intent::CreateWorkspace => "Create workspace",
                Intent::RemoveCheckout => "Remove checkout",
                Intent::CreateCheckout => "Create checkout",
                Intent::GenerateBranchName => "Generate branch name",
                Intent::OpenChangeRequest => "Open change request in browser",
                Intent::OpenIssue => "Open issue in browser",
                Intent::LinkIssuesToChangeRequest => "Link issues to change request",
                Intent::TeleportSession => "Teleport session",
                Intent::ArchiveSession => "Archive session",
                Intent::CloseChangeRequest => "Close change request",
            },
        }
    }
}

// ── Help display types ──

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

// ── ModeId ──

/// Identifies a UI mode for per-mode key bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeId {
    Normal,
    Help,
    Config,
    ActionMenu,
    DeleteConfirm,
    CloseConfirm,
    FilePicker,
    BranchInput,
    IssueSearch,
    CommandPalette,
}

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
            UiMode::CommandPalette { .. } => ModeId::CommandPalette,
        }
    }
}

// ── Keymap ──

/// Key binding map with shared (cross-mode) and per-mode bindings.
///
/// Resolution order: mode-specific bindings take priority over shared bindings.
#[derive(Debug, Clone)]
pub struct Keymap {
    shared: HashMap<KeyCombination, Action>,
    modes: HashMap<ModeId, HashMap<KeyCombination, Action>>,
}

/// Helper to construct a `KeyCombination` from a `KeyEvent`.
fn kc(code: KeyCode, modifiers: KeyModifiers) -> KeyCombination {
    KeyCombination::from(KeyEvent::new(code, modifiers))
}

impl Keymap {
    /// Look up the action bound to `key` in the given `mode`.
    ///
    /// Checks mode-specific bindings first, then shared bindings.
    pub fn resolve(&self, mode: ModeId, key: KeyCombination) -> Option<Action> {
        self.modes.get(&mode).and_then(|m| m.get(&key).copied()).or_else(|| self.shared.get(&key).copied())
    }

    /// Build the default keymap matching the current hardcoded bindings.
    pub fn defaults() -> Self {
        let mut shared = HashMap::new();

        // Shared navigation
        shared.insert(crokey::key!(j), Action::SelectNext);
        shared.insert(crokey::key!(down), Action::SelectNext);
        shared.insert(crokey::key!(k), Action::SelectPrev);
        shared.insert(crokey::key!(up), Action::SelectPrev);
        shared.insert(crokey::key!(enter), Action::Confirm);
        shared.insert(crokey::key!(esc), Action::Dismiss);

        // Shared toggles
        shared.insert(kc(KeyCode::Char('?'), KeyModifiers::NONE), Action::ToggleHelp);
        shared.insert(kc(KeyCode::Char('K'), KeyModifiers::SHIFT), Action::ToggleStatusBarKeys);

        let mut modes: HashMap<ModeId, HashMap<KeyCombination, Action>> = HashMap::new();

        // ── Normal mode ──
        {
            let normal = modes.entry(ModeId::Normal).or_default();
            normal.insert(crokey::key!(q), Action::Quit);
            normal.insert(crokey::key!(r), Action::Refresh);
            normal.insert(kc(KeyCode::Char('['), KeyModifiers::NONE), Action::PrevTab);
            normal.insert(kc(KeyCode::Char(']'), KeyModifiers::NONE), Action::NextTab);
            normal.insert(kc(KeyCode::Char('{'), KeyModifiers::NONE), Action::MoveTabLeft);
            normal.insert(kc(KeyCode::Char('}'), KeyModifiers::NONE), Action::MoveTabRight);
            normal.insert(crokey::key!(space), Action::ToggleMultiSelect);
            normal.insert(crokey::key!(h), Action::CycleHost);
            normal.insert(crokey::key!(l), Action::CycleLayout);
            normal.insert(kc(KeyCode::Char('T'), KeyModifiers::SHIFT), Action::CycleTheme);
            normal.insert(kc(KeyCode::Char('.'), KeyModifiers::NONE), Action::OpenActionMenu);
            normal.insert(crokey::key!(n), Action::OpenBranchInput);
            normal.insert(kc(KeyCode::Char('/'), KeyModifiers::NONE), Action::OpenCommandPalette);
            normal.insert(crokey::key!(a), Action::OpenFilePicker);
            normal.insert(crokey::key!(c), Action::ToggleProviders);
            normal.insert(kc(KeyCode::Char('D'), KeyModifiers::SHIFT), Action::ToggleDebug);
            normal.insert(crokey::key!(d), Action::Dispatch(Intent::RemoveCheckout));
            normal.insert(crokey::key!(p), Action::Dispatch(Intent::OpenChangeRequest));
        }

        // ── Config mode ──
        {
            let config = modes.entry(ModeId::Config).or_default();
            config.insert(crokey::key!(q), Action::Dismiss);
            config.insert(kc(KeyCode::Char('['), KeyModifiers::NONE), Action::PrevTab);
            config.insert(kc(KeyCode::Char(']'), KeyModifiers::NONE), Action::NextTab);
        }

        // ── Help mode ──
        {
            let help = modes.entry(ModeId::Help).or_default();
            help.insert(crokey::key!(q), Action::Dismiss);
        }

        // ── ActionMenu mode ──
        {
            let action_menu = modes.entry(ModeId::ActionMenu).or_default();
            action_menu.insert(crokey::key!(q), Action::Dismiss);
        }

        // ── DeleteConfirm mode ──
        {
            let delete_confirm = modes.entry(ModeId::DeleteConfirm).or_default();
            delete_confirm.insert(crokey::key!(y), Action::Confirm);
            delete_confirm.insert(crokey::key!(n), Action::Dismiss);
            delete_confirm.insert(crokey::key!(q), Action::Dismiss);
        }

        // ── CloseConfirm mode ──
        {
            let close_confirm = modes.entry(ModeId::CloseConfirm).or_default();
            close_confirm.insert(crokey::key!(y), Action::Confirm);
            close_confirm.insert(crokey::key!(n), Action::Dismiss);
            close_confirm.insert(crokey::key!(q), Action::Dismiss);
        }

        // ── CommandPalette mode ──
        // Keys are hardcoded in resolve_action (text input mode).
        modes.entry(ModeId::CommandPalette).or_default();

        Keymap { shared, modes }
    }

    /// Build a keymap from defaults, then apply user overrides from `KeysConfig`.
    ///
    /// Invalid key strings or action names are logged as warnings and skipped.
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

    /// Generate help sections from the active keymap bindings for Normal mode.
    ///
    /// Collects effective bindings (mode-specific + shared fallback), groups them
    /// by action, and organises into display sections with combined key names.
    pub fn help_sections(&self) -> Vec<HelpSection> {
        // Build the effective Normal-mode binding map: start with shared, overlay
        // mode-specific. This mirrors resolve() semantics so the help screen
        // accurately reflects what each key does in Normal mode.
        let mut effective: HashMap<KeyCombination, Action> = self.shared.clone();
        if let Some(normal_bindings) = self.modes.get(&ModeId::Normal) {
            effective.extend(normal_bindings);
        }

        // Invert: group keys by action for display.
        let mut action_keys: HashMap<Action, Vec<String>> = HashMap::new();
        for (key, action) in &effective {
            action_keys.entry(*action).or_default().push(key.to_string());
        }

        // Sort keys within each action for stable display order.
        for keys in action_keys.values_mut() {
            keys.sort();
            keys.dedup();
        }

        // Build a HelpBinding for a given action from the collected keys.
        let make_binding = |action: &Action| -> Option<HelpBinding> {
            action_keys.get(action).map(|keys| HelpBinding { key_display: keys.join(" / "), description: action.description() })
        };

        // Define sections and their actions in display order.
        let section_defs: &[(&str, &[Action])] = &[
            ("Navigation", &[Action::SelectNext, Action::SelectPrev]),
            ("Actions", &[
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
            ]),
            ("Multi-select (issues)", &[Action::ToggleMultiSelect]),
            ("Repos", &[Action::PrevTab, Action::NextTab, Action::MoveTabLeft, Action::MoveTabRight]),
            ("General", &[
                Action::ToggleProviders,
                Action::ToggleDebug,
                Action::CycleTheme,
                Action::CycleHost,
                Action::ToggleHelp,
                Action::Dismiss,
                Action::Quit,
            ]),
        ];

        section_defs
            .iter()
            .map(|(title, actions)| {
                let bindings = actions.iter().filter_map(&make_binding).collect();
                HelpSection { title, bindings }
            })
            .collect()
    }

    fn parse_binding(key_str: &str, action_str: &str) -> Option<(KeyCombination, Action)> {
        let combo: KeyCombination = key_str.parse().ok()?;
        let action = Action::from_config_str(action_str)?;
        Some((combo, action))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Action config string tests ──

    /// Every non-Dispatch action round-trips through config strings.
    #[test]
    fn round_trip_non_dispatch_actions() {
        let actions = [
            Action::SelectNext,
            Action::SelectPrev,
            Action::Confirm,
            Action::Dismiss,
            Action::Quit,
            Action::Refresh,
            Action::PrevTab,
            Action::NextTab,
            Action::MoveTabLeft,
            Action::MoveTabRight,
            Action::ToggleHelp,
            Action::ToggleMultiSelect,
            Action::ToggleProviders,
            Action::ToggleDebug,
            Action::ToggleStatusBarKeys,
            Action::CycleHost,
            Action::CycleLayout,
            Action::CycleTheme,
            Action::OpenActionMenu,
            Action::OpenBranchInput,
            Action::OpenIssueSearch,
            Action::OpenFilePicker,
            Action::OpenCommandPalette,
        ];
        for action in actions {
            let s = action.as_config_str();
            let parsed = Action::from_config_str(s).unwrap_or_else(|| panic!("failed to parse config str: {s}"));
            assert_eq!(parsed, action, "round-trip failed for {s}");
        }
    }

    /// Every Intent variant round-trips through Dispatch config strings.
    #[test]
    fn round_trip_dispatch_actions() {
        let intents = [
            Intent::SwitchToWorkspace,
            Intent::CreateWorkspace,
            Intent::RemoveCheckout,
            Intent::CreateCheckout,
            Intent::GenerateBranchName,
            Intent::OpenChangeRequest,
            Intent::OpenIssue,
            Intent::LinkIssuesToChangeRequest,
            Intent::TeleportSession,
            Intent::ArchiveSession,
            Intent::CloseChangeRequest,
        ];
        for intent in intents {
            let action = Action::Dispatch(intent);
            let s = action.as_config_str();
            let parsed = Action::from_config_str(s).unwrap_or_else(|| panic!("failed to parse config str: {s}"));
            assert_eq!(parsed, action, "round-trip failed for {s}");
        }
    }

    /// Unknown strings return None.
    #[test]
    fn unknown_string_returns_none() {
        assert_eq!(Action::from_config_str("nonexistent_action"), None);
        assert_eq!(Action::from_config_str(""), None);
        assert_eq!(Action::from_config_str("SelectNext"), None); // wrong case
    }

    /// Every action has a non-empty description.
    #[test]
    fn all_actions_have_descriptions() {
        let all_actions: Vec<Action> = vec![
            Action::SelectNext,
            Action::SelectPrev,
            Action::Confirm,
            Action::Dismiss,
            Action::Quit,
            Action::Refresh,
            Action::PrevTab,
            Action::NextTab,
            Action::MoveTabLeft,
            Action::MoveTabRight,
            Action::ToggleHelp,
            Action::ToggleMultiSelect,
            Action::ToggleProviders,
            Action::ToggleDebug,
            Action::ToggleStatusBarKeys,
            Action::CycleHost,
            Action::CycleLayout,
            Action::CycleTheme,
            Action::OpenActionMenu,
            Action::OpenBranchInput,
            Action::OpenIssueSearch,
            Action::OpenFilePicker,
            Action::OpenCommandPalette,
            Action::Dispatch(Intent::SwitchToWorkspace),
            Action::Dispatch(Intent::CreateWorkspace),
            Action::Dispatch(Intent::RemoveCheckout),
            Action::Dispatch(Intent::CreateCheckout),
            Action::Dispatch(Intent::GenerateBranchName),
            Action::Dispatch(Intent::OpenChangeRequest),
            Action::Dispatch(Intent::OpenIssue),
            Action::Dispatch(Intent::LinkIssuesToChangeRequest),
            Action::Dispatch(Intent::TeleportSession),
            Action::Dispatch(Intent::ArchiveSession),
            Action::Dispatch(Intent::CloseChangeRequest),
        ];
        for action in all_actions {
            let desc = action.description();
            assert!(!desc.is_empty(), "empty description for {:?}", action);
        }
    }

    // ── Keymap tests ──

    #[test]
    fn defaults_resolve_shared_navigation() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(j)), Some(Action::SelectNext));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(down)), Some(Action::SelectNext));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(k)), Some(Action::SelectPrev));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(up)), Some(Action::SelectPrev));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(enter)), Some(Action::Confirm));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(esc)), Some(Action::Dismiss));
    }

    #[test]
    fn shared_bindings_work_across_modes() {
        let km = Keymap::defaults();
        let modes = [ModeId::Normal, ModeId::Help, ModeId::Config, ModeId::ActionMenu, ModeId::FilePicker];
        for mode in modes {
            assert_eq!(km.resolve(mode, crokey::key!(j)), Some(Action::SelectNext), "j should be SelectNext in {mode:?}");
            assert_eq!(km.resolve(mode, crokey::key!(enter)), Some(Action::Confirm), "enter should be Confirm in {mode:?}");
        }
    }

    #[test]
    fn normal_mode_specific_bindings() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(q)), Some(Action::Quit));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(r)), Some(Action::Refresh));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(space)), Some(Action::ToggleMultiSelect));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(h)), Some(Action::CycleHost));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(l)), Some(Action::CycleLayout));
        assert_eq!(km.resolve(ModeId::Normal, kc(KeyCode::Char('T'), KeyModifiers::SHIFT)), Some(Action::CycleTheme));
        assert_eq!(km.resolve(ModeId::Normal, kc(KeyCode::Char('.'), KeyModifiers::NONE)), Some(Action::OpenActionMenu));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(n)), Some(Action::OpenBranchInput));
        assert_eq!(km.resolve(ModeId::Normal, kc(KeyCode::Char('/'), KeyModifiers::NONE)), Some(Action::OpenCommandPalette));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(a)), Some(Action::OpenFilePicker));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(c)), Some(Action::ToggleProviders));
        assert_eq!(km.resolve(ModeId::Normal, kc(KeyCode::Char('D'), KeyModifiers::SHIFT)), Some(Action::ToggleDebug));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(d)), Some(Action::Dispatch(Intent::RemoveCheckout)));
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(p)), Some(Action::Dispatch(Intent::OpenChangeRequest)));
    }

    #[test]
    fn mode_specific_overrides_shared() {
        let km = Keymap::defaults();
        // q is Quit in Normal, but Dismiss in Help/Config/ActionMenu/DeleteConfirm/CloseConfirm
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(q)), Some(Action::Quit));
        assert_eq!(km.resolve(ModeId::Help, crokey::key!(q)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::Config, crokey::key!(q)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::ActionMenu, crokey::key!(q)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::DeleteConfirm, crokey::key!(q)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::CloseConfirm, crokey::key!(q)), Some(Action::Dismiss));
    }

    #[test]
    fn tab_switching_in_normal_and_config() {
        let km = Keymap::defaults();
        let bracket_left = kc(KeyCode::Char('['), KeyModifiers::NONE);
        let bracket_right = kc(KeyCode::Char(']'), KeyModifiers::NONE);

        assert_eq!(km.resolve(ModeId::Normal, bracket_left), Some(Action::PrevTab));
        assert_eq!(km.resolve(ModeId::Normal, bracket_right), Some(Action::NextTab));
        assert_eq!(km.resolve(ModeId::Config, bracket_left), Some(Action::PrevTab));
        assert_eq!(km.resolve(ModeId::Config, bracket_right), Some(Action::NextTab));
    }

    #[test]
    fn delete_confirm_has_y_n_bindings() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(ModeId::DeleteConfirm, crokey::key!(y)), Some(Action::Confirm));
        assert_eq!(km.resolve(ModeId::DeleteConfirm, crokey::key!(n)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::DeleteConfirm, crokey::key!(q)), Some(Action::Dismiss));
    }

    #[test]
    fn close_confirm_has_y_n_bindings() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(ModeId::CloseConfirm, crokey::key!(y)), Some(Action::Confirm));
        assert_eq!(km.resolve(ModeId::CloseConfirm, crokey::key!(n)), Some(Action::Dismiss));
        assert_eq!(km.resolve(ModeId::CloseConfirm, crokey::key!(q)), Some(Action::Dismiss));
    }

    #[test]
    fn help_mode_toggle_with_question_mark() {
        let km = Keymap::defaults();
        let question_mark = kc(KeyCode::Char('?'), KeyModifiers::NONE);
        // ? is a shared binding for ToggleHelp
        assert_eq!(km.resolve(ModeId::Normal, question_mark), Some(Action::ToggleHelp));
        assert_eq!(km.resolve(ModeId::Help, question_mark), Some(Action::ToggleHelp));
    }

    #[test]
    fn toggle_status_bar_keys_is_shared_across_modes() {
        let km = Keymap::defaults();
        let shift_k = kc(KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(km.resolve(ModeId::Normal, shift_k), Some(Action::ToggleStatusBarKeys));
        assert_eq!(km.resolve(ModeId::Help, shift_k), Some(Action::ToggleStatusBarKeys));
        assert_eq!(km.resolve(ModeId::Config, shift_k), Some(Action::ToggleStatusBarKeys));
        assert_eq!(km.resolve(ModeId::ActionMenu, shift_k), Some(Action::ToggleStatusBarKeys));
        assert_eq!(km.resolve(ModeId::DeleteConfirm, shift_k), Some(Action::ToggleStatusBarKeys));
        assert_eq!(km.resolve(ModeId::CloseConfirm, shift_k), Some(Action::ToggleStatusBarKeys));
    }

    #[test]
    fn unbound_key_returns_none() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(ModeId::Normal, crokey::key!(f12)), None);
        assert_eq!(km.resolve(ModeId::Help, crokey::key!(x)), None);
        assert_eq!(km.resolve(ModeId::Config, crokey::key!(z)), None);
    }

    #[test]
    fn file_picker_falls_through_to_shared() {
        let km = Keymap::defaults();
        // FilePicker has no mode-specific bindings, so shared bindings resolve
        assert_eq!(km.resolve(ModeId::FilePicker, crokey::key!(j)), Some(Action::SelectNext));
        assert_eq!(km.resolve(ModeId::FilePicker, crokey::key!(k)), Some(Action::SelectPrev));
        assert_eq!(km.resolve(ModeId::FilePicker, crokey::key!(enter)), Some(Action::Confirm));
        assert_eq!(km.resolve(ModeId::FilePicker, crokey::key!(esc)), Some(Action::Dismiss));
    }

    // ── from_config tests ──

    #[test]
    fn from_config_overrides_shared_binding() {
        let mut keys = KeysConfig::default();
        keys.shared.insert("g".into(), "select_next".into());
        let keymap = Keymap::from_config(&keys);
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('g'), KeyModifiers::NONE)), Some(Action::SelectNext));
        // original 'j' still works
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('j'), KeyModifiers::NONE)), Some(Action::SelectNext));
    }

    #[test]
    fn from_config_overrides_mode_binding() {
        let mut keys = KeysConfig::default();
        keys.normal.insert("x".into(), "quit".into());
        let keymap = Keymap::from_config(&keys);
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('x'), KeyModifiers::NONE)), Some(Action::Quit));
        // original 'q' still works
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
    }

    #[test]
    fn from_config_skips_invalid_key_string() {
        let mut keys = KeysConfig::default();
        keys.shared.insert("NOT_A_VALID_KEY!!!".into(), "quit".into());
        let keymap = Keymap::from_config(&keys);
        // defaults still work despite invalid override
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
    }

    #[test]
    fn from_config_skips_invalid_action_name() {
        let mut keys = KeysConfig::default();
        keys.shared.insert("g".into(), "nonexistent_action".into());
        let keymap = Keymap::from_config(&keys);
        // 'g' was not bound by default, and the invalid override is skipped
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('g'), KeyModifiers::NONE)), None);
    }

    #[test]
    fn from_config_empty_uses_defaults() {
        let keys = KeysConfig::default();
        let keymap = Keymap::from_config(&keys);
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('j'), KeyModifiers::NONE)), Some(Action::SelectNext));
        assert_eq!(keymap.resolve(ModeId::Normal, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
    }

    // ── ModeId from UiMode tests ──

    #[test]
    fn mode_id_from_ui_mode() {
        assert_eq!(ModeId::from(&UiMode::Normal), ModeId::Normal);
        assert_eq!(ModeId::from(&UiMode::Help), ModeId::Help);
        assert_eq!(ModeId::from(&UiMode::Config), ModeId::Config);
        assert_eq!(ModeId::from(&UiMode::ActionMenu { items: vec![], index: 0 }), ModeId::ActionMenu);
    }

    // ── help_sections tests ──

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
    }

    #[test]
    fn help_sections_bindings_have_descriptions() {
        let keymap = Keymap::defaults();
        let sections = keymap.help_sections();
        for section in &sections {
            for binding in &section.bindings {
                assert!(!binding.description.is_empty(), "empty description in section {}", section.title);
                assert!(!binding.key_display.is_empty(), "empty key_display in section {}", section.title);
            }
        }
    }
}
