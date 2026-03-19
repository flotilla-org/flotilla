use flotilla_protocol::{
    CheckoutSelector, CheckoutStatus, Command, CommandAction, HostName, ManagedTerminalId, RepoSelector, WorkItemIdentity,
};
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
    shimmer::shimmer_spans,
    ui_helpers,
};

pub struct DeleteConfirmWidget {
    pub info: Option<CheckoutStatus>,
    pub loading: bool,
    pub terminal_keys: Vec<ManagedTerminalId>,
    pub identity: WorkItemIdentity,
    pub remote_host: Option<HostName>,
}

impl DeleteConfirmWidget {
    pub fn new(terminal_keys: Vec<ManagedTerminalId>, identity: WorkItemIdentity, remote_host: Option<HostName>) -> Self {
        Self { info: None, loading: true, terminal_keys, identity, remote_host }
    }

    /// Update the checkout status info after the async fetch completes.
    pub fn update_info(&mut self, status: CheckoutStatus) {
        self.info = Some(status);
        self.loading = false;
    }
}

impl InteractiveWidget for DeleteConfirmWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                if self.loading {
                    return Outcome::Consumed;
                }
                if let Some(ref info) = self.info {
                    let pending_ctx = PendingActionContext {
                        identity: self.identity.clone(),
                        description: format!("Remove {}", info.branch),
                        repo_identity: ctx.model.active_repo_identity().clone(),
                    };
                    let action = CommandAction::RemoveCheckout {
                        checkout: CheckoutSelector::Query(info.branch.clone()),
                        terminal_keys: self.terminal_keys.clone(),
                    };
                    let command = Command {
                        host: self.remote_host.clone(),
                        context_repo: Some(RepoSelector::Identity(ctx.model.active_repo_identity().clone())),
                        action,
                    };
                    ctx.commands.push_with_context(command, Some(pending_ctx));
                }
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext) {
        let popup = ui_helpers::popup_area(area, 60, 50);
        frame.render_widget(Clear, popup);

        let theme = ctx.theme;
        let mut lines: Vec<Line> = Vec::new();

        const MAX_FILES: usize = 10;
        const MAX_COMMITS: usize = 5;

        if self.loading {
            lines.push(Line::from(shimmer_spans("Loading safety info...", theme)));
        } else if let Some(ref info) = self.info {
            lines.push(Line::from(vec![Span::raw("Branch: "), Span::styled(&info.branch, Style::default().bold())]));
            lines.push(Line::from(""));

            if let Some(pr_status) = &info.change_request_status {
                let color = theme.change_request_status_color(pr_status);
                let status_text = pr_status.as_str();
                lines.push(Line::from(vec![
                    Span::raw(format!("{}: ", ctx.model.active_labels().change_requests.abbr)),
                    Span::styled(status_text, Style::default().fg(color).bold()),
                ]));
                if let Some(sha) = &info.merge_commit_sha {
                    lines.push(Line::from(format!("Merge commit: {}", sha)));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    format!("No {} found", ctx.model.active_labels().change_requests.abbr),
                    Style::default().fg(theme.muted),
                )));
            }

            lines.push(Line::from(""));

            if info.has_uncommitted {
                if info.uncommitted_files.is_empty() {
                    lines.push(Line::from(Span::styled("\u{26a0} Has uncommitted changes", Style::default().fg(theme.error).bold())));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("\u{26a0} {} uncommitted file(s):", info.uncommitted_files.len()),
                        Style::default().fg(theme.error).bold(),
                    )));
                    for file_line in info.uncommitted_files.iter().take(MAX_FILES) {
                        lines.push(Line::from(Span::styled(file_line.to_string(), Style::default().fg(theme.muted))));
                    }
                    if info.uncommitted_files.len() > MAX_FILES {
                        lines.push(Line::from(Span::styled(
                            format!("...and {} more", info.uncommitted_files.len() - MAX_FILES),
                            Style::default().fg(theme.muted),
                        )));
                    }
                }
            }

            if let Some(warning) = &info.base_detection_warning {
                lines.push(Line::from(Span::styled(format!("\u{26a0} {}", warning), Style::default().fg(theme.warning))));
            } else if !info.unpushed_commits.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("\u{26a0} {} unpushed commit(s):", info.unpushed_commits.len()),
                    Style::default().fg(theme.error).bold(),
                )));
                for commit in info.unpushed_commits.iter().take(MAX_COMMITS) {
                    lines.push(Line::from(commit.to_string()));
                }
            }

            if !info.has_uncommitted
                && info.unpushed_commits.is_empty()
                && info.base_detection_warning.is_none()
                && info.change_request_status.as_ref().is_some_and(|s| s.eq_ignore_ascii_case("merged"))
            {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("\u{2713} Safe to delete", Style::default().fg(theme.status_ok).bold())));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("y/Enter: confirm    n/Esc: cancel", Style::default().fg(theme.muted))));
        }

        let title = match &self.remote_host {
            Some(host) => format!(" Remove {} on {} ", ctx.model.active_labels().checkouts.noun_capitalized(), host),
            None => format!(" Remove {} ", ctx.model.active_labels().checkouts.noun_capitalized()),
        };
        let paragraph = Paragraph::new(lines).block(Block::bordered().style(theme.block_style()).title(title)).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, popup);
    }

    fn mode_id(&self) -> ModeId {
        ModeId::DeleteConfirm
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CheckoutStatus, CommandAction, HostName, WorkItemIdentity};

    use super::*;
    use crate::app::test_support::TestWidgetHarness;

    fn make_widget() -> DeleteConfirmWidget {
        DeleteConfirmWidget::new(vec![], WorkItemIdentity::Session("test".into()), None)
    }

    fn make_widget_with_info(branch: &str) -> DeleteConfirmWidget {
        let mut widget = make_widget();
        widget.update_info(CheckoutStatus {
            branch: branch.into(),
            change_request_status: None,
            merge_commit_sha: None,
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        });
        widget
    }

    #[test]
    fn mode_id_is_delete_confirm() {
        let widget = make_widget();
        assert_eq!(widget.mode_id(), ModeId::DeleteConfirm);
    }

    #[test]
    fn delete_confirm_y_sends_remove_checkout() {
        let mut widget = make_widget_with_info("feat/x");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        match cmd {
            Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
                assert_eq!(checkout, CheckoutSelector::Query("feat/x".into()));
            }
            other => panic!("expected RemoveCheckout, got {:?}", other),
        }
    }

    #[test]
    fn delete_confirm_enter_sends_remove_checkout() {
        let mut widget = make_widget_with_info("feat/y");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        match cmd {
            Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
                assert_eq!(checkout, CheckoutSelector::Query("feat/y".into()));
            }
            other => panic!("expected RemoveCheckout, got {:?}", other),
        }
    }

    #[test]
    fn delete_confirm_attaches_pending_context() {
        let identity = WorkItemIdentity::Session("custom-id".into());
        let mut widget = DeleteConfirmWidget::new(vec![], identity.clone(), None);
        widget.update_info(CheckoutStatus {
            branch: "feat/a".into(),
            change_request_status: None,
            merge_commit_sha: None,
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        });
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::Confirm, &mut ctx);

        let (_, ctx) = harness.commands.take_next().expect("should have command");
        let ctx = ctx.expect("should have pending context");
        assert_eq!(ctx.identity, identity);
    }

    #[test]
    fn delete_confirm_routes_to_remote_host_when_set() {
        let hostname = HostName::new("feta");
        let mut widget = DeleteConfirmWidget::new(vec![], WorkItemIdentity::Session("test".into()), Some(hostname.clone()));
        widget.update_info(CheckoutStatus {
            branch: "feat/x".into(),
            change_request_status: None,
            merge_commit_sha: None,
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        });
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        widget.handle_action(Action::Confirm, &mut ctx);

        let (cmd, _) = harness.commands.take_next().expect("command");
        assert_eq!(cmd.host, Some(hostname));
        assert!(matches!(cmd.action, CommandAction::RemoveCheckout { .. }));
    }

    #[test]
    fn delete_confirm_ignores_while_loading() {
        let mut widget = make_widget(); // loading=true, info=None
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn delete_confirm_esc_cancels() {
        let mut widget = make_widget_with_info("feat/z");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn delete_confirm_n_cancels() {
        let mut widget = make_widget_with_info("feat/z");
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        // 'n' resolves to Dismiss in the keymap for DeleteConfirm mode
        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn delete_confirm_y_with_no_info_does_not_push_command() {
        let mut widget = make_widget();
        widget.loading = false; // not loading, but no info either
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
        assert!(harness.commands.take_next().is_none());
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
