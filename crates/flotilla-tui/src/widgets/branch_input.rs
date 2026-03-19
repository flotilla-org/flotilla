use std::any::Any;

use crossterm::event::KeyEvent;
use flotilla_protocol::{CheckoutTarget, Command, CommandAction, RepoSelector};
use ratatui::{layout::Rect, style::Style, text::Line, widgets::Paragraph, Frame};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext, WidgetStatusData};
use crate::{
    app::ui_state::BranchInputKind,
    keymap::{Action, ModeId},
    shimmer::shimmer_spans,
    ui_helpers,
};

pub struct BranchInputWidget {
    input: Input,
    kind: BranchInputKind,
    pending_issue_ids: Vec<(String, String)>,
}

impl BranchInputWidget {
    pub fn new(kind: BranchInputKind) -> Self {
        Self { input: Input::default(), kind, pending_issue_ids: Vec::new() }
    }

    /// Whether the widget is in the async generating state.
    pub fn is_generating(&self) -> bool {
        self.kind == BranchInputKind::Generating
    }

    /// Update the input text and switch to manual mode after async generation completes.
    pub fn prefill(&mut self, name: &str, issue_ids: Vec<(String, String)>) {
        self.input = Input::from(name);
        self.kind = BranchInputKind::Manual;
        self.pending_issue_ids = issue_ids;
    }
}

impl InteractiveWidget for BranchInputWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                if self.kind == BranchInputKind::Generating {
                    return Outcome::Consumed;
                }
                let branch = self.input.value().to_string();
                let issue_ids = std::mem::take(&mut self.pending_issue_ids);
                if !branch.is_empty() {
                    let repo_identity = ctx.repo_order[ctx.active_repo].clone();
                    let cmd = Command {
                        host: ctx.target_host.cloned(),
                        context_repo: None,
                        action: CommandAction::Checkout {
                            repo: RepoSelector::Identity(repo_identity),
                            target: CheckoutTarget::FreshBranch(branch),
                            issue_ids,
                        },
                    };
                    ctx.commands.push(cmd);
                }
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        if self.kind == BranchInputKind::Generating {
            return Outcome::Consumed;
        }
        self.input.handle_event(&crossterm::event::Event::Key(key));
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let theme = ctx.theme;
        let (_outer, inner) = ui_helpers::render_popup_frame(frame, area, 50, 20, " New Branch ", theme.block_style());

        if self.kind == BranchInputKind::Generating {
            let spans = shimmer_spans("  Generating branch name...", theme);
            let paragraph = Paragraph::new(Line::from(spans));
            frame.render_widget(paragraph, inner);
            return;
        }

        let input_text = self.input.value();
        let display = format!("> {}", input_text);
        let paragraph = Paragraph::new(display).style(Style::default().fg(theme.input_text));
        frame.render_widget(paragraph, inner);

        let cursor_x = inner.x + 2 + self.input.visual_cursor() as u16;
        let cursor_y = inner.y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }

    fn mode_id(&self) -> ModeId {
        ModeId::BranchInput
    }

    fn captures_raw_keys(&self) -> bool {
        true
    }

    fn status_data(&self) -> WidgetStatusData {
        WidgetStatusData::BranchInput { generating: self.is_generating() }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use flotilla_protocol::{CheckoutTarget, Command, CommandAction};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn mode_id_is_branch_input() {
        let widget = BranchInputWidget::new(BranchInputKind::Manual);
        assert_eq!(widget.mode_id(), ModeId::BranchInput);
    }

    #[test]
    fn captures_raw_keys() {
        let widget = BranchInputWidget::new(BranchInputKind::Manual);
        assert!(widget.captures_raw_keys());
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_with_input_pushes_checkout_command() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        widget.input = Input::from("my-branch");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        match cmd {
            Command { action: CommandAction::Checkout { target, issue_ids, .. }, .. } => {
                assert_eq!(target, CheckoutTarget::FreshBranch("my-branch".into()));
                assert!(issue_ids.is_empty());
            }
            other => panic!("expected Checkout, got {:?}", other),
        }
    }

    #[test]
    fn confirm_with_pending_issues_forwards_them() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        widget.input = Input::from("feat/issue-42");
        widget.pending_issue_ids = vec![("github".into(), "42".into())];
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        match cmd {
            Command { action: CommandAction::Checkout { issue_ids, .. }, .. } => {
                assert_eq!(issue_ids, vec![("github".into(), "42".into())]);
            }
            other => panic!("expected Checkout, got {:?}", other),
        }
    }

    #[test]
    fn confirm_empty_returns_finished_no_command() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_while_generating_is_consumed() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Generating);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn dismiss_while_generating_returns_finished() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Generating);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn raw_key_appends_to_input() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_raw_key(key(KeyCode::Char('q')), &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "q");
    }

    #[test]
    fn raw_key_ignored_while_generating() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Generating);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_raw_key(key(KeyCode::Char('a')), &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.input.value(), "");
    }

    #[test]
    fn prefill_switches_to_manual_and_sets_input() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Generating);
        widget.prefill("my-branch", vec![("gh".into(), "1".into())]);
        assert_eq!(widget.input.value(), "my-branch");
        assert_eq!(widget.kind, BranchInputKind::Manual);
        assert_eq!(widget.pending_issue_ids.len(), 1);
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = BranchInputWidget::new(BranchInputKind::Manual);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
