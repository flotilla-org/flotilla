use std::collections::BTreeMap;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::RepoIdentity;
use ratatui::{layout::Rect, style::Style, Frame};

use crate::{
    app::{ui_state::DragState, TabId, TuiModel, UiState},
    segment_bar::{self, BarStyle, ThemedRibbonStyle, ThemedTabBarStyle},
    theme::{BarKind, Theme},
    widgets::AppAction,
};

/// Action returned from a tab bar click. The caller interprets the action
/// and mutates `App` state accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarAction {
    /// Switch to the flotilla config screen.
    SwitchToConfig,
    /// Switch to a repo tab and start a potential drag.
    SwitchToRepo(usize),
    /// Open the file picker to add a new repo.
    OpenFilePicker,
    /// No recognized tab was hit. The caller should continue with
    /// normal mouse handling.
    None,
}

/// Tab bar strip component. Handles rendering, hit-testing, drag-reorder,
/// and tab navigation. Owned by `Screen`.
#[derive(Default)]
pub struct Tabs {
    /// Click target areas populated during render.
    tab_areas: BTreeMap<TabId, Rect>,
    /// Tab drag-reorder state.
    pub drag: DragState,
    /// Whether a drag is visually active.
    drag_active: bool,
}

impl Tabs {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Rendering ──

    /// Render the tab bar into `area`, populating click targets for later
    /// hit-testing.
    pub fn render(&mut self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        self.drag_active = self.drag.active;

        let mut items = Vec::new();
        let mut tab_ids = Vec::new();

        // Flotilla logo tab
        let flotilla_style = theme.logo_style(ui.is_config);
        items.push(segment_bar::SegmentItem {
            label: TabId::FLOTILLA_LABEL.to_string(),
            key_hint: None,
            active: ui.is_config,
            dragging: false,
            style_override: Some(flotilla_style),
        });
        tab_ids.push(TabId::Flotilla);

        // Repo tabs
        for (i, repo_identity) in model.repo_order.iter().enumerate() {
            let rm = &model.repos[repo_identity];
            let name = TuiModel::repo_name(&rm.path);
            let is_active = !ui.is_config && i == model.active_repo;
            let loading = if rm.loading { " ⟳" } else { "" };
            let changed = if rm.has_unseen_changes { "*" } else { "" };
            let label = format!("{name}{changed}{loading}");

            items.push(segment_bar::SegmentItem {
                label,
                key_hint: None,
                active: is_active,
                dragging: is_active && self.drag_active,
                style_override: None,
            });
            tab_ids.push(TabId::Repo(i));
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

        // Map hit regions to tab areas
        self.tab_areas.clear();
        for hit in hits {
            if let Some(tab_id) = tab_ids.get(hit.index) {
                self.tab_areas.insert(tab_id.clone(), hit.area);
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
        let hit =
            self.tab_areas.iter().find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height).map(|(id, _)| id.clone());

        match hit {
            Some(TabId::Flotilla) => TabBarAction::SwitchToConfig,
            Some(TabId::Repo(i)) => TabBarAction::SwitchToRepo(i),
            Some(TabId::Add) => TabBarAction::OpenFilePicker,
            _ => TabBarAction::None,
        }
    }

    // ── Drag handling ──

    /// Handle a drag event during tab reordering. Returns `true` if a swap
    /// occurred and the caller should update model state.
    pub fn handle_drag(&self, column: u16, row: u16, repo_order: &mut [RepoIdentity], active_repo: &mut usize) -> bool {
        let Some(dragging_idx) = self.drag.dragging_tab else {
            return false;
        };

        if !self.drag.active {
            return false;
        }

        for (id, r) in &self.tab_areas {
            if let TabId::Repo(i) = *id {
                if column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height && i != dragging_idx {
                    repo_order.swap(dragging_idx, i);
                    *active_repo = i;
                    // Note: we can't update drag.dragging_tab here because we take &self.
                    // The caller must update drag.dragging_tab = Some(i) after this returns true.
                    return true;
                }
            }
        }

        false
    }

    /// Update the drag state after a successful swap.
    pub fn update_drag_index(&mut self, new_idx: usize) {
        self.drag.dragging_tab = Some(new_idx);
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
                    TabBarAction::SwitchToConfig => {
                        self.drag.dragging_tab = None;
                        actions.push(AppAction::SwitchToConfig);
                    }
                    TabBarAction::SwitchToRepo(i) => {
                        actions.push(AppAction::SwitchToRepo(i));
                        // Start potential drag
                        self.drag.dragging_tab = Some(i);
                        self.drag.start_x = x;
                        self.drag.active = false;
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
                    actions.push(AppAction::SaveTabOrder);
                }
                self.drag.active = false;
            }
            _ => {}
        }

        actions
    }

    // ── Tab navigation ──

    /// Switch to the next tab (forward). Wraps from the last repo to config,
    /// and from config to the first repo.
    pub fn next_tab(&mut self, model: &mut TuiModel, ui: &mut UiState) {
        self.step_tab(model, ui, TabDirection::Forward);
    }

    /// Switch to the previous tab (backward). Wraps from the first repo to config,
    /// and from config to the last repo.
    pub fn prev_tab(&mut self, model: &mut TuiModel, ui: &mut UiState) {
        self.step_tab(model, ui, TabDirection::Backward);
    }

    /// Switch directly to a specific repo tab by index.
    pub fn switch_to(&self, idx: usize, model: &mut TuiModel, ui: &mut UiState) {
        if idx < model.repo_order.len() {
            ui.is_config = false;
            model.active_repo = idx;
            let key = &model.repo_order[idx];
            model.repos.get_mut(key).expect("active repo must have model entry").has_unseen_changes = false;
        }
    }

    /// Move the current tab left (delta = -1) or right (delta = 1).
    /// Returns true if a swap occurred.
    pub fn move_tab(&self, delta: isize, model: &mut TuiModel) -> bool {
        let len = model.repo_order.len();
        if len < 2 {
            return false;
        }
        let cur = model.active_repo;
        let new_idx = cur as isize + delta;
        if new_idx < 0 || new_idx >= len as isize {
            return false;
        }
        let new_idx = new_idx as usize;
        model.repo_order.swap(cur, new_idx);
        model.active_repo = new_idx;
        true
    }

    /// Read-only access to the tab areas for external code that still
    /// references them (e.g. gear icon placement in the table area).
    pub fn tab_areas(&self) -> &BTreeMap<TabId, Rect> {
        &self.tab_areas
    }

    // ── Private helpers ──

    fn step_tab(&mut self, model: &mut TuiModel, ui: &mut UiState, direction: TabDirection) {
        if model.repo_order.is_empty() {
            return;
        }
        if ui.is_config {
            ui.is_config = false;
            model.active_repo = match direction {
                TabDirection::Forward => 0,
                TabDirection::Backward => model.repo_order.len() - 1,
            };
            return;
        }

        match direction {
            TabDirection::Forward => {
                if model.active_repo + 1 < model.repo_order.len() {
                    self.switch_to(model.active_repo + 1, model, ui);
                } else {
                    ui.is_config = true;
                }
            }
            TabDirection::Backward => {
                if model.active_repo > 0 {
                    self.switch_to(model.active_repo - 1, model, ui);
                } else {
                    ui.is_config = true;
                }
            }
        }
    }
}

enum TabDirection {
    Forward,
    Backward,
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
    fn handle_click_detects_flotilla_tab() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::Flotilla, Rect::new(0, 0, 10, 1));
        assert_eq!(tabs.handle_click(5, 0), TabBarAction::SwitchToConfig);
    }

    #[test]
    fn handle_click_detects_repo_tab() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::Repo(0), Rect::new(10, 0, 10, 1));
        assert_eq!(tabs.handle_click(15, 0), TabBarAction::SwitchToRepo(0));
    }

    #[test]
    fn handle_click_detects_add_button() {
        let mut tabs = Tabs::new();
        tabs.tab_areas.insert(TabId::Add, Rect::new(30, 0, 5, 1));
        assert_eq!(tabs.handle_click(32, 0), TabBarAction::OpenFilePicker);
    }

    // ── Tab navigation ──

    #[test]
    fn next_tab_advances_active_repo() {
        let mut app = stub_app_with_repos(3);
        let mut tabs = Tabs::new();
        assert_eq!(app.model.active_repo, 0);
        tabs.next_tab(&mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 1);
    }

    #[test]
    fn next_tab_wraps_to_config() {
        let mut app = stub_app_with_repos(2);
        let mut tabs = Tabs::new();
        tabs.switch_to(1, &mut app.model, &mut app.ui);
        tabs.next_tab(&mut app.model, &mut app.ui);
        assert!(app.ui.is_config);
    }

    #[test]
    fn next_tab_from_config_goes_to_first() {
        let mut app = stub_app_with_repos(3);
        let mut tabs = Tabs::new();
        app.ui.is_config = true;
        tabs.next_tab(&mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 0);
        assert!(!app.ui.is_config);
    }

    #[test]
    fn next_tab_noop_with_no_repos() {
        let mut app = stub_app_with_repos(0);
        let mut tabs = Tabs::new();
        tabs.next_tab(&mut app.model, &mut app.ui);
    }

    #[test]
    fn prev_tab_decrements_active_repo() {
        let mut app = stub_app_with_repos(3);
        let mut tabs = Tabs::new();
        tabs.switch_to(2, &mut app.model, &mut app.ui);
        tabs.prev_tab(&mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 1);
    }

    #[test]
    fn prev_tab_wraps_to_config() {
        let mut app = stub_app_with_repos(2);
        let mut tabs = Tabs::new();
        // active_repo is 0
        tabs.prev_tab(&mut app.model, &mut app.ui);
        assert!(app.ui.is_config);
    }

    #[test]
    fn prev_tab_from_config_goes_to_last() {
        let mut app = stub_app_with_repos(3);
        let mut tabs = Tabs::new();
        app.ui.is_config = true;
        tabs.prev_tab(&mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 2);
        assert!(!app.ui.is_config);
    }

    #[test]
    fn prev_tab_noop_with_no_repos() {
        let mut app = stub_app_with_repos(0);
        let mut tabs = Tabs::new();
        tabs.prev_tab(&mut app.model, &mut app.ui);
    }

    // ── switch_to ──

    #[test]
    fn switch_to_clears_unseen_changes() {
        let mut app = stub_app_with_repos(2);
        let tabs = Tabs::new();
        let key = app.model.repo_order[1].clone();
        app.model.repos.get_mut(&key).expect("repo model").has_unseen_changes = true;
        tabs.switch_to(1, &mut app.model, &mut app.ui);
        assert!(!app.model.repos[&key].has_unseen_changes);
    }

    #[test]
    fn switch_to_sets_active_repo_and_mode() {
        let mut app = stub_app_with_repos(3);
        let tabs = Tabs::new();
        app.ui.is_config = true;
        tabs.switch_to(2, &mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 2);
        assert!(!app.ui.is_config);
    }

    #[test]
    fn switch_to_noop_for_out_of_range() {
        let mut app = stub_app_with_repos(2);
        let tabs = Tabs::new();
        tabs.switch_to(5, &mut app.model, &mut app.ui);
        assert_eq!(app.model.active_repo, 0);
    }

    // ── move_tab ──

    #[test]
    fn move_tab_swaps_repos_forward() {
        let mut app = stub_app_with_repos(3);
        let tabs = Tabs::new();
        assert_eq!(app.model.active_repo, 0);
        let path0 = app.model.repo_order[0].clone();
        let path1 = app.model.repo_order[1].clone();
        let result = tabs.move_tab(1, &mut app.model);
        assert!(result);
        assert_eq!(app.model.active_repo, 1);
        assert_eq!(app.model.repo_order[0], path1);
        assert_eq!(app.model.repo_order[1], path0);
    }

    #[test]
    fn move_tab_swaps_repos_backward() {
        let mut app = stub_app_with_repos(3);
        let tabs = Tabs::new();
        tabs.switch_to(2, &mut app.model, &mut app.ui);
        let path1 = app.model.repo_order[1].clone();
        let path2 = app.model.repo_order[2].clone();
        let result = tabs.move_tab(-1, &mut app.model);
        assert!(result);
        assert_eq!(app.model.active_repo, 1);
        assert_eq!(app.model.repo_order[1], path2);
        assert_eq!(app.model.repo_order[2], path1);
    }

    #[test]
    fn move_tab_returns_false_at_boundary() {
        let mut app = stub_app_with_repos(3);
        let tabs = Tabs::new();
        assert!(!tabs.move_tab(-1, &mut app.model));
        tabs.switch_to(2, &mut app.model, &mut app.ui);
        assert!(!tabs.move_tab(1, &mut app.model));
    }

    #[test]
    fn move_tab_returns_false_with_single_repo() {
        let mut app = stub_app_with_repos(1);
        let tabs = Tabs::new();
        assert!(!tabs.move_tab(1, &mut app.model));
        assert!(!tabs.move_tab(-1, &mut app.model));
    }
}
