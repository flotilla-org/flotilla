//! Right-pane convoy detail widget.

use flotilla_protocol::namespace::ConvoySummary;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use super::glyphs::{convoy_glyph, task_glyph};

pub struct ConvoyDetail<'a> {
    pub convoy: &'a ConvoySummary,
    pub selected_task: Option<&'a str>,
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

        // Body: task tree OR initializing placeholder
        let body_block = Block::default().borders(Borders::ALL).border_style(border_style).title(" Tasks ");
        let body_area = chunks[1];
        if self.convoy.initializing {
            let p = Paragraph::new("initializing…").block(body_block);
            f.render_widget(p, body_area);
            return;
        }

        let items: Vec<TreeItem<String>> = self
            .convoy
            .tasks
            .iter()
            .map(|t| {
                let g = task_glyph(t.phase);
                let label =
                    Line::from(vec![Span::styled(g.symbol, g.style), Span::raw(format!(" {} ({} proc)", t.name, t.processes.len()))]);
                TreeItem::new_leaf(t.name.clone(), label)
            })
            .collect();

        // TreeState is built from `selected_task` on every render. Selection lives
        // in `ConvoysUiState` (the source of truth); expansion isn't needed because
        // tasks render as flat leaves today. Lift TreeState if we get nested tasks.
        let mut state = TreeState::default();
        if let Some(name) = self.selected_task {
            state.select(vec![name.to_string()]);
        }
        let tree = Tree::new(&items)
            .expect("unique task names")
            .block(body_block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(tree, body_area, &mut state);
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::namespace::{ConvoyId, ConvoyPhase, ConvoySummary, ProcessSummary, TaskPhase, TaskSummary};
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn multi_task_convoy() -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("flotilla", "fix-bug-123"),
            namespace: "flotilla".into(),
            name: "fix-bug-123".into(),
            workflow_ref: "review-and-fix".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            tasks: vec![
                TaskSummary {
                    name: "implement".into(),
                    depends_on: vec![],
                    phase: TaskPhase::Running,
                    processes: vec![ProcessSummary { role: "coder".into(), command_preview: "claude".into() }],
                    host: None,
                    checkout: None,
                    workspace_ref: None,
                    ready_at: None,
                    started_at: None,
                    finished_at: None,
                    message: None,
                },
                TaskSummary {
                    name: "review".into(),
                    depends_on: vec!["implement".into()],
                    phase: TaskPhase::Pending,
                    processes: vec![ProcessSummary { role: "reviewer".into(), command_preview: "claude".into() }],
                    host: None,
                    checkout: None,
                    workspace_ref: None,
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
                ConvoyDetail { convoy: &convoy, selected_task: None, focused: false }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn convoy_detail_with_selected_task_renders() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let convoy = multi_task_convoy();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_task: Some("review"), focused: true }.render(f, f.area());
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
        convoy.tasks.clear();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy, selected_task: None, focused: false }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
