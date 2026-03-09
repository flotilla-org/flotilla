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
        make_work_item_cr("42", "Add login page"),
        make_work_item_issue("10", "Users need authentication"),
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
        .with_provider_status("my-project", "code_review", "GitHub", ProviderStatus::Ok)
        .with_provider_status("my-project", "issues", "GitHub", ProviderStatus::Error);
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
        make_work_item_cr("99", "Build analytics dashboard"),
    ];

    let mut harness = TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
