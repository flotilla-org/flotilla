//! The open-view set: the TUI's tab truth (ADR 0013).
//!
//! Tabs are containers for open Views. This module owns the ordered set, the
//! active index, pinning policy, and the mapping to/from the persisted
//! `open-views.toml` entries. It knows nothing about rendering or data.

use flotilla_core::config::OpenViewEntry;
use flotilla_protocol::{RepoIdentity, ViewAddress};

/// What an open tab points at: a parsed View address, or the raw entry that
/// failed to parse. A broken entry renders an error view in place — it never
/// invalidates the rest of the tab set (ADR 0013 loud-failure rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewTarget {
    View(ViewAddress),
    Broken { raw: String, error: String },
}

/// One open View in this TUI instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenView {
    pub target: ViewTarget,
    /// User-set label. Display-only; never part of view identity.
    pub label_override: Option<String>,
}

impl OpenView {
    fn parse(entry: OpenViewEntry) -> Self {
        let target = match entry.address.parse::<ViewAddress>() {
            Ok(address) => ViewTarget::View(address),
            Err(error) => ViewTarget::Broken { raw: entry.address, error },
        };
        Self { target, label_override: entry.label }
    }

    fn of(address: ViewAddress) -> Self {
        Self { target: ViewTarget::View(address), label_override: None }
    }

    pub fn address(&self) -> Option<&ViewAddress> {
        match &self.target {
            ViewTarget::View(address) => Some(address),
            ViewTarget::Broken { .. } => None,
        }
    }

    /// The string persisted for this view — broken entries keep their raw
    /// address so a newer binary (or a fixed typo) can still resolve them.
    fn persisted_address(&self) -> String {
        match &self.target {
            ViewTarget::View(address) => address.to_string(),
            ViewTarget::Broken { raw, .. } => raw.clone(),
        }
    }

    fn is_pinned(&self) -> bool {
        matches!(self.target, ViewTarget::View(ViewAddress::Overview))
    }
}

/// The ordered set of open Views and the active tab. Index 0 is always the
/// pinned overview; it cannot be closed or displaced.
pub struct OpenViews {
    views: Vec<OpenView>,
    active: usize,
    /// The previously active index, for "dismiss overview" style returns.
    last_active: Option<usize>,
}

impl OpenViews {
    /// Build from persisted entries, restoring the pinned-overview invariant
    /// if the file was hand-edited out of shape.
    pub fn from_entries(entries: Vec<OpenViewEntry>) -> Self {
        let mut views: Vec<OpenView> = entries.into_iter().map(OpenView::parse).collect();
        match views.iter().position(OpenView::is_pinned) {
            Some(0) => {}
            Some(idx) => {
                let overview = views.remove(idx);
                views.insert(0, overview);
            }
            None => views.insert(0, OpenView::of(ViewAddress::Overview)),
        }
        // Duplicate addresses collapse to the first occurrence (identity is
        // the address; open-or-focus means duplicates are unrepresentable).
        let mut seen = Vec::new();
        views.retain(|view| match view.address() {
            Some(address) => {
                if seen.contains(address) {
                    false
                } else {
                    seen.push(address.clone());
                    true
                }
            }
            None => true,
        });
        Self { views, active: 0, last_active: None }
    }

    /// The default set for a config with no `open-views.toml`: overview,
    /// the flotilla convoys view, and one repo view per registered repo —
    /// matching what the pre-View TUI always showed.
    pub fn seed(repos: impl IntoIterator<Item = RepoIdentity>) -> Self {
        let mut entries = vec![OpenViewEntry { address: ViewAddress::Overview.to_string(), label: None }, OpenViewEntry {
            address: ViewAddress::Convoys { namespace: "flotilla".to_string() }.to_string(),
            label: None,
        }];
        entries.extend(repos.into_iter().map(|identity| OpenViewEntry { address: ViewAddress::Repo(identity).to_string(), label: None }));
        let mut views = Self::from_entries(entries);
        // Land on the first repo tab when there is one, like the old TUI did.
        views.active = if views.views.len() > 2 { 2 } else { views.views.len() - 1 };
        views
    }

    /// A single-View set for scoped mode (`flotilla view <address>`): no
    /// pinned overview, no tab shell. The lone view sits at index 0, which
    /// the pinned-index guards conveniently make unmovable and unclosable.
    pub fn scoped(address: ViewAddress) -> Self {
        Self { views: vec![OpenView::of(address)], active: 0, last_active: None }
    }

    /// Replace the whole set with one View (scoped-mode in-place navigation).
    pub fn replace_with(&mut self, address: ViewAddress) {
        self.views = vec![OpenView::of(address)];
        self.active = 0;
        self.last_active = None;
    }

    pub fn to_entries(&self) -> Vec<OpenViewEntry> {
        self.views.iter().map(|view| OpenViewEntry { address: view.persisted_address(), label: view.label_override.clone() }).collect()
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &OpenView> {
        self.views.iter()
    }

    pub fn get(&self, idx: usize) -> Option<&OpenView> {
        self.views.get(idx)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut OpenView> {
        self.views.get_mut(idx)
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> &OpenView {
        &self.views[self.active]
    }

    /// The active View's address, if the active tab isn't broken.
    pub fn active_address(&self) -> Option<&ViewAddress> {
        self.active().address()
    }

    /// The repo identity of the active tab, when it is a repo view.
    pub fn active_repo_identity(&self) -> Option<&RepoIdentity> {
        match self.active_address() {
            Some(ViewAddress::Repo(identity)) => Some(identity),
            _ => None,
        }
    }

    pub fn find(&self, address: &ViewAddress) -> Option<usize> {
        self.views.iter().position(|view| view.address() == Some(address))
    }

    pub fn switch_to(&mut self, idx: usize) -> bool {
        if idx < self.views.len() && idx != self.active {
            self.last_active = Some(self.active);
            self.active = idx;
            true
        } else {
            false
        }
    }

    /// Return to the previously active tab (e.g. dismissing the overview).
    pub fn switch_to_last(&mut self) -> bool {
        match self.last_active {
            Some(idx) if idx < self.views.len() => self.switch_to(idx),
            _ => false,
        }
    }

    /// Cycle forward/backward through the tabs, wrapping.
    pub fn step(&mut self, delta: isize) {
        let len = self.views.len() as isize;
        let next = (self.active as isize + delta).rem_euclid(len) as usize;
        self.switch_to(next);
    }

    /// Focus the View at `address`, opening a new tab after the active one if
    /// it isn't open. Returns true if a tab was newly opened.
    pub fn open_or_focus(&mut self, address: ViewAddress) -> bool {
        if let Some(idx) = self.find(&address) {
            self.switch_to(idx);
            return false;
        }
        let idx = (self.active + 1).min(self.views.len());
        // Never in front of the pinned overview.
        let idx = idx.max(1);
        self.views.insert(idx, OpenView::of(address));
        self.last_active = Some(self.active);
        self.active = idx;
        true
    }

    /// Move the tab at `idx` by `delta`. The pinned overview neither moves
    /// nor is displaced. Returns the new index if a swap happened.
    pub fn move_tab(&mut self, idx: usize, delta: isize) -> Option<usize> {
        let len = self.views.len() as isize;
        let target = idx as isize + delta;
        if idx == 0 || target < 1 || target >= len {
            return None;
        }
        let target = target as usize;
        self.views.swap(idx, target);
        if self.active == idx {
            self.active = target;
        } else if self.active == target {
            self.active = idx;
        }
        self.last_active = None;
        Some(target)
    }

    /// Swap two tabs (drag reorder). Pinned overview never participates.
    pub fn swap(&mut self, a: usize, b: usize) -> bool {
        if a == 0 || b == 0 || a >= self.views.len() || b >= self.views.len() || a == b {
            return false;
        }
        self.views.swap(a, b);
        if self.active == a {
            self.active = b;
        } else if self.active == b {
            self.active = a;
        }
        self.last_active = None;
        true
    }

    /// Close the tab at `idx`. The pinned overview is not closable. When the
    /// active tab closes, focus moves to the tab on its left. Returns true
    /// if a tab was closed.
    pub fn close(&mut self, idx: usize) -> bool {
        if idx == 0 || idx >= self.views.len() {
            return false;
        }
        self.views.remove(idx);
        self.last_active = None;
        if self.active >= idx {
            self.active -= 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> ViewAddress {
        s.parse().expect("valid address")
    }

    fn entry(address: &str) -> OpenViewEntry {
        OpenViewEntry { address: address.to_string(), label: None }
    }

    fn three_tabs() -> OpenViews {
        OpenViews::from_entries(vec![entry("overview"), entry("convoys/flotilla"), entry("repo/github.com/o/r")])
    }

    #[test]
    fn missing_overview_is_restored_at_front() {
        let views = OpenViews::from_entries(vec![entry("convoys/flotilla")]);
        assert_eq!(views.get(0).and_then(OpenView::address), Some(&ViewAddress::Overview));
        assert_eq!(views.len(), 2);
    }

    #[test]
    fn misplaced_overview_moves_to_front() {
        let views = OpenViews::from_entries(vec![entry("convoys/flotilla"), entry("overview")]);
        assert_eq!(views.get(0).and_then(OpenView::address), Some(&ViewAddress::Overview));
        assert_eq!(views.len(), 2);
    }

    #[test]
    fn duplicate_addresses_collapse() {
        let views = OpenViews::from_entries(vec![entry("overview"), entry("convoys/flotilla"), entry("convoys/flotilla")]);
        assert_eq!(views.len(), 2);
    }

    #[test]
    fn broken_addresses_survive_as_broken_tabs_and_round_trip() {
        // "workflow" is a plausible FUTURE kind — a newer config must degrade
        // to a broken tab here, and the raw address must survive re-saving.
        let views = OpenViews::from_entries(vec![entry("overview"), entry("workflow/ns/w"), entry("garbage")]);
        assert_eq!(views.len(), 3);
        assert!(matches!(&views.get(1).expect("tab").target, ViewTarget::Broken { raw, .. } if raw == "workflow/ns/w"));
        let entries = views.to_entries();
        assert_eq!(entries[1].address, "workflow/ns/w");
        assert_eq!(entries[2].address, "garbage");
    }

    #[test]
    fn seed_matches_the_pre_view_tab_bar() {
        let repo = RepoIdentity { authority: "github.com".to_string(), path: "o/r".to_string() };
        let views = OpenViews::seed(vec![repo]);
        let addresses: Vec<String> = views.iter().map(|view| view.address().expect("parsed").to_string()).collect();
        assert_eq!(addresses, vec!["overview", "convoys/flotilla", "repo/github.com/o/r"]);
        assert_eq!(views.active_index(), 2, "seed lands on the first repo tab");
    }

    #[test]
    fn step_wraps_both_ways() {
        let mut views = three_tabs();
        views.step(-1);
        assert_eq!(views.active_index(), 2);
        views.step(1);
        assert_eq!(views.active_index(), 0);
        views.step(1);
        assert_eq!(views.active_index(), 1);
    }

    #[test]
    fn open_or_focus_focuses_existing() {
        let mut views = three_tabs();
        assert!(!views.open_or_focus(addr("convoys/flotilla")));
        assert_eq!(views.active_index(), 1);
        assert_eq!(views.len(), 3);
    }

    #[test]
    fn open_or_focus_inserts_after_active_never_before_overview() {
        let mut views = three_tabs();
        assert!(views.open_or_focus(addr("repo/github.com/o/other")));
        assert_eq!(views.active_index(), 1, "inserted after the pinned overview when it was active");
        assert_eq!(views.len(), 4);
    }

    #[test]
    fn move_tab_guards_the_pinned_overview() {
        let mut views = three_tabs();
        assert_eq!(views.move_tab(0, 1), None, "pinned tab does not move");
        assert_eq!(views.move_tab(1, -1), None, "nothing displaces the pinned tab");
        views.switch_to(1);
        assert_eq!(views.move_tab(1, 1), Some(2));
        assert_eq!(views.active_index(), 2, "active follows its tab");
    }

    #[test]
    fn close_guards_pinned_and_moves_focus_left() {
        let mut views = three_tabs();
        assert!(!views.close(0), "overview is not closable");
        views.switch_to(2);
        assert!(views.close(2));
        assert_eq!(views.active_index(), 1);
        assert_eq!(views.len(), 2);
        assert!(!views.close(5));
    }

    #[test]
    fn close_before_active_shifts_active_index() {
        let mut views = three_tabs();
        views.switch_to(2);
        assert!(views.close(1));
        assert_eq!(views.active_index(), 1);
        assert_eq!(views.active_address(), Some(&addr("repo/github.com/o/r")));
    }

    #[test]
    fn switch_to_last_returns_after_switch() {
        let mut views = three_tabs();
        views.switch_to(2);
        views.switch_to(0);
        assert!(views.switch_to_last());
        assert_eq!(views.active_index(), 2);
    }

    #[test]
    fn active_repo_identity_only_for_repo_views() {
        let mut views = three_tabs();
        assert_eq!(views.active_repo_identity(), None);
        views.switch_to(2);
        assert_eq!(views.active_repo_identity().map(|id| id.path.as_str()), Some("o/r"));
    }
}
