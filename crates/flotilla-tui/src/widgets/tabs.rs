use std::collections::BTreeMap;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::ViewAddress;
use ratatui::{layout::Rect, style::Style, Frame};

use crate::{
    app::{ui_state::DragState, OpenView, OpenViews, TabId, TuiModel, UiState, ViewTarget},
    segment_bar::{self, BarStyle, ThemedRibbonStyle, ThemedTabBarStyle},
    theme::{BarKind, Theme},
    widgets::AppAction,
};

/// Action returned from a tab bar click. The caller interprets the action
/// and mutates `App` state accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarAction {
    /// Switch to the open View at this tab index.
    SwitchTo(usize),
    /// Close the open View at this tab index (its `×` was clicked).
    Close(usize),
    /// Open the file picker to add a new repo.
    OpenFilePicker,
    /// No recognized tab was hit. The caller should continue with
    /// normal mouse handling.
    None,
}

/// Trailing close affordance shown on the active (closable) tab.
const CLOSE_AFFORDANCE: &str = "×";

/// Tab bar strip component. Handles rendering, hit-testing, and drag-reorder
/// over the open-view set. Owned by `Screen`. Tab *semantics* (switch, move,
/// close, pinning) live on `OpenViews`.
#[derive(Default)]
pub struct Tabs {
    /// Click target areas populated during render.
    tab_areas: BTreeMap<TabId, Rect>,
    /// Close-affordance hit area for the active tab, when it is closable.
    close_area: Option<(usize, Rect)>,
    /// Tab drag-reorder state.
    pub drag: DragState,
    /// Whether a drag is visually active.
    drag_active: bool,
}

/// Repo tab decorations (unseen-changes and loading markers), and the
/// dangling-repo fallback name.
fn repo_label(identity: &flotilla_protocol::RepoIdentity, model: &TuiModel, level: usize) -> String {
    match model.repos.get(identity) {
        Some(rm) => {
            let name = if level > 0 { format!("{}/{}", identity.authority, identity.path) } else { TuiModel::repo_name(&rm.path) };
            let loading = if rm.loading { " ⟳" } else { "" };
            let changed = if rm.has_unseen_changes { "*" } else { "" };
            format!("{name}{changed}{loading}")
        }
        // Dangling repo view: the repo is no longer tracked. The tab
        // stays, loudly (ADR 0013).
        None => {
            let name = identity.path.rsplit('/').next().unwrap_or(&identity.path);
            format!("⚠ {name}")
        }
    }
}

/// The default (pre-override) label for an open View's tab at a
/// qualification `level`: 0 is the short form; each higher level widens the
/// label with more qualifying parameters (saturating per kind).
fn default_label(view: &OpenView, model: &TuiModel, level: usize) -> String {
    match &view.target {
        ViewTarget::View(ViewAddress::Overview) => " ⚓ flotilla ".to_string(),
        ViewTarget::View(ViewAddress::Convoys { namespace }) if namespace == "flotilla" && level == 0 => " 🚢 convoys ".to_string(),
        ViewTarget::View(ViewAddress::Convoys { namespace }) => format!(" 🚢 {namespace} "),
        ViewTarget::View(ViewAddress::Convoy { namespace, name }) => match level {
            0 => format!("🚢 {name}"),
            _ => format!("🚢 {namespace}/{name}"),
        },
        ViewTarget::View(ViewAddress::Vessel { namespace, convoy, vessel }) => match level {
            0 => vessel.clone(),
            1 => format!("{convoy}/{vessel}"),
            _ => format!("{namespace}/{convoy}/{vessel}"),
        },
        ViewTarget::View(ViewAddress::Project { namespace, name }) => match level {
            0 => format!("⛰ {name}"),
            _ => format!("⛰ {namespace}/{name}"),
        },
        ViewTarget::View(ViewAddress::Repo(identity)) => repo_label(identity, model, level),
        ViewTarget::Broken { .. } => "⚠ invalid".to_string(),
    }
}

/// The deepest qualification level a view's label can widen to.
fn max_label_level(view: &OpenView) -> usize {
    match &view.target {
        ViewTarget::View(ViewAddress::Vessel { .. }) => 2,
        _ => 1,
    }
}

/// The visible label for an open View's tab: the user override when set,
/// otherwise the kind default (short form).
pub fn tab_label(view: &OpenView, model: &TuiModel) -> String {
    match &view.label_override {
        Some(label) => label.clone(),
        None => default_label(view, model, 0),
    }
}

/// Labels for the whole tab bar: short defaults, progressively widened with
/// qualifying parameters only where two tabs would otherwise read the same
/// (vessel labels widen to convoy/vessel, then namespace/convoy/vessel).
/// User overrides never widen.
fn tab_labels(views: &OpenViews, model: &TuiModel) -> Vec<String> {
    let mut levels = vec![0usize; views.len()];
    let mut labels: Vec<String> = views.iter().map(|view| tab_label(view, model)).collect();
    // Each pass widens every still-colliding label by one level; two passes
    // reach the deepest qualification any kind has.
    for _ in 0..2 {
        let mut changed = false;
        for (i, view) in views.iter().enumerate() {
            let collides = labels.iter().enumerate().any(|(j, other)| j != i && other == &labels[i]);
            if collides && view.label_override.is_none() && levels[i] < max_label_level(view) {
                levels[i] += 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        for (i, view) in views.iter().enumerate() {
            if view.label_override.is_none() {
                labels[i] = default_label(view, model, levels[i]);
            }
        }
    }
    labels
}

impl Tabs {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Rendering ──

    /// Render the tab bar into `area`, populating click targets for later
    /// hit-testing.
    pub fn render(&mut self, views: &OpenViews, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        self.drag_active = self.drag.active;

        let mut items = Vec::new();
        let mut tab_ids = Vec::new();
        let active_idx = views.active_index();
        let labels = tab_labels(views, model);

        for (i, view) in views.iter().enumerate() {
            let is_active = i == active_idx;
            let mut label = labels[i].clone();
            // The active tab carries a mouse close affordance when closable
            // (everything but the pinned overview at index 0).
            if is_active && i != 0 {
                label = format!("{} {CLOSE_AFFORDANCE} ", label.trim_end_matches(' '));
            }
            let style_override = matches!(view.address(), Some(ViewAddress::Overview)).then(|| theme.logo_style(is_active));
            items.push(segment_bar::SegmentItem {
                label,
                key_hint: None,
                active: is_active,
                dragging: is_active && self.drag_active && i != 0,
                style_override,
            });
            tab_ids.push(TabId::View(i));
        }

        // [+] button
        items.push(segment_bar::SegmentItem {
            label: "[+]".to_string(),
            key_hint: None,
            active: false,
            dragging: false,
            style_override: Some(Style::default().fg(theme.status_ok)),
        });
        tab_ids.push(TabId::Add);

        // Render
        let tab_style: Box<dyn BarStyle> = match theme.tab_bar.kind {
            BarKind::Pipe => Box::new(ThemedTabBarStyle { theme, site: &theme.tab_bar }),
            BarKind::Chevron => Box::new(ThemedRibbonStyle { theme, site: &theme.tab_bar }),
        };
        let hits = segment_bar::render(&items, tab_style.as_ref(), area, frame.buffer_mut());

        // Map hit regions to tab areas; carve the close affordance out of
        // the active tab's segment (the trailing "× " cell pair).
        self.tab_areas.clear();
        self.close_area = None;
        for hit in hits {
            if let Some(tab_id) = tab_ids.get(hit.index) {
                if *tab_id == TabId::View(active_idx) && active_idx != 0 && hit.area.width >= 3 {
                    let close = Rect::new(hit.area.x + hit.area.width - 3, hit.area.y, 3, hit.area.height);
                    self.close_area = Some((active_idx, close));
                }
                self.tab_areas.insert(*tab_id, hit.area);
            }
        }

        // Write back to shared layout so other components can read tab_areas
        ui.layout.tab_areas = self.tab_areas.clone();
    }

    // ── Click hit-testing ──

    /// Hit-test a left mouse click against the rendered tab areas.
    ///
    /// Returns a `TabBarAction` describing what was clicked. The caller
    /// is responsible for actually performing the action on `App`.
    pub fn handle_click(&self, x: u16, y: u16) -> TabBarAction {
        if let Some((idx, area)) = self.close_area {
            if x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height {
                return TabBarAction::Close(idx);
            }
        }
        let hit = self.tab_areas.iter().find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height).map(|(id, _)| *id);

        match hit {
            Some(TabId::View(i)) => TabBarAction::SwitchTo(i),
            Some(TabId::Add) => TabBarAction::OpenFilePicker,
            None => TabBarAction::None,
        }
    }

    // ── Drag handling ──

    /// Handle a drag event during tab reordering. Returns `true` if a swap
    /// occurred (the caller persists the new order on mouse-up).
    pub fn handle_drag(&mut self, column: u16, row: u16, views: &mut OpenViews) -> bool {
        let Some(dragging_idx) = self.drag.dragging_tab else {
            return false;
        };

        if !self.drag.active {
            return false;
        }

        for (id, r) in &self.tab_areas {
            if let TabId::View(i) = *id {
                if column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height && i != dragging_idx {
                    if views.swap(dragging_idx, i) {
                        self.drag.dragging_tab = Some(i);
                        return true;
                    }
                    return false;
                }
            }
        }

        false
    }

    // ── Mouse event handling ──

    /// Handle a mouse event on the tab bar. Returns app actions to process.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Vec<AppAction> {
        let mut actions = Vec::new();
        let x = mouse.column;
        let y = mouse.row;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let tab_action = self.handle_click(x, y);
                match tab_action {
                    TabBarAction::SwitchTo(i) => {
                        actions.push(AppAction::SwitchToTab(i));
                        // Start potential drag (the pinned tab never drags)
                        self.drag.dragging_tab = (i != 0).then_some(i);
                        self.drag.start_x = x;
                        self.drag.active = false;
                    }
                    TabBarAction::Close(i) => {
                        self.drag.dragging_tab = None;
                        actions.push(AppAction::CloseTab(i));
                    }
                    TabBarAction::OpenFilePicker => {
                        actions.push(AppAction::OpenFilePicker);
                    }
                    TabBarAction::None => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.drag.dragging_tab.is_some() && !self.drag.active => {
                let dx = (x as i16 - self.drag.start_x as i16).unsigned_abs();
                if dx >= 2 {
                    self.drag.active = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.drag.dragging_tab.take().is_some() => {
                if self.drag.active {
                    actions.push(AppAction::PersistOpenViews);
                }
                self.drag.active = false;
            }
            _ => {}
        }

        actions
    }

    /// Read-only access to the tab areas for external code that still
    /// references them (e.g. gear icon placement in the table area).
    pub fn tab_areas(&self) -> &BTreeMap<TabId, Rect> {
        &self.tab_areas
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;
    use crate::app::test_support::stub_app_with_repos;

    // ── Click hit-testing ──

    #[test]
    fn handle_click_returns_none_for_miss() {
        let tabs = Tabs::new();
        assert_eq!(tabs.handle_click(100, 100), TabBarAction::None);
    }

    #[test]
    fn handle_click_detects_view_tabs() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::View(0), Rect::new(0, 0, 10, 1));
        tabs.tab_areas.insert(TabId::View(2), Rect::new(10, 0, 10, 1));
        assert_eq!(tabs.handle_click(5, 0), TabBarAction::SwitchTo(0));
        assert_eq!(tabs.handle_click(15, 0), TabBarAction::SwitchTo(2));
    }

    #[test]
    fn handle_click_detects_add_button() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::Add, Rect::new(30, 0, 5, 1));
        assert_eq!(tabs.handle_click(32, 0), TabBarAction::OpenFilePicker);
    }

    #[test]
    fn close_affordance_wins_over_the_tab_area() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::View(2), Rect::new(10, 0, 10, 1));
        tabs.close_area = Some((2, Rect::new(17, 0, 3, 1)));
        assert_eq!(tabs.handle_click(12, 0), TabBarAction::SwitchTo(2));
        assert_eq!(tabs.handle_click(18, 0), TabBarAction::Close(2));
    }

    // ── Drag ──

    #[test]
    fn drag_swaps_open_views_and_follows_the_tab() {
        let mut app = stub_app_with_repos(2);
        let mut tabs = Tabs::new();
        // Tabs: 0 overview, 1 convoys, 2 repo0, 3 repo1
        tabs.tab_areas.insert(TabId::View(2), Rect::new(20, 0, 10, 1));
        tabs.tab_areas.insert(TabId::View(3), Rect::new(30, 0, 10, 1));
        tabs.drag.dragging_tab = Some(2);
        tabs.drag.active = true;

        let before: Vec<_> = app.views.iter().map(|view| view.address().cloned()).collect();
        assert!(tabs.handle_drag(35, 0, &mut app.views));
        let after: Vec<_> = app.views.iter().map(|view| view.address().cloned()).collect();
        assert_eq!(after[2], before[3]);
        assert_eq!(after[3], before[2]);
        assert_eq!(tabs.drag.dragging_tab, Some(3));
    }

    #[test]
    fn drag_never_swaps_with_the_pinned_overview() {
        let mut app = stub_app_with_repos(1);
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::View(0), Rect::new(0, 0, 10, 1));
        tabs.drag.dragging_tab = Some(2);
        tabs.drag.active = true;
        assert!(!tabs.handle_drag(5, 0, &mut app.views));
    }

    // ── Labels ──

    #[test]
    fn label_override_wins_over_kind_default() {
        let app = stub_app_with_repos(1);
        let view = app.views.get(1).expect("convoys tab").clone();
        assert_eq!(tab_label(&view, &app.model), " 🚢 convoys ");
        let renamed = OpenView { label_override: Some("mine".to_string()), ..view };
        assert_eq!(tab_label(&renamed, &app.model), "mine");
    }

    #[test]
    fn colliding_short_labels_widen_with_qualifying_parameters() {
        let app = stub_app_with_repos(0);
        let mut views = OpenViews::from_entries(vec![]);
        // Two vessels both named "leg-1", in different convoys, plus one
        // uniquely-named vessel.
        views.open_or_focus("vessel/flotilla/alpha/leg-1".parse().expect("valid"));
        views.open_or_focus("vessel/flotilla/bravo/leg-1".parse().expect("valid"));
        views.open_or_focus("vessel/flotilla/bravo/solo".parse().expect("valid"));

        let labels = tab_labels(&views, &app.model);
        assert_eq!(labels[1], "alpha/leg-1", "colliding label widens");
        assert_eq!(labels[2], "bravo/leg-1", "colliding label widens");
        assert_eq!(labels[3], "solo", "unique label stays short");
    }

    #[test]
    fn cross_namespace_vessel_collisions_widen_to_the_namespace() {
        let app = stub_app_with_repos(0);
        let mut views = OpenViews::from_entries(vec![]);
        // Same convoy and vessel names in two namespaces: convoy/vessel is
        // still ambiguous, so the labels widen all the way to the namespace.
        views.open_or_focus("vessel/ns-one/alpha/leg-1".parse().expect("valid"));
        views.open_or_focus("vessel/ns-two/alpha/leg-1".parse().expect("valid"));

        let labels = tab_labels(&views, &app.model);
        assert_eq!(labels[1], "ns-one/alpha/leg-1");
        assert_eq!(labels[2], "ns-two/alpha/leg-1");
    }

    #[test]
    fn dangling_repo_view_labels_loudly() {
        let app = stub_app_with_repos(0);
        let address: ViewAddress = "repo/github.com/gone/repo".parse().expect("valid");
        let mut views = OpenViews::from_entries(vec![]);
        views.open_or_focus(address);
        let view = views.active().clone();
        assert_eq!(tab_label(&view, &app.model), "⚠ repo");
    }
}
