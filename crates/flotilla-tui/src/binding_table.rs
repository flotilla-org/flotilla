// Binding table: flat declarative key binding definitions with hint annotations.

use std::collections::{HashMap, HashSet};

use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyModifiers};

use crate::{
    app::intent::Intent,
    keymap::Action,
    status_bar::{KeyChip, StatusBarAction},
};

// ── Core types ───────────────────────────────────────────────────────

/// Flat enum for hashable binding mode identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingModeId {
    Shared,
    Normal,
    Overview,
    Help,
    ActionMenu,
    DeleteConfirm,
    CloseConfirm,
    BranchInput,
    IssueSearch,
    CommandPalette,
    FilePicker,
    SearchActive,
}

/// What widgets return from `binding_mode()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyBindingMode {
    Single(BindingModeId),
    Composed(Vec<BindingModeId>),
}

impl KeyBindingMode {
    /// The "primary" mode: for `Single`, the mode itself; for `Composed`,
    /// the last (highest-priority) mode in the stack.
    pub fn primary(&self) -> BindingModeId {
        match self {
            KeyBindingMode::Single(id) => *id,
            KeyBindingMode::Composed(ids) => ids.last().copied().unwrap_or(BindingModeId::Normal),
        }
    }
}

impl From<BindingModeId> for KeyBindingMode {
    fn from(id: BindingModeId) -> Self {
        KeyBindingMode::Single(id)
    }
}

/// Widget-provided status content for the status bar.
#[derive(Debug, Clone, Default)]
pub struct StatusFragment {
    pub status: Option<StatusContent>,
}

#[derive(Debug, Clone)]
pub enum StatusContent {
    Label(String),
    ActiveInput { prefix: String, text: String },
    Progress { label: String, text: String },
}

/// A single entry in the binding table.
pub struct Binding {
    pub mode: BindingModeId,
    pub key: &'static str,
    pub action: Action,
    pub hint: Option<&'static str>,
    /// Override the display key shown on the hint chip (e.g. "ENT" for "enter").
    /// When `None`, uses `key` as the display string.
    pub hint_key: Option<&'static str>,
}

// ── Binding table helpers ────────────────────────────────────────────

/// Create a binding without a hint.
const fn b(mode: BindingModeId, key: &'static str, action: Action) -> Binding {
    Binding { mode, key, action, hint: None, hint_key: None }
}

/// Create a binding with a hint annotation for the status bar.
const fn h(mode: BindingModeId, key: &'static str, action: Action, hint: &'static str) -> Binding {
    Binding { mode, key, action, hint: Some(hint), hint_key: None }
}

/// Create a binding with a hint and a custom display key for the chip.
const fn hk(mode: BindingModeId, key: &'static str, hint_key: &'static str, action: Action, hint: &'static str) -> Binding {
    Binding { mode, key, action, hint: Some(hint), hint_key: Some(hint_key) }
}

// ── The flat binding table ───────────────────────────────────────────

pub static BINDINGS: &[Binding] = &[
    // ── Shared ──
    b(BindingModeId::Shared, "j", Action::SelectNext),
    b(BindingModeId::Shared, "down", Action::SelectNext),
    b(BindingModeId::Shared, "k", Action::SelectPrev),
    b(BindingModeId::Shared, "up", Action::SelectPrev),
    b(BindingModeId::Shared, "enter", Action::Confirm),
    b(BindingModeId::Shared, "esc", Action::Dismiss),
    b(BindingModeId::Shared, "?", Action::ToggleHelp),
    b(BindingModeId::Shared, "S-K", Action::ToggleStatusBarKeys),
    // ── Normal ──
    // Hint order matters: ENT, ., n, ?, q matches the old status bar layout.
    hk(BindingModeId::Normal, "enter", "ENT", Action::Confirm, "Open"),
    h(BindingModeId::Normal, ".", Action::OpenActionMenu, "Menu"),
    h(BindingModeId::Normal, "n", Action::OpenBranchInput, "New"),
    h(BindingModeId::Normal, "?", Action::ToggleHelp, "Help"),
    h(BindingModeId::Normal, "q", Action::Quit, "Quit"),
    b(BindingModeId::Normal, "r", Action::Refresh),
    b(BindingModeId::Normal, "[", Action::PrevTab),
    b(BindingModeId::Normal, "]", Action::NextTab),
    b(BindingModeId::Normal, "{", Action::MoveTabLeft),
    b(BindingModeId::Normal, "}", Action::MoveTabRight),
    b(BindingModeId::Normal, "space", Action::ToggleMultiSelect),
    b(BindingModeId::Normal, "h", Action::CycleHost),
    b(BindingModeId::Normal, "l", Action::CycleLayout),
    b(BindingModeId::Normal, "S-T", Action::CycleTheme),
    b(BindingModeId::Normal, "/", Action::OpenCommandPalette),
    b(BindingModeId::Normal, "a", Action::OpenFilePicker),
    b(BindingModeId::Normal, "c", Action::ToggleProviders),
    b(BindingModeId::Normal, "u", Action::ToggleArchived),
    b(BindingModeId::Normal, "S-D", Action::ToggleDebug),
    b(BindingModeId::Normal, "d", Action::Dispatch(Intent::RemoveCheckout)),
    b(BindingModeId::Normal, "p", Action::Dispatch(Intent::OpenChangeRequest)),
    // ── Overview (replaces old Config) ──
    h(BindingModeId::Overview, "j", Action::SelectNext, "Down"),
    h(BindingModeId::Overview, "k", Action::SelectPrev, "Up"),
    h(BindingModeId::Overview, "[", Action::PrevTab, "Prev"),
    h(BindingModeId::Overview, "]", Action::NextTab, "Next"),
    h(BindingModeId::Overview, "q", Action::Dismiss, "Quit"),
    // ── Help ──
    h(BindingModeId::Help, "j", Action::SelectNext, "Down"),
    h(BindingModeId::Help, "k", Action::SelectPrev, "Up"),
    hk(BindingModeId::Help, "esc", "ESC", Action::Dismiss, "Close"),
    h(BindingModeId::Help, "?", Action::ToggleHelp, "Close"),
    b(BindingModeId::Help, "q", Action::Dismiss),
    // ── ActionMenu ──
    h(BindingModeId::ActionMenu, "j", Action::SelectNext, "Down"),
    h(BindingModeId::ActionMenu, "k", Action::SelectPrev, "Up"),
    hk(BindingModeId::ActionMenu, "enter", "ENT", Action::Confirm, "Select"),
    hk(BindingModeId::ActionMenu, "esc", "ESC", Action::Dismiss, "Close"),
    b(BindingModeId::ActionMenu, "q", Action::Dismiss),
    // ── DeleteConfirm ──
    h(BindingModeId::DeleteConfirm, "y", Action::Confirm, "Yes"),
    h(BindingModeId::DeleteConfirm, "n", Action::Dismiss, "No"),
    b(BindingModeId::DeleteConfirm, "q", Action::Dismiss),
    // ── CloseConfirm ──
    h(BindingModeId::CloseConfirm, "y", Action::Confirm, "Yes"),
    h(BindingModeId::CloseConfirm, "n", Action::Dismiss, "No"),
    b(BindingModeId::CloseConfirm, "q", Action::Dismiss),
    // ── BranchInput ──
    hk(BindingModeId::BranchInput, "enter", "ENT", Action::Confirm, "Create"),
    hk(BindingModeId::BranchInput, "esc", "ESC", Action::Dismiss, "Cancel"),
    // ── IssueSearch ──
    hk(BindingModeId::IssueSearch, "enter", "ENT", Action::Confirm, "Apply"),
    hk(BindingModeId::IssueSearch, "esc", "ESC", Action::Dismiss, "Cancel"),
    // ── CommandPalette ──
    // No shared fallback: typing keys must reach handle_raw_key for text input.
    // Tab is handled by handle_raw_key for "fill" behavior (not bound here).
    b(BindingModeId::CommandPalette, "up", Action::SelectPrev),
    b(BindingModeId::CommandPalette, "down", Action::SelectNext),
    hk(BindingModeId::CommandPalette, "enter", "ENT", Action::Confirm, "Run"),
    hk(BindingModeId::CommandPalette, "esc", "ESC", Action::Dismiss, "Close"),
    // ── FilePicker ──
    // No shared fallback: typing keys must reach handle_raw_key for text input.
    // Tab is handled by handle_raw_key for directory completion (not bound here).
    b(BindingModeId::FilePicker, "up", Action::SelectPrev),
    b(BindingModeId::FilePicker, "down", Action::SelectNext),
    hk(BindingModeId::FilePicker, "enter", "ENT", Action::Confirm, "Select"),
    hk(BindingModeId::FilePicker, "esc", "ESC", Action::Dismiss, "Cancel"),
    // ── SearchActive ──
    h(BindingModeId::SearchActive, "esc", Action::Dismiss, "Clear"),
];

// ── Compiled bindings ────────────────────────────────────────────────

pub struct CompiledBindings {
    pub key_map: HashMap<BindingModeId, HashMap<KeyCombination, Action>>,
    pub hints: HashMap<BindingModeId, Vec<KeyChip>>,
    /// The original binding table entries that had hints, used to rebuild
    /// hints after user config overrides change the key_map.
    /// Each entry stores (original_key_combo, action, hint_label).
    hint_entries: HashMap<BindingModeId, Vec<(KeyCombination, Action, &'static str)>>,
    no_shared_fallback: HashSet<BindingModeId>,
}

impl CompiledBindings {
    /// Parse key strings and build both the key map and hint maps.
    pub fn from_table(bindings: &[Binding]) -> Self {
        Self::from_table_with_no_shared_fallback(bindings, &[])
    }

    /// Like `from_table`, but the given modes will not fall back to `Shared`
    /// bindings when no mode-specific binding is found. Use this for text-input
    /// modes (e.g. `CommandPalette`, `FilePicker`) so that characters like
    /// `j`, `k`, `?` reach the widget's text handler instead of being
    /// intercepted by shared navigation bindings.
    pub fn from_table_with_no_shared_fallback(bindings: &[Binding], no_fallback: &[BindingModeId]) -> Self {
        let mut key_map: HashMap<BindingModeId, HashMap<KeyCombination, Action>> = HashMap::new();
        let mut hint_entries: HashMap<BindingModeId, Vec<(KeyCombination, Action, &'static str)>> = HashMap::new();

        for binding in bindings {
            let combo = parse_key_string(binding.key);
            key_map.entry(binding.mode).or_default().insert(combo, binding.action);

            if let Some(hint_label) = binding.hint {
                hint_entries.entry(binding.mode).or_default().push((combo, binding.action, hint_label));
            }
        }

        let no_shared_fallback: HashSet<BindingModeId> = no_fallback.iter().copied().collect();
        let hints = Self::build_hints(&key_map, &hint_entries);
        CompiledBindings { key_map, hints, hint_entries, no_shared_fallback }
    }

    /// Rebuild hints from the current key_map. Called after user config
    /// overrides to keep chips and click targets in sync with actual bindings.
    pub fn rebuild_hints(&mut self) {
        self.hints = Self::build_hints(&self.key_map, &self.hint_entries);
    }

    /// Build hint chips from hint entries and the current key_map.
    ///
    /// For each hinted entry, check if the original key still maps to the
    /// same action. If so, use that key (possibly rebound). If the original
    /// key was rebound to a different action, search for the action's new key.
    fn build_hints(
        key_map: &HashMap<BindingModeId, HashMap<KeyCombination, Action>>,
        hint_entries: &HashMap<BindingModeId, Vec<(KeyCombination, Action, &'static str)>>,
    ) -> HashMap<BindingModeId, Vec<KeyChip>> {
        let mut hints: HashMap<BindingModeId, Vec<KeyChip>> = HashMap::new();

        for (mode, entries) in hint_entries {
            if let Some(mode_map) = key_map.get(mode) {
                for (original_combo, action, label) in entries {
                    // Check if the original key still maps to this action.
                    // If the key was rebound to a different action, drop the
                    // hint — it was tied to a specific key and is no longer
                    // relevant.
                    let combo = if mode_map.get(original_combo) == Some(action) {
                        *original_combo
                    } else {
                        continue; // key was rebound — drop the hint
                    };

                    let (display, code, modifiers) = display_for_combo(&combo);
                    hints.entry(*mode).or_default().push(KeyChip::new(&display, label, StatusBarAction::combo(code, modifiers)));
                }
            }
        }

        hints
    }

    /// Resolve a key combination against the given binding mode.
    ///
    /// For `Single`: check the mode first, then fall back to `Shared` (unless
    /// the mode is in `no_shared_fallback`).
    /// For `Composed`: check modes in reverse order (later wins), then `Shared`
    /// (unless any mode in the stack is in `no_shared_fallback`).
    pub fn resolve(&self, mode: &KeyBindingMode, key: KeyCombination) -> Option<Action> {
        match mode {
            KeyBindingMode::Single(id) => {
                let mode_result = self.key_map.get(id).and_then(|m| m.get(&key).copied());
                if mode_result.is_some() {
                    return mode_result;
                }
                if self.no_shared_fallback.contains(id) {
                    return None;
                }
                self.key_map.get(&BindingModeId::Shared).and_then(|m| m.get(&key).copied())
            }
            KeyBindingMode::Composed(ids) => {
                // Check in reverse order so later modes win.
                for id in ids.iter().rev() {
                    if let Some(action) = self.key_map.get(id).and_then(|m| m.get(&key).copied()) {
                        return Some(action);
                    }
                }
                if ids.iter().any(|id| self.no_shared_fallback.contains(id)) {
                    return None;
                }
                // Fall back to Shared.
                self.key_map.get(&BindingModeId::Shared).and_then(|m| m.get(&key).copied())
            }
        }
    }

    /// Collect hint chips for the given binding mode.
    ///
    /// For `Single`: Shared hints + mode hints (mode overrides by key).
    /// For `Composed`: merge all layers; later modes win by key.
    ///
    /// Shared hints are omitted entirely when the mode (or any mode in the
    /// stack) is in `no_shared_fallback`.
    pub fn hints_for(&self, mode: &KeyBindingMode) -> Vec<KeyChip> {
        match mode {
            KeyBindingMode::Single(id) => {
                let suppress_shared = self.no_shared_fallback.contains(id);
                let mut by_key: HashMap<String, KeyChip> = HashMap::new();
                // Start with Shared hints (unless suppressed).
                if !suppress_shared {
                    if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                        for chip in shared_hints {
                            by_key.insert(chip.key.clone(), chip.clone());
                        }
                    }
                }
                // Mode hints override by key.
                if let Some(mode_hints) = self.hints.get(id) {
                    for chip in mode_hints {
                        by_key.insert(chip.key.clone(), chip.clone());
                    }
                }
                // Preserve insertion order: shared first, then mode-specific.
                let mut result = Vec::new();
                if !suppress_shared {
                    if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                        for chip in shared_hints {
                            if let Some(c) = by_key.remove(&chip.key) {
                                result.push(c);
                            }
                        }
                    }
                }
                if let Some(mode_hints) = self.hints.get(id) {
                    for chip in mode_hints {
                        if let Some(c) = by_key.remove(&chip.key) {
                            result.push(c);
                        }
                    }
                }
                result
            }
            KeyBindingMode::Composed(ids) => {
                let suppress_shared = ids.iter().any(|id| self.no_shared_fallback.contains(id));
                let mut by_key: HashMap<String, KeyChip> = HashMap::new();
                // Start with Shared hints (unless suppressed).
                if !suppress_shared {
                    if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                        for chip in shared_hints {
                            by_key.insert(chip.key.clone(), chip.clone());
                        }
                    }
                }
                // Layer modes in order; later wins.
                for id in ids {
                    if let Some(mode_hints) = self.hints.get(id) {
                        for chip in mode_hints {
                            by_key.insert(chip.key.clone(), chip.clone());
                        }
                    }
                }
                // Preserve order: shared (unless suppressed), then each mode layer.
                let mut result = Vec::new();
                let mut seen = HashSet::new();
                if !suppress_shared {
                    if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                        for chip in shared_hints {
                            if seen.insert(chip.key.clone()) {
                                if let Some(c) = by_key.get(&chip.key) {
                                    result.push(c.clone());
                                }
                            }
                        }
                    }
                }
                for id in ids {
                    if let Some(mode_hints) = self.hints.get(id) {
                        for chip in mode_hints {
                            if seen.insert(chip.key.clone()) {
                                if let Some(c) = by_key.get(&chip.key) {
                                    result.push(c.clone());
                                }
                            }
                        }
                    }
                }
                result
            }
        }
    }
}

// ── Key string parsing ───────────────────────────────────────────────

/// Parse a key string like "j", "esc", "S-K", "C-q", "A-x", "[", "space" into a `KeyCombination`.
fn parse_key_string(s: &str) -> KeyCombination {
    // Strip modifier prefixes: C- (Ctrl), A- (Alt), S- (Shift).
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = s;
    loop {
        if let Some(r) = rest.strip_prefix("C-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("A-") {
            modifiers |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("S-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }

    // Named keys.
    let code = match rest {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        _ => {
            if rest.len() == 1 {
                let ch = rest.chars().next().expect("non-empty single-char key string");
                KeyCode::Char(ch)
            } else {
                panic!("unknown key string: {s}");
            }
        }
    };

    KeyCombination::from(crossterm::event::KeyEvent::new(code, modifiers))
}

/// Convert a `KeyCombination` back to a display string, KeyCode, and KeyModifiers.
/// Used to rebuild hint chips after user config overrides.
fn display_for_combo(combo: &KeyCombination) -> (String, KeyCode, KeyModifiers) {
    let code = match combo.codes {
        crokey::OneToThree::One(c) => c,
        crokey::OneToThree::Two(c, _) => c,
        crokey::OneToThree::Three(c, _, _) => c,
    };
    let modifiers = combo.modifiers;

    let base = match code {
        KeyCode::Char(' ') => "SPACE".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "ENT".into(),
        KeyCode::Esc => "ESC".into(),
        KeyCode::Tab => "TAB".into(),
        KeyCode::Up => "UP".into(),
        KeyCode::Down => "DOWN".into(),
        KeyCode::Left => "LEFT".into(),
        KeyCode::Right => "RIGHT".into(),
        _ => format!("{code:?}"),
    };

    let display = if modifiers.is_empty() {
        base
    } else {
        let mut prefix = String::new();
        if modifiers.contains(KeyModifiers::CONTROL) {
            prefix.push_str("C-");
        }
        if modifiers.contains(KeyModifiers::ALT) {
            prefix.push_str("A-");
        }
        if modifiers.contains(KeyModifiers::SHIFT) {
            prefix.push_str("S-");
        }
        format!("{prefix}{}", base.to_uppercase())
    };

    (display, code, modifiers)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_bindings_resolve_single_mode() {
        let compiled = CompiledBindings::from_table(BINDINGS);
        let mode = KeyBindingMode::Single(BindingModeId::Normal);
        // 'q' in Normal should be Quit.
        let q = parse_key_string("q");
        assert_eq!(compiled.resolve(&mode, q), Some(Action::Quit));
    }

    #[test]
    fn compiled_bindings_resolve_composed_mode_later_wins() {
        // Build a small table where two modes bind the same key differently.
        let table = &[Binding { mode: BindingModeId::Normal, key: "q", action: Action::Quit, hint: None, hint_key: None }, Binding {
            mode: BindingModeId::Help,
            key: "q",
            action: Action::Dismiss,
            hint: None,
            hint_key: None,
        }];
        let compiled = CompiledBindings::from_table(table);
        // Composed: [Normal, Help] — Help is later, so it wins.
        let mode = KeyBindingMode::Composed(vec![BindingModeId::Normal, BindingModeId::Help]);
        let q = parse_key_string("q");
        assert_eq!(compiled.resolve(&mode, q), Some(Action::Dismiss));
    }

    #[test]
    fn compiled_bindings_shared_fallback() {
        let compiled = CompiledBindings::from_table(BINDINGS);
        // Help mode has no 'j' binding, so it should fall back to Shared.
        let mode = KeyBindingMode::Single(BindingModeId::Help);
        let j = parse_key_string("j");
        assert_eq!(compiled.resolve(&mode, j), Some(Action::SelectNext));
    }

    #[test]
    fn hints_for_single_mode_includes_shared() {
        // Create a table with a shared hint and a mode hint.
        let table =
            &[Binding { mode: BindingModeId::Shared, key: "?", action: Action::ToggleHelp, hint: Some("Help"), hint_key: None }, Binding {
                mode: BindingModeId::Normal,
                key: "q",
                action: Action::Quit,
                hint: Some("Quit"),
                hint_key: None,
            }];
        let compiled = CompiledBindings::from_table(table);
        let mode = KeyBindingMode::Single(BindingModeId::Normal);
        let hints = compiled.hints_for(&mode);
        let keys: Vec<&str> = hints.iter().map(|h| h.key.as_str()).collect();
        assert!(keys.contains(&"?"), "should include shared hint '?'");
        assert!(keys.contains(&"q"), "should include mode hint 'q'");
    }

    #[test]
    fn hints_for_composed_mode_overrides_by_key() {
        // Two modes both hint 'q' with different labels — later wins.
        let table =
            &[Binding { mode: BindingModeId::Normal, key: "q", action: Action::Quit, hint: Some("Quit"), hint_key: None }, Binding {
                mode: BindingModeId::Help,
                key: "q",
                action: Action::Dismiss,
                hint: Some("Close"),
                hint_key: None,
            }];
        let compiled = CompiledBindings::from_table(table);
        let mode = KeyBindingMode::Composed(vec![BindingModeId::Normal, BindingModeId::Help]);
        let hints = compiled.hints_for(&mode);
        // Help is later, so its label should win.
        let q_hint = hints.iter().find(|h| h.key == "q").expect("should have hint for 'q'");
        assert_eq!(q_hint.label, "Close");
    }

    #[test]
    fn from_table_parses_all_keys_without_panic() {
        // Just call from_table on the BINDINGS constant and verify it doesn't panic.
        let compiled = CompiledBindings::from_table(BINDINGS);
        assert!(compiled.key_map.contains_key(&BindingModeId::Normal));
    }

    #[test]
    fn parse_key_string_ctrl_modifier() {
        let combo = parse_key_string("C-q");
        let (display, code, modifiers) = display_for_combo(&combo);
        assert_eq!(code, KeyCode::Char('q'));
        assert!(modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(display, "C-Q");
    }

    #[test]
    fn parse_key_string_alt_modifier() {
        let combo = parse_key_string("A-x");
        let (display, code, modifiers) = display_for_combo(&combo);
        assert_eq!(code, KeyCode::Char('x'));
        assert!(modifiers.contains(KeyModifiers::ALT));
        assert_eq!(display, "A-X");
    }

    #[test]
    fn parse_key_string_combined_modifiers() {
        let combo = parse_key_string("C-S-k");
        let (display, code, modifiers) = display_for_combo(&combo);
        // crokey normalizes Shift+char to uppercase KeyCode
        assert_eq!(code, KeyCode::Char('K'));
        assert!(modifiers.contains(KeyModifiers::CONTROL));
        assert!(modifiers.contains(KeyModifiers::SHIFT));
        assert_eq!(display, "C-S-K");
    }

    #[test]
    fn hint_chip_uses_combo_for_ctrl_binding() {
        let table = &[Binding { mode: BindingModeId::Normal, key: "C-q", action: Action::Quit, hint: Some("Quit"), hint_key: None }];
        let compiled = CompiledBindings::from_table(table);
        let hints = compiled.hints_for(&KeyBindingMode::Single(BindingModeId::Normal));
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].key, "C-Q");
        assert_eq!(hints[0].action, StatusBarAction::combo(KeyCode::Char('q'), KeyModifiers::CONTROL));
    }

    #[test]
    fn no_shared_fallback_skips_shared_bindings() {
        let table = &[
            b(BindingModeId::Shared, "j", Action::SelectNext),
            b(BindingModeId::Shared, "esc", Action::Dismiss),
            b(BindingModeId::CommandPalette, "esc", Action::Dismiss),
            b(BindingModeId::CommandPalette, "enter", Action::Confirm),
        ];
        let compiled = CompiledBindings::from_table_with_no_shared_fallback(table, &[BindingModeId::CommandPalette]);
        let mode = KeyBindingMode::Single(BindingModeId::CommandPalette);
        // esc resolves from mode-specific binding
        assert_eq!(compiled.resolve(&mode, parse_key_string("esc")), Some(Action::Dismiss));
        // j does NOT resolve — shared fallback is suppressed
        assert_eq!(compiled.resolve(&mode, parse_key_string("j")), None);
    }

    #[test]
    fn hints_for_no_shared_fallback_excludes_shared_hints() {
        let table =
            &[Binding { mode: BindingModeId::Shared, key: "?", action: Action::ToggleHelp, hint: Some("Help"), hint_key: None }, Binding {
                mode: BindingModeId::CommandPalette,
                key: "esc",
                action: Action::Dismiss,
                hint: Some("Close"),
                hint_key: None,
            }];
        let compiled = CompiledBindings::from_table_with_no_shared_fallback(table, &[BindingModeId::CommandPalette]);
        let hints = compiled.hints_for(&KeyBindingMode::Single(BindingModeId::CommandPalette));
        let keys: Vec<&str> = hints.iter().map(|h| h.key.as_str()).collect();
        assert!(!keys.contains(&"?"), "should NOT include shared hint '?' for no-fallback mode");
        assert!(keys.contains(&"ESC"), "should include mode-specific hint");
    }
}
