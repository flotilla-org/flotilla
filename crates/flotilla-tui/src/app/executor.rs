use flotilla_protocol::{Command, CommandAction, CommandValue};
use tracing::info;

use super::{
    ui_state::{PendingAction, PendingActionContext, PendingStatus},
    App,
};
use crate::widgets::{branch_input::BranchInputWidget, delete_confirm::DeleteConfirmWidget};

/// Dispatch a single protocol command through the daemon.
///
/// Most commands go through the shared `execute(command)` path and return a
/// command ID immediately. Issue fetch/search commands are spawned in the
/// background because they may do network I/O inline before returning.
///
/// When `pending_ctx` is provided the successful command ID is recorded as a
/// [`PendingAction`] on the active repo's UI state so the renderer can show
/// an in-flight indicator on the affected work item row.
pub async fn dispatch(cmd: Command, app: &mut App, pending_ctx: Option<PendingActionContext>) {
    app.model.status_message = None;

    let background_issue_command = matches!(
        cmd.action,
        CommandAction::SetIssueViewport { .. }
            | CommandAction::FetchMoreIssues { .. }
            | CommandAction::SearchIssues { .. }
            | CommandAction::ClearIssueSearch { .. }
    );

    if background_issue_command {
        let daemon = app.daemon.clone();
        tokio::spawn(async move {
            let _ = daemon.execute(cmd).await;
        });
        return;
    }

    match app.daemon.execute(cmd).await {
        Ok(command_id) => {
            if let Some(ctx) = pending_ctx {
                let action = PendingAction { command_id, status: PendingStatus::InFlight, description: ctx.description };
                if let Some(page) = app.screen.repo_pages.get_mut(&ctx.repo_identity) {
                    page.pending_actions.insert(ctx.identity, action);
                }
            }
        }
        Err(e) => {
            // Reset loading modes so the error message is visible.
            reset_loading_mode(app);
            app.model.status_message = Some(e);
        }
    }
}

/// Reset UI modes that are waiting for a command result.
///
/// When a command fails (either synchronously from `dispatch` or
/// asynchronously via `CommandValue::Error`), loading modes like
/// `DeleteConfirm { loading: true }` must be cleared so the user
/// can see the error message and isn't stuck in a loading state.
fn reset_loading_mode(app: &mut App) {
    // Pop loading widgets from the modal stack on error.
    if let Some(widget) = app.screen.modal_stack.last_mut() {
        if let Some(dcw) = widget.as_any_mut().downcast_mut::<DeleteConfirmWidget>() {
            if dcw.loading {
                app.screen.modal_stack.pop();
                return;
            }
        }
    }
    if let Some(widget) = app.screen.modal_stack.last_mut() {
        if let Some(biw) = widget.as_any_mut().downcast_mut::<BranchInputWidget>() {
            if biw.is_generating() {
                app.screen.modal_stack.pop();
            }
        }
    }
}

/// Interpret a CommandValue into UI state changes.
///
/// Called when a `CommandFinished` event arrives from the daemon.
pub fn handle_result(result: CommandValue, app: &mut App) {
    match result {
        CommandValue::Ok => {}
        CommandValue::RepoTracked { path, .. } => {
            info!(path = %path.display(), "tracked repo");
        }
        CommandValue::RepoUntracked { path } => {
            info!(path = %path.display(), "untracked repo");
        }
        CommandValue::Refreshed { repos } => {
            info!(count = repos.len(), "refresh completed");
        }
        CommandValue::CheckoutCreated { branch, .. } => {
            info!(%branch, "created checkout");
        }
        CommandValue::CheckoutRemoved { branch } => {
            info!(%branch, "removed checkout");
        }
        CommandValue::TerminalPrepared { repo_identity, target_host, branch, checkout_path, attachable_set_id, commands } => {
            if app.repo_path_for_identity(&repo_identity).is_some() {
                app.proto_commands.push(app.repo_command_for_identity(repo_identity, CommandAction::CreateWorkspaceFromPreparedTerminal {
                    target_host,
                    branch,
                    checkout_path,
                    attachable_set_id,
                    commands,
                }));
            } else {
                app.model.status_message = Some(format!("repo not found for terminal result: {repo_identity}"));
            }
        }
        CommandValue::BranchNameGenerated { name, issue_ids } => {
            let updated = app.screen.modal_stack.last_mut().and_then(|widget| widget.as_any_mut().downcast_mut::<BranchInputWidget>());
            if let Some(biw) = updated {
                biw.prefill(&name, issue_ids);
            } else {
                tracing::warn!("BranchNameGenerated arrived but no BranchInputWidget on stack");
            }
        }
        CommandValue::CheckoutStatus(info) => {
            let updated = app.screen.modal_stack.last_mut().and_then(|widget| widget.as_any_mut().downcast_mut::<DeleteConfirmWidget>());
            if let Some(dcw) = updated {
                dcw.update_info(info);
            } else {
                tracing::warn!("CheckoutStatus arrived but no DeleteConfirmWidget on stack");
            }
        }
        CommandValue::Error { message } => {
            reset_loading_mode(app);
            app.model.status_message = Some(message);
        }
        CommandValue::Cancelled => {
            reset_loading_mode(app);
            app.model.status_message = Some("Command cancelled".into());
        }
        CommandValue::AttachCommandResolved { .. } | CommandValue::CheckoutPathResolved { .. } => {
            tracing::warn!("unexpected internal step result reached UI handler");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{CheckoutStatus, CommandAction, HostName, PreparedTerminalCommand, RepoIdentity, WorkItemIdentity};

    use super::*;
    use crate::app::{test_support::stub_app, ui_state::BranchInputKind};

    #[test]
    fn terminal_prepared_queues_local_workspace_creation() {
        let mut app = stub_app();

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
                target_host: HostName::new("remote-a"),
                branch: "feat-x".into(),
                checkout_path: PathBuf::from("/remote/feat-x"),
                attachable_set_id: Some(flotilla_protocol::AttachableSetId::new("set-1")),
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
            },
            &mut app,
        );

        let (cmd, _) = app.proto_commands.take_next().expect("queued workspace creation");
        match cmd.action {
            CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
                assert_eq!(cmd.host, None);
                assert_eq!(target_host, HostName::new("remote-a"));
                assert_eq!(branch, "feat-x");
                assert_eq!(checkout_path, PathBuf::from("/remote/feat-x"));
                assert_eq!(attachable_set_id, Some(flotilla_protocol::AttachableSetId::new("set-1")));
                assert_eq!(commands, vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }]);
            }
            other => panic!("expected CreateWorkspaceFromPreparedTerminal, got {other:?}"),
        }
    }

    #[test]
    fn terminal_prepared_uses_originating_repo_not_active_repo() {
        let mut app = crate::app::test_support::stub_app_with_repos(2);
        app.model.active_repo = 1;

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/repo-0".into() },
                target_host: HostName::new("remote-a"),
                branch: "feat-x".into(),
                checkout_path: PathBuf::from("/remote/feat-x"),
                attachable_set_id: None,
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
            },
            &mut app,
        );

        let (cmd, _) = app.proto_commands.take_next().expect("queued workspace creation");
        assert_eq!(
            cmd.context_repo,
            Some(flotilla_protocol::RepoSelector::Identity(RepoIdentity { authority: "local".into(), path: "/tmp/repo-0".into() }))
        );
    }

    #[test]
    fn ok_is_noop() {
        let mut app = stub_app();
        handle_result(CommandValue::Ok, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn repo_tracked_does_not_set_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::RepoTracked { path: PathBuf::from("/tmp/new-repo"), resolved_from: None }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn repo_untracked_does_not_set_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::RepoUntracked { path: PathBuf::from("/tmp/old-repo") }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn refreshed_does_not_set_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::Refreshed { repos: vec![PathBuf::from("/tmp/repo-a"), PathBuf::from("/tmp/repo-b")] }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn checkout_created_does_not_set_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::CheckoutCreated { branch: "feat-new".into(), path: PathBuf::from("/tmp/wt") }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn checkout_removed_does_not_set_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::CheckoutRemoved { branch: "feat-old".into() }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn branch_name_generated_prefills_branch_input_widget() {
        let mut app = stub_app();
        app.screen.modal_stack.push(Box::new(BranchInputWidget::new(BranchInputKind::Generating)));

        handle_result(
            CommandValue::BranchNameGenerated { name: "feat/cool-thing".into(), issue_ids: vec![("gh".into(), "42".into())] },
            &mut app,
        );

        let widget = app
            .screen
            .modal_stack
            .last_mut()
            .expect("modal stack should still have widget")
            .as_any_mut()
            .downcast_mut::<BranchInputWidget>()
            .expect("should be BranchInputWidget");
        assert!(!widget.is_generating(), "widget should have switched from Generating to Manual");
    }

    #[test]
    fn branch_name_generated_without_widget_is_noop() {
        let mut app = stub_app();
        // No widget on the modal stack — should not panic.
        handle_result(CommandValue::BranchNameGenerated { name: "feat/orphan".into(), issue_ids: vec![] }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.screen.modal_stack.is_empty());
    }

    #[test]
    fn checkout_status_updates_delete_confirm_widget() {
        let mut app = stub_app();
        let widget = DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
        assert!(widget.loading, "widget should start in loading state");
        app.screen.modal_stack.push(Box::new(widget));

        let status = CheckoutStatus {
            branch: "feat/old".into(),
            change_request_status: Some("merged".into()),
            merge_commit_sha: Some("abc123".into()),
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        };
        handle_result(CommandValue::CheckoutStatus(status), &mut app);

        let dcw = app
            .screen
            .modal_stack
            .last_mut()
            .expect("modal stack should still have widget")
            .as_any_mut()
            .downcast_mut::<DeleteConfirmWidget>()
            .expect("should be DeleteConfirmWidget");
        assert!(!dcw.loading, "widget should no longer be loading");
        let info = dcw.info.as_ref().expect("info should be populated");
        assert_eq!(info.branch, "feat/old");
        assert_eq!(info.change_request_status.as_deref(), Some("merged"));
    }

    #[test]
    fn checkout_status_without_widget_is_noop() {
        let mut app = stub_app();
        let status = CheckoutStatus { branch: "orphan".into(), ..CheckoutStatus::default() };
        handle_result(CommandValue::CheckoutStatus(status), &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.screen.modal_stack.is_empty());
    }

    #[test]
    fn error_sets_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::Error { message: "something went wrong".into() }, &mut app);
        assert_eq!(app.model.status_message.as_deref(), Some("something went wrong"));
    }

    #[test]
    fn error_pops_loading_delete_confirm_widget() {
        let mut app = stub_app();
        let widget = DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
        assert!(widget.loading);
        app.screen.modal_stack.push(Box::new(widget));

        handle_result(CommandValue::Error { message: "fetch failed".into() }, &mut app);

        assert!(app.screen.modal_stack.is_empty(), "loading DeleteConfirmWidget should be popped");
        assert_eq!(app.model.status_message.as_deref(), Some("fetch failed"));
    }

    #[test]
    fn error_pops_generating_branch_input_widget() {
        let mut app = stub_app();
        app.screen.modal_stack.push(Box::new(BranchInputWidget::new(BranchInputKind::Generating)));

        handle_result(CommandValue::Error { message: "generation failed".into() }, &mut app);

        assert!(app.screen.modal_stack.is_empty(), "generating BranchInputWidget should be popped");
        assert_eq!(app.model.status_message.as_deref(), Some("generation failed"));
    }

    #[test]
    fn error_does_not_pop_manual_branch_input_widget() {
        let mut app = stub_app();
        app.screen.modal_stack.push(Box::new(BranchInputWidget::new(BranchInputKind::Manual)));

        handle_result(CommandValue::Error { message: "unrelated error".into() }, &mut app);

        assert_eq!(app.screen.modal_stack.len(), 1, "manual BranchInputWidget should remain");
        assert_eq!(app.model.status_message.as_deref(), Some("unrelated error"));
    }

    #[test]
    fn error_does_not_pop_non_loading_delete_confirm_widget() {
        let mut app = stub_app();
        let mut widget = DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
        widget.update_info(CheckoutStatus { branch: "feat/x".into(), ..CheckoutStatus::default() });
        assert!(!widget.loading);
        app.screen.modal_stack.push(Box::new(widget));

        handle_result(CommandValue::Error { message: "unrelated error".into() }, &mut app);

        assert_eq!(app.screen.modal_stack.len(), 1, "non-loading DeleteConfirmWidget should remain");
        assert_eq!(app.model.status_message.as_deref(), Some("unrelated error"));
    }

    #[test]
    fn cancelled_sets_status_message() {
        let mut app = stub_app();
        handle_result(CommandValue::Cancelled, &mut app);
        assert_eq!(app.model.status_message.as_deref(), Some("Command cancelled"));
    }

    #[test]
    fn cancelled_pops_loading_delete_confirm_widget() {
        let mut app = stub_app();
        let widget = DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
        app.screen.modal_stack.push(Box::new(widget));

        handle_result(CommandValue::Cancelled, &mut app);

        assert!(app.screen.modal_stack.is_empty(), "loading DeleteConfirmWidget should be popped on cancel");
        assert_eq!(app.model.status_message.as_deref(), Some("Command cancelled"));
    }

    #[test]
    fn attach_command_resolved_is_noop() {
        let mut app = stub_app();
        handle_result(CommandValue::AttachCommandResolved { command: "bash --login".into() }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn checkout_path_resolved_is_noop() {
        let mut app = stub_app();
        handle_result(CommandValue::CheckoutPathResolved { path: PathBuf::from("/tmp/wt") }, &mut app);
        assert!(app.model.status_message.is_none());
        assert!(app.proto_commands.take_next().is_none());
    }
}
