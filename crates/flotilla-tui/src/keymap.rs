// Keymap module: configurable key bindings for the TUI.

use std::hash::{Hash, Hasher};

use crokey::KeyCombination;
use flotilla_core::config::KeysConfig;

use crate::{
    app::intent::Intent,
    binding_table::{BindingModeId, CompiledBindings, KeyBindingMode, BINDINGS},
    status_bar::KeyChip,
};

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
    ToggleArchived,
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
    OpenContextualPalette,
    FillSelected,
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
    /// Returns true if the action is global — handled before the widget stack.
    ///
    /// Global actions are those that affect app-level state (tabs, theme, layout,
    /// host filter, debug panel, status bar keys, refresh) and should not flow
    /// through the widget stack.
    pub fn is_global(&self) -> bool {
        matches!(
            self,
            Action::PrevTab
                | Action::NextTab
                | Action::MoveTabLeft
                | Action::MoveTabRight
                | Action::CycleTheme
                | Action::CycleLayout
                | Action::CycleHost
                | Action::ToggleDebug
                | Action::ToggleStatusBarKeys
                | Action::Refresh
        )
    }

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
            "toggle_archived" => Action::ToggleArchived,
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
            "open_contextual_palette" => Action::OpenContextualPalette,
            "fill_selected" => Action::FillSelected,
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
            Action::ToggleArchived => "toggle_archived",
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
            Action::OpenContextualPalette => "open_contextual_palette",
            Action::FillSelected => "fill_selected",
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
            Action::ToggleArchived => "Toggle archived sessions",
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
            Action::OpenContextualPalette => "Open contextual palette (pre-filled)",
            Action::FillSelected => "Fill selected item",
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

// ── Keymap ──

/// Key binding map built from the flat binding table.
///
/// Resolution order: mode-specific bindings take priority over shared bindings.
pub struct Keymap {
    compiled: CompiledBindings,
}

impl Keymap {
    /// Look up the action bound to `key` in the given binding mode.
    pub fn resolve(&self, mode: &KeyBindingMode, key: KeyCombination) -> Option<Action> {
        self.compiled.resolve(mode, key)
    }

    /// Build the default keymap from the flat binding table.
    pub fn defaults() -> Self {
        Self {
            compiled: CompiledBindings::from_table_with_no_shared_fallback(BINDINGS, &[
                BindingModeId::CommandPalette,
                BindingModeId::FilePicker,
            ]),
        }
    }

    /// Build a keymap from defaults, then apply user overrides from `KeysConfig`.
    ///
    /// Invalid key strings or action names are logged as warnings and skipped.
    pub fn from_config(config: &KeysConfig) -> Self {
        let mut keymap = Self::defaults();

        let mode_configs: &[(&std::collections::HashMap<String, String>, BindingModeId)] = &[
            (&config.normal, BindingModeId::Normal),
            (&config.tab_page, BindingModeId::TabPage),
            (&config.help, BindingModeId::Help),
            (&config.config, BindingModeId::Overview),
            (&config.convoys, BindingModeId::Convoys),
            (&config.action_menu, BindingModeId::ActionMenu),
            (&config.delete_confirm, BindingModeId::DeleteConfirm),
            (&config.close_confirm, BindingModeId::CloseConfirm),
            (&config.command_palette, BindingModeId::CommandPalette),
            (&config.file_picker, BindingModeId::FilePicker),
        ];

        // Apply shared overrides
        for (key_str, action_str) in &config.shared {
            match Self::parse_binding(key_str, action_str) {
                Some((combo, action)) => {
                    keymap.compiled.key_map.entry(BindingModeId::Shared).or_default().insert(combo, action);
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
                        keymap.compiled.key_map.entry(*mode).or_default().insert(combo, action);
                    }
                    None => {
                        tracing::warn!(key = %key_str, action = %action_str, ?mode, "skipping invalid key binding");
                    }
                }
            }
        }

        // Rebuild hints so status bar chips and click targets reflect user overrides.
        keymap.compiled.rebuild_hints();

        keymap
    }

    /// Collect hint chips for a given binding mode.
    pub fn hints_for(&self, mode: &KeyBindingMode) -> Vec<KeyChip> {
        self.compiled.hints_for(mode)
    }

    /// Generate help sections from the active keymap bindings for Normal mode.
    ///
    /// Collects effective bindings (mode-specific + shared fallback), groups them
    /// by action, and organises into display sections with combined key names.
    pub fn help_sections(&self) -> Vec<HelpSection> {
        // Build the effective Normal-mode binding map: start with shared, then
        // TabPage (app-global tab-level keys), then Normal (repo-tab specific).
        // This mirrors the Composed([TabPage, Normal]) resolution order so the
        // help screen accurately reflects what each key does in Normal mode.
        let mut effective: std::collections::HashMap<KeyCombination, Action> = std::collections::HashMap::new();
        if let Some(shared_bindings) = self.compiled.key_map.get(&BindingModeId::Shared) {
            effective.extend(shared_bindings);
        }
        if let Some(tab_page_bindings) = self.compiled.key_map.get(&BindingModeId::TabPage) {
            effective.extend(tab_page_bindings);
        }
        if let Some(normal_bindings) = self.compiled.key_map.get(&BindingModeId::Normal) {
            effective.extend(normal_bindings);
        }

        // Invert: group keys by action for display.
        let mut action_keys: std::collections::HashMap<Action, Vec<String>> = std::collections::HashMap::new();
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
                Action::OpenCommandPalette,
                Action::OpenContextualPalette,
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
                Action::ToggleArchived,
                Action::ToggleDebug,
                Action::CycleTheme,
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
mod tests;
