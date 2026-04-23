//! Convoys tab page widget: list on the left, detail on the right.

mod detail;
mod glyphs;
mod list;

pub use detail::ConvoyDetail;
pub use list::ConvoyList;

use flotilla_protocol::{
    namespace::{ConvoyId, ConvoySummary},
    snapshot::RepoKey,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};

pub enum ConvoyScope {
    All,
    Repo(RepoKey),
}

pub struct ConvoysPage<'a> {
    pub convoys: Vec<&'a ConvoySummary>,
    pub scope: ConvoyScope,
    pub selected: Option<&'a ConvoyId>,
    pub filter: &'a str,
}

impl<'a> ConvoysPage<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        if self.convoys.is_empty() {
            let block = Block::default().borders(Borders::ALL).title(" Convoys ");
            let text = Line::from("No convoys. Create one via 'flotilla convoy create ...' (coming soon)");
            f.render_widget(Paragraph::new(text).block(block), area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        ConvoyList { convoys: self.convoys.as_slice(), selected: self.selected }.render(f, chunks[0]);
        if let Some(id) = self.selected {
            if let Some(convoy) = self.convoys.iter().find(|c| &c.id == id) {
                ConvoyDetail { convoy }.render(f, chunks[1]);
            }
        }
    }
}
