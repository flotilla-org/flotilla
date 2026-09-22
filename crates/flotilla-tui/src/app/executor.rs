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
        CommandValue::Ok => {}
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use flotilla_protocol::{commands::AttachMode, IssueRef, IssueSource, RepoSelector, ViewAddress};

    use super::*;
    use crate::{
        app::{
            test_support::{stub_app_with_daemon, ExecuteCalls, QueryCalls, StubDaemon},
            ui_state::ProjectIssueStartContext,
        },
        table_view::RowId,
    };

    fn dispatch_channels() -> (mpsc::UnboundedSender<Event>, mpsc::UnboundedReceiver<Event>) {
        mpsc::unbounded_channel()
    }

    #[tokio::test]
    async fn dispatch_executes_regular_commands_and_forwards_the_command_id() {
        let execute_calls: ExecuteCalls = Arc::new(Mutex::new(Vec::new()));
        let daemon = Arc::new(StubDaemon::builder().execute_result(Ok(42)).execute_calls(execute_calls.clone()).build());
        let mut app = stub_app_with_daemon(daemon, vec![]);
        let command = app.command(CommandAction::Refresh { repo: None });
        let (event_tx, mut event_rx) = dispatch_channels();

        dispatch(command.clone(), &mut app, None, event_tx);

        let event = event_rx.recv().await.expect("dispatch completion event");
        assert!(matches!(event, Event::CommandDispatchCompleted { result: Ok(42), pending_ctx: None }));
        assert_eq!(*execute_calls.lock().expect("execute calls lock"), vec![command]);
    }

    #[tokio::test]
    async fn dispatch_routes_attach_queries_without_executing_a_command() {
        let execute_calls: ExecuteCalls = Arc::new(Mutex::new(Vec::new()));
        let query_calls: QueryCalls = Arc::new(Mutex::new(Vec::new()));
        let daemon = Arc::new(
            StubDaemon::builder()
                .query_result(Ok(CommandValue::Ok))
                .execute_calls(execute_calls.clone())
                .query_calls(query_calls.clone())
                .build(),
        );
        let mut app = stub_app_with_daemon(daemon, vec![]);
        let command = app.command(CommandAction::Attach { reference: "session".into(), host: None, mode: AttachMode::default() });
        let session_id = app.session_id;
        let (event_tx, mut event_rx) = dispatch_channels();

        dispatch(command.clone(), &mut app, None, event_tx);

        let event = event_rx.recv().await.expect("attach dispatch completion event");
        assert!(matches!(event, Event::AttachDispatchCompleted(Ok(CommandValue::Ok))));
        assert!(execute_calls.lock().expect("execute calls lock").is_empty());
        assert_eq!(*query_calls.lock().expect("query calls lock"), vec![(command, session_id)]);
    }

    #[tokio::test]
    async fn successful_dispatch_acknowledges_pending_action() {
        let daemon = Arc::new(StubDaemon::builder().execute_result(Ok(73)).build());
        let mut app = stub_app_with_daemon(daemon, vec![]);
        let pending_ctx = PendingActionContext::project_issue_start(
            ProjectIssueStartContext {
                address: ViewAddress::Project { namespace: "default".into(), name: "project".into() },
                row_id: RowId::new("issue-9"),
                issue: IssueRef { source: IssueSource { service: "github".into(), scope: "org/repo".into() }, id: "9".into() },
                batch_id: 1,
            },
            "Start convoy".into(),
        );
        let command = app.command(CommandAction::QueryIssueFetchByIds { repo: RepoSelector::Path("/repo".into()), ids: vec!["9".into()] });
        let (event_tx, mut event_rx) = dispatch_channels();

        dispatch(command, &mut app, Some(pending_ctx), event_tx);
        assert_eq!(app.pending_dispatch_acks, 1);

        let Event::CommandDispatchCompleted { result, pending_ctx } = event_rx.recv().await.expect("dispatch completion event") else {
            panic!("expected command dispatch completion");
        };
        handle_dispatch_completion(result, pending_ctx, &mut app);

        assert_eq!(app.pending_dispatch_acks, 0);
        assert!(app.acknowledged_dispatches.contains(&73));
        assert!(app.command_project_issue_starts.contains_key(&73));
    }

    #[tokio::test]
    async fn failed_dispatch_sets_status_message() {
        let daemon = Arc::new(StubDaemon::builder().execute_result(Err("dispatch failed".into())).build());
        let mut app = stub_app_with_daemon(daemon, vec![]);
        let command = app.command(CommandAction::Refresh { repo: None });
        let (event_tx, mut event_rx) = dispatch_channels();

        dispatch(command, &mut app, None, event_tx);
        let Event::CommandDispatchCompleted { result, pending_ctx } = event_rx.recv().await.expect("dispatch completion event") else {
            panic!("expected command dispatch completion");
        };
        handle_dispatch_completion(result, pending_ctx, &mut app);

        assert_eq!(app.model.status_message.as_deref(), Some("dispatch failed"));
    }
}
