// Keymap module: configurable key bindings for the TUI.

use std::hash::Hash;

use crokey::KeyCombination;
use flotilla_core::config::KeysConfig;

use crate::{
    binding_table::{BindingModeId, CompiledBindings, KeyBindingMode, BINDINGS},
    status_bar::KeyChip,
};

/// An action that can be triggered by a key binding.
///
/// Most variants correspond to UI-level operations (navigation, mode transitions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    SelectNext,
    SelectPrev,
    NextPanel,
    PrevPanel,
    Confirm,
    Dismiss,
    Quit,
    Refresh,
    PrevTab,
    NextTab,
    MoveTabLeft,
    MoveTabRight,
    CloseTab,
    ToggleHelp,
    ToggleMultiSelect,
    ToggleDebug,
    ToggleStatusBarKeys,
    CycleHost,
    CycleTheme,
    OpenActionMenu,
    OpenFind,
    FetchMore,
    OpenFilePicker,
    OpenCommandPalette,
    OpenContextualPalette,
    Describe,
    FillSelected,
    /// Open the command palette pre-filled to complete the selected convoy work.
    CompleteConvoyWork,
    /// Attach the active workspace manager to the selected convoy vessel's workspace.
    AttachConvoyVessel,
    /// Materialize or focus the selected convoy/vessel in the connected PM.
    OpenInPm,
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
                | Action::CloseTab
                | Action::CycleTheme
                | Action::CycleHost
                | Action::ToggleDebug
                | Action::ToggleStatusBarKeys
                | Action::Refresh
        )
    }

    /// Parse an action from its snake_case config string representation.
    ///
    pub fn from_config_str(s: &str) -> Option<Action> {
        let action = match s {
            "select_next" => Action::SelectNext,
            "select_prev" => Action::SelectPrev,
            "next_panel" => Action::NextPanel,
            "prev_panel" => Action::PrevPanel,
            "confirm" => Action::Confirm,
            "dismiss" => Action::Dismiss,
            "quit" => Action::Quit,
            "refresh" => Action::Refresh,
            "prev_tab" => Action::PrevTab,
            "next_tab" => Action::NextTab,
            "move_tab_left" => Action::MoveTabLeft,
            "move_tab_right" => Action::MoveTabRight,
            "close_tab" => Action::CloseTab,
            "toggle_help" => Action::ToggleHelp,
            "toggle_multi_select" => Action::ToggleMultiSelect,
            "toggle_debug" => Action::ToggleDebug,
            "toggle_status_bar_keys" => Action::ToggleStatusBarKeys,
            "cycle_host" => Action::CycleHost,
            "cycle_theme" => Action::CycleTheme,
            "open_action_menu" => Action::OpenActionMenu,
            "open_find" => Action::OpenFind,
            "fetch_more" => Action::FetchMore,
            "open_file_picker" => Action::OpenFilePicker,
            "open_command_palette" => Action::OpenCommandPalette,
            "open_contextual_palette" => Action::OpenContextualPalette,
            "describe" => Action::Describe,
            "fill_selected" => Action::FillSelected,
            "complete_convoy_work" => Action::CompleteConvoyWork,
            "attach_convoy_vessel" => Action::AttachConvoyVessel,
            "open_in_pm" => Action::OpenInPm,
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
            Action::NextPanel => "next_panel",
            Action::PrevPanel => "prev_panel",
            Action::Confirm => "confirm",
            Action::Dismiss => "dismiss",
            Action::Quit => "quit",
            Action::Refresh => "refresh",
            Action::PrevTab => "prev_tab",
            Action::NextTab => "next_tab",
            Action::MoveTabLeft => "move_tab_left",
            Action::MoveTabRight => "move_tab_right",
            Action::CloseTab => "close_tab",
            Action::ToggleHelp => "toggle_help",
            Action::ToggleMultiSelect => "toggle_multi_select",
            Action::ToggleDebug => "toggle_debug",
            Action::ToggleStatusBarKeys => "toggle_status_bar_keys",
            Action::CycleHost => "cycle_host",
            Action::CycleTheme => "cycle_theme",
            Action::OpenActionMenu => "open_action_menu",
            Action::OpenFind => "open_find",
            Action::FetchMore => "fetch_more",
            Action::OpenFilePicker => "open_file_picker",
            Action::OpenCommandPalette => "open_command_palette",
            Action::OpenContextualPalette => "open_contextual_palette",
            Action::Describe => "describe",
            Action::FillSelected => "fill_selected",
            Action::CompleteConvoyWork => "complete_convoy_work",
            Action::AttachConvoyVessel => "attach_convoy_vessel",
            Action::OpenInPm => "open_in_pm",
        }
    }

    /// Human-readable description of the action, suitable for help screen display.
    pub fn description(&self) -> &'static str {
        match self {
            Action::SelectNext => "Move selection down",
            Action::SelectPrev => "Move selection up",
            Action::NextPanel => "Move to next panel",
            Action::PrevPanel => "Move to previous panel",
            Action::Confirm => "Confirm / execute",
            Action::Dismiss => "Dismiss / go back",
            Action::Quit => "Quit the application",
            Action::Refresh => "Refresh all providers",
            Action::PrevTab => "Switch to previous tab",
            Action::NextTab => "Switch to next tab",
            Action::MoveTabLeft => "Move current tab left",
            Action::MoveTabRight => "Move current tab right",
            Action::CloseTab => "Close current tab",
            Action::ToggleHelp => "Toggle help screen",
            Action::ToggleMultiSelect => "Toggle multi-select",
            Action::ToggleDebug => "Toggle debug panel",
            Action::ToggleStatusBarKeys => "Toggle status bar key hints",
            Action::CycleHost => "Cycle host filter",
            Action::CycleTheme => "Cycle colour theme",
            Action::OpenActionMenu => "Open action menu",
            Action::OpenFind => "Find",
            Action::FetchMore => "Fetch more rows",
            Action::OpenFilePicker => "Open file picker",
            Action::OpenCommandPalette => "Open command palette",
            Action::OpenContextualPalette => "Open contextual palette (pre-filled)",
            Action::Describe => "Describe selected row",
            Action::FillSelected => "Fill selected item",
            Action::CompleteConvoyWork => "Force complete work",
            Action::AttachConvoyVessel => "Attach to vessel workspace",
            Action::OpenInPm => "Open in presentation manager",
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
            (&config.tab_page, BindingModeId::TabPage),
            (&config.tab_shell, BindingModeId::TabShell),
            (&config.help, BindingModeId::Help),
            (&config.config, BindingModeId::Overview),
            (&config.convoys, BindingModeId::Convoys),
            (&config.project, BindingModeId::Project),
            (&config.convoy_vessels, BindingModeId::ConvoyVessels),
            (&config.action_menu, BindingModeId::ActionMenu),
            (&config.delete_confirm, BindingModeId::DeleteConfirm),
            (&config.dispatch_confirm, BindingModeId::DispatchConfirm),
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
        // TabPage (app globals) and TabShell (tab management), then Normal
        // (repo-tab specific). This mirrors the
        // Composed([TabPage, TabShell, Normal]) resolution order so the
        // help screen accurately reflects what each key does in Normal mode.
        let mut effective: std::collections::HashMap<KeyCombination, Action> = std::collections::HashMap::new();
        if let Some(shared_bindings) = self.compiled.key_map.get(&BindingModeId::Shared) {
            effective.extend(shared_bindings);
        }
        if let Some(tab_page_bindings) = self.compiled.key_map.get(&BindingModeId::TabPage) {
            effective.extend(tab_page_bindings);
        }
        if let Some(tab_shell_bindings) = self.compiled.key_map.get(&BindingModeId::TabShell) {
            effective.extend(tab_shell_bindings);
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
            ("Navigation", &[Action::SelectNext, Action::SelectPrev, Action::NextPanel, Action::PrevPanel]),
            ("Actions", &[
                Action::Confirm,
                Action::OpenCommandPalette,
                Action::OpenContextualPalette,
                Action::OpenActionMenu,
                Action::OpenFind,
                Action::OpenFilePicker,
                Action::Refresh,
                Action::ToggleStatusBarKeys,
            ]),
            ("Multi-select (issues)", &[Action::ToggleMultiSelect]),
            ("Repos", &[Action::PrevTab, Action::NextTab, Action::MoveTabLeft, Action::MoveTabRight]),
            ("General", &[Action::ToggleDebug, Action::CycleTheme, Action::ToggleHelp, Action::Dismiss, Action::Quit]),
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
