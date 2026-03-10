mod support;

use flotilla_protocol::{ProviderData, SessionStatus};
use flotilla_tui::app::{Intent, ProviderStatus, UiMode};
use support::*;

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
                ("coding_agent", "Claude"),
            ],
        )
        .with_provider_status("my-project", "coding_agent", "Claude", ProviderStatus::Ok);
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
