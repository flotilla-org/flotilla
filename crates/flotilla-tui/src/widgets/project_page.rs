use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{QueryId, ViewAddress};
use ratatui::{layout::Rect, style::Modifier, widgets::Paragraph, Frame};

use super::{
    describe::DescribeWidget, table_action_menu::TableActionMenuWidget, table_search::TableSearchWidget, AppAction, InteractiveWidget,
    Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::{NamespaceMap, QueryTableCache},
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    table_view::{self, ProjectPanel, ProjectPanelKind, ProjectTableState, RowId},
    theme::Theme,
};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(bon::Builder)]
struct PanelLayout<'a> {
    panel: &'a ProjectPanel,
    header_line: usize,
    columns_line: usize,
    rows_start: usize,
}

impl<'a> PanelLayout<'a> {
    fn rows_end(&self) -> usize {
        self.rows_start + self.panel.table.rows.len()
    }
}

#[derive(Default)]
pub struct ProjectPageWidget {
    area: Rect,
    last_click: Option<(ProjectPanelKind, RowId, Instant)>,
}

impl ProjectPageWidget {
    fn filtered_panels(
        address: &ViewAddress,
        rows: &table_view::TableRows<'_>,
        state: &ProjectTableState,
    ) -> Result<Vec<ProjectPanel>, String> {
        table_view::project_panels(address, rows).map(|panels| Self::apply_filters(panels, state))
    }

    fn apply_filters(panels: Vec<ProjectPanel>, state: &ProjectTableState) -> Vec<ProjectPanel> {
        panels
            .into_iter()
            .map(|mut panel| {
                panel.table = panel.table.filtered(&state.table(panel.kind).filter);
                panel
            })
            .collect()
    }

    fn project_panels(
        address: &ViewAddress,
        namespaces: &NamespaceMap,
        query_tables: &QueryTableCache,
        state: &ProjectTableState,
    ) -> Result<Vec<ProjectPanel>, String> {
        let rows = crate::app::table_rows(namespaces, query_tables, state.issue_source_search());
        Self::filtered_panels(address, &rows, state)
    }

    fn panels(ctx: &WidgetContext<'_>) -> Result<Vec<ProjectPanel>, String> {
        let address = ctx.views.active_address().ok_or_else(|| "active view has no valid address".to_string())?;
        let state = ctx.views.active_project_table_state();
        Self::project_panels(address, ctx.namespaces, ctx.query_tables, state)
    }

    fn render_panels(ctx: &RenderContext<'_>) -> Result<Vec<ProjectPanel>, String> {
        let address = ctx.views.active_address().ok_or_else(|| "active view has no valid address".to_string())?;
        let state = ctx.views.active_project_table_state();
        Self::project_panels(address, ctx.namespaces, ctx.query_tables, state)
    }

    fn layouts(panels: &[ProjectPanel]) -> Vec<PanelLayout<'_>> {
        let mut next = 0;
        panels
            .iter()
            .map(|panel| {
                let layout = PanelLayout::builder().panel(panel).header_line(next).columns_line(next + 1).rows_start(next + 2).build();
                next = layout.rows_end() + 1;
                layout
            })
            .collect()
    }

    fn content_height(layouts: &[PanelLayout<'_>]) -> usize {
        layouts.last().map_or(0, |layout| layout.rows_end())
    }

    fn line_area(&self, content_line: usize, state: &ProjectTableState) -> Option<Rect> {
        let visible = content_line.checked_sub(state.scroll_offset())?;
        (visible < self.area.height as usize).then_some(Rect { y: self.area.y + visible as u16, height: 1, ..self.area })
    }

    fn reconcile(layouts: &[PanelLayout<'_>], state: &mut ProjectTableState) {
        for layout in layouts {
            state.table_mut(layout.panel.kind).reconcile(&layout.panel.table);
        }
    }

    fn ensure_active_visible(&self, layouts: &[PanelLayout<'_>], state: &mut ProjectTableState) {
        let Some(layout) = layouts.iter().find(|layout| layout.panel.kind == state.active()) else { return };
        let selected = state.table(layout.panel.kind).selected_index(&layout.panel.table);
        let line = if state.header_focused() {
            layout.header_line
        } else {
            selected.map_or(layout.header_line, |selected| layout.rows_start + selected)
        };
        let visible = self.area.height as usize;
        if visible == 0 {
            state.set_scroll_offset(0);
        } else if line < state.scroll_offset() {
            state.set_scroll_offset(line);
        } else if line >= state.scroll_offset().saturating_add(visible) {
            state.set_scroll_offset(line + 1 - visible);
        }
        state.set_scroll_offset(state.scroll_offset().min(Self::content_height(layouts).saturating_sub(visible)));
    }

    fn select_delta(layouts: &[PanelLayout<'_>], state: &mut ProjectTableState, delta: isize) {
        let Some(index) = layouts.iter().position(|layout| layout.panel.kind == state.active()) else { return };
        let current = &layouts[index];
        if state.header_focused() && delta > 0 && !current.panel.table.rows.is_empty() {
            state.focus_rows();
            state.table_mut(current.panel.kind).select_index(&current.panel.table, 0);
            return;
        }
        let selected = state.table(current.panel.kind).selected_index(&current.panel.table).unwrap_or(0);
        let last = current.panel.table.rows.len().saturating_sub(1);
        if !state.header_focused() && delta > 0 && selected < last {
            state.table_mut(current.panel.kind).select_delta(&current.panel.table, 1);
            return;
        }
        if !state.header_focused() && delta < 0 && selected > 0 {
            state.table_mut(current.panel.kind).select_delta(&current.panel.table, -1);
            return;
        }
        if !state.header_focused() && delta < 0 {
            state.focus_header();
            return;
        }
        let mut indices: Box<dyn Iterator<Item = usize>> =
            if delta > 0 { Box::new(index.saturating_add(1)..layouts.len()) } else { Box::new((0..index).rev()) };
        if let Some(candidate) = indices.next() {
            let next = &layouts[candidate];
            state.set_active(next.panel.kind);
            state.table_mut(next.panel.kind).reconcile(&next.panel.table);
            if delta < 0 && !next.panel.table.rows.is_empty() {
                state.focus_rows();
                let last = next.panel.table.rows.len() - 1;
                state.table_mut(next.panel.kind).select_index(&next.panel.table, last);
            } else {
                state.focus_header();
            }
        }
    }

    fn select_panel_delta(layouts: &[PanelLayout<'_>], state: &mut ProjectTableState, delta: isize) {
        let Some(index) = layouts.iter().position(|layout| layout.panel.kind == state.active()) else { return };
        let candidate = if delta > 0 {
            index.checked_add(1).filter(|candidate| *candidate < layouts.len())
        } else if delta < 0 {
            index.checked_sub(1)
        } else {
            None
        };
        let Some(candidate) = candidate else { return };
        let next = &layouts[candidate];
        state.set_active(next.panel.kind);
        state.table_mut(next.panel.kind).reconcile(&next.panel.table);
        state.focus_header();
    }

    fn fetch_more_query(layouts: &[PanelLayout<'_>], state: &ProjectTableState, trigger: super::table::FetchTrigger) -> Option<QueryId> {
        let layout = layouts.iter().find(|layout| layout.panel.kind == state.active())?;
        if layout.panel.kind != ProjectPanelKind::Issues
            || !super::table::TablePanel::should_fetch_more(&layout.panel.table, state.table(layout.panel.kind), trigger)
        {
            return None;
        }
        let ViewAddress::Issues { scope } = &layout.panel.target else { return None };
        Some(QueryId::Issues {
            scope: scope.clone(),
            search: state.issue_source_search().filter(|search| !search.is_empty()).map(str::to_owned),
        })
    }

    fn active_row<'a>(
        layouts: &'a [PanelLayout<'a>],
        state: &'a ProjectTableState,
    ) -> Option<(&'a ProjectPanel, &'a table_view::ProjectedRow)> {
        let layout = layouts.iter().find(|layout| layout.panel.kind == state.active())?;
        if state.header_focused() {
            return None;
        }
        state.table(layout.panel.kind).selected_row(&layout.panel.table).map(|row| (layout.panel, row))
    }

    fn active_action(layouts: &[PanelLayout<'_>], state: &ProjectTableState) -> Option<AppAction> {
        if state.header_focused() {
            return layouts
                .iter()
                .find(|layout| layout.panel.kind == state.active())
                .map(|layout| AppAction::DrillView(layout.panel.target.clone()));
        }
        Self::active_row(layouts, state).and_then(|(_, row)| super::table::TablePanel::row_action(row))
    }

    fn render_composite(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, panels: &[ProjectPanel], state: &mut ProjectTableState) {
        self.area = area;
        let layouts = Self::layouts(panels);
        Self::reconcile(&layouts, state);
        self.ensure_active_visible(&layouts, state);
        for layout in &layouts {
            if let Some(header_area) = self.line_area(layout.header_line, state) {
                let mut label = format!(" {}  › {}", layout.panel.table.title, layout.panel.target);
                if let Some(as_of) = layout.panel.table.meta.as_of {
                    label.push_str(&format!(" · as of {}", as_of.format("%Y-%m-%d %H:%M")));
                }
                if layout.panel.table.meta.has_more {
                    label.push_str(" · more available");
                }
                if layout.panel.table.meta.availability == table_view::TableAvailability::Loading {
                    label.push_str(" · loading");
                }
                if !layout.panel.table.meta.conditions.is_empty() {
                    label.push_str(&format!(" · ⚠ {}", layout.panel.table.meta.conditions.join("; ")));
                }
                let style = if state.active() == layout.panel.kind && state.header_focused() {
                    theme.header_style().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else if state.active() == layout.panel.kind {
                    theme.header_style().add_modifier(Modifier::BOLD)
                } else {
                    theme.header_style()
                };
                frame.render_widget(Paragraph::new(label).style(style), header_area);
            }
            if let Some(columns_area) = self.line_area(layout.columns_line, state) {
                super::table::TablePanel::render_header(frame, columns_area, theme, &layout.panel.table);
            }
            let first = state.scroll_offset().saturating_sub(layout.rows_start);
            let visible_end = state.scroll_offset().saturating_add(area.height as usize);
            let end = layout.rows_end().min(visible_end);
            if first < layout.panel.table.rows.len() && end > layout.rows_start.saturating_add(first) {
                let count = end - layout.rows_start - first;
                if let Some(rows_area) = self.line_area(layout.rows_start + first, state) {
                    let rows_area = Rect { height: count as u16, ..rows_area };
                    let mut unfocused_state = state.table(layout.panel.kind).clone();
                    if state.active() == layout.panel.kind && state.header_focused() {
                        unfocused_state.clear_selection();
                    }
                    super::table::TablePanel::render_rows(frame, rows_area, theme, &layout.panel.table, &unfocused_state, first, count);
                }
            }
        }
    }
}

impl InteractiveWidget for ProjectPageWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        let Ok(panels) = Self::panels(ctx) else { return Outcome::Ignored };
        let layouts = Self::layouts(&panels);
        Self::reconcile(&layouts, ctx.views.active_project_table_state_mut());
        match action {
            Action::OpenFind => {
                return Outcome::Push(Box::new(TableSearchWidget::find(&ctx.views.active_table_state().filter)));
            }
            Action::Dismiss
                if ctx.views.active_project_table_state().active() == ProjectPanelKind::Issues
                    && ctx.views.active_project_table_state().active_table().source_search.is_some() =>
            {
                ctx.app_actions.push(AppAction::SetSourceSearch(None));
                return Outcome::Consumed;
            }
            _ => {}
        }
        match action {
            Action::SelectNext => {
                let query = {
                    let state = ctx.views.active_project_table_state_mut();
                    Self::select_delta(&layouts, state, 1);
                    let query = Self::fetch_more_query(&layouts, state, super::table::FetchTrigger::NearBottom);
                    self.ensure_active_visible(&layouts, state);
                    query
                };
                if let Some(query) = query {
                    ctx.app_actions.push(AppAction::FetchMore(query));
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                let state = ctx.views.active_project_table_state_mut();
                Self::select_delta(&layouts, state, -1);
                self.ensure_active_visible(&layouts, state);
                Outcome::Consumed
            }
            Action::NextPanel => {
                let state = ctx.views.active_project_table_state_mut();
                Self::select_panel_delta(&layouts, state, 1);
                self.ensure_active_visible(&layouts, state);
                Outcome::Consumed
            }
            Action::PrevPanel => {
                let state = ctx.views.active_project_table_state_mut();
                Self::select_panel_delta(&layouts, state, -1);
                self.ensure_active_visible(&layouts, state);
                Outcome::Consumed
            }
            Action::Confirm => {
                let action = Self::active_action(&layouts, ctx.views.active_project_table_state());
                if let Some(action) = action {
                    ctx.app_actions.push(action);
                }
                Outcome::Consumed
            }
            Action::Dismiss => {
                ctx.app_actions.push(AppAction::BackView);
                Outcome::Consumed
            }
            Action::FetchMore => {
                if let Some(query) =
                    Self::fetch_more_query(&layouts, ctx.views.active_project_table_state(), super::table::FetchTrigger::Explicit)
                {
                    ctx.app_actions.push(AppAction::FetchMore(query));
                }
                Outcome::Consumed
            }
            Action::Describe => match Self::active_row(&layouts, ctx.views.active_project_table_state()) {
                Some((panel, row)) => Outcome::Push(Box::new(DescribeWidget::new(panel.table.title.clone(), row.describe.clone()))),
                None => Outcome::Consumed,
            },
            Action::OpenActionMenu => match Self::active_row(&layouts, ctx.views.active_project_table_state()) {
                Some((_, row)) if !row.actions.is_empty() => Outcome::Push(Box::new(TableActionMenuWidget::new(row.actions.clone()))),
                Some(_) => {
                    ctx.app_actions.push(AppAction::ShowStatus("No actions available for the selected row".into()));
                    Outcome::Consumed
                }
                None => Outcome::Consumed,
            },
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if mouse.column < self.area.x
            || mouse.column >= self.area.x.saturating_add(self.area.width)
            || mouse.row < self.area.y
            || mouse.row >= self.area.y.saturating_add(self.area.height)
        {
            return Outcome::Ignored;
        }
        let Ok(panels) = Self::panels(ctx) else { return Outcome::Ignored };
        let layouts = Self::layouts(&panels);
        let line = ctx.views.active_project_table_state().scroll_offset() + (mouse.row - self.area.y) as usize;
        match mouse.kind {
            MouseEventKind::ScrollDown => return self.handle_action(Action::SelectNext, ctx),
            MouseEventKind::ScrollUp => return self.handle_action(Action::SelectPrev, ctx),
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {}
            _ => return Outcome::Ignored,
        }
        let Some(layout) =
            layouts.iter().find(|layout| line == layout.header_line || (line >= layout.rows_start && line < layout.rows_end()))
        else {
            return Outcome::Ignored;
        };
        if line == layout.header_line {
            ctx.app_actions.push(AppAction::DrillView(layout.panel.target.clone()));
            return Outcome::Consumed;
        }
        let row_index = line - layout.rows_start;
        let query = {
            let state = ctx.views.active_project_table_state_mut();
            state.set_active(layout.panel.kind);
            state.focus_rows();
            state.table_mut(layout.panel.kind).select_index(&layout.panel.table, row_index);
            Self::fetch_more_query(&layouts, state, super::table::FetchTrigger::NearBottom)
        };
        if let Some(query) = query {
            ctx.app_actions.push(AppAction::FetchMore(query));
        }
        let row = &layout.panel.table.rows[row_index];
        if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
            return if row.actions.is_empty() {
                ctx.app_actions.push(AppAction::ShowStatus("No actions available for the selected row".into()));
                Outcome::Consumed
            } else {
                Outcome::Push(Box::new(TableActionMenuWidget::new(row.actions.clone())))
            };
        }
        let now = Instant::now();
        let double_click = self
            .last_click
            .as_ref()
            .is_some_and(|(kind, id, at)| *kind == layout.panel.kind && id == &row.id && now.duration_since(*at) <= DOUBLE_CLICK_WINDOW);
        self.last_click = Some((layout.panel.kind, row.id.clone(), now));
        if double_click {
            self.last_click = None;
            if let Some(action) = super::table::TablePanel::row_action(row) {
                ctx.app_actions.push(action);
            }
        }
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let Ok(panels) = Self::render_panels(ctx) else { return };
        self.render_composite(frame, area, ctx.theme, &panels, ctx.views.active_project_table_state_mut());
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::Project.into()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::table_view::{Alignment, CellTone, CellValue, ProjectedColumn, ProjectedRow, TableMeta, TableView, WidthHint};

    fn panel(kind: ProjectPanelKind, title: &str, target: &str, row: &str) -> ProjectPanel {
        ProjectPanel {
            kind,
            target: target.parse().expect("valid address"),
            table: TableView {
                title: title.into(),
                columns: vec![ProjectedColumn {
                    id: "name",
                    label: "NAME",
                    width: WidthHint::Flexible { minimum: 8, weight: 1 },
                    alignment: Alignment::Left,
                }],
                rows: vec![ProjectedRow {
                    id: RowId::new(row),
                    cells: vec![CellValue { text: row.into(), tone: CellTone::Plain }],
                    drill: None,
                    describe: vec![],
                    actions: vec![],
                }],
                meta: TableMeta::default(),
            },
        }
    }

    #[test]
    fn composite_renders_the_four_panels_as_one_stacked_page() {
        let panels = vec![
            panel(ProjectPanelKind::Convoys, "Convoys", "convoys/flotilla", "convoy-a"),
            panel(ProjectPanelKind::Checkouts, "Checkouts", "checkouts?project=flotilla%2Froadmap", "checkout-a"),
            panel(ProjectPanelKind::Issues, "Issues", "issues?project=flotilla%2Froadmap", "issue-a"),
            panel(ProjectPanelKind::Independents, "Independents", "independents?project=flotilla%2Froadmap", "governor"),
        ];
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("terminal");
        let mut widget = ProjectPageWidget::default();
        let mut state = ProjectTableState::default();
        terminal.draw(|frame| widget.render_composite(frame, frame.area(), &Theme::classic(), &panels, &mut state)).expect("render");

        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        let convoy = rendered.find("Convoys").expect("convoys header");
        let checkout = rendered.find("Checkouts").expect("checkouts header");
        let issue = rendered.find("Issues").expect("issues header");
        let independents = rendered.find("Independents").expect("independents header");
        assert!(convoy < checkout && checkout < issue && issue < independents, "panels should retain the fixed v1 order");
        assert!(rendered.contains("convoy-a"));
        assert!(rendered.contains("checkout-a"));
        assert!(rendered.contains("issue-a"));
        assert!(rendered.contains("governor"));
    }

    #[test]
    fn empty_issue_panel_can_be_selected_and_expanded_or_fetched() {
        let mut panels = vec![
            panel(ProjectPanelKind::Convoys, "Convoys", "convoys/flotilla", "convoy-a"),
            panel(ProjectPanelKind::Checkouts, "Checkouts", "checkouts?project=flotilla%2Froadmap", "checkout-a"),
            panel(ProjectPanelKind::Issues, "Issues", "issues?project=flotilla%2Froadmap", "placeholder"),
        ];
        panels[2].table.rows.clear();
        panels[2].table.meta.has_more = true;
        let layouts = ProjectPageWidget::layouts(&panels);
        let mut state = ProjectTableState::default();
        state.set_active(ProjectPanelKind::Checkouts);

        ProjectPageWidget::select_delta(&layouts, &mut state, 1);

        assert_eq!(state.active(), ProjectPanelKind::Issues);
        assert!(ProjectPageWidget::fetch_more_query(&layouts, &state, crate::widgets::table::FetchTrigger::Explicit).is_some());
        assert!(matches!(ProjectPageWidget::active_action(&layouts, &state), Some(AppAction::DrillView(ViewAddress::Issues { .. }))));
    }

    #[test]
    fn populated_panel_header_is_keyboard_focusable_and_expands_independently_of_rows() {
        let panels = vec![
            panel(ProjectPanelKind::Convoys, "Convoys", "convoys/flotilla", "convoy-a"),
            panel(ProjectPanelKind::Checkouts, "Checkouts", "checkouts?project=flotilla%2Froadmap", "checkout-a"),
            panel(ProjectPanelKind::Issues, "Issues", "issues?project=flotilla%2Froadmap", "issue-a"),
        ];
        let layouts = ProjectPageWidget::layouts(&panels);
        let mut state = ProjectTableState::default();
        ProjectPageWidget::reconcile(&layouts, &mut state);

        ProjectPageWidget::select_delta(&layouts, &mut state, -1);

        assert!(state.header_focused());
        assert!(matches!(ProjectPageWidget::active_action(&layouts, &state), Some(AppAction::DrillView(ViewAddress::Convoys { .. }))));
    }

    #[test]
    fn panel_navigation_crosses_a_growing_issue_window_without_disabling_fetch_on_scroll() {
        let mut panels = vec![
            panel(ProjectPanelKind::Issues, "Issues", "issues?project=flotilla%2Froadmap", "issue-0"),
            panel(ProjectPanelKind::Independents, "Independents", "independents?project=flotilla%2Froadmap", "governor"),
        ];
        for index in 1..8 {
            let mut row = panels[0].table.rows[0].clone();
            row.id = RowId::new(format!("issue-{index}"));
            panels[0].table.rows.push(row);
        }
        panels[0].table.meta.has_more = true;

        let mut state = ProjectTableState::default();
        state.set_active(ProjectPanelKind::Issues);
        state.focus_rows();
        state.table_mut(ProjectPanelKind::Issues).select_index(&panels[0].table, 0);

        for keypress in 0..20 {
            let layouts = ProjectPageWidget::layouts(&panels);
            ProjectPageWidget::select_delta(&layouts, &mut state, 1);
            if ProjectPageWidget::fetch_more_query(&layouts, &state, crate::widgets::table::FetchTrigger::NearBottom).is_some() {
                let mut row = panels[0].table.rows[0].clone();
                row.id = RowId::new(format!("fetched-{keypress}"));
                panels[0].table.rows.push(row);
            }
            ProjectPageWidget::reconcile(&ProjectPageWidget::layouts(&panels), &mut state);
        }

        assert!(panels[0].table.rows.len() > 8, "near-bottom row navigation should still fetch more issues");
        assert_eq!(state.active(), ProjectPanelKind::Issues, "the growing issue window should reproduce the row-navigation trap");
        ProjectPageWidget::select_panel_delta(&ProjectPageWidget::layouts(&panels), &mut state, 1);
        assert_eq!(state.active(), ProjectPanelKind::Independents);
        assert!(state.header_focused());
    }

    #[test]
    fn each_panel_keeps_its_own_local_filter() {
        let panels = vec![
            panel(ProjectPanelKind::Convoys, "Convoys", "convoys/flotilla", "convoy-a"),
            panel(ProjectPanelKind::Checkouts, "Checkouts", "checkouts?project=flotilla%2Froadmap", "checkout-a"),
            panel(ProjectPanelKind::Issues, "Issues", "issues?project=flotilla%2Froadmap", "issue-a"),
        ];
        let mut state = ProjectTableState::default();
        state.table_mut(ProjectPanelKind::Checkouts).filter = "missing".into();

        let panels = ProjectPageWidget::apply_filters(panels, &state);

        assert_eq!(panels[0].table.rows.len(), 1);
        assert!(panels[1].table.rows.is_empty());
        assert_eq!(panels[2].table.rows.len(), 1);
    }
}
