use std::collections::BTreeMap;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::ViewAddress;
use ratatui::{layout::Rect, style::Style, Frame};
use unicode_width::UnicodeWidthStr;

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
fn default_address_label(address: &ViewAddress, model: &TuiModel, level: usize) -> String {
    match address {
        ViewAddress::Overview => " ⚓ flotilla ".to_string(),
        ViewAddress::Convoys { namespace, .. } if namespace == "flotilla" && level == 0 => " 🚢 convoys ".to_string(),
        ViewAddress::Convoys { namespace, .. } => format!(" 🚢 {namespace} "),
        ViewAddress::Independents { scope: Some(scope) } => match level {
            0 => format!("⛵ {} independents", scope.name),
            _ => format!("⛵ {}/{} independents", scope.namespace, scope.name),
        },
        ViewAddress::Independents { scope: None } => " ⛵ independents ".to_string(),
        ViewAddress::Convoy { namespace, name } => match level {
            0 => format!("🚢 {name}"),
            _ => format!("🚢 {namespace}/{name}"),
        },
        ViewAddress::Vessel { namespace, convoy, vessel } => match level {
            0 => vessel.clone(),
            1 => format!("{convoy}/{vessel}"),
            _ => format!("{namespace}/{convoy}/{vessel}"),
        },
        ViewAddress::Project { namespace, name } => match level {
            0 => format!("⛰ {name}"),
            _ => format!("⛰ {namespace}/{name}"),
        },
        ViewAddress::Issues { scope } => match level {
            0 => format!("◉ {} issues", scope.name),
            _ => format!("◉ {}/{} issues", scope.namespace, scope.name),
        },
        ViewAddress::Checkouts { scope: Some(scope) } => match level {
            0 => format!("⌂ {} checkouts", scope.name),
            _ => format!("⌂ {}/{} checkouts", scope.namespace, scope.name),
        },
        ViewAddress::Checkouts { scope: None } => "⌂ checkouts".to_string(),
        ViewAddress::Repo { identity, .. } => repo_label(identity, model, level),
    }
}

fn default_label(view: &OpenView, model: &TuiModel, level: usize) -> String {
    match &view.target {
        ViewTarget::View(address) => default_address_label(address, model, level),
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
    tab_label_at_level(view, model, 0)
}

fn tab_label_at_level(view: &OpenView, model: &TuiModel, level: usize) -> String {
    tab_label_parts_at_level(view, model, level).join(" › ")
}

fn tab_label_parts_at_level(view: &OpenView, model: &TuiModel, level: usize) -> Vec<String> {
    if !view.has_history() {
        return vec![match &view.label_override {
            Some(label) => label.clone(),
            None => default_label(view, model, level),
        }];
    }

    let addresses = view.breadcrumb_addresses();
    let last = addresses.len().saturating_sub(1);
    addresses
        .iter()
        .enumerate()
        .map(|(index, address)| {
            if index == 0 {
                if let Some(label) = &view.label_override {
                    return label.trim().to_string();
                }
            }
            let address_level = if index == last { level } else { 0 };
            default_address_label(address, model, address_level).trim().to_string()
        })
        .collect::<Vec<_>>()
}

#[derive(Debug)]
struct TabLabel {
    text: String,
    lineage: Option<Vec<String>>,
}

/// Labels for the whole tab bar: short defaults, progressively widened with
/// qualifying parameters only where two tabs would otherwise read the same
/// (vessel labels widen to convoy/vessel, then namespace/convoy/vessel).
/// User overrides never widen.
fn tab_labels(views: &OpenViews, model: &TuiModel) -> Vec<TabLabel> {
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
                labels[i] = tab_label_at_level(view, model, levels[i]);
            }
        }
    }
    views
        .iter()
        .enumerate()
        .map(|(index, view)| {
            let parts = tab_label_parts_at_level(view, model, levels[index]);
            TabLabel { text: parts.join(" › "), lineage: (parts.len() > 1).then_some(parts) }
        })
        .collect()
}

fn truncate_lineage(parts: &[String], max_width: usize) -> String {
    let full = parts.join(" › ");
    if full.width() <= max_width {
        return full;
    }

    let Some(leaf) = parts.last() else {
        return String::new();
    };
    let leaf_width = leaf.width();
    const ELLIPSIS_SEPARATOR: &str = "… › ";
    let fixed_width = ELLIPSIS_SEPARATOR.width() + leaf_width;
    if max_width < fixed_width {
        return leaf.clone();
    }

    let ancestors = parts[..parts.len() - 1].join(" › ");
    let ancestor_width = max_width - fixed_width;
    let start = ancestors
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(ancestors.len()))
        .find(|&index| ancestors[index..].width() <= ancestor_width)
        .unwrap_or(ancestors.len());
    format!("…{} › {leaf}", &ancestors[start..])
}

fn rendered_bar_width(items: &[segment_bar::SegmentItem], style: &dyn BarStyle) -> usize {
    let items_width: usize = items.iter().map(|item| style.render_item(item).width).sum();
    items_width + style.separator().width * items.len().saturating_sub(1)
}

fn truncate_lineages_to_fit(
    items: &mut [segment_bar::SegmentItem],
    labels: &[TabLabel],
    active_idx: usize,
    style: &dyn BarStyle,
    max_width: usize,
) {
    for (index, label) in labels.iter().enumerate() {
        let Some(parts) = &label.lineage else {
            continue;
        };
        let total_width = rendered_bar_width(items, style);
        if total_width <= max_width {
            break;
        }

        let overflow = total_width - max_width;
        let target_width = label.text.width().saturating_sub(overflow);
        let mut text = truncate_lineage(parts, target_width);
        if index == active_idx && index != 0 {
            text = format!("{} {CLOSE_AFFORDANCE} ", text.trim_end_matches(' '));
        }
        items[index].label = text;
    }
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
            let mut label = labels[i].text.clone();
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
        truncate_lineages_to_fit(&mut items, &labels, active_idx, tab_style.as_ref(), usize::from(area.width));
        let close_offset = (active_idx != 0)
            .then(|| tab_style.render_item(&items[active_idx]).last_cell_offset_of(CLOSE_AFFORDANCE))
            .flatten()
            .and_then(|offset| u16::try_from(offset).ok());
        let hits = segment_bar::render(&items, tab_style.as_ref(), area, frame.buffer_mut());

        // Map hit regions to tab areas; carve the close affordance's exact
        // rendered cell out of the active tab's segment.
        self.tab_areas.clear();
        self.close_area = None;
        for hit in hits {
            if let Some(tab_id) = tab_ids.get(hit.index) {
                if *tab_id == TabId::View(active_idx) {
                    if let Some(offset) = close_offset.filter(|&offset| offset < hit.area.width) {
                        let close = Rect::new(hit.area.x + offset, hit.area.y, 1, hit.area.height);
                        self.close_area = Some((active_idx, close));
                    }
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
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

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

    #[test]
    fn rendered_close_affordance_is_the_only_close_hotspot_for_every_bar_style() {
        for theme in [Theme::classic(), Theme::catppuccin_mocha()] {
            let mut app = stub_app_with_repos(2);
            let active_idx = app.views.active_index();
            app.views.get_mut(active_idx).expect("active view").label_override = Some("custom × label".to_string());
            let mut tabs = Tabs::new();
            let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("terminal");

            terminal.draw(|frame| tabs.render(&app.views, &app.model, &mut app.ui, &theme, frame, frame.area())).expect("render tabs");

            let active_area = *tabs.tab_areas().get(&TabId::View(active_idx)).expect("active tab area");
            let close_x = (active_area.x..active_area.right())
                .rev()
                .find(|&x| terminal.backend().buffer()[(x, active_area.y)].symbol() == CLOSE_AFFORDANCE)
                .expect("rendered close affordance");

            for (tab_id, area) in tabs.tab_areas() {
                let TabId::View(idx) = *tab_id else {
                    continue;
                };
                for x in area.x..area.right() {
                    let expected = if idx == active_idx && x == close_x { TabBarAction::Close(idx) } else { TabBarAction::SwitchTo(idx) };
                    assert_eq!(tabs.handle_click(x, area.y), expected, "unexpected hit at x={x} for {:?}", theme.tab_bar.kind);
                }
            }
        }
    }

    // ── Drag ──

    #[test]
    fn drag_swaps_open_views_and_follows_the_tab() {
        let mut app = stub_app_with_repos(2);
        app.views.open_or_focus("project/flotilla/one".parse().expect("project address"));
        app.views.open_or_focus("project/flotilla/two".parse().expect("project address"));
        let mut tabs = Tabs::new();
        // Tabs: 0 overview, 1 convoys, 2 project one, 3 project two
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
    fn drilled_tab_label_shows_lineage_and_back_restores_plain_label() {
        let app = stub_app_with_repos(0);
        let mut views = OpenViews::scoped("project/flotilla/cleat".parse().expect("valid project"));

        assert!(views.drill("convoys/flotilla?project=flotilla%2Fcleat".parse().expect("valid scoped convoys")));
        assert_eq!(tab_label(views.active(), &app.model), "⛰ cleat › 🚢 convoys");

        assert!(views.back());
        assert_eq!(tab_label(views.active(), &app.model), "⛰ cleat");
    }

    #[test]
    fn narrow_tab_bar_truncates_drill_lineage_from_the_left_and_keeps_the_leaf() {
        let mut app = stub_app_with_repos(0);
        app.views.switch_to(1);
        assert!(app.views.drill("convoy/flotilla/an-exceptionally-long-convoy-name".parse().expect("valid convoy")));
        assert!(app.views.drill("vessel/flotilla/an-exceptionally-long-convoy-name/leaf".parse().expect("valid vessel")));
        let mut tabs = Tabs::new();
        let theme = Theme::classic();
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).expect("terminal");

        terminal.draw(|frame| tabs.render(&app.views, &app.model, &mut app.ui, &theme, frame, frame.area())).expect("render tabs");

        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("…"));
        assert!(rendered.contains("leaf ×"));
        assert!(!rendered.contains("exceptionally"));
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
        assert_eq!(labels[1].text, "alpha/leg-1", "colliding label widens");
        assert_eq!(labels[2].text, "bravo/leg-1", "colliding label widens");
        assert_eq!(labels[3].text, "solo", "unique label stays short");
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
        assert_eq!(labels[1].text, "ns-one/alpha/leg-1");
        assert_eq!(labels[2].text, "ns-two/alpha/leg-1");
    }
}
