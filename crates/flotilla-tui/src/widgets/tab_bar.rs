use std::collections::BTreeMap;

use ratatui::{layout::Rect, style::Style, Frame};

use crate::{
    app::{ui_state::DragState, TabId, TuiModel, UiState},
    segment_bar::{self, BarStyle, ThemedRibbonStyle, ThemedTabBarStyle},
    theme::{BarKind, Theme},
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
    /// A click in the event log filter area — cycle the log level.
    CycleEventLogFilter,
    /// No recognized tab was hit. The caller should continue with
    /// normal mouse handling.
    None,
}

/// Standalone tab bar component. Handles rendering and mouse hit-testing
/// for the top-level tab strip.
///
/// Does not implement `InteractiveWidget` — it will be composed into
/// `BaseView` in a future step.
#[derive(Default)]
pub struct TabBar {
    /// Click target areas populated during the most recent render.
    tab_areas: BTreeMap<TabId, Rect>,
    /// Event log filter click area populated during render.
    event_log_filter_area: Rect,
}

impl TabBar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the tab bar into `area`, populating click targets for later
    /// hit-testing.
    ///
    /// The caller should also pass `ui` so the tab areas are written back
    /// into `ui.layout.tab_areas` (shared with other components that still
    /// read from there).
    pub fn render(&mut self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        let mut items = Vec::new();
        let mut tab_ids = Vec::new();

        // Flotilla logo tab
        let flotilla_style = theme.logo_style(ui.mode.is_config());
        items.push(segment_bar::SegmentItem {
            label: TabId::FLOTILLA_LABEL.to_string(),
            key_hint: None,
            active: ui.mode.is_config(),
            dragging: false,
            style_override: Some(flotilla_style),
        });
        tab_ids.push(TabId::Flotilla);

        // Repo tabs
        for (i, repo_identity) in model.repo_order.iter().enumerate() {
            let rm = &model.repos[repo_identity];
            let rui = &ui.repo_ui[repo_identity];
            let name = TuiModel::repo_name(&rm.path);
            let is_active = !ui.mode.is_config() && i == model.active_repo;
            let loading = if rm.loading { " ⟳" } else { "" };
            let changed = if rui.has_unseen_changes { "*" } else { "" };
            let label = format!("{name}{changed}{loading}");

            items.push(segment_bar::SegmentItem {
                label,
                key_hint: None,
                active: is_active,
                dragging: is_active && ui.drag.active,
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

    /// Hit-test a left mouse click against the rendered tab areas.
    ///
    /// Returns a `TabBarAction` describing what was clicked. The caller
    /// is responsible for actually performing the action on `App`.
    pub fn handle_click(&self, x: u16, y: u16, _is_config_mode: bool) -> TabBarAction {
        // Check event log filter area
        let ef = self.event_log_filter_area;
        if ef.width > 0 && x >= ef.x && x < ef.x + ef.width && y >= ef.y && y < ef.y + ef.height {
            return TabBarAction::CycleEventLogFilter;
        }

        // Check which tab area was clicked
        let hit =
            self.tab_areas.iter().find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height).map(|(id, _)| id.clone());

        match hit {
            Some(TabId::Flotilla) => TabBarAction::SwitchToConfig,
            Some(TabId::Repo(i)) => TabBarAction::SwitchToRepo(i),
            Some(TabId::Add) => TabBarAction::OpenFilePicker,
            // TabId::Gear is not a tab bar item — it's rendered in the table
            // border and handled by the table mouse path in key_handlers.rs.
            _ => TabBarAction::None,
        }
    }

    /// Handle a drag event during tab reordering. Returns `true` if a swap
    /// occurred and the caller should update model state.
    pub fn handle_drag(&self, column: u16, row: u16, drag: &mut DragState, repo_order: &mut [RepoIdentity], active_repo: &mut usize) -> bool
    where
        RepoIdentity: Clone,
    {
        let Some(dragging_idx) = drag.dragging_tab else {
            return false;
        };

        if !drag.active {
            let dx = (column as i16 - drag.start_x as i16).unsigned_abs();
            if dx >= 2 {
                drag.active = true;
            }
        }

        if drag.active {
            for (id, r) in &self.tab_areas {
                if let TabId::Repo(i) = *id {
                    if column >= r.x && column < r.x + r.width && row >= r.y && row < r.y + r.height && i != dragging_idx {
                        repo_order.swap(dragging_idx, i);
                        *active_repo = i;
                        drag.dragging_tab = Some(i);
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Update the event log filter area (set during content rendering, not
    /// tab bar rendering, but needed for click detection).
    pub fn set_event_log_filter_area(&mut self, area: Rect) {
        self.event_log_filter_area = area;
    }

    /// Read-only access to the tab areas for external code that still
    /// references them (e.g. gear icon placement in the table area).
    pub fn tab_areas(&self) -> &BTreeMap<TabId, Rect> {
        &self.tab_areas
    }
}

use flotilla_protocol::RepoIdentity;

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn handle_click_returns_none_for_miss() {
        let tab_bar = TabBar::new();
        assert_eq!(tab_bar.handle_click(100, 100, false), TabBarAction::None);
    }

    #[test]
    fn handle_click_detects_flotilla_tab() {
        let mut tab_bar = TabBar::new();
        tab_bar.tab_areas.insert(TabId::Flotilla, Rect::new(0, 0, 10, 1));
        assert_eq!(tab_bar.handle_click(5, 0, false), TabBarAction::SwitchToConfig);
    }

    #[test]
    fn handle_click_detects_repo_tab() {
        let mut tab_bar = TabBar::new();
        tab_bar.tab_areas.insert(TabId::Repo(0), Rect::new(10, 0, 10, 1));
        assert_eq!(tab_bar.handle_click(15, 0, false), TabBarAction::SwitchToRepo(0));
    }

    #[test]
    fn handle_click_detects_add_button() {
        let mut tab_bar = TabBar::new();
        tab_bar.tab_areas.insert(TabId::Add, Rect::new(30, 0, 5, 1));
        assert_eq!(tab_bar.handle_click(32, 0, false), TabBarAction::OpenFilePicker);
    }

    #[test]
    fn gear_click_not_handled_by_tab_bar() {
        // TabId::Gear is rendered in the table border, not the tab bar.
        // Even if it ends up in tab_areas, the tab bar should not handle it.
        let mut tab_bar = TabBar::new();
        tab_bar.tab_areas.insert(TabId::Gear, Rect::new(40, 0, 3, 1));
        assert_eq!(tab_bar.handle_click(41, 0, false), TabBarAction::None);
    }

    #[test]
    fn handle_click_detects_event_log_filter() {
        let mut tab_bar = TabBar::new();
        tab_bar.event_log_filter_area = Rect::new(50, 0, 5, 1);
        assert_eq!(tab_bar.handle_click(52, 0, false), TabBarAction::CycleEventLogFilter);
    }

    #[test]
    fn handle_click_event_log_filter_takes_priority() {
        let mut tab_bar = TabBar::new();
        // Overlapping areas: event log filter should win
        tab_bar.event_log_filter_area = Rect::new(0, 0, 10, 1);
        tab_bar.tab_areas.insert(TabId::Flotilla, Rect::new(0, 0, 10, 1));
        assert_eq!(tab_bar.handle_click(5, 0, false), TabBarAction::CycleEventLogFilter);
    }
}
