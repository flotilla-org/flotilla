use std::any::Any;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, Frame};

use super::{event_log::EventLogWidget, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{app::ui_state::UiMode, binding_table::BindingModeId, keymap::Action};

/// Overview page widget for the Flotilla (overview) tab.
///
/// Replaces the Config-mode rendering path that previously lived in BaseView.
/// Composes an `EventLogWidget` which handles the two-column layout: providers
/// and hosts status on the left, event log on the right.
pub struct OverviewPage {
    pub event_log: EventLogWidget,
}

impl Default for OverviewPage {
    fn default() -> Self {
        Self::new()
    }
}

impl OverviewPage {
    pub fn new() -> Self {
        Self { event_log: EventLogWidget::new() }
    }
}

impl InteractiveWidget for OverviewPage {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.event_log.select_next();
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.event_log.select_prev();
                Outcome::Consumed
            }
            Action::Dismiss => {
                // Switch back to Normal mode (leave the Flotilla tab).
                // This mirrors the old BaseView Config-mode dismiss behaviour:
                // pressing q/Esc on the overview page returns to the active repo tab.
                *ctx.mode = UiMode::Normal;
                Outcome::Consumed
            }
            Action::Quit => {
                ctx.app_actions.push(super::AppAction::Quit);
                Outcome::Consumed
            }
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),
            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.event_log.handle_click(mouse.column, mouse.row) {
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            MouseEventKind::ScrollDown => {
                self.event_log.select_next();
                Outcome::Consumed
            }
            MouseEventKind::ScrollUp => {
                self.event_log.select_prev();
                Outcome::Consumed
            }
            _ => {
                let _ = ctx;
                Outcome::Ignored
            }
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        // Delegate to EventLogWidget's InteractiveWidget::render which produces
        // the full two-column layout (providers + hosts on the left, event log
        // on the right).
        InteractiveWidget::render(&mut self.event_log, frame, area, ctx);
    }

    fn mode_id(&self) -> BindingModeId {
        BindingModeId::Overview
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::{
        app::{test_support::TestWidgetHarness, UiState},
        keymap::Keymap,
        theme::Theme,
        widgets::{RenderContext, WidgetStatusData},
    };

    #[test]
    fn overview_page_renders_without_panic() {
        let mut page = OverviewPage::new();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal creation should succeed");
        let harness = TestWidgetHarness::new();
        let theme = Theme::classic();
        let keymap = Keymap::defaults();
        let in_flight = HashMap::new();
        let mut ui = UiState::new(&harness.model.repo_order);

        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &harness.model,
                    ui: &mut ui,
                    theme: &theme,
                    keymap: &keymap,
                    in_flight: &in_flight,
                    active_widget_mode: Some(BindingModeId::Overview),
                    active_widget_data: WidgetStatusData::None,
                };
                page.render(frame, frame.area(), &mut ctx);
            })
            .expect("draw should succeed");
    }

    #[test]
    fn overview_page_event_log_navigation() {
        let mut page = OverviewPage::new();
        page.event_log.count = 5;
        page.event_log.selected = Some(1);

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.event_log.selected, Some(2));

        let outcome = page.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.event_log.selected, Some(1));
    }

    #[test]
    fn overview_page_dismiss_switches_to_normal() {
        let mut page = OverviewPage::new();
        let mut harness = TestWidgetHarness::new();
        harness.mode = crate::app::ui_state::UiMode::Config;

        {
            let mut ctx = harness.ctx();
            let outcome = page.handle_action(Action::Dismiss, &mut ctx);
            assert!(matches!(outcome, Outcome::Consumed));
            assert!(!ctx.app_actions.iter().any(|a| matches!(a, super::super::AppAction::Quit)));
        }

        assert!(matches!(harness.mode, crate::app::ui_state::UiMode::Normal));
    }

    #[test]
    fn overview_page_mode_id_is_config() {
        let page = OverviewPage::new();
        assert_eq!(page.mode_id(), BindingModeId::Overview);
    }

    #[test]
    fn overview_page_toggle_help_pushes_widget() {
        let mut page = OverviewPage::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::ToggleHelp, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn overview_page_scroll_navigates_event_log() {
        let mut page = OverviewPage::new();
        page.event_log.count = 5;
        page.event_log.selected = Some(1);

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let mouse_down = MouseEvent { kind: MouseEventKind::ScrollDown, column: 10, row: 10, modifiers: KeyModifiers::NONE };
        let outcome = page.handle_mouse(mouse_down, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.event_log.selected, Some(2));

        let mouse_up = MouseEvent { kind: MouseEventKind::ScrollUp, column: 10, row: 10, modifiers: KeyModifiers::NONE };
        let outcome = page.handle_mouse(mouse_up, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.event_log.selected, Some(1));
    }
}
