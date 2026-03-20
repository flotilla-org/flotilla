use flotilla_protocol::{Command, WorkItemIdentity};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::ui_state::PendingActionContext,
    keymap::{Action, ModeId},
    ui_helpers,
};

pub struct CloseConfirmWidget {
    pub id: String,
    pub title: String,
    pub identity: WorkItemIdentity,
    pub command: Command,
}

impl CloseConfirmWidget {
    pub fn new(id: String, title: String, identity: WorkItemIdentity, command: Command) -> Self {
        Self { id, title, identity, command }
    }
}

impl InteractiveWidget for CloseConfirmWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                let pending_ctx = PendingActionContext {
                    identity: self.identity.clone(),
                    description: format!("Close {}", self.id),
                    repo_identity: ctx.model.active_repo_identity().clone(),
                };
                ctx.commands.push_with_context(self.command.clone(), Some(pending_ctx));
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let popup = ui_helpers::popup_area(area, 50, 30);
        frame.render_widget(Clear, popup);

        let theme = ctx.theme;
        let noun = &ctx.model.active_labels().change_requests.noun;
        let lines = vec![
            Line::from(vec![Span::raw(format!("{} #", noun)), Span::styled(&self.id, Style::default().bold())]),
            Line::from(Span::styled(self.title.as_str(), Style::default().fg(theme.muted))),
            Line::from(""),
            Line::from(Span::styled("y/Enter: confirm    n/Esc: cancel", Style::default().fg(theme.muted))),
        ];

        let block_title = format!(" Close {} ", noun);
        let paragraph =
            Paragraph::new(lines).block(Block::bordered().style(theme.block_style()).title(block_title)).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, popup);
    }

    fn mode_id(&self) -> ModeId {
        ModeId::CloseConfirm
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CommandAction, HostName, RepoSelector, WorkItemIdentity};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn make_widget() -> CloseConfirmWidget {
        CloseConfirmWidget::new("PR-1".into(), "Fix all the things".into(), WorkItemIdentity::ChangeRequest("PR-1".into()), Command {
            host: None,
            context_repo: None,
            action: CommandAction::CloseChangeRequest { id: "PR-1".into() },
        })
    }

    #[test]
    fn mode_id_is_close_confirm() {
        let widget = make_widget();
        assert_eq!(widget.mode_id(), ModeId::CloseConfirm);
    }

    #[test]
    fn confirm_pushes_command_and_finishes() {
        let mut widget = make_widget();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        assert!(matches!(cmd.action, CommandAction::CloseChangeRequest { ref id } if id == "PR-1"));
    }

    #[test]
    fn dismiss_returns_finished() {
        let mut widget = make_widget();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn close_confirm_attaches_pending_context() {
        let identity = WorkItemIdentity::ChangeRequest("PR-42".into());
        let mut widget = CloseConfirmWidget::new("PR-42".into(), "test".into(), identity.clone(), Command {
            host: None,
            context_repo: None,
            action: CommandAction::CloseChangeRequest { id: "PR-42".into() },
        });
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::Confirm, &mut ctx);

        let (_, ctx) = harness.commands.take_next().expect("should have command");
        let ctx = ctx.expect("should have pending context");
        assert_eq!(ctx.identity, identity);
    }

    #[test]
    fn close_confirm_preserves_resolved_remote_command() {
        let mut harness = TestWidgetHarness::new();
        let expected = Command {
            host: Some(HostName::new("remote-host")),
            context_repo: Some(RepoSelector::Identity(harness.model.active_repo_identity().clone())),
            action: CommandAction::CloseChangeRequest { id: "PR-1".into() },
        };
        let mut widget =
            CloseConfirmWidget::new("PR-1".into(), "test".into(), WorkItemIdentity::ChangeRequest("PR-1".into()), expected.clone());
        let mut ctx = harness.ctx();

        widget.handle_action(Action::Confirm, &mut ctx);

        let (command, _) = harness.commands.take_next().expect("should have command");
        assert_eq!(command, expected);
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let mut widget = make_widget();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
