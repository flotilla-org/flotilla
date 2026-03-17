use crate::keymap::Action;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub key_hint: Option<&'static str>,
    pub action: Action,
}

pub fn all_entries() -> Vec<PaletteEntry> {
    vec![
        PaletteEntry { name: "search", description: "filter items in view", key_hint: Some("/"), action: Action::OpenIssueSearch },
        PaletteEntry { name: "refresh", description: "refresh active repo", key_hint: Some("r"), action: Action::Refresh },
        PaletteEntry { name: "branch", description: "create a new branch", key_hint: Some("n"), action: Action::OpenBranchInput },
        PaletteEntry { name: "help", description: "show key bindings", key_hint: Some("?"), action: Action::ToggleHelp },
        PaletteEntry { name: "quit", description: "exit flotilla", key_hint: Some("q"), action: Action::Quit },
        PaletteEntry { name: "layout", description: "cycle view layout", key_hint: Some("l"), action: Action::CycleLayout },
        PaletteEntry { name: "host", description: "cycle target host", key_hint: Some("h"), action: Action::CycleHost },
        PaletteEntry { name: "theme", description: "cycle color theme", key_hint: None, action: Action::CycleTheme },
        PaletteEntry { name: "providers", description: "show provider health", key_hint: None, action: Action::ToggleProviders },
        PaletteEntry { name: "debug", description: "show debug panel", key_hint: None, action: Action::ToggleDebug },
        PaletteEntry { name: "actions", description: "open context menu", key_hint: Some("."), action: Action::OpenActionMenu },
        PaletteEntry { name: "add repo", description: "track a repository", key_hint: None, action: Action::OpenFilePicker },
        PaletteEntry { name: "select", description: "toggle multi-select", key_hint: Some("space"), action: Action::ToggleMultiSelect },
        PaletteEntry { name: "keys", description: "toggle key hints", key_hint: Some("K"), action: Action::ToggleStatusBarKeys },
    ]
}

pub fn filter_entries<'a>(entries: &'a [PaletteEntry], prefix: &str) -> Vec<&'a PaletteEntry> {
    if prefix.is_empty() {
        return entries.iter().collect();
    }
    let lower = prefix.to_lowercase();
    entries.iter().filter(|e| e.name.to_lowercase().starts_with(&lower)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_entries_returns_expected_count() {
        let entries = all_entries();
        assert_eq!(entries.len(), 14);
        assert_eq!(entries[0].name, "search");
        assert_eq!(entries[entries.len() - 1].name, "keys");
    }

    #[test]
    fn filter_by_prefix() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "re");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "refresh");
    }

    #[test]
    fn filter_empty_returns_all() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "");
        assert_eq!(filtered.len(), entries.len());
    }

    #[test]
    fn filter_case_insensitive() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "HELP");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let entries = all_entries();
        let filtered = filter_entries(&entries, "zzz");
        assert!(filtered.is_empty());
    }
}
