use flotilla_protocol::{Command, CommandAction, CommandValue};
use tokio::sync::mpsc;
use tracing::info;

use super::{
    ui_state::{PendingActionContext, PendingActionTarget, PendingStatus},
    App,
};
use crate::event::Event;

/// Dispatch a single protocol command through the daemon.
///
/// When `pending_ctx` is provided, row-level progress is recorded before the
/// request starts so the renderer can show an indicator while acknowledgement
/// is outstanding.
pub fn dispatch(cmd: Command, app: &mut App, pending_ctx: Option<PendingActionContext>, event_tx: mpsc::UnboundedSender<Event>) {
    let project_issue_start = pending_ctx.as_ref().is_some_and(|ctx| matches!(ctx.target, PendingActionTarget::ProjectIssueStart(_)));
    if !project_issue_start {
        app.set_status_message(None);
    }

    // Pane attach is a query that resolves a command for the TUI process to
    // run temporarily outside raw mode. It must not go through the ordinary
    // command lifecycle (`execute` rejects query commands).
    if matches!(&cmd.action, CommandAction::Attach { .. } | CommandAction::AttachTransient { .. }) {
        let daemon = app.daemon.clone();
        let session_id = app.session_id;
        tokio::spawn(async move {
            let result = daemon.execute_query(cmd, session_id).await;
            let _ = event_tx.send(Event::AttachDispatchCompleted(result));
        });
        return;
    }

    if let Some(ctx) = &pending_ctx {
        match &ctx.target {
            PendingActionTarget::ProjectIssueStart(project_ctx) => {
                app.set_project_issue_start_pending(project_ctx, PendingStatus::Submitting, ctx.description.clone());
            }
            PendingActionTarget::TableRow(row_ctx) => {
                if let Err(message) = app.views.begin_pending_row(row_ctx, ctx.description.clone()) {
                    app.set_status_message(Some(message));
                    return;
                }
            }
        }
        app.pending_dispatch_acks += 1;
    }

    let daemon = app.daemon.clone();
    tokio::spawn(async move {
        let result = daemon.execute(cmd).await;
        let _ = event_tx.send(Event::CommandDispatchCompleted { result, pending_ctx });
    });
}
pub fn handle_dispatch_completion(result: Result<u64, String>, pending_ctx: Option<PendingActionContext>, app: &mut App) {
    if pending_ctx.is_some() {
        debug_assert!(app.pending_dispatch_acks > 0, "pending-action acknowledgement without a tracked dispatch");
        app.pending_dispatch_acks = app.pending_dispatch_acks.saturating_sub(1);
    }

    match result {
        Ok(command_id) => {
            let finished = app.recent_command_finishes.remove(&command_id);
            if let Some(finished) = finished {
                if let Some(ctx) = pending_ctx {
                    match ctx.target {
                        PendingActionTarget::ProjectIssueStart(project_ctx) => match finished.row_error_message {
                            Some(message) => app.record_project_issue_start_result(project_ctx, Err(message)),
                            None => app.record_project_issue_start_result(project_ctx, Ok(None)),
                        },
                        PendingActionTarget::TableRow(row_ctx) => match finished.row_error_message {
                            Some(message) => app.views.mark_pending_row_failed(&row_ctx, message),
                            None => app.views.mark_pending_row(&row_ctx, command_id),
                        },
                    }
                }
            } else if let Some(ctx) = pending_ctx {
                app.acknowledged_dispatches.insert(command_id);
                match &ctx.target {
                    PendingActionTarget::ProjectIssueStart(project_ctx) => {
                        app.command_project_issue_starts.insert(command_id, project_ctx.clone());
                        app.set_project_issue_start_pending(project_ctx, PendingStatus::InFlight { command_id }, ctx.description.clone());
                    }
                    PendingActionTarget::TableRow(row_ctx) => app.views.mark_pending_row(row_ctx, command_id),
                }
            }
        }
        Err(message) => {
            let mut handled_by_project_batch = false;
            if let Some(ctx) = pending_ctx {
                match ctx.target {
                    PendingActionTarget::ProjectIssueStart(project_ctx) => {
                        app.record_project_issue_start_result(project_ctx, Err(message.clone()));
                        handled_by_project_batch = true;
                    }
                    PendingActionTarget::TableRow(row_ctx) => app.views.mark_pending_row_failed(&row_ctx, message.clone()),
                }
            }
            if !handled_by_project_batch {
                app.set_status_message(Some(message));
            }
        }
    }

    if app.pending_dispatch_acks == 0 {
        app.recent_command_finishes.clear();
    }
}

pub fn handle_attach_dispatch_completion(result: Result<CommandValue, String>, app: &mut App) {
    match result {
        Ok(CommandValue::AttachCommandResolved { plan, .. }) => {
            app.pending_attach_plan = Some(plan);
        }
        Ok(CommandValue::Error { message }) | Err(message) => {
            app.set_status_message(Some(message));
        }
        Ok(other) => {
            app.set_status_message(Some(format!("unexpected attach response: {other:?}")));
        }
    }
}

/// Interpret a CommandValue into UI state changes.
///
/// Called when a `CommandFinished` event arrives from the daemon.
pub fn handle_result(result: CommandValue, app: &mut App) {
    match result {
        CommandValue::Ok
        | CommandValue::ConvoyBriefDelivered { .. }
        | CommandValue::ConvoyBriefQueued { .. }
        | CommandValue::ConvoyBriefWithdrawn { .. } => {}
        CommandValue::RepoTracked { path, .. } => {
            info!(path = %path.display(), "tracked repo");
        }
        CommandValue::RepoUntracked { path } => {
            info!(path = %path.display(), "untracked repo");
        }
        CommandValue::Refreshed { repos, .. } => {
            info!(count = repos.len(), "refresh completed");
        }
        CommandValue::CheckoutCreated { branch, .. } => {
            info!(%branch, "created checkout");
        }
        CommandValue::CheckoutRemoved { branch } => {
            info!(%branch, "removed checkout");
        }
        CommandValue::BranchNameGenerated { .. } => tracing::warn!("unexpected branch-name result reached UI handler"),
        CommandValue::Error { message } => {
            app.set_status_message(Some(message));
        }
        CommandValue::Cancelled => {
            app.set_status_message(Some("Command cancelled".into()));
        }
        CommandValue::TerminalPrepared { .. }
        | CommandValue::PreparedWorkspace(_)
        | CommandValue::AttachCommandResolved { .. }
        | CommandValue::CheckoutPathResolved { .. }
        | CommandValue::CheckoutStatus(_) => {
            tracing::warn!("unexpected internal step result reached UI handler");
        }
        CommandValue::RepoProviders(_)
        | CommandValue::HostList(_)
        | CommandValue::ProjectList(_)
        | CommandValue::DispatchQueue(_)
        | CommandValue::HostStatus(_)
        | CommandValue::HostProviders(_)
        | CommandValue::FleetHealth(_)
        | CommandValue::FleetList(_)
        | CommandValue::CrewList(_)
        | CommandValue::FleetReplicaSnapshot(_)
        | CommandValue::DaemonLogs { .. }
        | CommandValue::ConvoyExplanation(_)
        | CommandValue::ResourceRead(_)
        | CommandValue::ResourceObject(_)
        | CommandValue::ResourceDeleted(_)
        | CommandValue::ResourceAlreadyDeleted(_)
        | CommandValue::ResourceWatchEvent(_) => {
            tracing::warn!("query result reached TUI handler — should be handled by CLI");
        }
        CommandValue::EnvironmentSpecRead { .. } => {
            tracing::warn!("unexpected environment lifecycle result reached UI handler");
        }
        CommandValue::IssuePage(_) | CommandValue::IssuesByIds { .. } => {}
        CommandValue::ConvoyCreated { name } => {
            info!(%name, "convoy created");
            app.set_status_message(Some(format!("Convoy created: {name}")));
        }
        CommandValue::ConvoyStarted { name, attach_plan, .. } => {
            info!(%name, "convoy started");
            app.set_status_message(Some(format!("Convoy started: {name}")));
            if let Some(plan) = attach_plan {
                app.pending_attach_plan = Some(plan);
            }
        }
        CommandValue::WorkflowTemplateApplied { name } => {
            info!(%name, "workflow template applied");
            app.set_status_message(Some(format!("Workflow template applied: {name}")));
        }
        CommandValue::ProjectAdded { name } => {
            info!(%name, "project created");
            app.set_status_message(Some(format!("Project created: {name}")));
        }
        CommandValue::ProjectApplied { name } => {
            info!(%name, "project applied");
            app.set_status_message(Some(format!("Project applied: {name}")));
        }
        CommandValue::ProjectRegistered { name, members } => {
            info!(%name, %members, "project registered from declaration");
            app.set_status_message(Some(format!("Project registered: {name} ({members} members)")));
        }
        CommandValue::ProjectRefreshed { name, members, converged, changes } => {
            info!(%name, %members, %converged, ?changes, "project declaration refreshed");
            let outcome = if converged { format!("changed: {}", changes.join(", ")) } else { "already current".to_string() };
            app.set_status_message(Some(format!("Project refreshed: {name} ({members} members, {outcome})")));
        }
    }
}
