use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    keymap::{Action, ModeId},
    ui_helpers,
};

pub struct HelpWidget {
    scroll: u16,
}

impl Default for HelpWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpWidget {
    pub fn new() -> Self {
        Self { scroll: 0 }
    }
}

impl InteractiveWidget for HelpWidget {
    fn handle_action(&mut self, action: Action, _ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.scroll = self.scroll.saturating_add(1);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Dismiss | Action::ToggleHelp => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, _key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext) {
        let popup = ui_helpers::popup_area(area, 60, 85);
        frame.render_widget(Clear, popup);

        let mut help_text = vec![
            Line::from(Span::styled("Item Icons", Style::default().add_modifier(Modifier::BOLD))),
            Line::from("  \u{25cf}  Checkout with workspace    \u{25cb}  Checkout (no workspace)"),
            Line::from("  \u{25b6}  Running session            \u{25c6}  Idle session"),
            Line::from("  \u{2299}  Pull request               \u{25c7}  Issue"),
            Line::from("  \u{22b6}  Remote branch"),
            Line::from(""),
            Line::from(Span::styled("Column Indicators", Style::default().add_modifier(Modifier::BOLD))),
            Line::from("  WT: \u{25c6} main  \u{2713} checked out"),
            Line::from("  WS: \u{25cf} has workspace  2/3/\u{2026} multiple"),
            Line::from("  PR: \u{2713} merged  \u{2717} closed"),
            Line::from("  Git: ? untracked  M modified  \u{2191} ahead  \u{2193} behind"),
            Line::from(""),
        ];

        // Dynamic sections from keymap
        for section in ctx.keymap.help_sections() {
            help_text.push(Line::from(Span::styled(section.title, Style::default().add_modifier(Modifier::BOLD))));
            for binding in &section.bindings {
                help_text.push(Line::from(format!("  {:18}{}", binding.key_display, binding.description)));
            }
            help_text.push(Line::from(""));
        }

        // Mouse hints (not configurable)
        help_text.push(Line::from(Span::styled("Mouse", Style::default().add_modifier(Modifier::BOLD))));
        help_text.push(Line::from("  Click            Select item"));
        help_text.push(Line::from("  Double-click     Open workspace"));
        help_text.push(Line::from("  Right-click      Action menu"));
        help_text.push(Line::from("  Scroll wheel     Navigate list"));
        help_text.push(Line::from("  Drag tab         Reorder tabs"));

        let total_lines = help_text.len() as u16;
        let inner_height = popup.height.saturating_sub(2); // borders
        let max_scroll = total_lines.saturating_sub(inner_height);
        self.scroll = self.scroll.min(max_scroll);
        let scroll = self.scroll;

        let has_more_below = scroll < max_scroll;
        let has_more_above = scroll > 0;
        let title = match (has_more_above, has_more_below) {
            (true, true) => " Help \u{2191}\u{2193} ",
            (false, true) => " Help \u{2193} ",
            (true, false) => " Help \u{2191} ",
            (false, false) => " Help ",
        };

        let paragraph = Paragraph::new(help_text)
            .block(Block::bordered().style(ctx.theme.block_style()).title(title))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, popup);
    }

    fn mode_id(&self) -> ModeId {
        ModeId::Help
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    #[test]
    fn mode_id_is_help() {
        let widget = HelpWidget::new();
        assert_eq!(widget.mode_id(), ModeId::Help);
    }

    #[test]
    fn select_next_increments_scroll() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 1);
    }

    #[test]
    fn select_prev_decrements_scroll() {
        let mut widget = HelpWidget { scroll: 5 };
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 4);
    }

    #[test]
    fn select_prev_at_zero_stays() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.scroll, 0);
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn toggle_help_returns_finished() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::ToggleHelp, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = HelpWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
