use std::{
    collections::HashSet,
    ops::Range,
    time::{Duration, Instant},
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::ViewAddress;
use ratatui::{
    layout::{Alignment as RatatuiAlignment, Constraint, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Cell, Row, Table},
    Frame,
};

use super::{
    describe::DescribeWidget, table_action_menu::TableActionMenuWidget, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    shimmer::Shimmer,
    table_view::{self, Alignment, CellTone, ProjectedColumn, RowId, RowState, TableView, WidthHint},
    theme::Theme,
};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchTrigger {
    NearBottom,
    Explicit,
}

/// Reusable presentation and interaction policy for a curated table embedded
/// in any surface. `TableWidget` adds its border and breadcrumbs; the Project
/// page composes several panels into its one scrolling document.
pub(crate) struct TablePanel;

pub(crate) struct RowDecorations<'a> {
    pub(crate) multi_selected: &'a HashSet<RowId>,
}

impl TablePanel {
    pub(crate) fn row_action(row: &table_view::ProjectedRow) -> Option<AppAction> {
        if let Some(target) = row.drill.clone() {
            Some(AppAction::DrillView(target))
        } else if let [action] = row.actions.as_slice() {
            Some(AppAction::ExecuteTableIntent(action.intent.clone()))
        } else {
            None
        }
    }

    pub(crate) fn should_fetch_more(view: &TableView, state: &table_view::TableState, trigger: FetchTrigger) -> bool {
        view.meta.has_more
            && (trigger == FetchTrigger::Explicit || view.rows.len().saturating_sub(state.selected_index(view).unwrap_or(0) + 1) <= 5)
    }

    pub(crate) fn render_header(frame: &mut Frame, area: Rect, theme: &Theme, view: &TableView) {
        let header =
            Row::new(view.columns.iter().map(|column| Cell::from(Line::from(column.label).alignment(ratatui_alignment(column.alignment)))))
                .style(theme.header_style());
        let widths = width_constraints(&view.columns, area.width);
        frame.render_widget(Table::new(Vec::<Row>::new(), widths).header(header).column_spacing(1), area);
    }

    pub(crate) fn render_rows(
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        view: &TableView,
        state: &table_view::TableState,
        decorations: RowDecorations<'_>,
        visible: Range<usize>,
    ) {
        Self::render_rows_with_elapsed(frame, area, theme, view, state, decorations, visible, None);
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_rows_at(
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        view: &TableView,
        state: &table_view::TableState,
        decorations: RowDecorations<'_>,
        visible: Range<usize>,
        elapsed: Duration,
    ) {
        Self::render_rows_with_elapsed(frame, area, theme, view, state, decorations, visible, Some(elapsed));
    }

    #[allow(clippy::too_many_arguments)]
    fn render_rows_with_elapsed(
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        view: &TableView,
        state: &table_view::TableState,
        decorations: RowDecorations<'_>,
        visible: Range<usize>,
        elapsed: Option<Duration>,
    ) {
        let first = visible.start;
        let count = visible.end.saturating_sub(visible.start);
        if count == 0 {
            return;
        }
        let widths = width_constraints(&view.columns, area.width);
        let column_widths = widths
            .iter()
            .map(|constraint| match constraint {
                Constraint::Length(width) => *width as usize,
                _ => unreachable!("table width constraints are always resolved lengths"),
            })
            .collect::<Vec<_>>();
        let rows = view.rows.iter().skip(first).take(count).map(|row| {
            let selected = state.selected() == Some(&row.id);
            let multi = decorations.multi_selected.contains(&row.id);
            let row_state = state.row_state(&row.id);
            let shimmer = row_state.filter(|state| state.is_pending()).map(|_| {
                elapsed.map_or_else(
                    || Shimmer::new(area.width as usize, theme),
                    |elapsed| Shimmer::new_at(area.width as usize, elapsed, theme),
                )
            });
            let mut offset = 0usize;
            let cells = row.cells.iter().zip(&view.columns).enumerate().map(|(index, (cell, column))| {
                let mut text = cell.text.clone();
                if index == 0 {
                    if matches!(row_state, Some(RowState::Failed { .. })) {
                        text = format!("x {text}");
                    } else if multi {
                        text = format!("* {text}");
                    }
                }
                let line = match &shimmer {
                    Some(shimmer) => Line::from(shimmer.spans(&text, offset)).alignment(ratatui_alignment(column.alignment)),
                    None => Line::from(text).alignment(ratatui_alignment(column.alignment)),
                };
                offset += column_widths[index] + 1;
                let rendered = Cell::from(line);
                if shimmer.is_some() {
                    rendered
                } else {
                    rendered.style(cell_style(cell.tone, theme))
                }
            });
            let style = if selected {
                Style::default().bg(theme.row_highlight).add_modifier(Modifier::BOLD)
            } else if multi {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new(cells).style(style)
        });
        frame.render_widget(Table::new(rows, widths).column_spacing(1), area);
    }
}

#[derive(Default)]
pub struct TableWidget {
    rows_area: Rect,
    last_click: Option<(RowId, Instant)>,
}

impl TableWidget {
    fn projected(ctx: &WidgetContext<'_>) -> Result<TableView, String> {
        let address = ctx.views.active_address().ok_or_else(|| "active view has no valid address".to_string())?;
        let filter = ctx.views.active_table_state().filter.clone();
        let source_search = ctx.views.active_table_state().source_search.as_deref();
        let rows = crate::app::table_rows(ctx.namespaces, ctx.query_tables, source_search);
        table_view::project(address, &rows).map(|view| view.filtered(&filter))
    }

    fn click_row(&self, mouse: MouseEvent, row_count: usize) -> Option<usize> {
        if !self.contains_rows_area(mouse) {
            return None;
        }
        let index = (mouse.row - self.rows_area.y) as usize;
        (index < row_count).then_some(index)
    }

    fn contains_rows_area(&self, mouse: MouseEvent) -> bool {
        !(mouse.column < self.rows_area.x
            || mouse.column >= self.rows_area.x.saturating_add(self.rows_area.width)
            || mouse.row < self.rows_area.y
            || mouse.row >= self.rows_area.y.saturating_add(self.rows_area.height))
    }

    fn demand_query(ctx: &WidgetContext<'_>) -> Option<flotilla_protocol::QueryId> {
        let address = ctx.views.active_address()?;
        table_view::query_for(address, ctx.views.active_table_state().source_search.as_deref())
    }

    fn request_more_if_needed(view: &TableView, ctx: &mut WidgetContext<'_>, trigger: FetchTrigger) {
        if !view.meta.has_more {
            return;
        }
        if TablePanel::should_fetch_more(view, ctx.views.active_table_state(), trigger) {
            if let Some(query) = Self::demand_query(ctx) {
                ctx.app_actions.push(AppAction::FetchMore(query));
            }
        }
    }

    pub fn render_table(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        view: &TableView,
        state: &mut table_view::TableState,
        breadcrumbs: &[ViewAddress],
    ) {
        state.reconcile(view);
        let mut title = view.title.clone();
        if !state.filter.is_empty() {
            title.push_str(&format!(" · find \"{}\"", state.filter));
        }
        if let Some(as_of) = view.meta.as_of {
            title.push_str(&format!(" · as of {}", as_of.format("%Y-%m-%d %H:%M")));
        }
        if view.meta.has_more {
            title.push_str(" · more available");
        }
        if view.meta.availability == table_view::TableAvailability::Loading {
            title.push_str(" · loading");
        }
        if !view.meta.conditions.is_empty() {
            title.push_str(&format!(" · ⚠ {}", view.meta.conditions.join("; ")));
        }
        let block = Block::bordered().style(theme.block_style()).title(format!(" {title} "));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let breadcrumb_height = u16::from(!breadcrumbs.is_empty());
        if breadcrumb_height == 1 {
            let breadcrumb_area = Rect { height: 1, ..inner };
            let labels = breadcrumbs.iter().map(ViewAddress::human_label).collect::<Vec<_>>();
            frame.render_widget(Line::styled(labels.join("  ›  "), Style::default().fg(theme.muted)), breadcrumb_area);
        }
        let table_area =
            Rect { y: inner.y.saturating_add(breadcrumb_height), height: inner.height.saturating_sub(breadcrumb_height), ..inner };
        self.rows_area = Rect { y: table_area.y.saturating_add(1), height: table_area.height.saturating_sub(1), ..table_area };
        state.ensure_selected_visible(view, self.rows_area.height as usize);

        TablePanel::render_header(frame, Rect { height: 1, ..table_area }, theme, view);
        TablePanel::render_rows(
            frame,
            self.rows_area,
            theme,
            view,
            state,
            RowDecorations { multi_selected: &state.multi_selected },
            state.scroll_offset..state.scroll_offset.saturating_add(self.rows_area.height as usize),
        );
    }
}

pub(crate) fn ratatui_alignment(alignment: Alignment) -> RatatuiAlignment {
    match alignment {
        Alignment::Left => RatatuiAlignment::Left,
        Alignment::Right => RatatuiAlignment::Right,
    }
}

pub(crate) fn width_constraints(columns: &[ProjectedColumn], available_width: u16) -> Vec<Constraint> {
    let spacing = columns.len().saturating_sub(1) as u16;
    let minimum_width = columns.iter().fold(spacing, |total, column| {
        total.saturating_add(match column.width {
            WidthHint::Fixed(width) => width,
            WidthHint::Flexible { minimum, .. } => minimum,
        })
    });
    let extra = available_width.saturating_sub(minimum_width);
    let total_weight = columns
        .iter()
        .filter_map(|column| match column.width {
            WidthHint::Flexible { weight, .. } => Some(weight),
            WidthHint::Fixed(_) => None,
        })
        .sum::<u16>()
        .max(1);
    columns
        .iter()
        .map(|column| {
            let width = match column.width {
                WidthHint::Fixed(width) => width,
                WidthHint::Flexible { minimum, weight } => minimum.saturating_add(extra.saturating_mul(weight) / total_weight),
            };
            Constraint::Length(width)
        })
        .collect()
}

pub(crate) fn cell_style(tone: CellTone, theme: &Theme) -> Style {
    let color: Color = match tone {
        CellTone::Plain => theme.text,
        CellTone::Muted => theme.muted,
        CellTone::Success => theme.status_ok,
        CellTone::Warning => theme.warning,
        CellTone::Error => theme.error,
    };
    Style::default().fg(color)
}

impl InteractiveWidget for TableWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        if action == Action::Dismiss && ctx.views.active_table_state().source_search.is_some() {
            ctx.app_actions.push(AppAction::SetSourceSearch(None));
            return Outcome::Consumed;
        }
        if action == Action::Dismiss && !ctx.views.active_table_state().filter.is_empty() {
            ctx.app_actions.push(AppAction::SetTableFilter(String::new()));
            return Outcome::Consumed;
        }
        if action == Action::OpenFind {
            return Outcome::Push(Box::new(super::table_search::TableSearchWidget::find(&ctx.views.active_table_state().filter)));
        }
        let Ok(view) = Self::projected(ctx) else { return Outcome::Ignored };
        ctx.views.active_table_state_mut().reconcile(&view);
        match action {
            Action::SelectNext => {
                ctx.views.active_table_state_mut().select_delta(&view, 1);
                Self::request_more_if_needed(&view, ctx, FetchTrigger::NearBottom);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                ctx.views.active_table_state_mut().select_delta(&view, -1);
                Outcome::Consumed
            }
            Action::ToggleMultiSelect => {
                ctx.views.active_table_state_mut().toggle_selected_row();
                Outcome::Consumed
            }
            Action::Confirm => {
                if let Some(action) = ctx.views.active_table_state().selected_row(&view).and_then(TablePanel::row_action) {
                    ctx.app_actions.push(action);
                }
                Outcome::Consumed
            }
            Action::Dismiss => {
                ctx.app_actions.push(AppAction::BackView);
                Outcome::Consumed
            }
            Action::FetchMore => {
                Self::request_more_if_needed(&view, ctx, FetchTrigger::Explicit);
                Outcome::Consumed
            }
            Action::Describe => match ctx.views.active_table_state().selected_row(&view) {
                Some(row) => Outcome::Push(Box::new(DescribeWidget::new(view.title.clone(), row.describe.clone()))),
                None => Outcome::Consumed,
            },
            Action::OpenActionMenu => match ctx.views.active_table_state().selected_row(&view) {
                Some(row) if !row.actions.is_empty() => Outcome::Push(Box::new(TableActionMenuWidget::new(row.actions.clone()))),
                Some(_) => {
                    ctx.app_actions.push(AppAction::ShowStatus("No actions available for the selected row".into()));
                    Outcome::Consumed
                }
                None => {
                    ctx.app_actions.push(AppAction::ShowStatus("No table row selected".into()));
                    Outcome::Consumed
                }
            },
            Action::OpenInPm => {
                if let Some(intent) = ctx
                    .views
                    .active_table_state()
                    .selected_row(&view)
                    .and_then(|row| row.actions.iter().find(|action| action.id == "open_in_pm"))
                    .map(|action| action.intent.clone())
                {
                    ctx.app_actions.push(AppAction::ExecuteTableIntent(intent));
                }
                Outcome::Consumed
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        let Ok(view) = Self::projected(ctx) else { return Outcome::Ignored };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if !self.contains_rows_area(mouse) {
                    return Outcome::Ignored;
                }
                ctx.views.active_table_state_mut().select_delta(&view, 1);
                Self::request_more_if_needed(&view, ctx, FetchTrigger::NearBottom);
                return Outcome::Consumed;
            }
            MouseEventKind::ScrollUp => {
                if !self.contains_rows_area(mouse) {
                    return Outcome::Ignored;
                }
                ctx.views.active_table_state_mut().select_delta(&view, -1);
                return Outcome::Consumed;
            }
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {}
            _ => return Outcome::Ignored,
        }
        let Some(visible_index) = self.click_row(mouse, view.rows.len().saturating_sub(ctx.views.active_table_state().scroll_offset))
        else {
            return Outcome::Ignored;
        };
        let index = ctx.views.active_table_state().scroll_offset + visible_index;
        ctx.views.active_table_state_mut().select_index(&view, index);
        Self::request_more_if_needed(&view, ctx, FetchTrigger::NearBottom);
        let row = &view.rows[index];
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
            .is_some_and(|(last_row, last_at)| last_row == &row.id && now.duration_since(*last_at) <= DOUBLE_CLICK_WINDOW);
        self.last_click = Some((row.id.clone(), now));
        if double_click {
            self.last_click = None;
            if let Some(action) = TablePanel::row_action(row) {
                ctx.app_actions.push(action);
            }
        }
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        // `Screen` owns production base-page rendering so it can surface
        // projection failures as an error page. This implementation satisfies
        // the shared widget contract for direct embedding and test harnesses.
        let Some(address) = ctx.views.active_address().cloned() else { return };
        let filter = ctx.views.active_table_state().filter.clone();
        let source_search = ctx.views.active_table_state().source_search.as_deref();
        let rows = crate::app::table_rows(ctx.namespaces, ctx.query_tables, source_search);
        let Ok(view) = table_view::project(&address, &rows).map(|view| view.filtered(&filter)) else { return };
        let breadcrumbs = ctx.views.active().breadcrumb_addresses();
        self.render_table(frame, area, ctx.theme, &view, ctx.views.active_table_state_mut(), &breadcrumbs);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::Convoys.into()
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
    use crate::{
        convoy_model::{ConvoyId, ConvoyPhase, ConvoySummary, VesselSummary, WorkPhase},
        table_view::{Alignment, CellValue, ProjectedColumn, ProjectedRow, TableState},
    };

    fn view() -> TableView {
        TableView {
            title: "Convoys · dev".into(),
            columns: vec![ProjectedColumn {
                id: "name",
                label: "CONVOY",
                width: WidthHint::Flexible { minimum: 8, weight: 1 },
                alignment: Alignment::Left,
            }],
            rows: vec![ProjectedRow {
                id: RowId::new("dev/tables"),
                cells: vec![CellValue { text: "tables".into(), tone: CellTone::Plain }],
                drill: Some("convoy/dev/tables".parse().expect("valid address")),
                describe: vec![],
                actions: vec![],
            }],
            meta: Default::default(),
        }
    }

    fn snapshot_view() -> TableView {
        fn vessel(name: &str, phase: WorkPhase) -> VesselSummary {
            VesselSummary::builder().name(name.to_string()).depends_on(Vec::new()).phase(phase).crew(Vec::new()).build()
        }

        fn convoy(name: &str, phase: ConvoyPhase, initializing: bool, vessels: Vec<VesselSummary>, message: Option<&str>) -> ConvoySummary {
            ConvoySummary::builder()
                .id(ConvoyId::new("dev", name))
                .namespace("dev".to_string())
                .name(name.to_string())
                .workflow_ref("implement-review".to_string())
                .phase(phase)
                .maybe_message(message.map(ToString::to_string))
                .project_ref("flotilla".to_string())
                .vessels(vessels)
                .initializing(initializing)
                .build()
        }

        let mut convoys = [
            convoy(
                "tables",
                ConvoyPhase::Pending,
                true,
                vec![vessel("implement", WorkPhase::Pending), vessel("review", WorkPhase::Pending)],
                Some("waiting for workflow status"),
            ),
            convoy(
                "release",
                ConvoyPhase::Active,
                false,
                vec![vessel("implement", WorkPhase::Complete), vessel("review", WorkPhase::Running), vessel("publish", WorkPhase::Pending)],
                Some("review is running"),
            ),
            convoy(
                "docs",
                ConvoyPhase::Completed,
                false,
                vec![vessel("write", WorkPhase::Complete), vessel("review", WorkPhase::Complete)],
                None,
            ),
            convoy(
                "deploy",
                ConvoyPhase::Failed,
                false,
                vec![vessel("build", WorkPhase::Complete), vessel("ship", WorkPhase::Failed)],
                Some("workspace launch failed"),
            ),
        ];
        convoys[1].change_request = Some(flotilla_protocol::ConvoyChangeRequest {
            id: "815".into(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            repository_key: flotilla_protocol::RepositoryKey("repo_flotilla".into()),
        });
        convoys[2].change_request = Some(flotilla_protocol::ConvoyChangeRequest {
            id: "812".into(),
            status: flotilla_protocol::ChangeRequestStatus::Merged,
            repository_key: flotilla_protocol::RepositoryKey("repo_flotilla".into()),
        });
        let rows = convoys.iter().collect::<Vec<_>>();
        table_view::project(&"convoys/dev".parse().expect("valid address"), &table_view::TableRows {
            convoys: rows,
            ..table_view::TableRows::default()
        })
        .expect("project snapshot table")
    }

    #[test]
    fn reusable_table_renders_breadcrumb_headers_and_selected_row() {
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("terminal");
        let mut widget = TableWidget::default();
        let mut state = TableState::default();
        let address: ViewAddress = "convoys/dev".parse().expect("address");
        terminal
            .draw(|frame| {
                widget.render_table(frame, frame.area(), &Theme::classic(), &view(), &mut state, &[address]);
            })
            .expect("draw");
        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("convoys/dev"));
        assert!(rendered.contains("CONVOY"));
        assert!(rendered.contains("tables"));
    }

    #[test]
    fn pending_row_renders_an_animated_shimmer() {
        let mut terminal = Terminal::new(TestBackend::new(30, 1)).expect("terminal");
        let view = view();
        let row_id = view.rows[0].id.clone();
        let mut state = TableState::default();
        state.begin_pending(flotilla_protocol::QueryId::Convoys, row_id, "Delete convoy".into()).expect("pending row");
        terminal
            .draw(|frame| {
                TablePanel::render_rows_at(
                    frame,
                    frame.area(),
                    &Theme::classic(),
                    &view,
                    &state,
                    RowDecorations { multi_selected: &state.multi_selected },
                    0..1,
                    Duration::from_millis(500),
                );
            })
            .expect("draw");

        let styles = terminal.backend().buffer().content()[..6].iter().map(|cell| cell.style()).collect::<Vec<_>>();
        assert!(styles.windows(2).any(|pair| pair[0] != pair[1]), "the shimmer band should vary styling across the pending row");
    }

    #[test]
    fn row_hit_testing_excludes_breadcrumb_and_header() {
        let widget = TableWidget { rows_area: Rect::new(1, 3, 20, 4), last_click: None };
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(widget.click_row(click(2), 2), None);
        assert_eq!(widget.click_row(click(3), 2), Some(0));
        assert_eq!(widget.click_row(click(4), 2), Some(1));
    }

    #[test]
    fn curated_table_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).expect("terminal");
        let mut widget = TableWidget::default();
        let mut state = TableState::default();
        let address: ViewAddress = "convoys/dev".parse().expect("address");
        terminal
            .draw(|frame| {
                widget.render_table(frame, frame.area(), &Theme::classic(), &snapshot_view(), &mut state, &[address]);
            })
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn scoped_issue_metadata_and_condition_snapshot() {
        let issue = flotilla_protocol::test_support::TestIssue::new("Fix pagination").id("ENG-42").build();
        let row = flotilla_protocol::IssueRow { reference: issue.reference.clone(), issue };
        let scope = flotilla_protocol::QueryScope::new("flotilla", "roadmap");
        let query = flotilla_protocol::QueryId::Issues { scope, search: None };
        let state = flotilla_protocol::ResultSetState {
            demand: Some(flotilla_protocol::DemandBackedMetadata {
                as_of: "2026-07-20T12:00:00Z".parse().expect("timestamp"),
                has_more: true,
            }),
            conditions: vec![flotilla_protocol::ResultSetCondition::IssueSourceUnavailable {
                source: None,
                message: "one source unavailable".into(),
            }],
        };
        let view = table_view::project(&"issues?project=flotilla%2Froadmap".parse().expect("address"), &table_view::TableRows {
            issue_results: vec![table_view::QueryRows { query: &query, rows: std::slice::from_ref(&row), state: &state }],
            ..table_view::TableRows::default()
        })
        .expect("issue table");
        let mut terminal = Terminal::new(TestBackend::new(110, 7)).expect("terminal");
        let mut widget = TableWidget::default();
        let mut table_state = TableState::default();
        let address: ViewAddress = "issues?project=flotilla%2Froadmap".parse().expect("address");
        terminal
            .draw(|frame| {
                widget.render_table(frame, frame.area(), &Theme::classic(), &view, &mut table_state, &[address]);
            })
            .expect("draw");

        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("issues?project=flotilla/roadmap"));
        assert!(!rendered.contains("%2F"));
        insta::assert_snapshot!(terminal.backend());
    }
}
