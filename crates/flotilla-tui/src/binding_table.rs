// Binding table: flat declarative key binding definitions with hint annotations.

use std::collections::HashMap;

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
#[derive(Debug, Clone)]
pub enum KeyBindingMode {
    Single(BindingModeId),
    Composed(Vec<BindingModeId>),
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
    Progress(String),
}

/// A single entry in the binding table.
pub struct Binding {
    pub mode: BindingModeId,
    pub key: &'static str,
    pub action: Action,
    pub hint: Option<&'static str>,
}

// ── Binding table helpers ────────────────────────────────────────────

/// Create a binding without a hint.
const fn b(mode: BindingModeId, key: &'static str, action: Action) -> Binding {
    Binding { mode, key, action, hint: None }
}

/// Create a binding with a hint annotation for the status bar.
const fn h(mode: BindingModeId, key: &'static str, action: Action, hint: &'static str) -> Binding {
    Binding { mode, key, action, hint: Some(hint) }
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
    h(BindingModeId::Normal, ".", Action::OpenActionMenu, "Menu"),
    h(BindingModeId::Normal, "n", Action::OpenBranchInput, "New"),
    b(BindingModeId::Normal, "/", Action::OpenCommandPalette),
    b(BindingModeId::Normal, "a", Action::OpenFilePicker),
    b(BindingModeId::Normal, "c", Action::ToggleProviders),
    b(BindingModeId::Normal, "S-D", Action::ToggleDebug),
    b(BindingModeId::Normal, "d", Action::Dispatch(Intent::RemoveCheckout)),
    b(BindingModeId::Normal, "p", Action::Dispatch(Intent::OpenChangeRequest)),
    // ── Overview (replaces old Config) ──
    h(BindingModeId::Overview, "q", Action::Dismiss, "Quit"),
    b(BindingModeId::Overview, "[", Action::PrevTab),
    b(BindingModeId::Overview, "]", Action::NextTab),
    // ── Help ──
    b(BindingModeId::Help, "q", Action::Dismiss),
    // ── ActionMenu ──
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
    h(BindingModeId::BranchInput, "enter", Action::Confirm, "Create"),
    h(BindingModeId::BranchInput, "esc", Action::Dismiss, "Cancel"),
    // ── IssueSearch ──
    h(BindingModeId::IssueSearch, "enter", Action::Confirm, "Apply"),
    h(BindingModeId::IssueSearch, "esc", Action::Dismiss, "Cancel"),
    // ── CommandPalette ──
    // Keys are hardcoded in handle_key, but add hints for the status bar.
    // (No action bindings here — the hints are advisory only.)
    // ── FilePicker ──
    h(BindingModeId::FilePicker, "j", Action::SelectNext, "Down"),
    h(BindingModeId::FilePicker, "k", Action::SelectPrev, "Up"),
    h(BindingModeId::FilePicker, "enter", Action::Confirm, "Select"),
    h(BindingModeId::FilePicker, "esc", Action::Dismiss, "Cancel"),
    // ── SearchActive ──
    h(BindingModeId::SearchActive, "esc", Action::Dismiss, "Clear"),
];

// ── Compiled bindings ────────────────────────────────────────────────

pub struct CompiledBindings {
    pub key_map: HashMap<BindingModeId, HashMap<KeyCombination, Action>>,
    pub hints: HashMap<BindingModeId, Vec<KeyChip>>,
}

impl CompiledBindings {
    /// Parse key strings and build both the key map and hint maps.
    pub fn from_table(bindings: &[Binding]) -> Self {
        let mut key_map: HashMap<BindingModeId, HashMap<KeyCombination, Action>> = HashMap::new();
        let mut hints: HashMap<BindingModeId, Vec<KeyChip>> = HashMap::new();

        for binding in bindings {
            let combo = parse_key_string(binding.key);
            key_map.entry(binding.mode).or_default().insert(combo, binding.action);

            if let Some(hint_label) = binding.hint {
                let (code, modifiers) = key_code_for_hint(binding.key);
                let action = if modifiers == KeyModifiers::NONE { StatusBarAction::key(code) } else { StatusBarAction::shifted(code) };
                let chip = KeyChip::new(binding.key, hint_label, action);
                hints.entry(binding.mode).or_default().push(chip);
            }
        }

        CompiledBindings { key_map, hints }
    }

    /// Resolve a key combination against the given binding mode.
    ///
    /// For `Single`: check the mode first, then fall back to `Shared`.
    /// For `Composed`: check modes in reverse order (later wins), then `Shared`.
    pub fn resolve(&self, mode: &KeyBindingMode, key: KeyCombination) -> Option<Action> {
        match mode {
            KeyBindingMode::Single(id) => self
                .key_map
                .get(id)
                .and_then(|m| m.get(&key).copied())
                .or_else(|| self.key_map.get(&BindingModeId::Shared).and_then(|m| m.get(&key).copied())),
            KeyBindingMode::Composed(ids) => {
                // Check in reverse order so later modes win.
                for id in ids.iter().rev() {
                    if let Some(action) = self.key_map.get(id).and_then(|m| m.get(&key).copied()) {
                        return Some(action);
                    }
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
    pub fn hints_for(&self, mode: &KeyBindingMode) -> Vec<KeyChip> {
        match mode {
            KeyBindingMode::Single(id) => {
                let mut by_key: HashMap<String, KeyChip> = HashMap::new();
                // Start with Shared hints.
                if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                    for chip in shared_hints {
                        by_key.insert(chip.key.clone(), chip.clone());
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
                if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                    for chip in shared_hints {
                        if let Some(c) = by_key.remove(&chip.key) {
                            result.push(c);
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
                let mut by_key: HashMap<String, KeyChip> = HashMap::new();
                // Start with Shared hints.
                if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                    for chip in shared_hints {
                        by_key.insert(chip.key.clone(), chip.clone());
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
                // Preserve order: shared, then each mode layer.
                let mut result = Vec::new();
                let mut seen = std::collections::HashSet::new();
                if let Some(shared_hints) = self.hints.get(&BindingModeId::Shared) {
                    for chip in shared_hints {
                        if seen.insert(chip.key.clone()) {
                            if let Some(c) = by_key.get(&chip.key) {
                                result.push(c.clone());
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

/// Parse a key string like "j", "esc", "S-K", "[", "space" into a `KeyCombination`.
fn parse_key_string(s: &str) -> KeyCombination {
    // Handle shifted keys (S-X pattern).
    if let Some(rest) = s.strip_prefix("S-") {
        if rest.len() == 1 {
            let ch = rest.chars().next().expect("non-empty rest after S- prefix");
            return KeyCombination::from(crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::SHIFT));
        }
    }

    // Named keys.
    let (code, modifiers) = match s {
        "enter" => (KeyCode::Enter, KeyModifiers::NONE),
        "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "space" => (KeyCode::Char(' '), KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "up" => (KeyCode::Up, KeyModifiers::NONE),
        "down" => (KeyCode::Down, KeyModifiers::NONE),
        "left" => (KeyCode::Left, KeyModifiers::NONE),
        "right" => (KeyCode::Right, KeyModifiers::NONE),
        _ => {
            // Single character keys.
            if s.len() == 1 {
                let ch = s.chars().next().expect("non-empty single-char key string");
                (KeyCode::Char(ch), KeyModifiers::NONE)
            } else {
                panic!("unknown key string: {s}");
            }
        }
    };

    KeyCombination::from(crossterm::event::KeyEvent::new(code, modifiers))
}

/// Extract `KeyCode` and `KeyModifiers` for hint chip construction.
fn key_code_for_hint(s: &str) -> (KeyCode, KeyModifiers) {
    if let Some(rest) = s.strip_prefix("S-") {
        if rest.len() == 1 {
            let ch = rest.chars().next().expect("non-empty rest after S- prefix");
            return (KeyCode::Char(ch), KeyModifiers::SHIFT);
        }
    }

    match s {
        "enter" => (KeyCode::Enter, KeyModifiers::NONE),
        "esc" => (KeyCode::Esc, KeyModifiers::NONE),
        "space" => (KeyCode::Char(' '), KeyModifiers::NONE),
        "tab" => (KeyCode::Tab, KeyModifiers::NONE),
        "up" => (KeyCode::Up, KeyModifiers::NONE),
        "down" => (KeyCode::Down, KeyModifiers::NONE),
        "left" => (KeyCode::Left, KeyModifiers::NONE),
        "right" => (KeyCode::Right, KeyModifiers::NONE),
        _ => {
            if s.len() == 1 {
                let ch = s.chars().next().expect("non-empty single-char key string");
                (KeyCode::Char(ch), KeyModifiers::NONE)
            } else {
                panic!("unknown key string for hint: {s}");
            }
        }
    }
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
        let table = &[Binding { mode: BindingModeId::Normal, key: "q", action: Action::Quit, hint: None }, Binding {
            mode: BindingModeId::Help,
            key: "q",
            action: Action::Dismiss,
            hint: None,
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
        let table = &[Binding { mode: BindingModeId::Shared, key: "?", action: Action::ToggleHelp, hint: Some("Help") }, Binding {
            mode: BindingModeId::Normal,
            key: "q",
            action: Action::Quit,
            hint: Some("Quit"),
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
        let table = &[Binding { mode: BindingModeId::Normal, key: "q", action: Action::Quit, hint: Some("Quit") }, Binding {
            mode: BindingModeId::Help,
            key: "q",
            action: Action::Dismiss,
            hint: Some("Close"),
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
}
