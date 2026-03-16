use flotilla_protocol::{Command, CommandAction, CommandResult, WorkItemIdentity};
use tracing::info;

use super::{
    ui_state::{PendingAction, PendingActionContext, PendingStatus, UiMode},
    App,
};

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
                if let Some(rui) = app.ui.repo_ui.get_mut(&ctx.repo_identity) {
                    rui.pending_actions.insert(ctx.identity, PendingAction {
                        command_id,
                        status: PendingStatus::InFlight,
                        description: ctx.description,
                    });
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
/// asynchronously via `CommandResult::Error`), loading modes like
/// `DeleteConfirm { loading: true }` must be cleared so the user
/// can see the error message and isn't stuck in a loading state.
fn reset_loading_mode(app: &mut App) {
    match &app.ui.mode {
        UiMode::DeleteConfirm { loading: true, .. } | UiMode::BranchInput { kind: super::BranchInputKind::Generating, .. } => {
            app.ui.mode = UiMode::Normal;
        }
        _ => {}
    }
}

/// Interpret a CommandResult into UI state changes.
///
/// Called when a `CommandFinished` event arrives from the daemon.
pub fn handle_result(result: CommandResult, app: &mut App) {
    match result {
        CommandResult::Ok => {}
        CommandResult::RepoTracked { path, .. } => {
            info!(path = %path.display(), "tracked repo");
        }
        CommandResult::RepoUntracked { path } => {
            info!(path = %path.display(), "untracked repo");
        }
        CommandResult::Refreshed { repos } => {
            info!(count = repos.len(), "refresh completed");
        }
        CommandResult::CheckoutCreated { branch, .. } => {
            info!(%branch, "created checkout");
        }
        CommandResult::CheckoutRemoved { branch } => {
            info!(%branch, "removed checkout");
        }
        CommandResult::TerminalPrepared { repo_identity, target_host, branch, checkout_path, attachable_set_id, commands } => {
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
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            app.prefill_branch_input(&name, issue_ids);
        }
        CommandResult::CheckoutStatus(info) => {
            let (terminal_keys, identity) = match &app.ui.mode {
                UiMode::DeleteConfirm { terminal_keys, identity, .. } => (terminal_keys.clone(), identity.clone()),
                other => {
                    tracing::warn!(mode = ?std::mem::discriminant(other), "CheckoutStatus arrived outside DeleteConfirm");
                    (vec![], WorkItemIdentity::Session(String::new()))
                }
            };
            app.ui.mode = UiMode::DeleteConfirm { info: Some(info), loading: false, terminal_keys, identity };
        }
        CommandResult::Error { message } => {
            reset_loading_mode(app);
            app.model.status_message = Some(message);
        }
        CommandResult::Cancelled => {
            reset_loading_mode(app);
            app.model.status_message = Some("Command cancelled".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use flotilla_protocol::{CommandAction, HostName, PreparedTerminalCommand, RepoIdentity};

    use super::*;
    use crate::app::test_support::stub_app;

    #[test]
    fn terminal_prepared_queues_local_workspace_creation() {
        let mut app = stub_app();

        handle_result(
            CommandResult::TerminalPrepared {
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
            CommandResult::TerminalPrepared {
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
}
