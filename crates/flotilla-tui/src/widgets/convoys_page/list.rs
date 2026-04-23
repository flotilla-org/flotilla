//! Left-pane convoy list widget.

use flotilla_protocol::namespace::{ConvoyId, ConvoySummary};
use ratatui::Frame;

pub struct ConvoyList<'a> {
    pub convoys: &'a [&'a ConvoySummary],
    pub selected: Option<&'a ConvoyId>,
}

impl<'a> ConvoyList<'a> {
    pub fn render(&self, _f: &mut Frame, _area: ratatui::layout::Rect) {
        // Real rendering in Task 23.
    }
}
