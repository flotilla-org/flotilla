//! Right-pane convoy detail widget.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use super::glyphs::{convoy_glyph, work_glyph};
use crate::convoy_model::ConvoySummary;

pub struct ConvoyDetail<'a> {
    pub convoy: &'a ConvoySummary,
    pub selected_vessel: Option<&'a str>,
    pub focused: bool,
}

impl<'a> ConvoyDetail<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(0)]).split(area);

        let border_style = if self.focused { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };

        // Header
        let glyph = convoy_glyph(self.convoy.phase);
        let header = Paragraph::new(Line::from(vec![
            Span::styled(glyph.symbol, glyph.style),
            Span::raw(format!(" {} ", self.convoy.name)),
            Span::raw(format!("[{}]", self.convoy.workflow_ref)),
        ]))
        .block(Block::default().borders(Borders::ALL).border_style(border_style));
        f.render_widget(header, chunks[0]);

        // Body: vessel tree OR initializing placeholder
        let body_block = Block::default().borders(Borders::ALL).border_style(border_style).title(" Tasks ");
        let body_area = chunks[1];
        let body_inner = body_block.inner(body_area);
        f.render_widget(body_block, body_area);
        if self.convoy.initializing && !self.convoy.phase.is_terminal() {
            f.render_widget(Paragraph::new("initializing…"), body_inner);
            return;
        }

        let (message_area, tree_area) = if self.convoy.message.is_some() {
            let areas =
                Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(body_inner);
            (Some(areas[0]), areas[1])
        } else {
            (None, body_inner)
        };
        if let (Some(area), Some(message)) = (message_area, self.convoy.message.as_deref()) {
            f.render_widget(Paragraph::new(format!("{}: {message}", self.convoy.phase.label())), area);
        }

        let items: Vec<TreeItem<String>> = self
            .convoy
            .vessels
            .iter()
            .map(|t| {
                let g = work_glyph(t.phase);
                let label = Line::from(vec![Span::styled(g.symbol, g.style), Span::raw(format!(" {} ({} proc)", t.name, t.crew.len()))]);
                TreeItem::new_leaf(t.name.clone(), label)
            })
            .collect();

        // TreeState is built from `selected_vessel` on every render. Selection lives
        // in `ConvoysUiState` (the source of truth); expansion isn't needed because
        // tasks render as flat leaves today. Lift TreeState if we get nested tasks.
        let mut state = TreeState::default();
        if let Some(name) = self.selected_vessel {
            state.select(vec![name.to_string()]);
        }
        let tree = Tree::new(&items).expect("unique task names").highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(tree, tree_area, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::convoy_model::{ConvoyId, ConvoyPhase, ConvoySummary, ProcessSummary, VesselSummary, WorkPhase};

    fn multi_task_convoy() -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("flotilla", "fix-bug-123"),
            namespace: "flotilla".into(),
            name: "fix-bug-123".into(),
            workflow_ref: "review-and-fix".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            vessels: vec![
                VesselSummary {
                    name: "implement".into(),
                    depends_on: vec![],
                    phase: WorkPhase::Running,
                    crew: vec![ProcessSummary { role: "coder".into(), command_preview: "claude".into() }],
                    host: None,
                    checkout: None,
                    workspace_ref: None,
                    completion_target: None,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                },
                VesselSummary {
                    name: "review".into(),
                    depends_on: vec!["implement".into()],
                    phase: WorkPhase::Pending,
                    crew: vec![ProcessSummary { role: "reviewer".into(), command_preview: "claude".into() }],
                    host: None,
                    checkout: None,
                    workspace_ref: None,
                    completion_target: None,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                },
            ],
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        }
    }

    #[test]
    fn convoy_detail_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let convoy = multi_task_convoy();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_vessel: None, focused: false }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn convoy_detail_with_selected_vessel_renders() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let convoy = multi_task_convoy();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_vessel: Some("review"), focused: true }.render(f, f.area());
            })
            .unwrap();
        let rendered: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        // Both task names should appear; selection is style-only so the buffer
        // content check is for parity with the unselected snapshot.
        assert!(rendered.contains("implement"), "expected 'implement' task in render: {rendered}");
        assert!(rendered.contains("review"), "expected 'review' task in render: {rendered}");
    }

    #[test]
    fn convoy_detail_initializing_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let mut convoy = multi_task_convoy();
        convoy.initializing = true;
        convoy.vessels.clear();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_vessel: None, focused: false }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn terminal_failure_is_not_masked_by_initializing_placeholder() {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).expect("create terminal");
        let mut convoy = multi_task_convoy();
        convoy.phase = ConvoyPhase::Failed;
        convoy.message = Some("missing input 'topic'".into());
        convoy.initializing = true;
        convoy.vessels.clear();

        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_vessel: None, focused: false }.render(f, f.area());
            })
            .expect("render convoy detail");

        let rendered: String = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(!rendered.contains("initializing"), "terminal failure was masked: {rendered}");
        assert!(rendered.contains("missing input 'topic'"), "failure message was not rendered: {rendered}");
    }
}
