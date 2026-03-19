use std::any::Any;

use crossterm::event::KeyEvent;
use flotilla_protocol::{Command, CommandAction, RepoSelector};
use ratatui::{layout::Rect, Frame};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::ui_state::UiMode,
    keymap::{Action, ModeId},
};

pub struct IssueSearchWidget {
    input: Input,
}

impl Default for IssueSearchWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl IssueSearchWidget {
    pub fn new() -> Self {
        Self { input: Input::default() }
    }

    fn sync_mode(&self, ctx: &mut WidgetContext) {
        *ctx.mode = UiMode::IssueSearch { input: self.input.clone() };
    }
}

impl InteractiveWidget for IssueSearchWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                let query = self.input.value().to_string();
                if !query.is_empty() {
                    let repo_identity = ctx.repo_order[ctx.active_repo].clone();
                    let cmd = Command {
                        host: None,
                        context_repo: None,
                        action: CommandAction::SearchIssues { repo: RepoSelector::Identity(repo_identity.clone()), query: query.clone() },
                    };
                    ctx.commands.push(cmd);
                    if let Some(rui) = ctx.repo_ui.get_mut(&repo_identity) {
                        rui.active_search_query = Some(query);
                    }
                }
                *ctx.mode = UiMode::Normal;
                Outcome::Finished
            }
            Action::Dismiss => {
                // Clear the active issue search
                let repo_identity = ctx.repo_order[ctx.active_repo].clone();
                let cmd = Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::ClearIssueSearch { repo: RepoSelector::Identity(repo_identity.clone()) },
                };
                ctx.commands.push(cmd);
                if let Some(rui) = ctx.repo_ui.get_mut(&repo_identity) {
                    rui.active_search_query = None;
                }
                *ctx.mode = UiMode::Normal;
                Outcome::Finished
            }
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        self.input.handle_event(&crossterm::event::Event::Key(key));
        self.sync_mode(ctx);
        Outcome::Consumed
    }

    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &RenderContext) {
        // IssueSearch is displayed via the status bar, not a separate popup.
        // The status bar reads from UiMode::IssueSearch which is kept in sync
        // via sync_mode().
    }

    fn mode_id(&self) -> ModeId {
        ModeId::IssueSearch
    }

    fn captures_raw_keys(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use flotilla_protocol::{Command, CommandAction};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn mode_id_is_issue_search() {
        let widget = IssueSearchWidget::new();
        assert_eq!(widget.mode_id(), ModeId::IssueSearch);
    }

    #[test]
    fn captures_raw_keys() {
        let widget = IssueSearchWidget::new();
        assert!(widget.captures_raw_keys());
    }

    #[test]
    fn dismiss_clears_search_and_returns_finished() {
        let mut widget = IssueSearchWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected ClearIssueSearch command");
        assert!(matches!(cmd, Command { action: CommandAction::ClearIssueSearch { .. }, .. }));
    }

    #[test]
    fn confirm_with_query_pushes_search_command() {
        let mut widget = IssueSearchWidget::new();
        widget.input = Input::from("bug fix");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected SearchIssues command");
        match cmd {
            Command { action: CommandAction::SearchIssues { query, .. }, .. } => {
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
        }
    }

    #[test]
    fn confirm_with_query_sets_active_search() {
        let mut widget = IssueSearchWidget::new();
        widget.input = Input::from("bug fix");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::Confirm, &mut ctx);

        let repo_identity = &harness.model.repo_order[harness.model.active_repo];
        let rui = harness.repo_ui.get(repo_identity).expect("repo ui state");
        assert_eq!(rui.active_search_query.as_deref(), Some("bug fix"));
    }

    #[test]
    fn confirm_empty_returns_finished_no_command() {
        let mut widget = IssueSearchWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn raw_key_appends_to_input() {
        let mut widget = IssueSearchWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_raw_key(key(KeyCode::Char('a')), &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "a");
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = IssueSearchWidget::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
