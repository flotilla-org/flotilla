//! Right-pane convoy detail widget.

use flotilla_protocol::namespace::ConvoySummary;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use super::glyphs::{convoy_glyph, task_glyph};

pub struct ConvoyDetail<'a> {
    pub convoy: &'a ConvoySummary,
}

impl<'a> ConvoyDetail<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(0)]).split(area);

        // Header
        let glyph = convoy_glyph(self.convoy.phase);
        let header = Paragraph::new(Line::from(vec![
            Span::styled(glyph.symbol, glyph.style),
            Span::raw(format!(" {} ", self.convoy.name)),
            Span::raw(format!("[{}]", self.convoy.workflow_ref)),
        ]))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(header, chunks[0]);

        // Body: task tree OR initializing placeholder
        let body_block = Block::default().borders(Borders::ALL).title(" Tasks ");
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

        // TODO(task-attach): lift tree expansion state into ConvoysUiState once
        // tree interaction (expand/collapse) lands with the task-attach PR.
        let mut state = TreeState::default();
        let tree = Tree::new(&items).expect("unique task names").block(body_block);
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
                ConvoyDetail { convoy: &convoy }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn convoy_detail_initializing_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let mut convoy = multi_task_convoy();
        convoy.initializing = true;
        convoy.tasks.clear();
        terminal
            .draw(|f| {
                ConvoyDetail { convoy: &convoy }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
