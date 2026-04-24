use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

/// Helper to construct a `KeyCombination` from a `KeyEvent`.
fn kc(code: KeyCode, modifiers: KeyModifiers) -> KeyCombination {
    KeyCombination::from(KeyEvent::new(code, modifiers))
}

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
        Action::ToggleArchived,
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
        Action::OpenContextualPalette,
        Action::FillSelected,
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
        Action::ToggleArchived,
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
        Action::OpenContextualPalette,
        Action::FillSelected,
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

#[test]
fn toggle_archived_round_trips() {
    assert_eq!(Action::from_config_str("toggle_archived"), Some(Action::ToggleArchived));
    assert_eq!(Action::ToggleArchived.as_config_str(), "toggle_archived");
}

// ── Keymap tests ──

#[test]
fn defaults_resolve_shared_navigation() {
    let km = Keymap::defaults();
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(j)), Some(Action::SelectNext));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(down)), Some(Action::SelectNext));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(k)), Some(Action::SelectPrev));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(up)), Some(Action::SelectPrev));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(enter)), Some(Action::Confirm));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(esc)), Some(Action::Dismiss));
}

#[test]
fn shared_bindings_work_across_modes() {
    let km = Keymap::defaults();
    let modes = [BindingModeId::Normal, BindingModeId::Help, BindingModeId::Overview, BindingModeId::ActionMenu];
    for mode in modes {
        assert_eq!(
            km.resolve(&KeyBindingMode::from(mode), crokey::key!(j)),
            Some(Action::SelectNext),
            "j should be SelectNext in {mode:?}"
        );
        assert_eq!(
            km.resolve(&KeyBindingMode::from(mode), crokey::key!(enter)),
            Some(Action::Confirm),
            "enter should be Confirm in {mode:?}"
        );
    }
}

#[test]
fn normal_mode_specific_bindings() {
    let km = Keymap::defaults();
    // The effective Normal mode is Composed([TabPage, Normal]) — use that for keys that moved to TabPage.
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    // Keys from TabPage (app-global):
    assert_eq!(km.resolve(&normal_composed, crokey::key!(q)), Some(Action::Quit));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(h)), Some(Action::ToggleHelp));
    assert_eq!(km.resolve(&normal_composed, kc(KeyCode::Char('T'), KeyModifiers::SHIFT)), Some(Action::CycleTheme));
    assert_eq!(km.resolve(&normal_composed, kc(KeyCode::Char('/'), KeyModifiers::NONE)), Some(Action::OpenCommandPalette));
    assert_eq!(km.resolve(&normal_composed, kc(KeyCode::Char('D'), KeyModifiers::SHIFT)), Some(Action::ToggleDebug));
    // Keys from Normal (repo-tab specific):
    assert_eq!(km.resolve(&normal_composed, crokey::key!(r)), Some(Action::Refresh));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(space)), Some(Action::ToggleMultiSelect));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(l)), Some(Action::CycleLayout));
    assert_eq!(km.resolve(&normal_composed, kc(KeyCode::Char('.'), KeyModifiers::NONE)), Some(Action::OpenActionMenu));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(n)), Some(Action::OpenBranchInput));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(a)), Some(Action::OpenFilePicker));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(c)), Some(Action::ToggleProviders));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(d)), Some(Action::Dispatch(Intent::RemoveCheckout)));
    assert_eq!(km.resolve(&normal_composed, crokey::key!(p)), Some(Action::Dispatch(Intent::OpenChangeRequest)));
}

#[test]
fn mode_specific_overrides_shared() {
    let km = Keymap::defaults();
    // q is Quit in TabPage (composed with top-level tabs), but Dismiss in overlay modes.
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    let overview_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Overview]);
    assert_eq!(km.resolve(&normal_composed, crokey::key!(q)), Some(Action::Quit));
    // Overview overrides q → Dismiss (navigates back to repo tab, not quit app).
    assert_eq!(km.resolve(&overview_composed, crokey::key!(q)), Some(Action::Dismiss));
    // Overlay modes use Single and get their own q binding:
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Help), crokey::key!(q)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::ActionMenu), crokey::key!(q)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::DeleteConfirm), crokey::key!(q)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::CloseConfirm), crokey::key!(q)), Some(Action::Dismiss));
}

#[test]
fn tab_switching_in_normal_and_overview() {
    let km = Keymap::defaults();
    let bracket_left = kc(KeyCode::Char('['), KeyModifiers::NONE);
    let bracket_right = kc(KeyCode::Char(']'), KeyModifiers::NONE);

    // [/] live in TabPage; resolve through composed modes.
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    let overview_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Overview]);
    let convoys_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Convoys]);
    assert_eq!(km.resolve(&normal_composed, bracket_left), Some(Action::PrevTab));
    assert_eq!(km.resolve(&normal_composed, bracket_right), Some(Action::NextTab));
    assert_eq!(km.resolve(&overview_composed, bracket_left), Some(Action::PrevTab));
    assert_eq!(km.resolve(&overview_composed, bracket_right), Some(Action::NextTab));
    assert_eq!(km.resolve(&convoys_composed, bracket_left), Some(Action::PrevTab));
    assert_eq!(km.resolve(&convoys_composed, bracket_right), Some(Action::NextTab));
}

#[test]
fn delete_confirm_has_y_n_bindings() {
    let km = Keymap::defaults();
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::DeleteConfirm), crokey::key!(y)), Some(Action::Confirm));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::DeleteConfirm), crokey::key!(n)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::DeleteConfirm), crokey::key!(q)), Some(Action::Dismiss));
}

#[test]
fn close_confirm_has_y_n_bindings() {
    let km = Keymap::defaults();
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::CloseConfirm), crokey::key!(y)), Some(Action::Confirm));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::CloseConfirm), crokey::key!(n)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::CloseConfirm), crokey::key!(q)), Some(Action::Dismiss));
}

#[test]
fn help_mode_toggle_with_h() {
    let km = Keymap::defaults();
    // h maps to ToggleHelp in TabPage (composed with top-level tabs) and in Help mode.
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    assert_eq!(km.resolve(&normal_composed, crokey::key!(h)), Some(Action::ToggleHelp));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Help), crokey::key!(h)), Some(Action::ToggleHelp));
}

#[test]
fn question_mark_maps_to_contextual_palette_in_normal() {
    let km = Keymap::defaults();
    let question_mark = kc(KeyCode::Char('?'), KeyModifiers::NONE);
    // ? maps to OpenContextualPalette via TabPage, available in all top-level tab modes.
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    assert_eq!(km.resolve(&normal_composed, question_mark), Some(Action::OpenContextualPalette));
    // ? is not bound in overlay modes like Help (no TabPage composition there).
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Help), question_mark), None);
}

#[test]
fn toggle_status_bar_keys_is_shared_across_modes() {
    let km = Keymap::defaults();
    let shift_k = kc(KeyCode::Char('K'), KeyModifiers::SHIFT);
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), shift_k), Some(Action::ToggleStatusBarKeys));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Help), shift_k), Some(Action::ToggleStatusBarKeys));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Overview), shift_k), Some(Action::ToggleStatusBarKeys));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::ActionMenu), shift_k), Some(Action::ToggleStatusBarKeys));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::DeleteConfirm), shift_k), Some(Action::ToggleStatusBarKeys));
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::CloseConfirm), shift_k), Some(Action::ToggleStatusBarKeys));
}

#[test]
fn unbound_key_returns_none() {
    let km = Keymap::defaults();
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Normal), crokey::key!(f12)), None);
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Help), crokey::key!(x)), None);
    assert_eq!(km.resolve(&KeyBindingMode::from(BindingModeId::Overview), crokey::key!(z)), None);
}

#[test]
fn file_picker_no_shared_fallback() {
    let km = Keymap::defaults();
    let mode = KeyBindingMode::from(BindingModeId::FilePicker);
    // Mode-specific bindings resolve
    assert_eq!(km.resolve(&mode, crokey::key!(enter)), Some(Action::Confirm));
    assert_eq!(km.resolve(&mode, crokey::key!(esc)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&mode, crokey::key!(up)), Some(Action::SelectPrev));
    assert_eq!(km.resolve(&mode, crokey::key!(down)), Some(Action::SelectNext));
    // Typing keys do NOT resolve — no shared fallback
    assert_eq!(km.resolve(&mode, crokey::key!(j)), None);
    assert_eq!(km.resolve(&mode, crokey::key!(k)), None);
}

#[test]
fn command_palette_resolves_navigation_not_typing() {
    let km = Keymap::defaults();
    let mode = KeyBindingMode::from(BindingModeId::CommandPalette);
    // Navigation keys resolve
    assert_eq!(km.resolve(&mode, crokey::key!(esc)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&mode, crokey::key!(enter)), Some(Action::Confirm));
    assert_eq!(km.resolve(&mode, crokey::key!(up)), Some(Action::SelectPrev));
    assert_eq!(km.resolve(&mode, crokey::key!(down)), Some(Action::SelectNext));
    // Typing keys do NOT resolve (fall through to handle_raw_key)
    assert_eq!(km.resolve(&mode, crokey::key!(j)), None);
    assert_eq!(km.resolve(&mode, crokey::key!(k)), None);
    assert_eq!(km.resolve(&mode, kc(KeyCode::Char('?'), KeyModifiers::NONE)), None);
    // Tab resolves to FillSelected
    assert_eq!(km.resolve(&mode, crokey::key!(tab)), Some(Action::FillSelected));
}

#[test]
fn file_picker_resolves_navigation_not_typing() {
    let km = Keymap::defaults();
    let mode = KeyBindingMode::from(BindingModeId::FilePicker);
    // Navigation keys resolve
    assert_eq!(km.resolve(&mode, crokey::key!(esc)), Some(Action::Dismiss));
    assert_eq!(km.resolve(&mode, crokey::key!(enter)), Some(Action::Confirm));
    assert_eq!(km.resolve(&mode, crokey::key!(up)), Some(Action::SelectPrev));
    assert_eq!(km.resolve(&mode, crokey::key!(down)), Some(Action::SelectNext));
    // Typing keys do NOT resolve
    assert_eq!(km.resolve(&mode, crokey::key!(j)), None);
    assert_eq!(km.resolve(&mode, crokey::key!(k)), None);
    assert_eq!(km.resolve(&mode, kc(KeyCode::Char('?'), KeyModifiers::NONE)), None);
    // Tab resolves to FillSelected
    assert_eq!(km.resolve(&mode, crokey::key!(tab)), Some(Action::FillSelected));
}

// ── from_config tests ──

#[test]
fn from_config_overrides_shared_binding() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("g".into(), "select_next".into());
    let keymap = Keymap::from_config(&keys);
    assert_eq!(
        keymap.resolve(&KeyBindingMode::from(BindingModeId::Normal), kc(KeyCode::Char('g'), KeyModifiers::NONE)),
        Some(Action::SelectNext)
    );
    // original 'j' still works
    assert_eq!(
        keymap.resolve(&KeyBindingMode::from(BindingModeId::Normal), kc(KeyCode::Char('j'), KeyModifiers::NONE)),
        Some(Action::SelectNext)
    );
}

#[test]
fn from_config_overrides_mode_binding() {
    let mut keys = KeysConfig::default();
    keys.normal.insert("x".into(), "quit".into());
    let keymap = Keymap::from_config(&keys);
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    assert_eq!(keymap.resolve(&normal_composed, kc(KeyCode::Char('x'), KeyModifiers::NONE)), Some(Action::Quit));
    // original 'q' (now in TabPage) still resolves through the composed mode
    assert_eq!(keymap.resolve(&normal_composed, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
}

#[test]
fn from_config_skips_invalid_key_string() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("NOT_A_VALID_KEY!!!".into(), "quit".into());
    let keymap = Keymap::from_config(&keys);
    // defaults still work despite invalid override — 'q' now in TabPage
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    assert_eq!(keymap.resolve(&normal_composed, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
}

#[test]
fn from_config_skips_invalid_action_name() {
    let mut keys = KeysConfig::default();
    keys.shared.insert("g".into(), "nonexistent_action".into());
    let keymap = Keymap::from_config(&keys);
    // 'g' was not bound by default, and the invalid override is skipped
    assert_eq!(keymap.resolve(&KeyBindingMode::from(BindingModeId::Normal), kc(KeyCode::Char('g'), KeyModifiers::NONE)), None);
}

#[test]
fn from_config_empty_uses_defaults() {
    let keys = KeysConfig::default();
    let keymap = Keymap::from_config(&keys);
    let normal_composed = KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]);
    // j is in Shared, accessible from Normal composed mode.
    assert_eq!(keymap.resolve(&normal_composed, kc(KeyCode::Char('j'), KeyModifiers::NONE)), Some(Action::SelectNext));
    // q is now in TabPage, accessible through the composed mode.
    assert_eq!(keymap.resolve(&normal_composed, kc(KeyCode::Char('q'), KeyModifiers::NONE)), Some(Action::Quit));
}

#[test]
fn from_config_overrides_command_palette_binding() {
    let mut keys = KeysConfig::default();
    keys.command_palette.insert("ctrl-p".into(), "select_prev".into());
    keys.command_palette.insert("ctrl-n".into(), "select_next".into());
    let keymap = Keymap::from_config(&keys);
    let mode = KeyBindingMode::from(BindingModeId::CommandPalette);
    assert_eq!(keymap.resolve(&mode, kc(KeyCode::Char('p'), KeyModifiers::CONTROL)), Some(Action::SelectPrev));
    assert_eq!(keymap.resolve(&mode, kc(KeyCode::Char('n'), KeyModifiers::CONTROL)), Some(Action::SelectNext));
    // Default bindings still work
    assert_eq!(keymap.resolve(&mode, crokey::key!(up)), Some(Action::SelectPrev));
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
