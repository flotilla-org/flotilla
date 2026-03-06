use std::sync::Arc;

use tracing::info;

use super::ui_state::UiMode;
use super::App;
use flotilla_core::config;
use flotilla_protocol::{CommandResult, ProtoCommand};

/// Execute a single ProtoCommand against the app state.
///
/// Commands that modify the daemon-side state (providers, worktrees, etc.) are
/// routed through `flotilla_core::executor::execute()`.  Commands that modify
/// only TUI-local state (AddRepo) are handled here directly.
pub async fn execute(cmd: ProtoCommand, app: &mut App) {
    app.model.status_message = None;

    // Handle AddRepo locally — it needs to update AppModel
    if let ProtoCommand::AddRepo { ref path } = cmd {
        let path = path.clone();
        info!("adding repo {}", path.display());
        config::save_repo(&path);
        app.add_repo(path).await;
        app.switch_tab(app.model.repo_order.len() - 1);
        config::save_tab_order(&app.model.repo_order);
        trigger_active_refresh(app);
        return;
    }

    // Get what we need from the model
    let repo_root = app.model.active_repo_root().clone();
    let registry = Arc::clone(&app.model.active().registry);
    let providers = Arc::clone(&app.model.active().data.providers);

    let result = flotilla_core::executor::execute(cmd, &repo_root, &registry, &providers).await;

    // Handle result — update UI state
    handle_result(result, app);

    // Trigger refresh
    trigger_active_refresh(app);
}

fn handle_result(result: CommandResult, app: &mut App) {
    match result {
        CommandResult::Ok => {}
        CommandResult::WorktreeCreated { branch } => {
            info!("created worktree {branch}");
        }
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            app.prefill_branch_input(&name, issue_ids);
        }
        CommandResult::DeleteInfo(info) => {
            app.ui.mode = UiMode::DeleteConfirm {
                info: Some(flotilla_core::data::DeleteConfirmInfo {
                    branch: info.branch,
                    pr_status: info.pr_status,
                    merge_commit_sha: info.merge_commit_sha,
                    unpushed_commits: info.unpushed_commits,
                    has_uncommitted: info.has_uncommitted,
                    base_detection_warning: info.base_detection_warning,
                }),
                loading: false,
            };
        }
        CommandResult::Error { message } => {
            app.model.status_message = Some(message);
        }
    }
}

/// Trigger an immediate background refresh on the active repo.
fn trigger_active_refresh(app: &App) {
    app.model.active().refresh_handle.trigger_refresh();
}
