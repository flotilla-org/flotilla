use tracing::info;

use super::ui_state::UiMode;
use super::App;
use flotilla_protocol::{Command, CommandResult};

/// Execute a single Command by routing through the daemon handle.
///
/// Daemon-level commands (AddRepo, RemoveRepo, Refresh) are dispatched
/// directly to the daemon. Per-repo commands go through `daemon.execute()`.
/// Results are interpreted into UI state changes.
pub async fn execute(cmd: Command, app: &mut App) {
    app.model.status_message = None;

    let repo = app.model.active_repo_root().clone();

    match cmd {
        Command::AddRepo { ref path } => {
            info!("adding repo {}", path.display());
            if let Err(e) = app.daemon.add_repo(path).await {
                app.model.status_message = Some(e);
            }
            // RepoAdded event will add the tab via handle_daemon_event
            return;
        }
        Command::RemoveRepo { ref path } => {
            info!("removing repo {}", path.display());
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
        Command::SetIssueViewport { .. }
        | Command::FetchMoreIssues { .. }
        | Command::SearchIssues { .. }
        | Command::ClearIssueSearch { .. } => {
            let daemon = app.daemon.clone();
            tokio::spawn(async move {
                let _ = daemon.execute(&repo, cmd).await;
            });
            return;
        }
        _ => {}
    }

    match app.daemon.execute(&repo, cmd).await {
        Ok(result) => handle_result(result, app),
        Err(e) => app.model.status_message = Some(e),
    }
}

/// Interpret a CommandResult into UI state changes.
pub fn handle_result(result: CommandResult, app: &mut App) {
    match result {
        CommandResult::Ok => {}
        CommandResult::CheckoutCreated { branch } => {
            info!("created checkout {branch}");
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
