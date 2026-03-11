mod support;

use flotilla_protocol::{ProviderData, SessionStatus};
use flotilla_tui::app::{BranchInputKind, InFlightCommand, Intent, ProviderStatus, UiMode};
use std::path::PathBuf;
use support::*;
use tui_input::Input;

fn picker_entry(name: &str, is_git_repo: bool, is_added: bool) -> flotilla_tui::app::DirEntry {
    flotilla_tui::app::DirEntry {
        name: name.to_string(),
        is_dir: true,
        is_git_repo,
        is_added,
    }
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
    let mut harness = TestHarness::single_repo("my-project")
        .with_status_message("GitHub API rate limit exceeded");
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn help_screen() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::Help);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn help_screen_clamps_scroll_state_after_render() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::Help);
    harness.ui.help_scroll = u16::MAX;
    let _ = harness.render_to_string();
    assert!(harness.ui.help_scroll < u16::MAX);
}

#[test]
fn action_menu() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::ActionMenu {
        items: vec![
            Intent::CreateWorkspace,
            Intent::OpenChangeRequest,
            Intent::RemoveCheckout,
        ],
        index: 0,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn config_screen() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_mode(UiMode::Config)
        .with_provider_names(
            "my-project",
            vec![
                ("code_review", "GitHub"),
                ("issue_tracker", "GitHub"),
                ("vcs", "Git"),
                ("checkout_manager", "Git Worktrees"),
                ("cloud_agent", "Claude"),
            ],
        )
        .with_provider_status("my-project", "cloud_agent", "Claude", ProviderStatus::Ok);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn selected_item_preview() {
    let mut providers = ProviderData::default();
    let (path, checkout) =
        make_checkout("feat-dashboard", "/test/my-project/feat-dashboard", false);
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
    harness.in_flight.insert(
        1,
        InFlightCommand {
            repo: PathBuf::from("/test/my-project"),
            description: "Refreshing repository...".into(),
        },
    );
    harness.in_flight.insert(
        2,
        InFlightCommand {
            repo: PathBuf::from("/test/my-project"),
            description: "Refreshing repository...".into(),
        },
    );
    harness.in_flight.insert(
        3,
        InFlightCommand {
            repo: PathBuf::from("/test/other-project"),
            description: "Should not render".into(),
        },
    );

    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn branch_input_generating_popup() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::BranchInput {
        input: Input::from("feature/new-branch"),
        kind: BranchInputKind::Generating,
        pending_issue_ids: vec![],
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn issue_search_mode_status_bar() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::IssueSearch {
        input: Input::from("auth timeout"),
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn file_picker_popup() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::FilePicker {
        input: Input::from("/test"),
        dir_entries: vec![
            picker_entry("repo-a", true, false),
            picker_entry("repo-b", true, true),
            flotilla_tui::app::DirEntry {
                name: "notes.txt".into(),
                is_dir: false,
                is_git_repo: false,
                is_added: false,
            },
        ],
        selected: 1,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_safe_to_delete() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::DeleteConfirm {
        info: Some(flotilla_protocol::CheckoutStatus {
            branch: "feat-cleanup".into(),
            change_request_status: Some("MERGED".into()),
            merge_commit_sha: Some("abc1234".into()),
            unpushed_commits: vec![],
            has_uncommitted: false,
            uncommitted_files: vec![],
            base_detection_warning: None,
        }),
        loading: false,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_with_uncommitted_files() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::DeleteConfirm {
        info: Some(flotilla_protocol::CheckoutStatus {
            branch: "feat-wip".into(),
            change_request_status: Some("OPEN".into()),
            merge_commit_sha: None,
            unpushed_commits: vec!["abc1234 work in progress".into()],
            has_uncommitted: true,
            uncommitted_files: vec![
                " M src/main.rs".into(),
                " M src/lib.rs".into(),
                "?? TODO.txt".into(),
            ],
            base_detection_warning: None,
        }),
        loading: false,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn delete_confirm_with_many_uncommitted_files() {
    let files: Vec<String> = (0..15).map(|i| format!(" M src/file_{}.rs", i)).collect();
    let mut harness = TestHarness::single_repo("my-project")
        .with_height(50)
        .with_mode(UiMode::DeleteConfirm {
            info: Some(flotilla_protocol::CheckoutStatus {
                branch: "feat-big-wip".into(),
                change_request_status: None,
                merge_commit_sha: None,
                unpushed_commits: vec![],
                has_uncommitted: true,
                uncommitted_files: files,
                base_detection_warning: None,
            }),
            loading: false,
        });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn providers_overlay() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_provider_names(
            "my-project",
            vec![
                ("vcs", "Git"),
                ("checkout_manager", "Git Worktrees"),
                ("code_review", "GitHub"),
                ("cloud_agent", "Claude"),
            ],
        )
        .with_provider_status("my-project", "cloud_agent", "Claude", ProviderStatus::Ok)
        .with_provider_status("my-project", "code_review", "GitHub", ProviderStatus::Error);
    let repo = harness.model.repo_order[0].clone();
    harness.ui.repo_ui.get_mut(&repo).unwrap().show_providers = true;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn config_screen_cross_repo_worst_wins() {
    let mut harness = TestHarness::multi_repo(&["alpha", "beta"])
        .with_mode(UiMode::Config)
        .with_provider_names("alpha", vec![("code_review", "GitHub")])
        .with_provider_names("beta", vec![("code_review", "GitHub")])
        .with_provider_status("alpha", "code_review", "GitHub", ProviderStatus::Ok)
        .with_provider_status("beta", "code_review", "GitHub", ProviderStatus::Error);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn debug_panel_with_correlation_details() {
    let mut item = support::checkout_item("feat-xyz", "/test/my-project/feat-xyz", false);
    item.description = "Feature branch checkout".into();
    item.debug_group = vec![
        "Group #12".into(),
        "branch: feat-xyz".into(),
        "checkout: /test/my-project/feat-xyz".into(),
    ];
    let mut harness = TestHarness::single_repo("my-project")
        .with_provider_data(ProviderData::default(), vec![item]);
    harness.ui.show_debug = true;
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
