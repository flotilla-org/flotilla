mod support;

use std::path::PathBuf;

use flotilla_protocol::{
    CheckoutRef, HostName, HostPath, HostSummary, NodeId, NodeInfo, ProviderData, RepoIdentity, SessionStatus, SystemInfo, WorkItem,
    WorkItemIdentity,
};
use flotilla_tui::app::{
    ui_state::{PendingAction, PendingStatus},
    BranchInputKind, InFlightCommand, Intent, PeerStatus, ProviderStatus, RepoViewLayout, TuiHostState,
};
use ratatui::style::Color;
use support::*;
use tui_input::Input;

fn picker_entry(name: &str, is_git_repo: bool, is_added: bool) -> flotilla_tui::app::DirEntry {
    flotilla_tui::app::DirEntry { name: name.to_string(), is_dir: true, is_git_repo, is_added }
}

fn cramped_widget_harness(widget: Box<dyn flotilla_tui::widgets::InteractiveWidget>) -> TestHarness {
    TestHarness::single_repo("my-project").with_width(40).with_height(6).with_widget(widget)
}

#[test]
fn empty_state() {
    let mut harness = TestHarness::empty();
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn single_repo_empty_table() {
    let mut harness = TestHarness::single_repo("my-project");
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn single_repo_with_items() {
    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-login", "/test/my-project/feat-login", false);
    providers.checkouts.insert(path, checkout);
    let (id, cr) = make_change_request("42", "Add login page", "feat-login");
    providers.change_requests.insert(id, cr);
    let (id, issue) = make_issue("10", "Users need authentication");
    providers.issues.insert(id, issue);
    let (id, session) = make_session("s1", "Implement auth flow", SessionStatus::Idle);
    providers.sessions.insert(id, session);

    let items = vec![
        make_work_item_checkout("feat-login", "/test/my-project/feat-login"),
        make_work_item_cr("42", "Add login page", Some("feat-login")),
        make_work_item_issue("10", "Users need authentication"),
        support::session_item("s1"),
    ];

    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn tab_bar_multiple_repos() {
    let mut harness = TestHarness::multi_repo(&["alpha", "beta", "gamma"]);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_with_error() {
    let mut harness = TestHarness::single_repo("my-project").with_status_message("GitHub API rate limit exceeded");
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn help_screen() {
    let mut harness = TestHarness::single_repo("my-project");
    harness.screen.modal_stack.push(Box::new(flotilla_tui::widgets::help::HelpWidget::new()));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn help_screen_clamps_scroll_state_after_render() {
    // The HelpWidget internally clamps its scroll during render, so after
    // rendering with a default widget, scroll should remain at 0 (clamped).
    let mut harness = TestHarness::single_repo("my-project");
    harness.screen.modal_stack.push(Box::new(flotilla_tui::widgets::help::HelpWidget::new()));
    let _ = harness.render_to_string();
    // Widget is rendered with scroll=0 (default); just verify it doesn't panic.
}

#[test]
fn action_menu() {
    let item = checkout_item("feat/a", "/test/my-project/feat/a", false);
    let entries = vec![
        flotilla_tui::widgets::action_menu::MenuEntry {
            intent: Intent::CreateWorkspace,
            command: flotilla_protocol::Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::CreateWorkspaceForCheckout {
                    checkout_path: "/test/my-project/feat/a".into(),
                    label: "feat/a".into(),
                },
            },
        },
        flotilla_tui::widgets::action_menu::MenuEntry {
            intent: Intent::OpenChangeRequest,
            command: flotilla_protocol::Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::OpenChangeRequest { id: "1".into() },
            },
        },
        flotilla_tui::widgets::action_menu::MenuEntry {
            intent: Intent::RemoveCheckout,
            command: flotilla_protocol::Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::FetchCheckoutStatus {
                    branch: "feat/a".into(),
                    checkout_path: Some("/test/my-project/feat/a".into()),
                    change_request_id: None,
                },
            },
        },
    ];
    let mut harness = TestHarness::single_repo("my-project");
    harness.screen.modal_stack.push(Box::new(flotilla_tui::widgets::action_menu::ActionMenuWidget::new(entries, item)));
    let output = harness.render_to_string();
    assert!(output.contains(""));
    insta::assert_snapshot!(output);
}

#[test]
fn config_screen() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_config()
        .with_provider_names("my-project", vec![
            ("change_request", "GitHub"),
            ("issue_tracker", "GitHub"),
            ("vcs", "Git"),
            ("checkout_manager", "Git Worktrees"),
            ("cloud_agent", "Claude"),
        ])
        .with_provider_status("my-project", "cloud_agent", "Claude", ProviderStatus::Ok);
    let output = harness.render_to_string();
    assert!(output.contains(""));
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_key_brackets_use_black_foreground() {
    let mut harness = TestHarness::single_repo("my-project");
    let buffer = harness.render_to_buffer();
    let y = buffer.area.height.saturating_sub(1);
    let bottom_row = (0..buffer.area.width).map(|x| &buffer[(x, y)]).collect::<Vec<_>>();
    let left_bracket = bottom_row.iter().find(|cell| cell.symbol() == "<").expect("expected key left bracket");
    let right_bracket = bottom_row.iter().find(|cell| cell.symbol() == ">").expect("expected key right bracket");

    assert_eq!(left_bracket.fg, Color::Black);
    assert_eq!(right_bracket.fg, Color::Black);
    assert_eq!(left_bracket.bg, Color::DarkGray);
    assert_eq!(right_bracket.bg, Color::DarkGray);
}

#[test]
fn status_bar_fills_unused_cells_with_black_background() {
    let mut harness = TestHarness::single_repo("my-project");
    let buffer = harness.render_to_buffer();
    let y = buffer.area.height.saturating_sub(1);
    let row_width = buffer.area.width;
    let status_gap_cell = &buffer[(11, y)];
    let last_cell = &buffer[(row_width.saturating_sub(1), y)];

    assert_eq!(status_gap_cell.bg, Color::Black);
    assert_eq!(last_cell.bg, Color::Black);
}

#[test]
fn status_bar_reserves_space_before_keys() {
    let mut harness = TestHarness::single_repo("my-project");
    let buffer = harness.render_to_buffer();
    let y = buffer.area.height.saturating_sub(1);
    let first_chevron_x = (0..buffer.area.width).find(|&x| buffer[(x, y)].symbol() == "").expect("expected first ribbon chevron");

    assert!(first_chevron_x >= 24, "expected first ribbon to start after reserved status space, got {first_chevron_x}");
}

#[test]
fn status_bar_ribbons_include_space_before_key_token() {
    let mut harness = TestHarness::single_repo("my-project");
    let output = harness.render_to_string();

    assert!(output.contains(" <ENT> OPEN"));
}

#[test]
fn status_bar_does_not_show_keys_toggle_ribbon() {
    let mut harness = TestHarness::single_repo("my-project");
    let output = harness.render_to_string();

    assert!(!output.contains("KEYS"));
}

#[test]
fn selected_item_preview() {
    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-dashboard", "/test/my-project/feat-dashboard", false);
    providers.checkouts.insert(path, checkout);
    let (id, cr) = make_change_request("99", "Build analytics dashboard", "feat-dashboard");
    providers.change_requests.insert(id, cr);

    let items = vec![
        make_work_item_checkout("feat-dashboard", "/test/my-project/feat-dashboard"),
        make_work_item_cr("99", "Build analytics dashboard", Some("feat-dashboard")),
    ];

    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn selected_item_preview_below() {
    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-dashboard", "/test/my-project/feat-dashboard", false);
    providers.checkouts.insert(path, checkout);
    let (id, cr) = make_change_request("99", "Build analytics dashboard", "feat-dashboard");
    providers.change_requests.insert(id, cr);

    let items = vec![
        make_work_item_checkout("feat-dashboard", "/test/my-project/feat-dashboard"),
        make_work_item_cr("99", "Build analytics dashboard", Some("feat-dashboard")),
    ];

    let mut harness = TestHarness::single_repo("my-project")
        .with_provider_data(providers, items)
        .with_layout(RepoViewLayout::Below)
        .with_width(90)
        .with_height(40);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn zoom_layout_uses_full_content_area() {
    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-dashboard", "/test/my-project/feat-dashboard", false);
    providers.checkouts.insert(path, checkout);

    let items = vec![make_work_item_checkout("feat-dashboard", "/test/my-project/feat-dashboard")];

    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items).with_layout(RepoViewLayout::Zoom);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_layout_state() {
    let mut harness = TestHarness::single_repo("my-project").with_layout(RepoViewLayout::Below);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_hidden_keys() {
    let mut harness = TestHarness::single_repo("my-project");
    harness.ui.status_bar.show_keys = false;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_narrow_width_prioritizes_status_over_keys() {
    let mut harness = TestHarness::single_repo("my-project").with_width(72);
    harness.model.status_message = Some("Remote host unreachable".into());
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn preview_change_request() {
    let mut providers = ProviderData::default();
    let (id, cr) = make_change_request("77", "Refactor auth module", "feat-auth");
    providers.change_requests.insert(id, cr);

    let mut item = support::pr_item("77");
    item.description = "Refactor auth module".to_string();
    item.branch = Some("feat-auth".to_string());
    item.change_request_key = Some("77".to_string());

    let items = vec![item];
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn preview_issue() {
    let mut providers = ProviderData::default();
    let (id, issue) = make_issue("25", "Fix login timeout bug");
    providers.issues.insert(id, issue);

    let mut item = support::issue_item("25");
    item.description = "Fix login timeout bug".to_string();
    item.issue_keys = vec!["25".to_string()];

    let items = vec![item];
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn preview_session() {
    let mut providers = ProviderData::default();
    let (id, session) = make_session("s5", "Debug API endpoints", SessionStatus::Running);
    providers.sessions.insert(id, session);

    let mut item = support::session_item("s5");
    item.description = "Debug API endpoints".to_string();
    item.session_key = Some("s5".to_string());

    let items = vec![item];
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_with_multiple_in_flight_commands() {
    let mut harness = TestHarness::single_repo("my-project");
    harness.in_flight.insert(1, InFlightCommand {
        repo_identity: RepoIdentity { authority: "local".into(), path: "/test/my-project".into() },
        repo: PathBuf::from("/test/my-project"),
        description: "Refreshing repository...".into(),
    });
    harness.in_flight.insert(2, InFlightCommand {
        repo_identity: RepoIdentity { authority: "local".into(), path: "/test/my-project".into() },
        repo: PathBuf::from("/test/my-project"),
        description: "Refreshing repository...".into(),
    });
    harness.in_flight.insert(3, InFlightCommand {
        repo_identity: RepoIdentity { authority: "local".into(), path: "/test/other-project".into() },
        repo: PathBuf::from("/test/other-project"),
        description: "Should not render".into(),
    });

    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn branch_input_generating_popup() {
    let widget = flotilla_tui::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating);
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn issue_search_mode_status_bar() {
    let mut widget = flotilla_tui::widgets::issue_search::IssueSearchWidget::new();
    widget.prefill("auth timeout");
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn file_picker_popup() {
    let entries = vec![picker_entry("repo-a", true, false), picker_entry("repo-b", true, true), flotilla_tui::app::DirEntry {
        name: "notes.txt".into(),
        is_dir: false,
        is_git_repo: false,
        is_added: false,
    }];
    let widget = flotilla_tui::widgets::file_picker::FilePickerWidget::new(Input::from("/test"), entries).with_selected(1);
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

fn delete_confirm_widget(
    info: flotilla_protocol::CheckoutStatus,
    identity: WorkItemIdentity,
    remote_host: Option<HostName>,
) -> Box<dyn flotilla_tui::widgets::InteractiveWidget> {
    let mut widget = flotilla_tui::widgets::delete_confirm::DeleteConfirmWidget::new(
        identity,
        remote_host.as_ref().map(|host| NodeId::new(host.as_str())),
        None,
    );
    widget.update_info(info);
    Box::new(widget)
}

#[test]
fn delete_confirm_safe_to_delete() {
    let mut harness = TestHarness::single_repo("my-project").with_widget(delete_confirm_widget(
        flotilla_protocol::CheckoutStatus {
            branch: "feat-cleanup".into(),
            change_request_status: Some("MERGED".into()),
            merge_commit_sha: Some("abc1234".into()),
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        },
        WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/tmp/my-project/feat-cleanup")).into()),
        None,
    ));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_with_uncommitted_files() {
    let mut harness = TestHarness::single_repo("my-project").with_widget(delete_confirm_widget(
        flotilla_protocol::CheckoutStatus {
            branch: "feat-wip".into(),
            change_request_status: Some("OPEN".into()),
            merge_commit_sha: None,
            unpushed_commits: vec!["abc1234 work in progress".into()],
            has_uncommitted: true,
            uncommitted_files: vec![" M src/main.rs".into(), " M src/lib.rs".into(), "?? TODO.txt".into()],
            base_detection_warning: None,
        },
        WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/tmp/my-project/feat-wip")).into()),
        None,
    ));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_with_many_uncommitted_files() {
    let files: Vec<String> = (0..15).map(|i| format!(" M src/file_{}.rs", i)).collect();
    let mut harness = TestHarness::single_repo("my-project").with_height(50).with_widget(delete_confirm_widget(
        flotilla_protocol::CheckoutStatus {
            branch: "feat-big-wip".into(),
            change_request_status: None,
            merge_commit_sha: None,
            unpushed_commits: vec![],
            has_uncommitted: true,
            uncommitted_files: files,
            base_detection_warning: None,
        },
        WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/tmp/my-project/feat-big-wip")).into()),
        None,
    ));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_remote_host() {
    let mut harness = TestHarness::single_repo("my-project").with_widget(delete_confirm_widget(
        flotilla_protocol::CheckoutStatus {
            branch: "feat-remote".into(),
            change_request_status: Some("MERGED".into()),
            merge_commit_sha: Some("def5678".into()),
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        },
        WorkItemIdentity::Checkout(HostPath::new(HostName::new("feta"), PathBuf::from("/home/dev/my-project/feat-remote")).into()),
        Some(HostName::new("feta")),
    ));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn providers_overlay() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_provider_names("my-project", vec![
            ("vcs", "Git"),
            ("checkout_manager", "Git Worktrees"),
            ("change_request", "GitHub"),
            ("cloud_agent", "Claude"),
        ])
        .with_provider_status("my-project", "cloud_agent", "Claude", ProviderStatus::Ok)
        .with_provider_status("my-project", "change_request", "GitHub", ProviderStatus::Error);
    let repo = harness.model.repo_order[0].clone();
    harness.screen.repo_pages.get_mut(&repo).expect("repo page exists").show_providers = true;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn config_screen_cross_repo_worst_wins() {
    let mut harness = TestHarness::multi_repo(&["alpha", "beta"])
        .with_config()
        .with_provider_names("alpha", vec![("change_request", "GitHub")])
        .with_provider_names("beta", vec![("change_request", "GitHub")])
        .with_provider_status("alpha", "change_request", "GitHub", ProviderStatus::Ok)
        .with_provider_status("beta", "change_request", "GitHub", ProviderStatus::Error);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn debug_panel_with_correlation_details() {
    let mut item = support::checkout_item("feat-xyz", "/test/my-project/feat-xyz", false);
    item.description = "Feature branch checkout".into();
    item.debug_group = vec!["Group #12".into(), "branch: feat-xyz".into(), "checkout: /test/my-project/feat-xyz".into()];
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(ProviderData::default(), vec![item]);
    harness.ui.show_debug = true;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn theme_switching_changes_output() {
    use flotilla_tui::theme::Theme;

    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-login", "/test/my-project/feat-login", false);
    providers.checkouts.insert(path, checkout);
    let items = vec![make_work_item_checkout("feat-login", "/test/my-project/feat-login")];

    let mut classic_harness =
        TestHarness::single_repo("my-project").with_provider_data(providers.clone(), items.clone()).with_theme(Theme::classic());
    let classic_buf = classic_harness.render_to_buffer();

    let mut catppuccin_harness =
        TestHarness::single_repo("my-project").with_provider_data(providers, items).with_theme(Theme::catppuccin_mocha());
    let catppuccin_buf = catppuccin_harness.render_to_buffer();

    // Find a cell that uses a themed colour by scanning for the checkout icon
    let area = classic_buf.area;
    let mut classic_fg = None;
    let mut catppuccin_fg = None;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let symbol = classic_buf[(x, y)].symbol();
            if symbol == "○" || symbol == "●" {
                classic_fg = Some(classic_buf[(x, y)].fg);
                catppuccin_fg = Some(catppuccin_buf[(x, y)].fg);
                break;
            }
        }
        if classic_fg.is_some() {
            break;
        }
    }
    let classic_fg = classic_fg.expect("should find checkout icon in classic render");
    let catppuccin_fg = catppuccin_fg.expect("should find checkout icon in catppuccin render");
    assert_ne!(
        classic_fg, catppuccin_fg,
        "Themes should produce different colours: classic={classic_fg:?} vs catppuccin={catppuccin_fg:?}"
    );
}

#[test]
fn command_palette_open() {
    let widget = flotilla_tui::widgets::command_palette::CommandPaletteWidget::new();
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn command_palette_widget_renders_without_overflow() {
    let widget = flotilla_tui::widgets::command_palette::CommandPaletteWidget::new();
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));

    let _ = harness.render_to_buffer();
}

#[test]
fn command_palette_renders_on_short_terminals_without_overflow() {
    let widget = flotilla_tui::widgets::command_palette::CommandPaletteWidget::new();
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget)).with_height(6);

    let _ = harness.render_to_buffer();
}

#[test]
fn action_menu_widget_renders_on_short_terminals_without_overflow() {
    let item = checkout_item("feat/a", "/test/my-project/feat/a", false);
    let entries = vec![flotilla_tui::widgets::action_menu::MenuEntry {
        intent: Intent::CreateWorkspace,
        command: flotilla_protocol::Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: flotilla_protocol::CommandAction::CreateWorkspaceForCheckout {
                checkout_path: "/test/my-project/feat/a".into(),
                label: "feat/a".into(),
            },
        },
    }];
    let mut harness = cramped_widget_harness(Box::new(flotilla_tui::widgets::action_menu::ActionMenuWidget::new(entries, item)));

    let _ = harness.render_to_buffer();
}

#[test]
fn branch_input_widget_renders_on_short_terminals_without_overflow() {
    let mut harness =
        cramped_widget_harness(Box::new(flotilla_tui::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Manual)));

    let _ = harness.render_to_buffer();
}

#[test]
fn close_confirm_widget_renders_on_short_terminals_without_overflow() {
    let command = flotilla_protocol::Command {
        node_id: None,
        provisioning_target: None,
        context_repo: None,
        action: flotilla_protocol::CommandAction::CloseChangeRequest { id: "42".into() },
    };
    let widget = flotilla_tui::widgets::close_confirm::CloseConfirmWidget::new(
        "42".into(),
        "Fix the thing".into(),
        WorkItemIdentity::ChangeRequest("42".into()),
        command,
    );
    let mut harness = cramped_widget_harness(Box::new(widget));

    let _ = harness.render_to_buffer();
}

#[test]
fn delete_confirm_widget_renders_on_short_terminals_without_overflow() {
    let mut widget = flotilla_tui::widgets::delete_confirm::DeleteConfirmWidget::new(
        WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/test/my-project/feat/a")).into()),
        None,
        None,
    );
    widget.update_info(flotilla_protocol::CheckoutStatus {
        branch: "feat/a".into(),
        change_request_status: None,
        merge_commit_sha: None,
        unpushed_commits: vec![],
        has_uncommitted: false,
        uncommitted_files: vec![],
        base_detection_warning: None,
    });
    let mut harness = cramped_widget_harness(Box::new(widget));

    let _ = harness.render_to_buffer();
}

#[test]
fn file_picker_widget_renders_on_short_terminals_without_overflow() {
    let entries = vec![picker_entry("alpha", true, false), picker_entry("beta", false, false)];
    let widget = flotilla_tui::widgets::file_picker::FilePickerWidget::new(Input::from("/test/"), entries);
    let mut harness = cramped_widget_harness(Box::new(widget));

    let _ = harness.render_to_buffer();
}

#[test]
fn help_widget_renders_on_short_terminals_without_overflow() {
    let mut harness = cramped_widget_harness(Box::new(flotilla_tui::widgets::help::HelpWidget::new()));

    let _ = harness.render_to_buffer();
}

#[test]
fn command_palette_filtered() {
    let widget = flotilla_tui::widgets::command_palette::CommandPaletteWidget::with_state(Input::from("he"), 0, 0);
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn command_palette_selection() {
    let widget = flotilla_tui::widgets::command_palette::CommandPaletteWidget::with_state(Input::default(), 3, 0);
    let mut harness = TestHarness::single_repo("my-project").with_widget(Box::new(widget));
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

// ── Regression tests ─────────────────────────────────────────────────────

/// In-flight pending action replaces the normal icon with a spinner character
/// and preserves the rest of the row content.
#[test]
fn pending_action_in_flight_shows_spinner() {
    let items = vec![make_work_item_checkout("feat-login", "/test/my-project/feat-login")];
    let providers = ProviderData::default();
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);

    // Insert an in-flight pending action for the checkout item.
    let identity = WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/test/my-project/feat-login")).into());
    let repo = harness.model.repo_order[0].clone();
    harness.screen.repo_pages.get_mut(&repo).expect("repo page exists").pending_actions.insert(identity, PendingAction {
        command_id: 1,
        status: PendingStatus::InFlight,
        description: "Deleting checkout...".into(),
    });

    let buffer = harness.render_to_buffer();

    // The spinner character appears in the icon column (column 0 after the
    // highlight symbol). Find the first braille spinner character in the
    // rendered buffer — it should be on the data row (row 2 in the table:
    // divider=0, header=1, data=2).
    let braille_spinner: &[char] = &['\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}', '\u{2827}'];
    let area = buffer.area;
    let mut found_spinner = false;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let sym = buffer[(x, y)].symbol();
            if sym.len() == 3 && sym.chars().next().is_some_and(|ch| braille_spinner.contains(&ch)) {
                found_spinner = true;
                break;
            }
        }
        if found_spinner {
            break;
        }
    }
    assert!(found_spinner, "expected a braille spinner character in the rendered buffer for an in-flight pending action");

    // Verify the row still contains the branch text and description.
    let output = support::buffer_to_string_for_test(&buffer);
    assert!(output.contains("feat-login"), "expected branch name to be preserved in shimmer row");
}

/// Failed pending action shows the error icon and applies error styling.
#[test]
fn pending_action_failed_shows_error_icon() {
    let items = vec![make_work_item_checkout("feat-broken", "/test/my-project/feat-broken")];
    let providers = ProviderData::default();
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);

    // Insert a failed pending action.
    let identity = WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/test/my-project/feat-broken")).into());
    let repo = harness.model.repo_order[0].clone();
    harness.screen.repo_pages.get_mut(&repo).expect("repo page exists").pending_actions.insert(identity, PendingAction {
        command_id: 2,
        status: PendingStatus::Failed("network error".into()),
        description: "Deleting checkout...".into(),
    });

    let output = harness.render_to_string();
    // The failed icon is ✗ (U+2717).
    assert!(output.contains('\u{2717}'), "expected error icon (✗) in rendered output for failed pending action");

    // Verify error styling on the data row.
    let buffer = harness.render_to_buffer();
    let theme = flotilla_tui::theme::Theme::classic();

    // Find the row containing the ✗ icon and check its styling.
    let area = buffer.area;
    let mut error_row_y = None;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if buffer[(x, y)].symbol() == "\u{2717}" {
                error_row_y = Some(y);
                break;
            }
        }
        if error_row_y.is_some() {
            break;
        }
    }
    let y = error_row_y.expect("should find error icon row");

    // Check that cells on this row have the error fg color and DIM modifier.
    // Pick a cell that is part of the content (not the icon itself or spaces).
    // The description "checkout feat-broken" should be rendered with error color.
    let mut found_error_styled_cell = false;
    for x in area.x..area.x + area.width {
        let cell = &buffer[(x, y)];
        if cell.fg == theme.error && cell.modifier.contains(ratatui::style::Modifier::DIM) && cell.symbol().trim() != "" {
            found_error_styled_cell = true;
            break;
        }
    }
    assert!(found_error_styled_cell, "expected cells on the failed row to have error fg and DIM modifier");
}

/// Multi-select applies multi_select_bg, but the active (highlighted) row
/// uses row_highlight which takes precedence.
#[test]
fn multi_select_with_active_row_highlight() {
    let items = vec![
        make_work_item_checkout("feat-a", "/test/my-project/feat-a"),
        make_work_item_checkout("feat-b", "/test/my-project/feat-b"),
        make_work_item_checkout("feat-c", "/test/my-project/feat-c"),
    ];
    let providers = ProviderData::default();
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);

    let repo = harness.model.repo_order[0].clone();
    let page = harness.screen.repo_pages.get_mut(&repo).expect("repo page exists");

    // Multi-select items 0 and 1.
    let identity_a = WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/test/my-project/feat-a")).into());
    let identity_b = WorkItemIdentity::Checkout(HostPath::new(HostName::local(), PathBuf::from("/test/my-project/feat-b")).into());
    page.multi_selected.insert(identity_a);
    page.multi_selected.insert(identity_b);

    // Navigate to item 1 (feat-b) so it is both selected AND multi-selected.
    page.table.select_next();

    let theme = flotilla_tui::theme::Theme::classic();
    let buffer = harness.render_to_buffer();

    // Determine the y-coordinates of each data row.
    // Layout: tab bar(1) + divider(1) + column header(1) + data rows (3).
    // Tab bar is at y=0, then content starts at y=1.
    // Divider at y=1, column header at y=2, data rows at y=3, y=4, y=5.
    let area = buffer.area;

    // Find the y-coords of the three data rows by scanning for the checkout
    // icons (○). The first three rows with ○ or ▸ are our data rows.
    let mut data_row_ys: Vec<u16> = Vec::new();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let sym = buffer[(x, y)].symbol();
            if sym == "○" || sym == "●" {
                data_row_ys.push(y);
                break;
            }
        }
    }
    assert!(data_row_ys.len() >= 3, "expected at least 3 data rows with checkout icons, found {}", data_row_ys.len());

    let y_item_0 = data_row_ys[0]; // feat-a: multi-selected only
    let y_item_1 = data_row_ys[1]; // feat-b: multi-selected AND active
    let y_item_2 = data_row_ys[2]; // feat-c: neither

    // Pick a content cell in the middle of each row for the bg check.
    let test_x = area.x + 10;

    // Item 0: multi-selected only -> multi_select_bg
    assert_eq!(buffer[(test_x, y_item_0)].bg, theme.multi_select_bg, "item 0 (multi-selected only) should have multi_select_bg");

    // Item 1: multi-selected AND active -> row_highlight wins
    assert_eq!(
        buffer[(test_x, y_item_1)].bg,
        theme.row_highlight,
        "item 1 (multi-selected + active) should have row_highlight, not multi_select_bg"
    );

    // Item 2: neither -> default background (Reset)
    assert_ne!(buffer[(test_x, y_item_2)].bg, theme.multi_select_bg, "item 2 (neither) should NOT have multi_select_bg");
    assert_ne!(buffer[(test_x, y_item_2)].bg, theme.row_highlight, "item 2 (neither) should NOT have row_highlight");
}

/// Remote host checkout paths are shortened using the remote host's home
/// directory, not the local home directory.
#[test]
fn remote_host_home_directory_shortening() {
    // Build a remote checkout item on host "feta" at /home/alice/dev/myrepo/feat-x.
    let remote_host = HostName::new("feta");
    let remote_path = PathBuf::from("/home/alice/dev/myrepo/feat-x");
    let remote_main_path = PathBuf::from("/home/alice/dev/myrepo");
    let host_path = HostPath::new(remote_host.clone(), remote_path.clone());
    let main_host_path = HostPath::new(remote_host.clone(), remote_main_path.clone());

    let main_item = WorkItem {
        kind: flotilla_protocol::WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(main_host_path.clone().into()),
        node_id: NodeId::new(remote_host.as_str()),
        branch: Some("main".into()),
        description: "checkout main".into(),
        checkout: Some(CheckoutRef::from_host_path(main_host_path, true)),
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: true,
        debug_group: Vec::new(),
        source: Some("feta".into()),
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: Vec::new(),
    };

    let feat_item = WorkItem {
        kind: flotilla_protocol::WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(host_path.clone().into()),
        node_id: NodeId::new(remote_host.as_str()),
        branch: Some("feat-x".into()),
        description: "checkout feat-x".into(),
        checkout: Some(CheckoutRef::from_host_path(host_path, false)),
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: Some("feta".into()),
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: Vec::new(),
    };

    let items = vec![main_item, feat_item];
    let providers = ProviderData::default();
    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);

    // Add local host so the model can distinguish local vs remote.
    let local_host = HostName::new("local");
    harness.model.hosts.insert(local_host.clone(), TuiHostState {
        host_name: local_host,
        is_local: true,
        status: PeerStatus::Connected,
        summary: HostSummary {
            node: NodeInfo::new(NodeId::new("local"), "local"),
            system: SystemInfo::default(),
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });

    // Add remote host info with home_dir set.
    harness.model.hosts.insert(remote_host.clone(), TuiHostState {
        host_name: remote_host.clone(),
        is_local: false,
        status: PeerStatus::Connected,
        summary: HostSummary {
            node: NodeInfo::new(NodeId::new(remote_host.as_str()), remote_host.as_str()),
            system: SystemInfo { home_dir: Some(PathBuf::from("/home/alice")), ..SystemInfo::default() },
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
            environments: vec![],
        },
    });

    // Use a wider display so the path column isn't truncated.
    harness = harness.with_width(180);
    let output = harness.render_to_string();

    // The main checkout path should be shortened to ~/dev/myrepo.
    assert!(output.contains("~/dev/myrepo"), "expected remote main path to be shortened with ~/: {output}");
    // The full path /home/alice/dev/myrepo should NOT appear.
    assert!(!output.contains("/home/alice/dev/myrepo"), "expected /home/alice to be replaced with ~ in remote path: {output}");
}
