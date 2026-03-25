use flotilla_protocol::{Command, CommandAction, CommandValue};
use tracing::info;

use super::{
    ui_state::{PendingAction, PendingActionContext, PendingStatus},
    App, BackgroundUpdate,
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
        let background_updates = app.background_updates_tx.clone();
        let action = cmd.action.clone();
        tokio::spawn(async move {
            if let Err(error) = daemon.execute(cmd).await {
                let _ = background_updates.send(BackgroundUpdate::IssueCommandFailed { action, error });
            }
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
        CommandValue::TerminalPrepared { .. }
        | CommandValue::PreparedWorkspace(_)
        | CommandValue::AttachCommandResolved { .. }
        | CommandValue::CheckoutPathResolved { .. } => {
            tracing::warn!("unexpected internal step result reached UI handler");
        }
        CommandValue::RepoDetail(_)
        | CommandValue::RepoProviders(_)
        | CommandValue::RepoWork(_)
        | CommandValue::HostList(_)
        | CommandValue::HostStatus(_)
        | CommandValue::HostProviders(_) => {
            tracing::warn!("query result reached TUI handler — should be handled by CLI");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use async_trait::async_trait;
    use flotilla_core::daemon::DaemonHandle;
    use flotilla_protocol::{arg::Arg, CheckoutStatus, CommandAction, HostName, RepoIdentity, ResolvedPaneCommand, WorkItemIdentity};
    use tokio::sync::broadcast;

    use super::*;
    use crate::app::{test_support::stub_app, ui_state::BranchInputKind};

    struct FailingDaemon {
        tx: broadcast::Sender<flotilla_protocol::DaemonEvent>,
        error: String,
    }

    impl FailingDaemon {
        fn new(error: &str) -> Self {
            let (tx, _) = broadcast::channel(1);
            Self { tx, error: error.into() }
        }
    }

    #[async_trait]
    impl DaemonHandle for FailingDaemon {
        fn subscribe(&self) -> broadcast::Receiver<flotilla_protocol::DaemonEvent> {
            self.tx.subscribe()
        }

        async fn get_state(&self, _repo: &flotilla_protocol::RepoSelector) -> Result<flotilla_protocol::RepoSnapshot, String> {
            Err("stub".into())
        }

        async fn list_repos(&self) -> Result<Vec<flotilla_protocol::RepoInfo>, String> {
            Ok(vec![])
        }

        async fn execute(&self, _command: Command) -> Result<u64, String> {
            Err(self.error.clone())
        }

        async fn cancel(&self, _command_id: u64) -> Result<(), String> {
            Ok(())
        }

        async fn replay_since(
            &self,
            _last_seen: &HashMap<flotilla_protocol::StreamKey, u64>,
        ) -> Result<Vec<flotilla_protocol::DaemonEvent>, String> {
            Ok(vec![])
        }

        async fn get_status(&self) -> Result<flotilla_protocol::StatusResponse, String> {
            Ok(flotilla_protocol::StatusResponse { repos: vec![] })
        }

        async fn get_topology(&self) -> Result<flotilla_protocol::TopologyResponse, String> {
            Err("stub".into())
        }
    }

    #[test]
    fn terminal_prepared_does_not_queue_follow_up_workspace_command() {
        let mut app = stub_app();

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
                target_host: HostName::new("remote-a"),
                branch: "feat-x".into(),
                checkout_path: PathBuf::from("/remote/feat-x"),
                attachable_set_id: Some(flotilla_protocol::AttachableSetId::new("set-1")),
                commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash -l".into())] }],
            },
            &mut app,
        );

        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn terminal_prepared_ignores_originating_repo_for_follow_up_commands() {
        let mut app = crate::app::test_support::stub_app_with_repos(2);
        app.model.active_repo = 1;

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/repo-0".into() },
                target_host: HostName::new("remote-a"),
                branch: "feat-x".into(),
                checkout_path: PathBuf::from("/remote/feat-x"),
                attachable_set_id: None,
                commands: vec![ResolvedPaneCommand { role: "main".into(), args: vec![Arg::Literal("bash -l".into())] }],
            },
            &mut app,
        );

        assert!(app.proto_commands.take_next().is_none());
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

    #[tokio::test]
    async fn dispatch_background_issue_fetch_failure_clears_pending_and_sets_status_message() {
        let mut app = stub_app();
        let repo_identity = app.model.active_repo_identity().clone();
        app.model.repos.get_mut(&repo_identity).expect("repo exists").issue_fetch_pending = true;
        app.daemon = Arc::new(FailingDaemon::new("fetch failed"));

        dispatch(
            app.command(CommandAction::FetchMoreIssues {
                repo: flotilla_protocol::RepoSelector::Identity(repo_identity.clone()),
                desired_count: 50,
            }),
            &mut app,
            None,
        )
        .await;

        tokio::task::yield_now().await;
        app.drain_background_updates();

        assert!(!app.model.repos[&repo_identity].issue_fetch_pending);
        assert_eq!(app.model.status_message.as_deref(), Some("fetch failed"));
    }

    #[tokio::test]
    async fn dispatch_background_issue_search_failure_sets_status_message() {
        let mut app = stub_app();
        let repo_identity = app.model.active_repo_identity().clone();
        app.daemon = Arc::new(FailingDaemon::new("search failed"));

        dispatch(
            app.command(CommandAction::SearchIssues {
                repo: flotilla_protocol::RepoSelector::Identity(repo_identity),
                query: "bug".into(),
            }),
            &mut app,
            None,
        )
        .await;

        tokio::task::yield_now().await;
        app.drain_background_updates();

        assert_eq!(app.model.status_message.as_deref(), Some("search failed"));
    }
}
