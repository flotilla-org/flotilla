//! Right-pane convoy detail widget.

use flotilla_protocol::namespace::ConvoySummary;
use ratatui::Frame;

pub struct ConvoyDetail<'a> {
    pub convoy: &'a ConvoySummary,
}

impl<'a> ConvoyDetail<'a> {
    pub fn render(&self, _f: &mut Frame, _area: ratatui::layout::Rect) {
        // Real rendering in Task 24.
    }
}
