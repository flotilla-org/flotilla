//! Left-pane convoy list widget.

use flotilla_protocol::namespace::{ConvoyId, ConvoySummary};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use super::glyphs::convoy_glyph;

pub struct ConvoyList<'a> {
    pub convoys: &'a [&'a ConvoySummary],
    pub selected: Option<&'a ConvoyId>,
    pub focused: bool,
}

impl<'a> ConvoyList<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .convoys
            .iter()
            .map(|convoy| {
                let glyph = convoy_glyph(convoy.phase);
                let is_selected = self.selected == Some(&convoy.id);
                let mut line_style = Style::default();
                if is_selected {
                    line_style = line_style.add_modifier(Modifier::REVERSED);
                }
                ListItem::new(Line::from(vec![
                    Span::styled(glyph.symbol, glyph.style),
                    Span::raw(" "),
                    Span::styled(convoy.name.clone(), line_style),
                ]))
            })
            .collect();
        let border_style = if self.focused { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
        let block = Block::default().borders(Borders::ALL).border_style(border_style).title(" Convoys ");
        f.render_widget(List::new(items).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::namespace::{ConvoyPhase, ConvoySummary};
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn sample(name: &str, phase: ConvoyPhase) -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("flotilla", name),
            namespace: "flotilla".into(),
            name: name.into(),
            workflow_ref: "wf".into(),
            phase,
            message: None,
            repo_hint: None,
            tasks: vec![],
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        }
    }

    #[test]
    fn convoy_list_snapshot_three_phases() {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let a = sample("fix-a", ConvoyPhase::Active);
        let b = sample("fix-b", ConvoyPhase::Completed);
        let c = sample("fix-c", ConvoyPhase::Failed);
        let convoys: Vec<&ConvoySummary> = vec![&a, &b, &c];
        terminal
            .draw(|f| {
                ConvoyList { convoys: &convoys, selected: Some(&a.id), focused: true }.render(f, f.area());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
