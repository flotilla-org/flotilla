//! The open-view set: the TUI's tab truth (ADR 0013).
//!
//! Tabs are containers for open Views. This module owns the ordered set, the
//! active index, pinning policy, and the mapping to/from the persisted
//! `open-views.toml` entries. It knows nothing about rendering or data.

use std::collections::HashMap;

use flotilla_core::config::OpenViewEntry;
use flotilla_protocol::{QueryId, RepoIdentity, RepositoryKey, ViewAddress};
use serde::{Deserialize, Serialize};

use crate::table_view::{AuthoritativeRowUpdate, PendingRowContext, ProjectPanelKind, ProjectTableState, TableState};

/// What an open tab points at: a parsed View address, or the raw entry that
/// failed to parse. A broken entry renders an error view in place — it never
/// invalidates the rest of the tab set (ADR 0013 loud-failure rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewTarget {
    View(ViewAddress),
    Broken { raw: String, error: String },
}

/// One open View in this TUI instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenView {
    pub target: ViewTarget,
    /// User-set label. Display-only; never part of view identity.
    pub label_override: Option<String>,
    pub table_state: TableState,
    pub project_table_state: ProjectTableState,
    /// Addresses left behind by in-place drill navigation in this tab.
    /// History is ephemeral and deliberately not persisted.
    pub(crate) history: Vec<NavigationFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NavigationFrame {
    target: ViewTarget,
    table_state: TableState,
    project_table_state: ProjectTableState,
}

impl OpenView {
    fn parse(entry: OpenViewEntry) -> Self {
        let target = match entry.address.parse::<ViewAddress>() {
            Ok(address) => ViewTarget::View(address),
            Err(error) => ViewTarget::Broken { raw: entry.address, error },
        };
        Self {
            target,
            label_override: entry.label,
            table_state: TableState::default(),
            project_table_state: ProjectTableState::default(),
            history: Vec::new(),
        }
    }

    fn of(address: ViewAddress) -> Self {
        Self {
            target: ViewTarget::View(address),
            label_override: None,
            table_state: TableState::default(),
            project_table_state: ProjectTableState::default(),
            history: Vec::new(),
        }
    }

    pub fn address(&self) -> Option<&ViewAddress> {
        match &self.target {
            ViewTarget::View(address) => Some(address),
            ViewTarget::Broken { .. } => None,
        }
    }

    pub fn source_search(&self) -> Option<&str> {
        match self.address() {
            Some(ViewAddress::Project { .. }) => self.project_table_state.issue_source_search(),
            _ => self.table_state.source_search.as_deref(),
        }
    }

    fn focused_table_state(&self) -> &TableState {
        match self.address() {
            Some(ViewAddress::Project { .. }) => self.project_table_state.active_table(),
            _ => &self.table_state,
        }
    }

    fn focused_table_state_mut(&mut self) -> &mut TableState {
        let is_project = matches!(self.address(), Some(ViewAddress::Project { .. }));
        if is_project {
            self.project_table_state.active_table_mut()
        } else {
            &mut self.table_state
        }
    }

    fn table_state_mut(&mut self, panel: Option<ProjectPanelKind>) -> Option<&mut TableState> {
        match panel {
            Some(panel) if matches!(self.address(), Some(ViewAddress::Project { .. })) => Some(self.project_table_state.table_mut(panel)),
            Some(_) => None,
            None => Some(&mut self.table_state),
        }
    }

    fn visit_table_states_mut(&mut self, visit: &mut impl FnMut(&mut TableState)) {
        visit(&mut self.table_state);
        for panel in [ProjectPanelKind::Convoys, ProjectPanelKind::Checkouts, ProjectPanelKind::Issues, ProjectPanelKind::Independents] {
            visit(self.project_table_state.table_mut(panel));
        }
        for frame in &mut self.history {
            visit(&mut frame.table_state);
            for panel in [ProjectPanelKind::Convoys, ProjectPanelKind::Checkouts, ProjectPanelKind::Issues, ProjectPanelKind::Independents]
            {
                visit(frame.project_table_state.table_mut(panel));
            }
        }
    }

    pub fn breadcrumb_addresses(&self) -> Vec<ViewAddress> {
        self.history
            .iter()
            .filter_map(|frame| match &frame.target {
                ViewTarget::View(address) => Some(address.clone()),
                ViewTarget::Broken { .. } => None,
            })
            .chain(self.address().cloned())
            .collect()
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
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

/// Whether a set of open Views is the tabbed shell or a single scoped pane.
/// This is the pinning policy: the tabbed shell pins the overview at index 0;
/// a scoped pane pins its lone view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabSetMode {
    /// The full tab shell: pinned overview at index 0, user-managed tabs,
    /// persisted to `open-views.toml`.
    Tabbed,
    /// A scoped pane (`flotilla view <address>`): exactly one View; opens
    /// navigate in place with a back stack; nothing is persisted.
    Scoped,
}

/// The ordered set of open Views and the active tab.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenViews {
    views: Vec<OpenView>,
    active: usize,
    /// The previously active index, for "dismiss overview" style returns.
    last_active: Option<usize>,
    mode: TabSetMode,
}

impl OpenViews {
    pub fn begin_pending_row(&mut self, ctx: &PendingRowContext, description: String) -> Result<(), String> {
        let view = self
            .views
            .iter_mut()
            .find(|view| view.address() == Some(&ctx.address))
            .ok_or_else(|| format!("pending row view is no longer open: {}", ctx.address))?;
        let state = view.table_state_mut(ctx.panel).ok_or_else(|| "pending row panel does not match its view".to_string())?;
        state.begin_pending(ctx.query.clone(), ctx.row_id.clone(), description)
    }

    pub fn mark_pending_row(&mut self, ctx: &PendingRowContext, command_id: u64) {
        let Some(view) = self.views.iter_mut().find(|view| view.address() == Some(&ctx.address)) else { return };
        let Some(state) = view.table_state_mut(ctx.panel) else { return };
        state.mark_pending(&ctx.row_id, command_id);
    }

    pub fn mark_pending_row_failed(&mut self, ctx: &PendingRowContext, message: String) {
        let Some(view) = self.views.iter_mut().find(|view| view.address() == Some(&ctx.address)) else { return };
        let Some(state) = view.table_state_mut(ctx.panel) else { return };
        state.mark_failed(&ctx.row_id, message);
    }

    pub fn finish_pending_row_command(&mut self, command_id: u64, error: Option<&str>) {
        for view in &mut self.views {
            view.visit_table_states_mut(&mut |state| state.finish_command(command_id, error));
        }
    }

    pub fn reconcile_authoritative_rows(&mut self, query: &QueryId, update: &AuthoritativeRowUpdate) {
        for view in &mut self.views {
            view.visit_table_states_mut(&mut |state| state.reconcile_authoritative(query, update));
        }
    }

    pub fn has_pending_rows(&self) -> bool {
        self.views.iter().any(|view| {
            view.table_state.has_pending_rows()
                || view.project_table_state.tables().iter().any(|table| table.has_pending_rows())
                || view.history.iter().any(|frame| {
                    frame.table_state.has_pending_rows() || frame.project_table_state.tables().iter().any(|table| table.has_pending_rows())
                })
        })
    }

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
        Self { views, active: 0, last_active: None, mode: TabSetMode::Tabbed }
    }

    /// The default set for a config with no `open-views.toml`: overview,
    /// the flotilla convoys view, and one repo view per registered repo —
    /// matching what the pre-View TUI always showed.
    pub fn seed(repos: impl IntoIterator<Item = RepoIdentity>) -> Self {
        Self::seed_with_keys(repos.into_iter().map(|identity| (identity, None)))
    }

    pub fn seed_with_keys(repos: impl IntoIterator<Item = (RepoIdentity, Option<RepositoryKey>)>) -> Self {
        let mut entries = vec![OpenViewEntry { address: ViewAddress::Overview.to_string(), label: None }, OpenViewEntry {
            address: ViewAddress::Convoys { namespace: "flotilla".to_string() }.to_string(),
            label: None,
        }];
        entries.extend(repos.into_iter().map(|(identity, repository_key)| {
            OpenViewEntry {
                address: match repository_key {
                    Some(key) => ViewAddress::repo_with_key(identity, key),
                    None => ViewAddress::repo(identity),
                }
                .to_string(),
                label: None,
            }
        }));
        let mut views = Self::from_entries(entries);
        // Land on the first repo tab when there is one, like the old TUI did.
        views.active = if views.views.len() > 2 { 2 } else { views.views.len() - 1 };
        views
    }

    pub fn bind_repository_keys(&mut self, keys: &HashMap<RepoIdentity, RepositoryKey>) {
        for view in &mut self.views {
            bind_repository_key(&mut view.target, keys);
            for frame in &mut view.history {
                bind_repository_key(&mut frame.target, keys);
            }
        }
    }

    /// A single-View set for scoped mode (`flotilla view <address>`): no
    /// pinned overview, no tab shell; opens navigate in place.
    pub fn scoped(address: ViewAddress) -> Self {
        Self { views: vec![OpenView::of(address)], active: 0, last_active: None, mode: TabSetMode::Scoped }
    }

    pub fn mode(&self) -> TabSetMode {
        self.mode
    }

    pub fn is_scoped(&self) -> bool {
        self.mode == TabSetMode::Scoped
    }

    /// Whether the tab at `idx` is pinned (unmovable, unclosable): the
    /// overview in tabbed mode; the lone view in scoped mode.
    fn is_pinned_index(&self, idx: usize) -> bool {
        match self.mode {
            TabSetMode::Tabbed => idx == 0,
            TabSetMode::Scoped => true,
        }
    }

    /// Remove another tab currently holding `address` so in-place navigation
    /// preserves ADR 0013's one-open-View-per-address invariant.
    fn remove_other_address(&mut self, address: &ViewAddress) -> bool {
        let Some(index) = self.find(address) else { return true };
        if index == self.active {
            return true;
        }
        if self.is_pinned_index(index) {
            return false;
        }
        self.views.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        self.last_active = None;
        true
    }

    /// Return to the address the active tab's last in-place drill left behind.
    /// Returns true if navigation happened.
    pub fn back(&mut self) -> bool {
        let Some(frame) = self.views.get(self.active).and_then(|view| view.history.last()) else { return false };
        let target_address = match &frame.target {
            ViewTarget::View(address) => Some(address.clone()),
            ViewTarget::Broken { .. } => None,
        };
        if let Some(address) = target_address {
            if !self.remove_other_address(&address) {
                return false;
            }
        }
        let Some(view) = self.views.get_mut(self.active) else { return false };
        let Some(frame) = view.history.pop() else { return false };
        view.target = frame.target;
        view.table_state = frame.table_state;
        view.project_table_state = frame.project_table_state;
        true
    }

    /// Drill into `address` in the active tab, preserving the current target
    /// on that tab's history stack. This never grows or switches the tab set.
    pub fn drill(&mut self, address: ViewAddress) -> bool {
        if self.views.get(self.active).and_then(OpenView::address) == Some(&address) {
            return false;
        }
        if !self.remove_other_address(&address) {
            return false;
        }
        let Some(view) = self.views.get_mut(self.active) else { return false };
        let previous = NavigationFrame {
            target: std::mem::replace(&mut view.target, ViewTarget::View(address)),
            table_state: std::mem::take(&mut view.table_state),
            project_table_state: std::mem::take(&mut view.project_table_state),
        };
        view.history.push(previous);
        true
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

    pub fn active_mut(&mut self) -> &mut OpenView {
        &mut self.views[self.active]
    }

    pub fn active_table_state(&self) -> &TableState {
        self.active().focused_table_state()
    }

    pub fn active_table_state_mut(&mut self) -> &mut TableState {
        self.active_mut().focused_table_state_mut()
    }

    pub fn active_project_table_state(&self) -> &ProjectTableState {
        &self.active().project_table_state
    }

    pub fn active_project_table_state_mut(&mut self) -> &mut ProjectTableState {
        &mut self.active_mut().project_table_state
    }

    pub fn active_source_search(&self) -> Option<&str> {
        self.active().source_search()
    }

    /// The active View's address, if the active tab isn't broken.
    pub fn active_address(&self) -> Option<&ViewAddress> {
        self.active().address()
    }

    /// The repo identity of the active tab, when it is a repo view.
    pub fn active_repo_identity(&self) -> Option<&RepoIdentity> {
        match self.active_address() {
            Some(ViewAddress::Repo { identity, .. }) => Some(identity),
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

    /// Focus the View at `address`. In tabbed mode, opens a new tab after
    /// the active one if it isn't open; in scoped mode, navigates in place,
    /// pushing the previous address onto the back stack. Returns true if
    /// the view set changed.
    pub fn open_or_focus(&mut self, address: ViewAddress) -> bool {
        if self.active_address() == Some(&address) {
            return false;
        }
        if self.is_scoped() {
            return self.drill(address);
        }
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

    /// Move the tab at `idx` by `delta`. A pinned tab neither moves nor is
    /// displaced. Returns the new index if a swap happened.
    pub fn move_tab(&mut self, idx: usize, delta: isize) -> Option<usize> {
        let len = self.views.len() as isize;
        let target = idx as isize + delta;
        if self.is_pinned_index(idx) || target < 1 || target >= len {
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

    /// Swap two tabs (drag reorder). Pinned tabs never participate.
    pub fn swap(&mut self, a: usize, b: usize) -> bool {
        if self.is_pinned_index(a) || self.is_pinned_index(b) || a >= self.views.len() || b >= self.views.len() || a == b {
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

    /// Close the tab at `idx`. Pinned tabs are not closable. When the
    /// active tab closes, focus moves to the tab on its left. Returns true
    /// if a tab was closed.
    pub fn close(&mut self, idx: usize) -> bool {
        if self.is_pinned_index(idx) || idx >= self.views.len() {
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

fn bind_repository_key(target: &mut ViewTarget, keys: &HashMap<RepoIdentity, RepositoryKey>) {
    let ViewTarget::View(ViewAddress::Repo { identity, repository_key }) = target else { return };
    if repository_key.is_none() {
        *repository_key = keys.get(identity).cloned();
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
    fn canonical_query_family_addresses_focus_the_existing_view() {
        let mut views = three_tabs();
        assert!(views.open_or_focus(addr("ISSUES?project=flotilla%2froadmap")));
        let len = views.len();
        assert!(!views.open_or_focus(addr("issues?project=flotilla%2Froadmap")));
        assert_eq!(views.len(), len);
        assert_eq!(views.active_address().map(ToString::to_string).as_deref(), Some("issues?project=flotilla%2Froadmap"));
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

    #[test]
    fn scoped_set_navigates_in_place_with_a_back_stack() {
        let mut views = OpenViews::scoped(addr("convoy/flotilla/manifest"));
        assert!(views.is_scoped());
        assert_eq!(views.len(), 1);

        // In-place open: still one view, previous address remembered.
        assert!(views.open_or_focus(addr("vessel/flotilla/manifest/leg-1")));
        assert_eq!(views.len(), 1);
        assert_eq!(views.active_address(), Some(&addr("vessel/flotilla/manifest/leg-1")));

        // Re-opening the current address is a no-op (no self-push).
        assert!(!views.open_or_focus(addr("vessel/flotilla/manifest/leg-1")));

        // Back pops the history; a second back has nowhere to go.
        assert!(views.back());
        assert_eq!(views.active_address(), Some(&addr("convoy/flotilla/manifest")));
        assert!(!views.back());
    }

    #[test]
    fn tabbed_drill_mutates_only_the_active_tab_and_back_restores_it() {
        let mut views = three_tabs();
        views.switch_to(1);

        assert!(views.drill(addr("convoy/flotilla/manifest")));
        assert_eq!(views.len(), 3, "drill must not grow the tab set");
        assert_eq!(views.active_address(), Some(&addr("convoy/flotilla/manifest")));

        views.switch_to(2);
        assert_eq!(views.active_address(), Some(&addr("repo/github.com/o/r")), "another tab keeps its own current address");
        assert!(!views.back(), "another tab has no history of the first tab's drill");

        views.switch_to(1);
        assert!(views.back());
        assert_eq!(views.active_address(), Some(&addr("convoys/flotilla")));
    }

    #[test]
    fn repository_key_binding_updates_a_repo_address_in_navigation_history() {
        let identity = RepoIdentity { authority: "github.com".into(), path: "o/r".into() };
        let key = RepositoryKey("repo_history".into());
        let mut views = OpenViews::scoped(ViewAddress::repo(identity.clone()));
        assert!(views.drill(addr("convoy/flotilla/manifest")));

        views.bind_repository_keys(&HashMap::from([(identity.clone(), key.clone())]));
        assert!(views.back());

        assert_eq!(views.active_address(), Some(&ViewAddress::repo_with_key(identity, key)));
    }

    #[test]
    fn back_restores_the_table_state_owned_by_the_previous_view() {
        let mut views = three_tabs();
        views.switch_to(1);
        views.active_table_state_mut().filter = "failed".into();

        views.drill(addr("convoy/flotilla/manifest"));
        assert!(views.active_table_state().filter.is_empty(), "a drilled view starts with independent state");
        views.active_table_state_mut().filter = "review".into();

        assert!(views.back());
        assert_eq!(views.active_table_state().filter, "failed");
    }

    #[test]
    fn drill_removes_an_existing_target_tab_before_mutating_the_active_tab() {
        let mut views = OpenViews::from_entries(vec![
            entry("overview"),
            entry("convoys/flotilla"),
            entry("convoy/flotilla/manifest"),
            entry("repo/github.com/o/r"),
        ]);
        views.switch_to(1);

        assert!(views.drill(addr("convoy/flotilla/manifest")));
        assert_eq!(views.active_index(), 1);
        assert_eq!(views.active_address(), Some(&addr("convoy/flotilla/manifest")));
        assert_eq!(views.iter().filter(|view| view.address() == Some(&addr("convoy/flotilla/manifest"))).count(), 1);
        assert_eq!(views.len(), 3);
    }

    #[test]
    fn back_removes_a_new_tab_that_collides_with_the_history_target() {
        let mut views = three_tabs();
        views.switch_to(1);
        assert!(views.drill(addr("convoy/flotilla/manifest")));

        assert!(views.open_or_focus(addr("convoys/flotilla")));
        assert_eq!(views.len(), 4);
        views.switch_to(1);

        assert!(views.back());
        assert_eq!(views.active_index(), 1);
        assert_eq!(views.active_address(), Some(&addr("convoys/flotilla")));
        assert_eq!(views.iter().filter(|view| view.address() == Some(&addr("convoys/flotilla"))).count(), 1);
        assert_eq!(views.len(), 3);
    }

    #[test]
    fn scoped_lone_view_is_pinned() {
        let mut views = OpenViews::scoped(addr("convoys/flotilla"));
        assert!(!views.close(0), "scoped view is not closable");
        assert_eq!(views.move_tab(0, 1), None, "scoped view does not move");
        assert!(!views.swap(0, 0));
        assert_eq!(views.len(), 1);
    }
}
