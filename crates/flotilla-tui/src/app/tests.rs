use crossterm::event::KeyCode;
use flotilla_protocol::WorkItemIdentity;
use tempfile::tempdir;
use test_support::*;

use super::*;

fn insert_local_host(model: &mut TuiModel, name: &str) {
    let host_name = HostName::new(name);
    model.hosts.insert(host_name.clone(), TuiHostState {
        host_name: host_name.clone(),
        is_local: true,
        status: PeerStatus::Connected,
        summary: HostSummary {
            host_name,
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
        },
    });
}

fn insert_peer_host(model: &mut TuiModel, name: &str, status: PeerStatus) {
    let host_name = HostName::new(name);
    model.hosts.insert(host_name.clone(), TuiHostState {
        host_name: host_name.clone(),
        is_local: false,
        status,
        summary: HostSummary {
            host_name,
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
        },
    });
}

// -- CommandQueue --

#[test]
fn command_queue_push_and_take_fifo() {
    let mut q = CommandQueue::default();
    q.push(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
    q.push(Command {
        host: None,
        context_repo: Some(RepoSelector::Path(PathBuf::from("/repo"))),
        action: CommandAction::OpenChangeRequest { id: "1".into() },
    });
    assert!(matches!(q.take_next(), Some((Command { action: CommandAction::Refresh { .. }, .. }, _))));
    assert!(matches!(q.take_next(), Some((Command { action: CommandAction::OpenChangeRequest { .. }, .. }, _))));
}

#[test]
fn command_queue_empty_returns_none() {
    let mut q = CommandQueue::default();
    assert!(q.take_next().is_none());
}

// -- TuiModel::repo_name --

#[test]
fn repo_name_extracts_directory_name() {
    assert_eq!(TuiModel::repo_name(Path::new("/home/user/project")), "project");
}

#[test]
fn repo_name_root_path() {
    let name = TuiModel::repo_name(Path::new("/"));
    assert_eq!(name, "/");
}

// -- TuiModel::from_repo_info --

#[test]
fn from_repo_info_builds_correct_model() {
    let repos_info =
        vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default()), repo_info("/tmp/repo-b", "repo-b", RepoLabels::default())];
    let model = TuiModel::from_repo_info(repos_info);
    assert_eq!(model.repos.len(), 2);
    assert_eq!(model.repo_order.len(), 2);
    assert_eq!(model.active_repo, 0);
    assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-a")));
    assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-b")));
    assert!(model.status_message.is_none());
}

#[test]
fn from_repo_info_preserves_order() {
    let repos_info = vec![repo_info("/z", "z", RepoLabels::default()), repo_info("/a", "a", RepoLabels::default())];
    let model = TuiModel::from_repo_info(repos_info);
    assert_eq!(model.repos[&model.repo_order[0]].path, PathBuf::from("/z"));
    assert_eq!(model.repos[&model.repo_order[1]].path, PathBuf::from("/a"));
}

#[test]
fn from_repo_info_empty() {
    let model = TuiModel::from_repo_info(vec![]);
    assert!(model.repos.is_empty());
    assert!(model.repo_order.is_empty());
}

#[test]
fn app_new_loads_layout_from_config() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[ui.preview]\nlayout = \"below\"\n").unwrap();

    let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

    assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
}

#[test]
fn persist_layout_writes_current_ui_state() {
    let dir = tempdir().unwrap();
    let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
    let config = Arc::new(ConfigStore::with_base(dir.path()));
    let mut app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

    app.ui.view_layout = RepoViewLayout::Right;
    app.persist_layout();

    let reloaded = ConfigStore::with_base(dir.path());
    let cfg = reloaded.load_config();
    assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
}

// -- format_error_status --

#[test]
fn format_error_status_no_errors() {
    assert!(format_error_status(&[], Path::new("/repo")).is_none());
}

#[test]
fn format_error_status_single_error() {
    let errors = vec![provider_error("change_request", "github", "rate limited")];
    let msg = format_error_status(&errors, Path::new("/tmp/my-repo")).unwrap();
    assert!(msg.contains("my-repo"));
    assert!(msg.contains("change_request"));
    assert!(msg.contains("rate limited"));
    assert!(msg.contains("(github)"));
}

#[test]
fn format_error_status_suppresses_issues_disabled() {
    let errors = vec![provider_error("issues", "github", "repo has disabled issues")];
    assert!(format_error_status(&errors, Path::new("/repo")).is_none());
}

#[test]
fn format_error_status_mixed_suppressed_and_real() {
    let errors = vec![provider_error("issues", "github", "repo has disabled issues"), provider_error("vcs", "git", "not a git repo")];
    let msg = format_error_status(&errors, Path::new("/repo")).unwrap();
    assert!(msg.contains("not a git repo"));
    assert!(!msg.contains("disabled issues"));
}

#[test]
fn format_error_status_empty_provider_no_suffix() {
    let errors = vec![provider_error("vcs", "", "error")];
    let msg = format_error_status(&errors, Path::new("/r")).unwrap();
    assert!(!msg.contains("()"));
}

#[test]
fn format_error_status_multiple_errors_joined() {
    let errors = vec![provider_error("vcs", "git", "err1"), provider_error("cr", "gh", "err2")];
    let msg = format_error_status(&errors, Path::new("/r")).unwrap();
    assert!(msg.contains("; "));
}

#[test]
fn apply_snapshot_updates_provider_data() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let snap = snapshot(&repo_path);
    app.apply_snapshot(snap);
    assert!(!app.model.repos[&repo].loading);
}

#[test]
fn apply_snapshot_updates_issue_metadata() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.issue_has_more = true;
    snap.issue_total = Some(42);
    snap.issue_search_results = Some(vec![]);
    app.apply_snapshot(snap);

    let rm = &app.model.repos[&repo];
    assert!(rm.issue_has_more);
    assert_eq!(rm.issue_total, Some(42));
    assert!(rm.issue_search_active);
}

#[test]
fn apply_snapshot_maps_provider_health_to_statuses() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.provider_health.insert("vcs".into(), HashMap::from([("git".into(), true), ("wt".into(), false)]));
    app.apply_snapshot(snap);

    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "wt".into())], ProviderStatus::Error,);
}

#[test]
fn apply_snapshot_sets_error_status_message() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);

    let mut snap = snapshot(&repo_path);
    snap.errors = vec![provider_error("cr", "gh", "fail")];
    app.apply_snapshot(snap);

    assert!(app.model.status_message.is_some());
    assert!(app.model.status_message.as_ref().unwrap().contains("fail"));
}

#[test]
fn dismissing_status_message_hides_only_that_message() {
    let mut app = stub_app();
    app.set_status_message(Some("rate limit exceeded".into()));

    let id = app.visible_status_items()[0].id;
    app.dismiss_status_item(id);

    assert!(app.visible_status_items().is_empty());
}

#[test]
fn new_status_message_reappears_after_dismissing_old_one() {
    let mut app = stub_app();
    app.set_status_message(Some("old error".into()));
    app.dismiss_status_item(0);

    app.set_status_message(Some("new error".into()));

    assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR new error".into() }]);
}

#[test]
fn visible_status_items_use_shared_error_and_peer_labels() {
    let mut app = stub_app();
    app.set_status_message(Some("boom".into()));
    insert_peer_host(&mut app.model, "host-a", PeerStatus::Disconnected);

    assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR boom".into() }, VisibleStatusItem {
        id: 1,
        text: "HOST DOWN host-a".into()
    },]);
}

#[test]
fn apply_snapshot_clears_status_on_no_errors() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);
    app.set_status_message(Some("old error".into()));

    let snap = snapshot(&repo_path);
    app.apply_snapshot(snap);

    assert!(app.model.status_message.is_none());
}

#[test]
fn apply_snapshot_unknown_repo_is_noop() {
    let mut app = stub_app();
    let snap = snapshot(Path::new("/nonexistent"));
    app.apply_snapshot(snap);
}

#[test]
fn apply_snapshot_requests_initial_issue_fetch() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);

    let snap = snapshot(&repo_path);
    app.apply_snapshot(snap);

    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some((Command { action: CommandAction::SetIssueViewport { .. }, .. }, _))));
    // Second snapshot should NOT queue another
    let snap2 = snapshot(&repo_path);
    app.apply_snapshot(snap2);
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn apply_snapshot_sets_unseen_changes_for_inactive_tab() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    // First snapshot to establish baseline providers
    let snap1 = snapshot(&inactive_path);
    app.apply_snapshot(snap1);

    // Second snapshot with different providers
    let mut snap2 = snapshot(&inactive_path);
    snap2.seq = 2;
    snap2.work_items = vec![checkout_item("feat", "/wt", false)];
    let mut different_providers = ProviderData::default();
    different_providers.checkouts.insert(
        flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from("/wt")),
        flotilla_protocol::Checkout {
            branch: "feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        },
    );
    snap2.providers = different_providers;
    app.apply_snapshot(snap2);

    assert!(app.model.repos[&inactive_repo].has_unseen_changes);
}

// -- apply_delta --

#[test]
fn apply_delta_updates_issue_metadata() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let mut change = delta(&repo_path, vec![]);
    change.issue_total = Some(10);
    change.issue_has_more = true;
    app.apply_delta(change);

    let rm = &app.model.repos[&repo];
    assert_eq!(rm.issue_total, Some(10));
    assert!(rm.issue_has_more);
    assert!(!rm.issue_fetch_pending);
}

#[test]
fn apply_delta_unknown_repo_is_noop() {
    let mut app = stub_app();
    let mut change = delta(Path::new("/nonexistent"), vec![]);
    change.seq = 1;
    change.prev_seq = 0;
    app.apply_delta(change);
}

#[test]
fn apply_delta_provider_health_added() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Added(true),
    }]);
    app.apply_delta(change);

    assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
    assert!(app.model.repos[&repo].provider_health["vcs"]["git"]);
}

#[test]
fn apply_delta_provider_health_removed() {
    let mut app = stub_app();
    let repo = app.model.active_repo_identity().clone();
    let repo_path = active_repo_path(&app);

    app.model.repos.get_mut(&repo).unwrap().provider_health.entry("vcs".into()).or_default().insert("git".into(), true);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Removed,
    }]);
    app.apply_delta(change);

    assert!(!app.model.repos[&repo].provider_health.contains_key("vcs"));
}

#[test]
fn apply_delta_errors_changed_updates_status() {
    let mut app = stub_app();
    let repo_path = active_repo_path(&app);

    let change = delta(&repo_path, vec![flotilla_protocol::Change::ErrorsChanged(vec![provider_error("cr", "gh", "broken")])]);
    app.apply_delta(change);

    assert!(app.model.status_message.as_ref().unwrap().contains("broken"));
}

#[test]
fn apply_delta_data_change_on_inactive_tab_sets_unseen() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    let change = delta(&inactive_path, vec![flotilla_protocol::Change::Session {
        key: "s1".into(),
        op: flotilla_protocol::EntryOp::Added(flotilla_protocol::CloudAgentSession {
            title: "new session".into(),
            status: flotilla_protocol::SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }),
    }]);
    app.apply_delta(change);

    assert!(app.model.repos[&inactive_repo].has_unseen_changes);
}

#[test]
fn apply_delta_health_only_change_does_not_set_unseen() {
    let mut app = stub_app_with_repos(2);
    let inactive_repo = app.model.repo_order[1].clone();
    let inactive_path = app.model.repos[&inactive_repo].path.clone();

    let change = delta(&inactive_path, vec![flotilla_protocol::Change::ProviderHealth {
        category: "vcs".into(),
        provider: "git".into(),
        op: flotilla_protocol::EntryOp::Added(true),
    }]);
    app.apply_delta(change);

    assert!(!app.model.repos[&inactive_repo].has_unseen_changes);
}

// -- handle_repo_added / handle_repo_removed --

#[test]
fn handle_repo_added_adds_new_repo() {
    let mut app = stub_app();
    assert_eq!(app.model.repos.len(), 1);

    let info = repo_info("/tmp/new-repo", "new-repo", RepoLabels::default());
    app.handle_repo_added(info);

    assert_eq!(app.model.repos.len(), 2);
    assert!(app.model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/new-repo")));
    assert_eq!(app.model.repos[app.model.repo_order.last().unwrap()].path, PathBuf::from("/tmp/new-repo"));
    // Adding a repo should not switch to it (it may arrive asynchronously)
    assert_eq!(app.model.active_repo, 0);
}

#[test]
fn handle_repo_added_duplicate_is_noop() {
    let mut app = stub_app();
    let existing_path = app.model.repo_order[0].clone();
    let info = repo_info(app.model.repos[&existing_path].path.clone(), "dup", RepoLabels::default());
    app.handle_repo_added(info);
    assert_eq!(app.model.repos.len(), 1);
}

#[test]
fn handle_repo_removed_removes_repo() {
    let mut app = stub_app_with_repos(2);
    let path = app.model.repo_order[0].clone();

    app.handle_repo_removed(&path);

    assert_eq!(app.model.repos.len(), 1);
    assert!(!app.model.repos.contains_key(&path));
    assert!(!app.model.repo_order.contains(&path));
}

#[test]
fn handle_repo_removed_last_repo_sets_quit() {
    let mut app = stub_app();
    let path = app.model.repo_order[0].clone();

    app.handle_repo_removed(&path);

    assert!(app.should_quit);
}

#[test]
fn handle_repo_removed_adjusts_active_index() {
    let mut app = stub_app_with_repos(3);
    app.model.active_repo = 2;
    let last_path = app.model.repo_order[2].clone();

    app.handle_repo_removed(&last_path);

    assert_eq!(app.model.active_repo, 1);
}

#[test]
fn handle_repo_removed_syncs_layout_from_new_active_page() {
    let mut app = stub_app_with_repos(2);
    // Give the two repo pages different layouts.
    let repo0 = app.model.repo_order[0].clone();
    let repo1 = app.model.repo_order[1].clone();
    app.screen.repo_pages.get_mut(&repo0).expect("page 0").layout = RepoViewLayout::Zoom;
    app.screen.repo_pages.get_mut(&repo1).expect("page 1").layout = RepoViewLayout::Below;

    // Active repo is 1 (Below layout). Remove it.
    app.model.active_repo = 1;
    app.handle_repo_removed(&repo1);

    // Active repo should now be 0, and ui.view_layout should match its page.
    assert_eq!(app.model.active_repo, 0);
    assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
}

// -- handle_daemon_event --

#[test]
fn handle_daemon_event_command_started_tracked() {
    let mut app = stub_app();
    let repo = app.model.active_repo_root().clone();

    app.handle_daemon_event(DaemonEvent::CommandStarted {
        command_id: 99,
        host: HostName::local(),
        repo_identity: app.model.active_repo_identity().clone(),
        repo: repo.clone(),
        description: "test cmd".into(),
    });

    assert!(app.in_flight.contains_key(&99));
    assert_eq!(app.in_flight[&99].description, "test cmd");
}

#[test]
fn step_failure_surfaces_error_in_status_message() {
    let mut app = stub_app();
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand {
        repo_identity: repo_identity.clone(),
        repo: repo_path.clone(),
        description: "Creating checkout...".into(),
    });

    app.handle_daemon_event(DaemonEvent::CommandStepUpdate {
        command_id: 42,
        host: HostName::local(),
        repo_identity,
        repo: repo_path,
        step_index: 0,
        step_count: 1,
        description: "Create checkout for branch my-branch".into(),
        status: StepStatus::Failed { message: "branch already exists: my-branch".into() },
    });

    let msg = app.model.status_message.as_deref().expect("status_message should be set");
    assert!(msg.contains("branch already exists"), "expected error detail in status message, got: {msg}");
}

#[test]
fn peer_disconnect_clears_selected_target_host() {
    let mut app = stub_app();
    app.ui.target_host = Some(HostName::new("alpha"));
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

    app.handle_daemon_event(DaemonEvent::PeerStatusChanged { host: HostName::new("alpha"), status: PeerConnectionState::Disconnected });

    assert_eq!(app.ui.target_host, None);
    assert_eq!(app.model.hosts.get(&HostName::new("alpha")).unwrap().status, PeerStatus::Disconnected);
}

#[test]
fn host_removed_event_deletes_host_and_clears_selected_target_host() {
    let mut app = stub_app();
    app.ui.target_host = Some(HostName::new("alpha"));
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

    app.handle_daemon_event(DaemonEvent::HostRemoved { host: HostName::new("alpha"), seq: 2 });

    assert_eq!(app.ui.target_host, None);
    assert!(!app.model.hosts.contains_key(&HostName::new("alpha")));
}

// -- Convenience accessors --

#[test]
fn selected_work_item_none_when_no_selection() {
    let app = stub_app();
    assert!(app.selected_work_item().is_none());
}

#[test]
fn selected_work_item_returns_item() {
    let mut app = stub_app();
    setup_selectable_table(&mut app, vec![checkout_item("feat", "/wt", false)]);
    let item = app.selected_work_item();
    assert!(item.is_some());
    assert_eq!(item.unwrap().branch.as_deref(), Some("feat"));
}

// -- CloseConfirm flow (via widget stack) --

fn push_close_confirm_widget(app: &mut App, id: &str) {
    let widget = crate::widgets::close_confirm::CloseConfirmWidget::new(
        id.into(),
        "Test PR".into(),
        WorkItemIdentity::Session("test".into()),
        Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: id.into() } },
    );
    app.screen.modal_stack.push(Box::new(widget));
}

#[test]
fn close_confirm_y_dispatches_command() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Char('y')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
}

#[test]
fn close_confirm_enter_dispatches_command() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
}

#[test]
fn close_confirm_esc_cancels() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn close_confirm_n_cancels() {
    let mut app = stub_app();
    push_close_confirm_widget(&mut app, "42");
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

// -- CommandQueue with PendingActionContext --

#[test]
fn command_queue_push_with_context() {
    use crate::app::ui_state::PendingActionContext;

    let mut q = CommandQueue::default();
    let ctx = PendingActionContext {
        identity: WorkItemIdentity::Session("s1".into()),
        description: "Archive session".into(),
        repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
    };
    q.push_with_context(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } }, Some(ctx));
    let (cmd, ctx) = q.take_next().expect("should have one entry");
    assert!(matches!(cmd.action, CommandAction::Refresh { .. }));
    assert!(ctx.is_some());
    assert_eq!(ctx.unwrap().description, "Archive session");
}

#[test]
fn command_queue_push_without_context() {
    let mut q = CommandQueue::default();
    q.push(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
    let (_, ctx) = q.take_next().expect("should have one entry");
    assert!(ctx.is_none());
}

// -- Pending action lifecycle on CommandFinished --

#[test]
fn command_finished_ok_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::local(),
        repo_identity: repo.clone(),
        repo: repo_path,
        result: CommandValue::Ok,
    });

    assert!(!app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn command_finished_error_transitions_to_failed() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::local(),
        repo_identity: repo.clone(),
        repo: repo_path,
        result: CommandValue::Error { message: "boom".into() },
    });

    let pending = &app.screen.repo_pages[&repo].pending_actions[&identity];
    assert!(matches!(pending.status, PendingStatus::Failed(ref msg) if msg == "boom"));
}

#[test]
fn command_finished_cancelled_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 42,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::local(),
        repo_identity: repo.clone(),
        repo: repo_path,
        result: CommandValue::Cancelled,
    });

    assert!(!app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn orphaned_command_finished_harmlessly_ignored() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo].path.clone();
    let identity = WorkItemIdentity::Session("s1".into());

    // Insert pending action with command_id 99 (different from finished event)
    app.screen.repo_pages.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
        command_id: 99,
        status: PendingStatus::InFlight,
        description: "test".into(),
    });
    app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::local(),
        repo_identity: repo.clone(),
        repo: repo_path,
        result: CommandValue::Ok,
    });

    // The pending action with command_id 99 should still be there
    assert!(app.screen.repo_pages[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn local_checkout_created_does_not_queue_workspace() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "my-desktop");
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand { repo_identity: repo_identity.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::new("my-desktop"),
        repo_identity,
        repo: repo_path,
        result: CommandValue::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/tmp/repo/wt-feat") },
    });

    assert!(app.proto_commands.take_next().is_none(), "workspace creation is now handled by checkout plan, not TUI");
}

#[test]
fn remote_checkout_created_does_not_queue_workspace() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "my-desktop");
    let repo_identity = app.model.repo_order[0].clone();
    let repo_path = app.model.repos[&repo_identity].path.clone();

    app.in_flight.insert(42, InFlightCommand { repo_identity: repo_identity.clone(), repo: repo_path.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        host: HostName::new("remote-a"),
        repo_identity,
        repo: repo_path,
        result: CommandValue::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/remote/wt-feat") },
    });

    assert!(app.proto_commands.take_next().is_none(), "remote checkout should not auto-create local workspace");
}

// -- TuiHostState / hosts map --

#[test]
fn host_snapshot_event_populates_hosts_map() {
    let mut app = stub_app();
    app.handle_daemon_event(DaemonEvent::HostSnapshot(Box::new(flotilla_protocol::HostSnapshot {
        seq: 1,
        host_name: HostName::new("desktop"),
        is_local: true,
        connection_status: PeerConnectionState::Connected,
        summary: HostSummary {
            host_name: HostName::new("desktop"),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
        },
    })));
    assert_eq!(app.model.my_host(), Some(&HostName::new("desktop")));
    assert!(app.model.hosts.get(&HostName::new("desktop")).unwrap().is_local);
}

#[test]
fn my_host_returns_none_before_host_snapshot() {
    let app = stub_app();
    assert!(app.model.my_host().is_none());
}

#[test]
fn peer_host_names_returns_sorted_non_local() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "local");
    insert_peer_host(&mut app.model, "beta", PeerStatus::Connected);
    insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);
    assert_eq!(app.model.peer_host_names(), vec![HostName::new("alpha"), HostName::new("beta")]);
}
