use flotilla_protocol::{Command, CommandAction, CommandResult};
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
                if let Some(rui) = app.ui.repo_ui.get_mut(&ctx.repo_path) {
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
        CommandResult::RepoAdded { path } => {
            info!(path = %path.display(), "added repo");
        }
        CommandResult::RepoRemoved { path } => {
            info!(path = %path.display(), "removed repo");
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
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            app.prefill_branch_input(&name, issue_ids);
        }
        CommandResult::CheckoutStatus(info) => {
            let terminal_keys = match &app.ui.mode {
                UiMode::DeleteConfirm { terminal_keys, .. } => terminal_keys.clone(),
                _ => vec![],
            };
            app.ui.mode = UiMode::DeleteConfirm { info: Some(info), loading: false, terminal_keys };
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
