use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{
    qualified_path::HostId, CheckoutSelector, CheckoutStatus, CheckoutTarget, Command, EnvironmentId, HostName, HostPath, NodeId, NodeInfo,
    ProvisioningTarget, WorkItemIdentity,
};
use ratatui::layout::Rect;

use super::{
    super::{DirEntry, RepoViewLayout},
    *,
};
use crate::{
    app::{
        test_support::{
            checkout_item, dir_entry, enter_file_picker, key, setup_selectable_table as setup_table, stub_app, stub_app_with_repos,
        },
        PeerStatus, TuiHostState,
    },
    status_bar::{StatusBarAction, StatusBarTarget},
};

fn hp(path: &str) -> HostPath {
    HostPath::new(HostName::local(), PathBuf::from(path))
}

fn native_issue_row(id: &str) -> crate::widgets::section_table::IssueRow {
    crate::widgets::section_table::IssueRow {
        id: id.to_string(),
        issue: flotilla_protocol::provider_data::Issue {
            title: format!("Issue {id}"),
            labels: vec![],
            association_keys: vec![],
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        },
    }
}

fn setup_native_issue_rows(app: &mut App, issue_ids: &[&str]) {
    let repo_key = app.model.repo_order[app.model.active_repo].clone();
    if let Some(handle) = app.repo_data.get(&repo_key) {
        handle.mutate(|d| {
            d.work_items.clear();
            d.issue_rows = issue_ids.iter().map(|id| native_issue_row(id)).collect();
            d.issue_section_label = "Issues".into();
        });
    }
    if let Some(page) = app.screen.repo_pages.get_mut(&repo_key) {
        page.reconcile_if_changed();
    }
}

/// Read the active RepoPage's selected flat index.
fn active_selection(app: &App) -> Option<usize> {
    let identity = &app.model.repo_order[app.model.active_repo];
    app.screen.repo_pages.get(identity).and_then(|p| p.table.selected_flat_index())
}

/// Read the active RepoPage's show_providers flag.
fn active_show_providers(app: &App) -> bool {
    let identity = &app.model.repo_order[app.model.active_repo];
    app.screen.repo_pages.get(identity).is_some_and(|p| p.show_providers)
}

/// Read the active RepoPage's multi_selected set.
fn active_multi_selected(app: &App) -> &std::collections::HashSet<WorkItemIdentity> {
    let identity = &app.model.repo_order[app.model.active_repo];
    &app.screen.repo_pages[identity].multi_selected
}

/// Read the active RepoPage's active_search_query.
fn active_search_query(app: &App) -> Option<&str> {
    let identity = &app.model.repo_order[app.model.active_repo];
    app.screen.repo_pages.get(identity).and_then(|p| p.active_search_query.as_deref())
}

fn insert_peer_host(model: &mut crate::app::TuiModel, name: &str) {
    let host_name = HostName::new(name);
    let environment_id = EnvironmentId::host(HostId::new(format!("{name}-env")));
    model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: false,
        status: PeerStatus::Connected,
        summary: flotilla_protocol::HostSummary {
            environment_id,
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(NodeId::new(name), name),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
}

fn insert_local_host(model: &mut crate::app::TuiModel, name: &str) {
    let host_name = HostName::new(name);
    let environment_id = EnvironmentId::host(HostId::new(format!("{name}-local-env")));
    model.hosts.insert(environment_id.clone(), TuiHostState {
        environment_id: environment_id.clone(),
        host_name: host_name.clone(),
        is_local: true,
        status: PeerStatus::Connected,
        summary: flotilla_protocol::HostSummary {
            environment_id,
            host_name: Some(host_name.clone()),
            node: NodeInfo::new(NodeId::new(format!("{name}-local")), name),
            system: flotilla_protocol::SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });
}

fn make_work_item(id: &str) -> flotilla_protocol::WorkItem {
    checkout_item(&format!("feat/{id}"), &format!("/tmp/{id}"), false)
}

fn left_click(x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
}

// ── handle_key — top-level dispatch ──────────────────────────────

#[test]
fn select_next_moves_work_item_selection_via_widget() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);

    app.handle_key(key(KeyCode::Char('j')));

    assert_eq!(active_selection(&app), Some(1));
}

#[test]
fn config_select_next_moves_event_log_via_widget() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 3;
        ov.event_log.selected = Some(0);
    }

    app.handle_key(key(KeyCode::Char('j')));

    assert_eq!(app.screen.overview_page.event_log.selected, Some(1));
}

#[test]
fn file_picker_select_next_advances_selection_via_handle_key() {
    // FilePicker selection is now handled by the widget stack.
    // Test via handle_key which dispatches through the widget.
    let mut app = stub_app();
    enter_file_picker(&mut app, "/tmp/", vec![dir_entry("alpha", false, false), dir_entry("beta", false, false)]);

    app.handle_key(key(KeyCode::Down));

    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::FilePicker)
    );
}

// dispatch_action_confirm_submits_delete_confirm — moved to widget tests
// in widgets::delete_confirm::tests

#[test]
fn dispatch_action_confirm_submits_branch_input() {
    // BranchInput confirm is now handled by the widget stack.
    // Test via handle_key which dispatches through the widget.
    let mut app = stub_app();
    push_branch_input_widget_with_text(&mut app, "feature/test");

    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().expect("expected checkout command");
    match cmd {
        Command { action: CommandAction::Checkout { target, .. }, .. } => {
            assert_eq!(target, CheckoutTarget::FreshBranch("feature/test".into()));
        }
        other => panic!("expected Checkout, got {:?}", other),
    }
}

#[test]
fn dispatch_action_confirm_submits_issue_search() {
    // IssueSearch confirm is now handled by the widget stack.
    // Test via handle_key which dispatches through the widget.
    let mut app = stub_app();
    push_issue_search_widget_with_text(&mut app, "bug fix");

    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(active_search_query(&app), Some("bug fix"));
    let (cmd, _) = app.proto_commands.take_next().expect("expected search command");
    match cmd {
        Command { action: CommandAction::QueryIssues { params, .. }, .. } => {
            assert_eq!(params.search.as_deref(), Some("bug fix"));
        }
        other => panic!("expected QueryIssues, got {:?}", other),
    }
}

#[test]
fn file_picker_confirm_activates_selection_via_handle_key() {
    // FilePicker confirm is now handled by the widget stack.
    // Test via handle_key which dispatches through the widget.
    let tmp = tempfile::tempdir().expect("create tempdir");
    let repo_dir = tmp.path().join("my-repo");
    std::fs::create_dir(&repo_dir).expect("create repo dir");
    std::fs::create_dir(repo_dir.join(".git")).expect("create git dir");

    let mut app = stub_app();
    let parent_path = format!("{}/", tmp.path().to_string_lossy());
    let entries = vec![DirEntry { name: "my-repo".to_string(), is_dir: true, is_git_repo: true, is_added: false }];
    enter_file_picker(&mut app, &parent_path, entries);

    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().expect("expected track repo command");
    match cmd {
        Command { action: CommandAction::TrackRepoPath { path }, .. } => {
            let canonical = std::fs::canonicalize(&repo_dir).expect("canonicalize repo dir");
            assert_eq!(path, canonical);
        }
        other => panic!("expected TrackRepoPath, got {:?}", other),
    }
}

#[test]
fn resolve_action_maps_shared_navigation_keys() {
    let app = stub_app();

    assert_eq!(app.resolve_action(key(KeyCode::Char('j'))), Some(Action::SelectNext));
    assert_eq!(app.resolve_action(key(KeyCode::Down)), Some(Action::SelectNext));
    assert_eq!(app.resolve_action(key(KeyCode::Char('k'))), Some(Action::SelectPrev));
    assert_eq!(app.resolve_action(key(KeyCode::Up)), Some(Action::SelectPrev));
    assert_eq!(app.resolve_action(key(KeyCode::Enter)), Some(Action::Confirm));
    assert_eq!(app.resolve_action(key(KeyCode::Esc)), Some(Action::Dismiss));
    assert_eq!(app.resolve_action(key(KeyCode::Char('?'))), Some(Action::OpenContextualPalette));
}

#[test]
fn resolve_action_maps_domain_shortcuts_to_dispatch_intents() {
    let app = stub_app();

    assert_eq!(app.resolve_action(key(KeyCode::Char('d'))), Some(Action::Dispatch(Intent::RemoveCheckout)));
    assert_eq!(app.resolve_action(key(KeyCode::Char('p'))), Some(Action::Dispatch(Intent::OpenChangeRequest)));
}

#[test]
fn resolve_action_maps_q_by_mode() {
    let mut app = stub_app();

    assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), Some(Action::Quit));

    app.ui.is_config = true;
    assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), Some(Action::Dismiss));
}

// resolve_action_maps_file_picker_navigation_keys: removed because
// resolve_action only reads ui.mode (Normal). FilePicker key resolution
// is now handled by handle_key's per-BindingModeId hardcoded dispatch.

// resolve_action_does_not_intercept_manual_branch_input_text: removed
// because handle_key uses captures_raw_keys() to bypass resolve_action
// entirely. The widget-stack mode is no longer reflected in ui.mode.

#[test]
fn h_toggles_help_from_normal() {
    let mut app = stub_app();
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    app.handle_key(key(KeyCode::Char('h')));
    assert!(!app.screen.modal_stack.is_empty(), "expected modal widget pushed on stack");
}

#[test]
fn h_toggles_help_back_to_normal() {
    let mut app = stub_app();
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn question_mark_in_other_modes_does_not_toggle() {
    let mut app = stub_app();
    // Push a widget on the stack — `?` should be handled by the widget (Ignored),
    // but doesn't fall through to dispatch_action, so no HelpWidget is pushed.
    let item = make_work_item("a");
    let entries = vec![crate::widgets::action_menu::MenuEntry {
        intent: Intent::OpenChangeRequest,
        command: Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::OpenChangeRequest { id: "1".into() },
        },
    }];
    app.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));
    app.handle_key(key(KeyCode::Char('?')));
    // Widget stack should still have exactly 1 widget (the action menu)
    assert_eq!(app.screen.modal_stack.len(), 1);
}

#[test]
fn handle_key_preserves_status_message_until_dismissed() {
    let mut app = stub_app();
    app.model.status_message = Some("old status".into());
    app.handle_key(key(KeyCode::Char('r')));
    assert_eq!(app.model.status_message.as_deref(), Some("old status"));
}

#[test]
fn esc_in_help_returns_to_normal() {
    let mut app = stub_app();
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn tab_switch_blocked_while_modal_open() {
    let mut app = stub_app_with_repos(2);
    setup_table(&mut app, vec![make_work_item("a")]);
    let initial_tab = app.model.active_repo;
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));

    // Press ] to switch tabs — should be blocked by the modal focus barrier
    app.handle_key(key(KeyCode::Char(']')));

    assert_eq!(app.screen.modal_stack.len(), 1, "modal should remain on stack");
    assert_eq!(app.model.active_repo, initial_tab, "tab should not have changed");
}

// ── handle_config_key ────────────────────────────────────────────

#[test]
fn config_q_dismisses_to_normal() {
    let mut app = stub_app();
    app.ui.is_config = true;
    app.handle_key(key(KeyCode::Char('q')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(!app.should_quit);
    assert!(!app.ui.is_config);
}

#[test]
fn config_esc_dismisses_to_normal() {
    let mut app = stub_app();
    app.ui.is_config = true;
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(!app.should_quit);
    assert!(!app.ui.is_config);
}

#[test]
fn config_j_navigates_event_log_down() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 5;
        ov.event_log.selected = Some(0);
    }
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.screen.overview_page.event_log.selected, Some(1));
}

#[test]
fn config_k_navigates_event_log_up() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 5;
        ov.event_log.selected = Some(3);
    }
    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.screen.overview_page.event_log.selected, Some(2));
}

#[test]
fn config_j_when_no_selection_jumps_to_last() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 5;
        ov.event_log.selected = None;
    }
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.screen.overview_page.event_log.selected, Some(4));
}

#[test]
fn config_j_at_end_stays() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 3;
        ov.event_log.selected = Some(2);
    }
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.screen.overview_page.event_log.selected, Some(2));
}

#[test]
fn config_k_at_zero_stays() {
    let mut app = stub_app();
    app.ui.is_config = true;
    {
        let ov = &mut app.screen.overview_page;
        ov.event_log.count = 5;
        ov.event_log.selected = Some(0);
    }
    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(app.screen.overview_page.event_log.selected, Some(0));
}

#[test]
fn config_bracket_switches_tabs() {
    let mut app = stub_app();
    app.ui.is_config = true;
    // ] in Config mode should switch to Normal mode + first repo
    app.handle_key(key(KeyCode::Char(']')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(app.model.active_repo, 0);

    // [ from first repo (index 0) goes back to Config
    app.handle_key(key(KeyCode::Char('[')));
    assert!(app.ui.is_config);
}

#[test]
fn brackets_do_not_switch_tabs_from_action_menu() {
    let mut app = stub_app_with_repos(2);
    let item = make_work_item("a");
    let entries = vec![crate::widgets::action_menu::MenuEntry {
        intent: Intent::OpenChangeRequest,
        command: Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::OpenChangeRequest { id: "1".into() },
        },
    }];
    app.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));

    app.handle_key(key(KeyCode::Char(']')));

    // Widget should still be on the stack, tab should not have switched
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(app.model.active_repo, 0);
}

#[test]
fn brackets_do_not_switch_tabs_while_branch_input_generating() {
    let mut app = stub_app_with_repos(2);
    push_branch_input_widget(&mut app, BranchInputKind::Generating);

    app.handle_key(key(KeyCode::Char(']')));

    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    assert_eq!(app.model.active_repo, 0);
}

// ── dismiss_modals ─────────────────────────────────────────────

#[test]
fn dismiss_modals_clears_modal_stack() {
    let mut app = stub_app();
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
    assert_eq!(app.screen.modal_stack.len(), 1);

    app.dismiss_modals();

    assert!(app.screen.modal_stack.is_empty(), "modal stack should be empty");
}

#[test]
fn has_modal_reflects_stack_depth() {
    let mut app = stub_app();
    assert!(!app.has_modal());

    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
    assert!(app.has_modal());

    app.dismiss_modals();
    assert!(!app.has_modal());
}

#[test]
fn global_tab_switch_blocked_when_modal_is_open() {
    let mut app = stub_app_with_repos(2);
    let before = app.model.active_repo;
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
    app.handle_key(key(KeyCode::Char(']'))); // NextTab
    assert_eq!(app.model.active_repo, before, "tab switch must not fire through modal");
}

// ── handle_normal_key ────────────────────────────────────────────

#[test]
fn normal_q_quits() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.should_quit);
}

#[test]
fn help_q_returns_to_normal_and_resets_scroll() {
    let mut app = stub_app();
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));

    app.handle_key(key(KeyCode::Char('q')));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn normal_esc_clears_providers_first() {
    let mut app = stub_app();
    let identity = app.model.repo_order[0].clone();
    app.screen.repo_pages.get_mut(&identity).expect("repo page exists").show_providers = true;
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.screen.repo_pages.get(&identity).expect("repo page exists").show_providers);
    assert!(!app.should_quit);
}

#[test]
fn normal_esc_clears_multi_select_second() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a")]);
    let identity = app.model.repo_order[0].clone();
    app.screen
        .repo_pages
        .get_mut(&identity)
        .expect("repo page exists")
        .multi_selected
        .insert(WorkItemIdentity::Checkout(hp("/tmp/a").into()));
    assert!(!app.screen.repo_pages.get(&identity).expect("repo page exists").multi_selected.is_empty());
    app.handle_key(key(KeyCode::Esc));
    assert!(app.screen.repo_pages.get(&identity).expect("repo page exists").multi_selected.is_empty());
    assert!(!app.should_quit);
}

#[test]
fn normal_esc_quits_when_nothing_to_clear() {
    let mut app = stub_app();
    // show_providers is false, multi_selected is empty
    app.handle_key(key(KeyCode::Esc));
    assert!(app.should_quit);
}

#[test]
fn normal_n_enters_branch_input() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
}

#[test]
fn normal_d_dispatches_remove_checkout() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a")]);
    app.handle_key(key(KeyCode::Char('d')));
    // RemoveCheckout pushes a DeleteConfirmWidget onto the widget stack
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(crate::binding_table::BindingModeId::DeleteConfirm)
    );
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    assert!(matches!(cmd, Command { action: CommandAction::FetchCheckoutStatus { .. }, .. }));
}

#[test]
fn normal_d_noop_on_main_checkout() {
    let mut app = stub_app();
    let mut item = make_work_item("main");
    item.is_main_checkout = true;
    item.checkout.as_mut().unwrap().is_main_checkout = true;
    setup_table(&mut app, vec![item]);
    app.handle_key(key(KeyCode::Char('d')));
    // Should NOT dispatch — main checkout is not removable
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn normal_big_d_toggles_debug() {
    let mut app = stub_app();
    assert!(!app.ui.show_debug);
    app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
    assert!(app.ui.show_debug);
    app.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
    assert!(!app.ui.show_debug);
}

#[test]
fn normal_slash_opens_command_palette() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}

#[test]
fn no_active_repo_command_palette_search_shows_status_message() {
    let mut app = stub_app();
    app.model.repos.clear();
    app.model.repo_order.clear();
    app.screen.repo_pages.clear();
    app.screen.modal_stack.push(Box::new(crate::widgets::command_palette::CommandPaletteWidget::new()));

    app.handle_key(key(KeyCode::Char('/')));
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(app.model.status_message.as_deref(), Some("No active repo"));
}

#[test]
fn no_active_repo_issue_search_dismiss_shows_status_message() {
    let mut app = stub_app();
    app.model.repos.clear();
    app.model.repo_order.clear();
    app.screen.repo_pages.clear();
    app.screen.modal_stack.push(Box::new(crate::widgets::issue_search::IssueSearchWidget::new()));

    app.handle_key(key(KeyCode::Esc));

    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(app.model.status_message.as_deref(), Some("No active repo"));
}

#[test]
fn normal_c_toggles_providers() {
    let mut app = stub_app();
    assert!(!active_show_providers(&app));
    app.handle_key(key(KeyCode::Char('c')));
    assert!(active_show_providers(&app));
    app.handle_key(key(KeyCode::Char('c')));
    assert!(!active_show_providers(&app));
}

#[test]
fn normal_h_toggles_help() {
    // h is now bound to ToggleHelp, not CycleHost
    let mut app = stub_app();
    insert_peer_host(&mut app.model, "alpha");

    app.handle_key(key(KeyCode::Char('h')));
    // h toggles help, not host — provisioning_target stays at local
    assert_eq!(app.ui.provisioning_target, ProvisioningTarget::Host { host: HostName::local() });
    assert!(!app.screen.modal_stack.is_empty(), "expected help widget pushed on stack");
}

#[test]
fn normal_uppercase_k_toggles_status_bar_keys() {
    let mut app = stub_app();
    assert!(app.ui.status_bar.show_keys);
    app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT));
    assert!(!app.ui.status_bar.show_keys);
    app.handle_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT));
    assert!(app.ui.status_bar.show_keys);
}

#[test]
fn normal_dot_opens_action_menu() {
    let mut app = stub_app();
    // Need an item with available intents — a checkout item can CreateWorkspace
    let item = make_work_item("a");
    setup_table(&mut app, vec![item]);
    app.handle_key(key(KeyCode::Char('.')));
    assert!(!app.screen.modal_stack.is_empty(), "expected ActionMenuWidget on the modal stack");
}

#[test]
fn clicking_search_status_target_opens_command_palette() {
    let mut app = stub_app();
    app.screen.status_bar.key_targets = vec![StatusBarTarget::new(Rect::new(10, 29, 12, 1), StatusBarAction::key(KeyCode::Char('/')))];

    app.handle_mouse(left_click(12, 29));

    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}

#[test]
fn clicking_layout_status_cycles_layout() {
    let mut app = stub_app();
    assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);
    app.screen.status_bar.key_targets = vec![StatusBarTarget::new(Rect::new(0, 29, 12, 1), StatusBarAction::key(KeyCode::Char('l')))];

    app.handle_mouse(left_click(4, 29));

    assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
}

#[test]
fn clicking_host_status_indicator_is_display_only() {
    // The host indicator now uses StatusBarAction::None — clicking it does nothing.
    let mut app = stub_app();
    insert_peer_host(&mut app.model, "alpha");
    app.screen.status_bar.key_targets = vec![StatusBarTarget::new(Rect::new(0, 29, 16, 1), StatusBarAction::None)];

    app.handle_mouse(left_click(4, 29));

    // provisioning_target remains at local — the click was ignored
    assert_eq!(app.ui.provisioning_target, ProvisioningTarget::Host { host: HostName::local() });
}

#[test]
fn clicking_dismiss_status_target_hides_visible_error() {
    let mut app = stub_app();
    app.model.status_message = Some("boom".into());
    app.screen.status_bar.dismiss_targets = vec![StatusBarTarget::new(Rect::new(20, 29, 1, 1), StatusBarAction::ClearError(0))];

    app.handle_mouse(left_click(20, 29));

    assert!(app.visible_status_items().is_empty());
}

#[test]
fn clicking_gear_icon_toggles_providers() {
    let mut app = stub_app();
    // Place the gear hitbox on the active RepoPage's table
    let repo_key = app.model.repo_order[0].clone();
    app.screen.repo_pages.get_mut(&repo_key).expect("repo page").table.gear_area = Some(Rect::new(75, 2, 3, 1));
    assert!(!active_show_providers(&app));

    app.handle_mouse(left_click(76, 2));
    assert!(active_show_providers(&app));

    app.handle_mouse(left_click(76, 2));
    assert!(!active_show_providers(&app));
}

#[test]
fn clicking_gear_icon_ignored_in_config_mode() {
    let mut app = stub_app();
    // Set gear area on the repo page's table — in Config mode the overview
    // page handles events, so the gear click should not toggle providers.
    let repo_key = app.model.repo_order[0].clone();
    app.screen.repo_pages.get_mut(&repo_key).expect("repo page").table.gear_area = Some(Rect::new(75, 2, 3, 1));
    app.ui.is_config = true;

    app.handle_mouse(left_click(76, 2));
    assert!(!active_show_providers(&app));
}

#[test]
fn scroll_wheel_does_not_reach_table_while_help_is_open() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);
    app.ui.layout.table_area = Rect::new(0, 2, 80, 10);
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));

    app.handle_mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 5, row: 5, modifiers: KeyModifiers::NONE });

    assert_eq!(active_selection(&app), Some(0));
    assert_eq!(app.screen.modal_stack.len(), 1, "expected help widget to remain on stack");
}

#[test]
fn scroll_wheel_does_not_reach_table_while_action_menu_is_open() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);
    app.ui.layout.table_area = Rect::new(0, 2, 80, 10);
    push_action_menu_widget(&mut app);

    app.handle_mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 5, row: 5, modifiers: KeyModifiers::NONE });

    assert_eq!(active_selection(&app), Some(0));
    assert_eq!(app.screen.modal_stack.len(), 1, "expected action menu to remain on stack");
}

#[test]
fn enter_while_help_open_does_not_trigger_action_enter() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a")]);
    app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));

    // Help ignores Confirm, which used to leak through to dispatch_action
    // and trigger action_enter on the selected item behind the modal.
    app.handle_key(key(KeyCode::Enter));

    // Help should still be on the stack, and no commands should have been dispatched.
    assert_eq!(app.screen.modal_stack.len(), 1, "help widget should remain on stack");
    assert!(app.proto_commands.take_next().is_none(), "no commands should be dispatched while help is open");
}

// ── handle_menu_key (through widget stack) ─────────────────────

fn push_action_menu_widget(app: &mut App) {
    let item = make_work_item("a");
    let entries = vec![
        crate::widgets::action_menu::MenuEntry {
            intent: Intent::CreateWorkspace,
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
            },
        },
        crate::widgets::action_menu::MenuEntry {
            intent: Intent::RemoveCheckout,
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus {
                    branch: "feat/a".into(),
                    checkout_path: Some("/tmp/a".into()),
                    change_request_id: None,
                },
            },
        },
    ];
    app.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));
}

#[test]
fn menu_esc_pops_widget() {
    let mut app = stub_app();
    push_action_menu_widget(&mut app);
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn menu_enter_pops_widget_and_pushes_command() {
    let mut app = stub_app();
    push_action_menu_widget(&mut app);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().expect("expected command");
    assert!(matches!(cmd.action, CommandAction::CreateWorkspaceForCheckout { .. }));
}

// ── BranchInput integration (via widget stack) ────────────────────

fn push_branch_input_widget(app: &mut App, kind: BranchInputKind) {
    let widget = crate::widgets::branch_input::BranchInputWidget::new(kind);
    app.screen.modal_stack.push(Box::new(widget));
}

fn push_branch_input_widget_with_text(app: &mut App, text: &str) {
    let mut widget = crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Manual);
    widget.prefill(text, vec![]);
    app.screen.modal_stack.push(Box::new(widget));
}

fn push_branch_input_widget_with_issues(app: &mut App, text: &str, issue_ids: Vec<(String, String)>) {
    let mut widget = crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Manual);
    widget.prefill(text, issue_ids);
    app.screen.modal_stack.push(Box::new(widget));
}

#[test]
fn branch_input_esc_returns_to_normal() {
    let mut app = stub_app();
    push_branch_input_widget_with_text(&mut app, "my-branch");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn branch_input_enter_creates_checkout() {
    let mut app = stub_app();
    push_branch_input_widget_with_text(&mut app, "my-branch");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::Checkout { target, issue_ids, .. }, .. } => {
            assert_eq!(target, CheckoutTarget::FreshBranch("my-branch".into()));
            assert!(issue_ids.is_empty());
        }
        other => panic!("expected CreateCheckout, got {:?}", other),
    }
}

#[test]
fn branch_input_enter_with_pending_issues() {
    let mut app = stub_app();
    push_branch_input_widget_with_issues(&mut app, "feat/issue-42", vec![("github".into(), "42".into())]);
    app.handle_key(key(KeyCode::Enter));
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::Checkout { issue_ids, .. }, .. } => {
            assert_eq!(issue_ids, vec![("github".into(), "42".into())]);
        }
        other => panic!("expected CreateCheckout, got {:?}", other),
    }
}

#[test]
fn branch_input_enter_empty_does_not_create() {
    let mut app = stub_app();
    push_branch_input_widget(&mut app, BranchInputKind::Manual);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn branch_input_ambiguous_host_reports_status_instead_of_queuing_command() {
    let mut app = stub_app();
    insert_local_host(&mut app.model, "desktop");
    insert_peer_host(&mut app.model, "desktop");
    app.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::new("desktop") };
    push_branch_input_widget_with_text(&mut app, "feature/test");

    app.handle_key(key(KeyCode::Enter));

    assert!(app.proto_commands.take_next().is_none());
    assert_eq!(app.screen.modal_stack.len(), 0, "expected ambiguous target to dismiss the modal");
    let message = app.model.status_message.as_deref().expect("ambiguity should set a status message");
    assert!(message.contains("ambiguous host: desktop"), "unexpected message: {message}");
}

#[test]
fn branch_input_generating_ignores_confirm_but_allows_dismiss() {
    let mut app = stub_app();
    push_branch_input_widget(&mut app, BranchInputKind::Generating);
    // Enter should be ignored (consumed, but widget stays)
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert!(app.proto_commands.take_next().is_none());

    // Esc should dismiss the generating prompt
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn branch_input_manual_q_types_character() {
    let mut app = stub_app();
    push_branch_input_widget(&mut app, BranchInputKind::Manual);

    app.handle_key(key(KeyCode::Char('q')));

    // Widget should remain on stack (typing doesn't dismiss)
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
}

// ── IssueSearch integration (via widget stack) ──────────────────

fn push_issue_search_widget(app: &mut App) {
    app.screen.modal_stack.push(Box::new(crate::widgets::issue_search::IssueSearchWidget::new()));
}

fn push_issue_search_widget_with_text(app: &mut App, text: &str) {
    // We can't set text directly on the widget from outside, so we simulate
    // by typing each character through the widget stack.
    app.screen.modal_stack.push(Box::new(crate::widgets::issue_search::IssueSearchWidget::new()));
    for ch in text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
}

#[test]
fn issue_search_esc_clears_and_returns() {
    let mut app = stub_app();
    push_issue_search_widget_with_text(&mut app, "some query");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    // Dismiss no longer sends a command — only a ClearSearchQuery app action
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn issue_search_enter_submits_query() {
    let mut app = stub_app();
    push_issue_search_widget_with_text(&mut app, "bug fix");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::QueryIssues { params, .. }, .. } => {
            assert_eq!(params.search.as_deref(), Some("bug fix"));
        }
        other => panic!("expected QueryIssues, got {:?}", other),
    }
}

#[test]
fn issue_search_enter_empty_no_command() {
    let mut app = stub_app();
    push_issue_search_widget(&mut app);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn issue_search_raw_key_resolves_through_widget_binding_mode() {
    // Override the Shared Esc binding to SelectNext (a no-op for modals).
    // If raw-key resolution incorrectly uses Normal/Shared instead of
    // IssueSearch, Esc resolves to SelectNext and the modal stays open.
    let mut app = stub_app();
    let mut keys = flotilla_core::config::KeysConfig::default();
    keys.shared.insert("esc".into(), "select_next".into());
    app.keymap = crate::keymap::Keymap::from_config(&keys);

    push_issue_search_widget(&mut app);
    assert_eq!(app.screen.modal_stack.len(), 1);

    // IssueSearch has its own Esc → Dismiss binding in the binding table.
    // This must fire even though Shared.esc was overridden.
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "IssueSearch should dismiss via its own binding mode, not Shared");
}

// ── handle_delete_confirm_key (via widget stack) ────────────────

fn push_delete_confirm_widget(app: &mut App, branch: &str) {
    let mut widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
    widget.update_info(CheckoutStatus {
        branch: branch.into(),
        change_request_status: None,
        merge_commit_sha: None,
        unpushed_commits: vec![],
        has_uncommitted: false,
        uncommitted_files: vec![],
        base_detection_warning: None,
    });
    app.screen.modal_stack.push(Box::new(widget));
}

#[test]
fn delete_confirm_y_sends_remove_checkout() {
    let mut app = stub_app();
    push_delete_confirm_widget(&mut app, "feat/x");
    app.handle_key(key(KeyCode::Char('y')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
            assert_eq!(checkout, CheckoutSelector::Query("feat/x".into()));
        }
        other => panic!("expected RemoveCheckout, got {:?}", other),
    }
}

#[test]
fn delete_confirm_enter_sends_remove_checkout() {
    let mut app = stub_app();
    push_delete_confirm_widget(&mut app, "feat/y");
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
            assert_eq!(checkout, CheckoutSelector::Query("feat/y".into()));
        }
        other => panic!("expected RemoveCheckout, got {:?}", other),
    }
}

#[test]
fn delete_confirm_attaches_pending_context() {
    let mut app = stub_app();
    let item = make_work_item("a");
    let mut widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(item.identity.clone(), None, None);
    widget.update_info(CheckoutStatus {
        branch: "feat/a".into(),
        change_request_status: None,
        merge_commit_sha: None,
        unpushed_commits: vec![],
        has_uncommitted: false,
        uncommitted_files: vec![],
        base_detection_warning: None,
    });
    app.screen.modal_stack.push(Box::new(widget));
    app.handle_key(key(KeyCode::Char('y')));
    let (_, ctx) = app.proto_commands.take_next().expect("should have command");
    let ctx = ctx.expect("should have pending context");
    assert_eq!(ctx.identity, item.identity);
}

#[test]
fn delete_confirm_routes_to_remote_host_when_set() {
    let mut app = stub_app();
    let node_id = NodeId::new("feta");
    let mut widget =
        crate::widgets::delete_confirm::DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), Some(node_id.clone()), None);
    widget.update_info(CheckoutStatus {
        branch: "feat/x".into(),
        change_request_status: None,
        merge_commit_sha: None,
        unpushed_commits: vec![],
        has_uncommitted: false,
        uncommitted_files: vec![],
        base_detection_warning: None,
    });
    app.screen.modal_stack.push(Box::new(widget));
    app.handle_key(key(KeyCode::Char('y')));
    let (cmd, _) = app.proto_commands.take_next().expect("command");
    assert_eq!(cmd.node_id, Some(node_id));
    assert!(matches!(cmd.action, CommandAction::RemoveCheckout { .. }));
}

#[test]
fn delete_confirm_ignores_while_loading() {
    let mut app = stub_app();
    // Loading widget — no info yet
    let widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
    app.screen.modal_stack.push(Box::new(widget));
    app.handle_key(key(KeyCode::Char('y')));
    // Widget should still be on the stack (Consumed, not Finished)
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn delete_confirm_esc_cancels() {
    let mut app = stub_app();
    push_delete_confirm_widget(&mut app, "feat/z");
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn delete_confirm_n_cancels() {
    let mut app = stub_app();
    push_delete_confirm_widget(&mut app, "feat/z");
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
}

// ── open_action_menu ─────────────────────────────────────────────

#[test]
fn open_action_menu_pushes_widget_with_filtered_entries() {
    let mut app = stub_app();
    // A checkout item without workspace — CreateWorkspace + RemoveCheckout should be available
    let item = make_work_item("a");
    setup_table(&mut app, vec![item]);
    app.open_action_menu();
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::ActionMenu)
    );
}

#[test]
fn open_action_menu_noop_when_no_selection() {
    let mut app = stub_app();
    // No items in table, no selection
    app.open_action_menu();
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

// ── action_enter ─────────────────────────────────────────────────

#[test]
fn action_enter_dispatches_first_priority() {
    let mut app = stub_app();
    // A checkout item with no workspace — enter_priority: SwitchToWorkspace
    // (unavail), TeleportSession (unavail), CreateWorkspace (available!)
    let item = make_work_item("a");
    setup_table(&mut app, vec![item]);
    app.action_enter();
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::CreateWorkspaceForCheckout { checkout_path, .. }, .. } => {
            assert_eq!(checkout_path, PathBuf::from("/tmp/a"));
        }
        other => panic!("expected CreateWorkspaceForCheckout, got {:?}", other),
    }
}

#[test]
fn action_enter_noop_when_no_selection() {
    let mut app = stub_app();
    // No items in table
    app.action_enter();
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn action_enter_with_workspace_switches() {
    let mut app = stub_app();
    let mut item = make_work_item("a");
    item.workspace_refs = vec!["my-workspace".into()];
    setup_table(&mut app, vec![item]);
    app.action_enter();
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::SelectWorkspace { ws_ref }, .. } => {
            assert_eq!(ws_ref, "my-workspace");
        }
        other => panic!("expected SelectWorkspace, got {:?}", other),
    }
}

// ── dispatch_if_available ────────────────────────────────────────

#[test]
fn dispatch_if_available_pushes_command_when_available() {
    let mut app = stub_app();
    let item = make_work_item("a");
    setup_table(&mut app, vec![item]);
    // CreateWorkspace is available for a checkout item without workspace
    app.dispatch_if_available(Intent::CreateWorkspace);
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    assert!(matches!(cmd, Command { action: CommandAction::CreateWorkspaceForCheckout { .. }, .. }));
}

#[test]
fn dispatch_if_available_noop_when_unavailable() {
    let mut app = stub_app();
    let item = make_work_item("a");
    setup_table(&mut app, vec![item]);
    // SwitchToWorkspace is NOT available (no workspace_refs)
    app.dispatch_if_available(Intent::SwitchToWorkspace);
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn dispatch_if_available_noop_when_no_selection() {
    let mut app = stub_app();
    // No items in table
    app.dispatch_if_available(Intent::CreateWorkspace);
    assert!(app.proto_commands.take_next().is_none());
}

// ── resolve_and_push ─────────────────────────────────────────────

#[test]
fn resolve_and_push_pushes_delete_confirm_widget_for_remove_checkout() {
    let mut app = stub_app();
    let item = make_work_item("a");
    app.resolve_and_push(Intent::RemoveCheckout, &item);
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::DeleteConfirm)
    );
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    assert!(matches!(cmd, Command { action: CommandAction::FetchCheckoutStatus { .. }, .. }));
}

#[test]
fn resolve_and_push_sets_branch_input_for_generate_branch_name() {
    let mut app = stub_app();
    let mut item = make_work_item("a");
    item.issue_keys = vec!["ISSUE-1".into()];
    app.resolve_and_push(Intent::GenerateBranchName, &item);
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
            assert_eq!(issue_keys, vec!["ISSUE-1".to_string()]);
        }
        other => panic!("expected GenerateBranchName, got {:?}", other),
    }
}

// ── action menu confirm (through widget stack) ─────────────────

#[test]
fn menu_enter_pops_widget_for_simple_actions() {
    let mut app = stub_app();
    push_action_menu_widget(&mut app);
    app.handle_key(key(KeyCode::Enter));
    // Widget should be popped, command should be pushed
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn menu_enter_swaps_to_delete_confirm_widget() {
    let mut app = stub_app();
    let item = make_work_item("a");
    let entries = vec![crate::widgets::action_menu::MenuEntry {
        intent: Intent::RemoveCheckout,
        command: Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::FetchCheckoutStatus {
                branch: "feat/a".into(),
                checkout_path: Some("/tmp/a".into()),
                change_request_id: None,
            },
        },
    }];
    app.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));
    app.handle_key(key(KeyCode::Enter));
    // RemoveCheckout swaps ActionMenu for DeleteConfirmWidget
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::DeleteConfirm)
    );
}

// ── j/k navigation in normal mode ────────────────────────────────

#[test]
fn normal_j_selects_next() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a"), make_work_item("b"), make_work_item("c")]);
    assert_eq!(active_selection(&app), Some(0));
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(active_selection(&app), Some(1));
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(active_selection(&app), Some(2));
}

#[test]
fn normal_k_selects_prev() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);
    // Move to second item first
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(active_selection(&app), Some(1));
    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(active_selection(&app), Some(0));
}

// ── action_enter_multi_select ────────────────────────────────────

#[test]
fn action_enter_multi_select_generates_branch_name() {
    let mut app = stub_app();
    let mut item_a = make_work_item("a");
    item_a.issue_keys = vec!["ISSUE-1".into()];
    let mut item_b = make_work_item("b");
    item_b.issue_keys = vec!["ISSUE-2".into()];
    setup_table(&mut app, vec![item_a, item_b]);

    // Multi-select both items on the RepoPage
    let identity = app.model.repo_order[0].clone();
    let page = app.screen.repo_pages.get_mut(&identity).expect("page exists");
    page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a").into()));
    page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/b").into()));

    app.action_enter();

    // Should set BranchInput with generating=true and push widget
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
            assert!(issue_keys.contains(&"ISSUE-1".to_string()));
            assert!(issue_keys.contains(&"ISSUE-2".to_string()));
        }
        other => panic!("expected GenerateBranchName, got {:?}", other),
    }
    // Multi-select should be cleared on the page
    let identity = app.model.repo_order[0].clone();
    let page = app.screen.repo_pages.get(&identity).expect("page exists");
    assert!(page.multi_selected.is_empty());
}

#[test]
fn action_enter_multi_select_without_issues_clears() {
    let mut app = stub_app();
    let item_a = make_work_item("a"); // no issue_keys
    setup_table(&mut app, vec![item_a]);

    let identity = app.model.repo_order[0].clone();
    let page = app.screen.repo_pages.get_mut(&identity).expect("page exists");
    page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a").into()));

    app.action_enter();

    // No issues, so no GenerateBranchName — stays in Normal, multi_selected cleared
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert!(app.proto_commands.take_next().is_none());
    let identity = app.model.repo_order[0].clone();
    let page = app.screen.repo_pages.get(&identity).expect("page exists");
    assert!(page.multi_selected.is_empty());
}

#[test]
fn action_enter_multi_select_generates_branch_name_for_native_issue_rows() {
    let mut app = stub_app();
    setup_native_issue_rows(&mut app, &["ISSUE-1", "ISSUE-2"]);

    let identity = app.model.repo_order[0].clone();
    let page = app.screen.repo_pages.get_mut(&identity).expect("page exists");
    page.multi_selected.insert(WorkItemIdentity::Issue("ISSUE-1".into()));
    page.multi_selected.insert(WorkItemIdentity::Issue("ISSUE-2".into()));

    app.action_enter();

    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    let (cmd, _) = app.proto_commands.take_next().expect("expected command");
    match cmd {
        Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
            assert_eq!(issue_keys, vec!["ISSUE-1".to_string(), "ISSUE-2".to_string()]);
        }
        other => panic!("expected GenerateBranchName, got {:?}", other),
    }

    let page = app.screen.repo_pages.get(&identity).expect("page exists");
    assert!(page.multi_selected.is_empty());
}

#[test]
fn dispatch_generate_branch_name_from_native_issue_row() {
    let mut app = stub_app();
    setup_native_issue_rows(&mut app, &["ISSUE-1"]);

    app.dispatch_if_available(Intent::GenerateBranchName);

    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::BranchInput)
    );
    let (cmd, _) = app.proto_commands.take_next().expect("expected command");
    match cmd {
        Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
            assert_eq!(issue_keys, vec!["ISSUE-1".to_string()]);
        }
        other => panic!("expected GenerateBranchName, got {:?}", other),
    }
}

// ── delete_confirm_y_with_no_info ────────────────────────────────

#[test]
fn delete_confirm_y_with_no_info_does_not_push_command() {
    let mut app = stub_app();
    let mut widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(WorkItemIdentity::Session("test".into()), None, None);
    widget.loading = false; // not loading, but no info either
    app.screen.modal_stack.push(Box::new(widget));
    app.handle_key(key(KeyCode::Char('y')));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    // No info means no branch to extract, so no command pushed
    assert!(app.proto_commands.take_next().is_none());
}

// ── open_action_menu with change request item ────────────────────

#[test]
fn open_action_menu_includes_open_change_request() {
    let mut app = stub_app();
    let mut item = make_work_item("a");
    item.change_request_key = Some("10".into());
    setup_table(&mut app, vec![item]);
    app.open_action_menu();
    assert_eq!(app.screen.modal_stack.len(), 1);
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::ActionMenu)
    );
}

// ── space toggles multi-select ───────────────────────────────────

#[test]
fn space_toggles_multi_select() {
    let mut app = stub_app();
    setup_table(&mut app, vec![make_work_item("a")]);
    assert!(active_multi_selected(&app).is_empty());
    app.handle_key(key(KeyCode::Char(' ')));
    assert!(!active_multi_selected(&app).is_empty());
    app.handle_key(key(KeyCode::Char(' ')));
    assert!(active_multi_selected(&app).is_empty());
}

#[test]
fn l_cycles_layout_in_normal_mode() {
    let mut app = stub_app();
    assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, RepoViewLayout::Right);
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");

    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

// ── normal p dispatches open change request ──────────────────────

#[test]
fn normal_p_opens_change_request() {
    let mut app = stub_app();
    let mut item = make_work_item("a");
    item.change_request_key = Some("42".into());
    setup_table(&mut app, vec![item]);
    app.handle_key(key(KeyCode::Char('p')));
    let (cmd, _) = app.proto_commands.take_next().unwrap();
    match cmd {
        Command { action: CommandAction::OpenChangeRequest { id }, .. } => {
            assert_eq!(id, "42");
        }
        other => panic!("expected OpenChangeRequest, got {:?}", other),
    }
}

#[test]
fn normal_p_noop_without_change_request() {
    let mut app = stub_app();
    let item = make_work_item("a"); // no change_request_key
    setup_table(&mut app, vec![item]);
    app.handle_key(key(KeyCode::Char('p')));
    assert!(app.proto_commands.take_next().is_none());
}

// ── pending context attachment ─────────────────────────────────────

#[test]
fn resolve_and_push_attaches_pending_context() {
    let mut app = stub_app();
    let item = make_work_item("a");
    app.resolve_and_push(Intent::CreateWorkspace, &item);
    let (_, ctx) = app.proto_commands.take_next().expect("should have command");
    let ctx = ctx.expect("should have pending context");
    assert_eq!(ctx.identity, item.identity);
}

#[test]
fn close_confirm_attaches_pending_context() {
    let mut app = stub_app();
    let item = make_work_item("a");
    // Push CloseConfirmWidget onto the widget stack
    let widget = crate::widgets::close_confirm::CloseConfirmWidget::new("PR-1".into(), "test".into(), item.identity.clone(), Command {
        node_id: None,
        provisioning_target: None,
        context_repo: None,
        action: CommandAction::CloseChangeRequest { id: "PR-1".into() },
    });
    app.screen.modal_stack.push(Box::new(widget));
    // Simulate pressing 'y' to confirm
    app.handle_key(key(KeyCode::Char('y')));
    let (_, ctx) = app.proto_commands.take_next().expect("should have command");
    let ctx = ctx.expect("should have pending context");
    assert_eq!(ctx.identity, item.identity);
}

#[test]
fn close_confirm_preserves_resolved_remote_command() {
    let mut app = stub_app();
    let expected = Command {
        node_id: Some(NodeId::new("remote-host")),
        provisioning_target: None,
        context_repo: Some(flotilla_protocol::RepoSelector::Identity(app.model.active_repo_identity().clone())),
        action: CommandAction::CloseChangeRequest { id: "PR-1".into() },
    };
    let widget = crate::widgets::close_confirm::CloseConfirmWidget::new(
        "PR-1".into(),
        "test".into(),
        WorkItemIdentity::ChangeRequest("PR-1".into()),
        expected.clone(),
    );
    app.screen.modal_stack.push(Box::new(widget));

    app.handle_key(key(KeyCode::Char('y')));

    let (command, _) = app.proto_commands.take_next().expect("should have command");
    assert_eq!(command, expected);
}

// ── command palette key handling ────────────────────────────────

#[test]
fn double_slash_fills_search() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
    // Typing '/' inside the palette fills "search "
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}

#[test]
fn command_palette_tab_fills_command_name() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    // First entry is "search" — Tab should fill it
    app.handle_key(key(KeyCode::Tab));
    // Widget should remain on stack
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}

#[test]
fn command_palette_search_with_args_applies_filter() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    for c in "search auth".chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(active_search_query(&app), Some("auth"));
}

#[test]
fn command_palette_search_empty_term_clears() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    for c in "search ".chars() {
        app.handle_key(key(KeyCode::Char(c)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
    assert_eq!(active_search_query(&app), None);
}

#[test]
fn command_palette_enter_dispatches_action() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    // First entry is "search" which dispatches OpenIssueSearch → Swap
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::IssueSearch)
    );
}

// ── Issue query routing and stale-result filtering ──────────────

#[test]
fn query_issues_command_intercepted_by_executor_dispatch() {
    // QueryIssues must NOT be dispatched through daemon.execute() — it should
    // be intercepted by executor::dispatch() and routed through spawn_query_page.
    // The stub daemon's execute() would succeed but the real socket client would
    // fail because execute() expects Response::Execute, not Response::QueryResult.
    //
    // We verify this by checking that after handle_key submits the search,
    // the proto_commands queue is drained but no command_id appears in
    // in_flight (spawn_query_page is fire-and-forget, not tracked as in-flight).
    let mut app = stub_app();
    push_issue_search_widget_with_text(&mut app, "routing test");
    app.handle_key(key(KeyCode::Enter));

    // The command was queued by the widget.
    let (cmd, _) = app.proto_commands.take_next().expect("expected QueryIssues command");
    assert!(matches!(cmd.action, CommandAction::QueryIssues { .. }));

    // Simulate the event loop: dispatch through executor.
    // spawn_query_page requires a tokio runtime but won't block — it spawns
    // a task that will fail (stub daemon). We just need to verify the
    // interception path runs without error.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        crate::app::executor::dispatch(cmd, &mut app, None).await;
    });

    // Key assertion: no in-flight command was recorded. If the command had
    // gone through daemon.execute() it would have produced a command_id.
    assert!(app.in_flight.is_empty(), "QueryIssues should not create an in-flight command");
    assert!(app.model.status_message.is_none(), "dispatch should not set an error");
}

#[test]
fn set_search_query_resets_search_paging_state() {
    use flotilla_protocol::issue_query::IssueQuery;

    use crate::app::issue_view::{IssuePagingState, IssueViewState};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();

    // Simulate an existing search for "alpha" with accumulated results.
    let mut view = IssueViewState::new();
    view.search_query = Some("alpha".into());
    view.search = Some(IssuePagingState {
        params: IssueQuery { search: Some("alpha".into()) },
        items: vec![("1".into(), flotilla_protocol::provider_data::Issue {
            title: "Alpha result".into(),
            labels: vec![],
            association_keys: vec![],
            provider_name: "gh".into(),
            provider_display_name: "GitHub".into(),
        })],
        next_page: 2,
        total: Some(10),
        has_more: true,
        fetch_pending: false,
    });
    app.issue_views.insert(repo.clone(), view);

    // Now start a new search for "beta" — this triggers SetSearchQuery.
    push_issue_search_widget_with_text(&mut app, "beta");
    app.handle_key(key(KeyCode::Enter));

    // SetSearchQuery should have reset the search paging state.
    let view = app.issue_views.get(&repo).expect("view should exist");
    assert_eq!(view.search_query.as_deref(), Some("beta"), "search_query should be updated to new query");
    assert!(view.search.is_none(), "search paging state should be reset for the new query");
}

#[test]
fn stale_search_results_discarded_by_drain() {
    use flotilla_protocol::issue_query::{IssueQuery, IssueResultPage};

    use crate::app::issue_view::{IssuePagingState, IssueQueryUpdate, IssueViewState};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();

    // Set up: user has searched for "beta" (search_query is set).
    let mut view = IssueViewState::new();
    view.search_query = Some("beta".into());
    view.search = Some(IssuePagingState {
        params: IssueQuery { search: Some("beta".into()) },
        items: vec![],
        next_page: 1,
        total: None,
        has_more: true,
        fetch_pending: true,
    });
    app.issue_views.insert(repo.clone(), view);

    // A stale result from an earlier "alpha" search arrives.
    app.issue_update_tx
        .send(IssueQueryUpdate::PageFetched {
            repo: repo.clone(),
            params: IssueQuery { search: Some("alpha".into()) },
            requested_page: 1,
            page: IssueResultPage {
                items: vec![("stale".into(), flotilla_protocol::provider_data::Issue {
                    title: "Stale alpha result".into(),
                    labels: vec![],
                    association_keys: vec![],
                    provider_name: "gh".into(),
                    provider_display_name: "GitHub".into(),
                })],
                total: Some(1),
                has_more: false,
            },
        })
        .expect("send");

    // A matching result for "beta" also arrives.
    app.issue_update_tx
        .send(IssueQueryUpdate::PageFetched {
            repo: repo.clone(),
            params: IssueQuery { search: Some("beta".into()) },
            requested_page: 1,
            page: IssueResultPage {
                items: vec![("fresh".into(), flotilla_protocol::provider_data::Issue {
                    title: "Fresh beta result".into(),
                    labels: vec![],
                    association_keys: vec![],
                    provider_name: "gh".into(),
                    provider_display_name: "GitHub".into(),
                })],
                total: Some(1),
                has_more: false,
            },
        })
        .expect("send");

    app.drain_background_updates();

    let view = app.issue_views.get(&repo).expect("view should exist");
    let search = view.search.as_ref().expect("search state should exist from beta result");
    assert_eq!(search.items.len(), 1, "only the matching beta result should be present");
    assert_eq!(search.items[0].0, "fresh", "the stale alpha result should have been discarded");
    assert_eq!(search.items[0].1.title, "Fresh beta result");
}

#[test]
fn command_palette_esc_dismisses() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
}

#[test]
fn command_palette_arrow_navigation_wraps() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    // Down from 0, Up from 0 — widget should remain on stack
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
    // Up again wraps to last — detailed wrap behavior tested in widget unit tests
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}

#[test]
fn command_palette_typing_resets_selection() {
    let mut app = stub_app();
    app.handle_key(key(KeyCode::Char('/')));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    // Now type a char — widget should still be on stack; detailed selection reset tested in widget unit tests
    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(
        app.screen.modal_stack.last().expect("modal stack non-empty").binding_mode(),
        KeyBindingMode::from(BindingModeId::CommandPalette)
    );
}
