//! Convoys tab page widget: list on the left, detail on the right.
//!
//! Per-scope filtering is applied at the App layer (`visible_convoys`); add a scope
//! field here when scope-specific rendering is needed.

mod detail;
mod glyphs;
mod list;
mod vessel_detail;

pub use detail::ConvoyDetail;
pub use list::ConvoyList;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
pub use vessel_detail::VesselDetail;

use crate::convoy_model::{ConvoyId, ConvoySummary};

pub struct ConvoysPage<'a> {
    pub convoys: Vec<&'a ConvoySummary>,
    pub selected: Option<&'a ConvoyId>,
    pub selected_vessel: Option<&'a str>,
    pub focus: crate::app::ConvoysFocus,
    pub filter: &'a str,
    /// Set on project dashboards: the Project the list is scoped to.
    pub project: Option<&'a str>,
}

impl<'a> ConvoysPage<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        if self.convoys.is_empty() {
            let (title, text) = match self.project {
                // The TUI has no projects query yet, so an empty project
                // dashboard cannot distinguish "no launched work" from "no
                // such project" — say so instead of implying either.
                Some(project) => (
                    format!(" Project {project} — Convoys "),
                    Line::from(format!("No convoys in project '{project}' — or no such project (projects aren't queryable yet).")),
                ),
                None => (" Convoys ".to_string(), Line::from("No convoys. Create one via 'flotilla convoy create ...' (coming soon)")),
            };
            let block = Block::default().borders(Borders::ALL).title(title);
            f.render_widget(Paragraph::new(text).block(block), area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        let list_focused = matches!(self.focus, crate::app::ConvoysFocus::List);
        ConvoyList { convoys: self.convoys.as_slice(), selected: self.selected, focused: list_focused }.render(f, chunks[0]);
        if let Some(id) = self.selected {
            if let Some(convoy) = self.convoys.iter().find(|c| &c.id == id) {
                ConvoyDetail { convoy, selected_vessel: self.selected_vessel, focused: !list_focused }.render(f, chunks[1]);
            }
        }
    }
}
