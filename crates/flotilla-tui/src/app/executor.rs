use flotilla_protocol::{Command, CommandAction, CommandValue};
use tokio::sync::mpsc;
use tracing::info;

use super::{
    ui_state::{PendingAction, PendingActionContext, PendingStatus},
    App,
};
use crate::{
    event::Event,
    widgets::{branch_input::BranchInputWidget, delete_confirm::DeleteConfirmWidget},
};

/// Dispatch a single protocol command through the daemon.
///
/// Query commands (`QueryIssues`) are routed through `spawn_query_page` so
/// results arrive via the background `issue_update_tx` channel.  Non-query
/// commands use the regular `execute` path.
///
/// When `pending_ctx` is provided a [`PendingAction`] is recorded before the
/// request starts so the renderer can show an indicator while the daemon
/// acknowledgement is outstanding.
pub fn dispatch(cmd: Command, app: &mut App, pending_ctx: Option<PendingActionContext>, event_tx: mpsc::UnboundedSender<Event>) {
    app.model.status_message = None;

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

    // Route issue query commands through the background query path.
    if let CommandAction::QueryIssues { repo, params, page, count } = cmd.action {
        let repo_identity = match repo {
            flotilla_protocol::RepoSelector::Identity(id) => id,
            _ => {
                app.model.status_message = Some("issue query requires RepoSelector::Identity".into());
                return;
            }
        };
        if !app.begin_issue_page_fetch(&repo_identity, &params, page) {
            return;
        }
        app.spawn_query_page(repo_identity, params, page, count);
        return;
    }

    if let Some(ctx) = &pending_ctx {
        let action = PendingAction { status: PendingStatus::Submitting, description: ctx.description.clone() };
        if let Some(page) = app.screen.repo_pages.get_mut(&ctx.repo_identity) {
            if page
                .pending_actions
                .get(&ctx.identity)
                .is_some_and(|pending| matches!(pending.status, PendingStatus::Submitting | PendingStatus::InFlight { .. }))
            {
                app.model.status_message = Some("An action is already in progress for this item".into());
                return;
            }
            page.pending_actions.insert(ctx.identity.clone(), action);
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
            let finished_error = app.recent_command_finishes.remove(&command_id);
            if let Some(finished_error) = finished_error {
                if let Some(ctx) = pending_ctx {
                    match finished_error {
                        Some(message) => set_pending_status(app, &ctx, PendingStatus::Failed(message)),
                        None => remove_pending_action(app, &ctx),
                    }
                }
            } else if let Some(ctx) = pending_ctx {
                app.acknowledged_dispatches.insert(command_id);
                set_pending_status(app, &ctx, PendingStatus::InFlight { command_id });
            }
        }
        Err(message) => {
            if let Some(ctx) = pending_ctx {
                remove_pending_action(app, &ctx);
            }
            reset_loading_mode(app);
            app.model.status_message = Some(message);
        }
    }

    if app.pending_dispatch_acks == 0 {
        app.recent_command_finishes.clear();
    }
}

pub fn handle_attach_dispatch_completion(result: Result<CommandValue, String>, app: &mut App) {
    match result {
        Ok(CommandValue::AttachCommandResolved { command, .. }) => {
            app.pending_attach_command = Some(command);
        }
        Ok(CommandValue::Error { message }) | Err(message) => {
            app.model.status_message = Some(message);
        }
        Ok(other) => {
            app.model.status_message = Some(format!("unexpected attach response: {other:?}"));
        }
    }
}

fn set_pending_status(app: &mut App, ctx: &PendingActionContext, status: PendingStatus) {
    if let Some(action) = app.screen.repo_pages.get_mut(&ctx.repo_identity).and_then(|page| page.pending_actions.get_mut(&ctx.identity)) {
        action.status = status;
    }
}

fn remove_pending_action(app: &mut App, ctx: &PendingActionContext) {
    if let Some(page) = app.screen.repo_pages.get_mut(&ctx.repo_identity) {
        page.pending_actions.remove(&ctx.identity);
    }
}

/// Reset UI modes that are waiting for a command result.
///
/// When a command fails (either in its dispatch acknowledgement or later via
/// `CommandValue::Error`), loading modes like
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
        | CommandValue::ProjectList(_)
        | CommandValue::HostStatus(_)
        | CommandValue::HostProviders(_)
        | CommandValue::FleetList(_)
        | CommandValue::CrewList(_)
        | CommandValue::FleetReplicaSnapshot(_) => {
            tracing::warn!("query result reached TUI handler — should be handled by CLI");
        }
        CommandValue::ImageEnsured { .. } | CommandValue::EnvironmentCreated { .. } | CommandValue::EnvironmentSpecRead { .. } => {
            tracing::warn!("unexpected environment lifecycle result reached UI handler");
        }
        CommandValue::IssuePage(_) | CommandValue::IssuesByIds { .. } => {}
        CommandValue::ConvoyCreated { name } => {
            info!(%name, "convoy created");
            app.model.status_message = Some(format!("Convoy created: {name}"));
        }
        CommandValue::ConvoyStarted { name, attach_command, .. } => {
            info!(%name, "convoy started");
            app.model.status_message = Some(format!("Convoy started: {name}"));
            if let Some(command) = attach_command {
                app.pending_attach_command = Some(command);
            }
        }
        CommandValue::WorkflowTemplateApplied { name } => {
            info!(%name, "workflow template applied");
            app.model.status_message = Some(format!("Workflow template applied: {name}"));
        }
        CommandValue::ProjectAdded { name } => {
            info!(%name, "project created");
            app.model.status_message = Some(format!("Project created: {name}"));
        }
        CommandValue::ProjectApplied { name } => {
            info!(%name, "project applied");
            app.model.status_message = Some(format!("Project applied: {name}"));
        }
        CommandValue::PlacementPolicyApplied { name } => {
            info!(%name, "placement policy applied");
            app.model.status_message = Some(format!("Placement policy applied: {name}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use crossterm::event::KeyCode;
    use flotilla_protocol::{
        arg::Arg,
        qualified_path::{HostId, QualifiedPath},
        CheckoutStatus, HostName, NodeId, RepoIdentity, ResolvedPaneCommand, WorkItemIdentity,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, Semaphore};

    use super::*;
    use crate::{
        app::{
            test_support::{
                activate_repo_tab, key, repo_info, session_item, setup_selectable_table, stub_app, stub_app_with_daemon,
                stub_app_with_query_result, StubDaemon,
            },
            ui_state::BranchInputKind,
        },
        event::EventHandler,
        widgets::{InteractiveWidget, RenderContext},
    };

    #[tokio::test]
    async fn dispatch_returns_while_daemon_ack_is_outstanding() {
        let execute_gate = Arc::new(Semaphore::new(0));
        let daemon = Arc::new(StubDaemon::builder().execute_gate(Arc::clone(&execute_gate)).build());
        let mut app =
            stub_app_with_daemon(daemon, vec![repo_info("/tmp/test-repo", "test-repo", flotilla_protocol::RepoLabels::default())]);
        let command = app.command(CommandAction::Refresh { repo: None });
        let repo_identity = app.model.repo_order[0].clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        activate_repo_tab(&mut app, 0);
        setup_selectable_table(&mut app, vec![session_item("session-1")]);
        let pending_ctx =
            PendingActionContext { identity: identity.clone(), description: "Refresh".into(), repo_identity: repo_identity.clone() };
        let mut events = EventHandler::new(Duration::from_secs(60));
        events.pause_terminal_input().await;

        dispatch(command, &mut app, Some(pending_ctx), events.sender());

        assert!(matches!(app.screen.repo_pages[&repo_identity].pending_actions[&identity].status, PendingStatus::Submitting));
        assert!(app.needs_animation(), "submitting actions should render their pending indicator");
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &app.model,
                    views: &mut app.views,
                    ui: &mut app.ui,
                    theme: &app.theme,
                    keymap: &app.keymap,
                    in_flight: &app.in_flight,
                    namespaces: &app.namespaces,
                    query_tables: &app.query_tables,
                };
                app.screen.render(frame, frame.area(), &mut ctx);
            })
            .expect("pending frame should render");
        let rendered: String = terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            rendered.chars().any(|ch| matches!(
                ch,
                '\u{280b}' | '\u{2819}' | '\u{2839}' | '\u{2838}' | '\u{283c}' | '\u{2834}' | '\u{2826}' | '\u{2827}'
            )),
            "submitting pending action should render a spinner"
        );

        events.sender().send(Event::Key(key(KeyCode::Char('q')))).expect("event loop should remain open");
        match events.next().await.expect("queued key event") {
            Event::Key(key) => app.handle_key(key),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(app.should_quit, "the app should continue handling keys while the acknowledgement is outstanding");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.next()).await.is_err(),
            "daemon acknowledgement should still be outstanding"
        );

        execute_gate.add_permits(1);
        let event = events.next().await.expect("dispatch completion event");
        match event {
            Event::CommandDispatchCompleted { result, pending_ctx } => {
                handle_dispatch_completion(result, pending_ctx, &mut app);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(app.screen.repo_pages[&repo_identity].pending_actions[&identity].status, PendingStatus::InFlight {
            command_id: 1
        }));
    }

    #[tokio::test]
    async fn dispatch_rejects_a_second_pending_action_for_the_same_identity() {
        let execute_gate = Arc::new(Semaphore::new(0));
        let daemon = Arc::new(StubDaemon::builder().execute_gate(Arc::clone(&execute_gate)).build());
        let mut app =
            stub_app_with_daemon(daemon, vec![repo_info("/tmp/test-repo", "test-repo", flotilla_protocol::RepoLabels::default())]);
        let repo_identity = app.model.repo_order[0].clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx =
            PendingActionContext { identity: identity.clone(), description: "Refresh".into(), repo_identity: repo_identity.clone() };
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        dispatch(app.command(CommandAction::Refresh { repo: None }), &mut app, Some(pending_ctx.clone()), event_tx.clone());
        dispatch(app.command(CommandAction::Refresh { repo: None }), &mut app, Some(pending_ctx), event_tx);

        assert_eq!(app.pending_dispatch_acks, 1);
        assert_eq!(app.model.status_message.as_deref(), Some("An action is already in progress for this item"));

        execute_gate.add_permits(2);
        let event = event_rx.recv().await.expect("first dispatch completion event");
        match event {
            Event::CommandDispatchCompleted { result, pending_ctx } => {
                handle_dispatch_completion(result, pending_ctx, &mut app);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let duplicate_event = tokio::time::timeout(Duration::from_millis(20), event_rx.recv()).await;
        assert!(!matches!(duplicate_event, Ok(Some(_))), "the duplicate dispatch should not reach the daemon");
        assert!(matches!(app.screen.repo_pages[&repo_identity].pending_actions[&identity].status, PendingStatus::InFlight {
            command_id: 1
        }));
    }

    #[tokio::test]
    async fn pane_attach_query_hands_the_resolved_command_to_the_event_loop() {
        let mut app = stub_app_with_query_result(Ok(CommandValue::AttachCommandResolved {
            command: "cleat attach terminal-scratch".to_string(),
            binding: None,
        }));
        let command = app.command(CommandAction::AttachTransient {
            reference: "terminal-scratch".to_string(),
            host: Some(flotilla_protocol::HostName::new("kiwi")),
        });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        dispatch(command, &mut app, None, event_tx);
        let event = event_rx.recv().await.expect("attach completion event");
        match event {
            Event::AttachDispatchCompleted(result) => handle_attach_dispatch_completion(result, &mut app),
            other => panic!("unexpected event: {other:?}"),
        }

        assert_eq!(app.pending_attach_command.as_deref(), Some("cleat attach terminal-scratch"));
        assert!(app.model.status_message.is_none());
    }

    #[tokio::test]
    async fn dispatch_error_clears_pending_state_and_resets_loading_modal() {
        let daemon = Arc::new(StubDaemon::builder().execute_result(Err("request timed out after 30s".into())).build());
        let mut app =
            stub_app_with_daemon(daemon, vec![repo_info("/tmp/test-repo", "test-repo", flotilla_protocol::RepoLabels::default())]);
        let repo_identity = app.model.repo_order[0].clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx =
            PendingActionContext { identity: identity.clone(), description: "Delete session".into(), repo_identity: repo_identity.clone() };
        app.screen.modal_stack.push(Box::new(DeleteConfirmWidget::new(identity.clone(), None, None)));
        let command = app.command(CommandAction::Refresh { repo: None });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        dispatch(command, &mut app, Some(pending_ctx), event_tx);
        let event = event_rx.recv().await.expect("dispatch completion event");
        match event {
            Event::CommandDispatchCompleted { result, pending_ctx } => {
                handle_dispatch_completion(result, pending_ctx, &mut app);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(!app.screen.repo_pages[&repo_identity].pending_actions.contains_key(&identity));
        assert!(app.screen.modal_stack.is_empty(), "loading modal should be reset after dispatch error");
        assert_eq!(app.model.status_message.as_deref(), Some("request timed out after 30s"));
    }

    #[test]
    fn command_finish_before_ack_reconciles_pending_action() {
        let mut app = stub_app();
        let repo_identity = app.model.repo_order[0].clone();
        let repo = app.model.repos[&repo_identity].path.clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx = PendingActionContext {
            identity: identity.clone(),
            description: "Archive session".into(),
            repo_identity: repo_identity.clone(),
        };
        app.screen
            .repo_pages
            .get_mut(&repo_identity)
            .expect("repo page")
            .pending_actions
            .insert(identity.clone(), PendingAction { status: PendingStatus::Submitting, description: pending_ctx.description.clone() });
        app.pending_dispatch_acks = 1;

        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandStarted {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo.clone()),
            description: pending_ctx.description.clone(),
        });
        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandFinished {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo),
            result: CommandValue::Ok,
        });

        handle_dispatch_completion(Ok(42), Some(pending_ctx), &mut app);

        assert!(!app.screen.repo_pages[&repo_identity].pending_actions.contains_key(&identity));
        assert!(app.recent_command_finishes.is_empty());
        assert!(app.acknowledged_dispatches.is_empty());
    }

    #[test]
    fn unrelated_finishes_cannot_evict_a_finish_awaiting_its_ack() {
        let mut app = stub_app();
        let repo_identity = app.model.repo_order[0].clone();
        let repo = app.model.repos[&repo_identity].path.clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx = PendingActionContext {
            identity: identity.clone(),
            description: "Archive session".into(),
            repo_identity: repo_identity.clone(),
        };
        app.screen
            .repo_pages
            .get_mut(&repo_identity)
            .expect("repo page")
            .pending_actions
            .insert(identity.clone(), PendingAction { status: PendingStatus::Submitting, description: pending_ctx.description.clone() });
        app.pending_dispatch_acks = 1;

        for command_id in std::iter::once(42).chain(100..166) {
            app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandStarted {
                command_id,
                node_id: NodeId::new(HostName::local().as_str()),
                repo_identity: repo_identity.clone(),
                repo: Some(repo.clone()),
                description: format!("command {command_id}"),
            });
            app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandFinished {
                command_id,
                node_id: NodeId::new(HostName::local().as_str()),
                repo_identity: repo_identity.clone(),
                repo: Some(repo.clone()),
                result: CommandValue::Ok,
            });
        }

        assert!(app.recent_command_finishes.contains_key(&42));
        assert_eq!(app.recent_command_finishes.len(), 67);

        handle_dispatch_completion(Ok(42), Some(pending_ctx), &mut app);

        assert!(!app.screen.repo_pages[&repo_identity].pending_actions.contains_key(&identity));
        assert_eq!(app.pending_dispatch_acks, 0);
        assert!(app.recent_command_finishes.is_empty());
        assert!(app.acknowledged_dispatches.is_empty());
    }

    #[test]
    fn command_error_before_ack_marks_pending_action_failed() {
        let mut app = stub_app();
        let repo_identity = app.model.repo_order[0].clone();
        let repo = app.model.repos[&repo_identity].path.clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx = PendingActionContext {
            identity: identity.clone(),
            description: "Archive session".into(),
            repo_identity: repo_identity.clone(),
        };
        app.screen
            .repo_pages
            .get_mut(&repo_identity)
            .expect("repo page")
            .pending_actions
            .insert(identity.clone(), PendingAction { status: PendingStatus::Submitting, description: pending_ctx.description.clone() });
        app.pending_dispatch_acks = 1;

        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandStarted {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo.clone()),
            description: pending_ctx.description.clone(),
        });
        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandFinished {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo),
            result: CommandValue::Error { message: "archive failed".into() },
        });

        handle_dispatch_completion(Ok(42), Some(pending_ctx), &mut app);

        assert!(matches!(
            app.screen.repo_pages[&repo_identity].pending_actions[&identity].status,
            PendingStatus::Failed(ref message) if message == "archive failed"
        ));
        assert_eq!(app.model.status_message.as_deref(), Some("archive failed"));
        assert!(app.recent_command_finishes.is_empty());
        assert!(app.acknowledged_dispatches.is_empty());
    }

    #[test]
    fn command_ack_before_finish_reconciles_pending_action() {
        let mut app = stub_app();
        let repo_identity = app.model.repo_order[0].clone();
        let repo = app.model.repos[&repo_identity].path.clone();
        let identity = WorkItemIdentity::Session("session-1".into());
        let pending_ctx = PendingActionContext {
            identity: identity.clone(),
            description: "Archive session".into(),
            repo_identity: repo_identity.clone(),
        };
        app.screen
            .repo_pages
            .get_mut(&repo_identity)
            .expect("repo page")
            .pending_actions
            .insert(identity.clone(), PendingAction { status: PendingStatus::Submitting, description: pending_ctx.description.clone() });
        app.pending_dispatch_acks = 1;

        handle_dispatch_completion(Ok(42), Some(pending_ctx), &mut app);
        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandStarted {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo.clone()),
            description: "Archive session".into(),
        });
        app.handle_daemon_event(flotilla_protocol::DaemonEvent::CommandFinished {
            command_id: 42,
            node_id: NodeId::new(HostName::local().as_str()),
            repo_identity: repo_identity.clone(),
            repo: Some(repo),
            result: CommandValue::Ok,
        });

        assert!(!app.screen.repo_pages[&repo_identity].pending_actions.contains_key(&identity));
        assert!(app.recent_command_finishes.is_empty());
        assert!(app.acknowledged_dispatches.is_empty());
    }

    #[test]
    fn terminal_prepared_does_not_queue_follow_up_workspace_command() {
        let mut app = stub_app();

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
                target_node_id: NodeId::new("remote-a"),
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
        crate::app::test_support::activate_repo_tab(&mut app, 1);

        handle_result(
            CommandValue::TerminalPrepared {
                repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/repo-0".into() },
                target_node_id: NodeId::new("remote-a"),
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
        handle_result(
            CommandValue::CheckoutCreated { branch: "feat-new".into(), path: QualifiedPath::host(HostId::new("host-a"), "/tmp/wt") },
            &mut app,
        );
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
        handle_result(CommandValue::AttachCommandResolved { command: "bash --login".into(), binding: None }, &mut app);
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
