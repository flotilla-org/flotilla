use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
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
    table_view::{self, Alignment, CellTone, ProjectedColumn, RowId, TableView, WidthHint},
    theme::Theme,
};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct TableWidget {
    rows_area: Rect,
    last_click: Option<(RowId, Instant)>,
}

impl TableWidget {
    fn projected(ctx: &WidgetContext<'_>) -> Result<TableView, String> {
        let address = ctx.views.active_address().ok_or_else(|| "active view has no valid address".to_string())?;
        let filter = ctx.views.active_table_state().filter.clone();
        let rows = crate::app::table_rows(ctx.namespaces);
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

    pub fn render_table(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        view: &TableView,
        state: &mut table_view::TableState,
        breadcrumbs: &[String],
    ) {
        state.reconcile(view);
        let block = Block::bordered().style(theme.block_style()).title(format!(" {} ", view.title));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let breadcrumb_height = u16::from(!breadcrumbs.is_empty());
        if breadcrumb_height == 1 {
            let breadcrumb_area = Rect { height: 1, ..inner };
            frame.render_widget(Line::styled(breadcrumbs.join("  ›  "), Style::default().fg(theme.muted)), breadcrumb_area);
        }
        let table_area =
            Rect { y: inner.y.saturating_add(breadcrumb_height), height: inner.height.saturating_sub(breadcrumb_height), ..inner };
        self.rows_area = Rect { y: table_area.y.saturating_add(1), height: table_area.height.saturating_sub(1), ..table_area };
        state.ensure_selected_visible(view, self.rows_area.height as usize);

        let header =
            Row::new(view.columns.iter().map(|column| Cell::from(Line::from(column.label).alignment(ratatui_alignment(column.alignment)))))
                .style(theme.header_style())
                .height(1);
        let rows = view.rows.iter().skip(state.scroll_offset).take(self.rows_area.height as usize).map(|row| {
            let selected = state.selected() == Some(&row.id);
            let cells = row.cells.iter().zip(&view.columns).map(|(cell, column)| {
                Cell::from(Line::from(cell.text.clone()).alignment(ratatui_alignment(column.alignment))).style(cell_style(cell.tone, theme))
            });
            let style = if selected { Style::default().bg(theme.row_highlight).add_modifier(Modifier::BOLD) } else { Style::default() };
            Row::new(cells).style(style)
        });
        let widths = width_constraints(&view.columns, table_area.width);
        frame.render_widget(Table::new(rows, widths).header(header).column_spacing(1), table_area);
    }
}

fn ratatui_alignment(alignment: Alignment) -> RatatuiAlignment {
    match alignment {
        Alignment::Left => RatatuiAlignment::Left,
        Alignment::Right => RatatuiAlignment::Right,
    }
}

fn width_constraints(columns: &[ProjectedColumn], available_width: u16) -> Vec<Constraint> {
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

fn cell_style(tone: CellTone, theme: &Theme) -> Style {
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
        let Ok(view) = Self::projected(ctx) else { return Outcome::Ignored };
        ctx.views.active_table_state_mut().reconcile(&view);
        match action {
            Action::SelectNext => {
                ctx.views.active_table_state_mut().select_delta(&view, 1);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                ctx.views.active_table_state_mut().select_delta(&view, -1);
                Outcome::Consumed
            }
            Action::Confirm => {
                if let Some(target) = ctx.views.active_table_state().selected_row(&view).and_then(|row| row.drill.clone()) {
                    ctx.app_actions.push(AppAction::DrillView(target));
                }
                Outcome::Consumed
            }
            Action::Dismiss => {
                ctx.app_actions.push(AppAction::BackView);
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
            if let Some(target) = &row.drill {
                ctx.app_actions.push(AppAction::DrillView(target.clone()));
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
        let rows = crate::app::table_rows(ctx.namespaces);
        let Ok(view) = table_view::project(&address, &rows).map(|view| view.filtered(&filter)) else { return };
        let breadcrumbs = ctx.views.active().breadcrumb_addresses().into_iter().map(ToString::to_string).collect::<Vec<_>>();
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

        let convoys = [
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
        let rows = convoys.iter().collect::<Vec<_>>();
        table_view::project(&"convoys/dev".parse().expect("valid address"), &table_view::TableRows { convoys: rows, independents: vec![] })
            .expect("project snapshot table")
    }

    #[test]
    fn reusable_table_renders_breadcrumb_headers_and_selected_row() {
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("terminal");
        let mut widget = TableWidget::default();
        let mut state = TableState::default();
        terminal
            .draw(|frame| {
                widget.render_table(frame, frame.area(), &Theme::classic(), &view(), &mut state, &["convoys/dev".into()]);
            })
            .expect("draw");
        let rendered = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("convoys/dev"));
        assert!(rendered.contains("CONVOY"));
        assert!(rendered.contains("tables"));
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
        terminal
            .draw(|frame| {
                widget.render_table(frame, frame.area(), &Theme::classic(), &snapshot_view(), &mut state, &["convoys/dev".into()]);
            })
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }
}
