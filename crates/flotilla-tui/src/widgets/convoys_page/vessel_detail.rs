//! Full-page detail for a single vessel (the `vessel/...` View).

use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::glyphs::work_glyph;
use crate::convoy_model::{ConvoySummary, VesselSummary};

pub struct VesselDetail<'a> {
    pub convoy: &'a ConvoySummary,
    pub vessel: &'a VesselSummary,
}

impl<'a> VesselDetail<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let glyph = work_glyph(self.vessel.phase);
        let block = Block::default().borders(Borders::ALL).title(format!(" {} · {} ", self.convoy.name, self.vessel.name));

        let mut lines = vec![
            Line::from(vec![Span::styled(glyph.symbol, glyph.style), Span::raw(format!(" {:?}", self.vessel.phase).to_lowercase())]),
            Line::raw(""),
        ];
        if let Some(host) = &self.vessel.host {
            lines.push(Line::raw(format!("host: {host}")));
        }
        if let Some(ws_ref) = &self.vessel.workspace_ref {
            lines.push(Line::raw(format!("attach: {ws_ref}")));
        }
        if !self.vessel.depends_on.is_empty() {
            lines.push(Line::raw(format!("depends on: {}", self.vessel.depends_on.join(", "))));
        }
        if let Some(message) = &self.vessel.message {
            lines.push(Line::raw(""));
            lines.push(Line::raw(message.clone()));
        }
        if !self.vessel.crew.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::raw("crew:"));
            for member in &self.vessel.crew {
                lines.push(Line::raw(format!("  {} {}", member.role, member.command_preview)));
            }
        }

        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}
