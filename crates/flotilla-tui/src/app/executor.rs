use tracing::info;

use super::ui_state::UiMode;
use super::App;
use flotilla_protocol::{Command, CommandResult};

/// Dispatch a single Command by routing through the daemon handle.
///
/// This function returns quickly — it sends the command to the daemon
/// without awaiting completion. Daemon-level commands (AddRepo, RemoveRepo,
/// Refresh) are dispatched directly. Per-repo commands go through
/// `daemon.execute()` which returns a command ID immediately. Results
/// arrive later via `CommandFinished` events.
pub async fn dispatch(cmd: Command, app: &mut App) {
    app.model.status_message = None;

    let repo = app.model.active_repo_root().clone();

    match cmd {
        Command::AddRepo { ref path } => {
            info!(path = %path.display(), "adding repo");
            if let Err(e) = app.daemon.add_repo(path).await {
                app.model.status_message = Some(e);
            }
            // RepoAdded event will add the tab via handle_daemon_event
            return;
        }
        Command::RemoveRepo { ref path } => {
            info!(path = %path.display(), "removing repo");
            if let Err(e) = app.daemon.remove_repo(path).await {
                app.model.status_message = Some(e);
            }
            // RepoRemoved event will update state via handle_daemon_event
            return;
        }
        Command::Refresh => {
            if let Err(e) = app.daemon.refresh(&repo).await {
                app.model.status_message = Some(e);
            }
            return;
        }
        // Issue commands are non-blocking — spawn background tasks so the main
        // loop stays responsive for key/mouse input while pages are fetched.
        // Use the repo from the command payload (not active tab) since the
        // command may target a background repo (e.g. initial fetch on load).
        Command::SetIssueViewport { ref repo, .. }
        | Command::FetchMoreIssues { ref repo, .. }
        | Command::SearchIssues { ref repo, .. }
        | Command::ClearIssueSearch { ref repo, .. } => {
            let daemon = app.daemon.clone();
            let repo = repo.clone();
            tokio::spawn(async move {
                let _ = daemon.execute(&repo, cmd).await;
            });
            return;
        }
        _ => {}
    }

    match app.daemon.execute(&repo, cmd).await {
        Ok(_command_id) => {
            // Result will arrive via CommandFinished event
        }
        Err(e) => app.model.status_message = Some(e),
    }
}

/// Interpret a CommandResult into UI state changes.
///
/// Called when a `CommandFinished` event arrives from the daemon.
pub fn handle_result(result: CommandResult, app: &mut App) {
    match result {
        CommandResult::Ok => {}
        CommandResult::CheckoutCreated { branch } => {
            info!(%branch, "created checkout");
        }
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            app.prefill_branch_input(&name, issue_ids);
        }
        CommandResult::CheckoutStatus(info) => {
            app.ui.mode = UiMode::DeleteConfirm {
                info: Some(info),
                loading: false,
            };
        }
        CommandResult::Error { message } => {
            app.model.status_message = Some(message);
        }
    }
}
