use crate::app::{
    test_support::{issue_item, set_active_table_items, stub_app_with_repos},
    App,
};

/// Read the selected flat index from the active RepoPage.
fn active_page_selection(app: &App) -> Option<usize> {
    let identity = &app.model.repo_order[app.model.active_repo];
    app.screen.repo_pages.get(identity).and_then(|p| p.table.selected_flat_index())
}

// ── switch_tab tests ─────────────────────────────────────────────

#[test]
fn switch_tab_sets_active_repo_and_mode() {
    let mut app = stub_app_with_repos(3);
    app.ui.is_config = true;
    app.switch_tab(2);
    assert_eq!(app.model.active_repo, 2);
    assert!(!app.ui.is_config);
}

#[test]
fn switch_tab_clears_unseen_changes() {
    let mut app = stub_app_with_repos(2);
    // Mark repo-1 as having unseen changes
    let key = app.model.repo_order[1].clone();
    app.model.repos.get_mut(&key).unwrap().has_unseen_changes = true;
    app.switch_tab(1);
    assert!(!app.model.repos[&key].has_unseen_changes);
}

#[test]
fn switch_tab_noop_for_out_of_range() {
    let mut app = stub_app_with_repos(2);
    app.switch_tab(5);
    // Should remain at the default active_repo
    assert_eq!(app.model.active_repo, 0);
}

#[test]
fn switch_tab_from_config_mode() {
    let mut app = stub_app_with_repos(2);
    app.ui.is_config = true;
    app.switch_tab(1);
    assert_eq!(app.model.active_repo, 1);
    assert!(!app.ui.is_config);
}

// ── next_tab tests ───────────────────────────────────────────────

#[test]
fn next_tab_advances_active_repo() {
    let mut app = stub_app_with_repos(3);
    assert_eq!(app.model.active_repo, 0);
    app.next_tab();
    assert_eq!(app.model.active_repo, 1);
}

#[test]
fn next_tab_wraps_to_config() {
    let mut app = stub_app_with_repos(2);
    app.switch_tab(1); // go to last repo
    app.next_tab();
    assert!(app.ui.is_config);
}

#[test]
fn next_tab_from_config_goes_to_first() {
    let mut app = stub_app_with_repos(3);
    app.ui.is_config = true;
    app.next_tab();
    assert_eq!(app.model.active_repo, 0);
    assert!(!app.ui.is_config);
}

#[test]
fn next_tab_noop_with_no_repos() {
    let mut app = stub_app_with_repos(0);
    // Should not panic
    app.next_tab();
}

// ── prev_tab tests ───────────────────────────────────────────────

#[test]
fn prev_tab_decrements_active_repo() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(2);
    app.prev_tab();
    assert_eq!(app.model.active_repo, 1);
}

#[test]
fn prev_tab_wraps_to_config() {
    let mut app = stub_app_with_repos(2);
    // active_repo is 0
    app.prev_tab();
    assert!(app.ui.is_config);
}

#[test]
fn prev_tab_from_config_goes_to_last() {
    let mut app = stub_app_with_repos(3);
    app.ui.is_config = true;
    app.prev_tab();
    assert_eq!(app.model.active_repo, 2);
    assert!(!app.ui.is_config);
}

#[test]
fn prev_tab_noop_with_no_repos() {
    let mut app = stub_app_with_repos(0);
    // Should not panic
    app.prev_tab();
}

// ── move_tab tests ───────────────────────────────────────────────

#[test]
fn move_tab_swaps_repos_forward() {
    let mut app = stub_app_with_repos(3);
    assert_eq!(app.model.active_repo, 0);
    let path0 = app.model.repo_order[0].clone();
    let path1 = app.model.repo_order[1].clone();
    let result = app.move_tab(1);
    assert!(result);
    assert_eq!(app.model.active_repo, 1);
    assert_eq!(app.model.repo_order[0], path1);
    assert_eq!(app.model.repo_order[1], path0);
}

#[test]
fn move_tab_swaps_repos_backward() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(2);
    let path1 = app.model.repo_order[1].clone();
    let path2 = app.model.repo_order[2].clone();
    let result = app.move_tab(-1);
    assert!(result);
    assert_eq!(app.model.active_repo, 1);
    assert_eq!(app.model.repo_order[1], path2);
    assert_eq!(app.model.repo_order[2], path1);
}

#[test]
fn move_tab_returns_false_at_boundary() {
    let mut app = stub_app_with_repos(3);
    // At index 0, can't move backward
    assert!(!app.move_tab(-1));
    // Move to last
    app.switch_tab(2);
    // At last index, can't move forward
    assert!(!app.move_tab(1));
}

#[test]
fn move_tab_returns_false_with_single_repo() {
    let mut app = stub_app_with_repos(1);
    assert!(!app.move_tab(1));
    assert!(!app.move_tab(-1));
}

// ── select_next tests ────────────────────────────────────────────

#[test]
fn select_next_from_none_selects_first() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..5).map(|i| issue_item(i.to_string())).collect());
    assert_eq!(active_page_selection(&app), None);
    app.select_next();
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_next_advances_selection() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..5).map(|i| issue_item(i.to_string())).collect());
    app.select_next(); // None -> 0
    app.select_next(); // 0 -> 1
    assert_eq!(active_page_selection(&app), Some(1));
}

#[test]
fn select_next_stays_at_end() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..3).map(|i| issue_item(i.to_string())).collect());
    // Select each item in order
    app.select_next(); // None -> 0
    app.select_next(); // 0 -> 1
    app.select_next(); // 1 -> 2
    app.select_next(); // 2 -> 2 (stays)
    assert_eq!(active_page_selection(&app), Some(2));
}

#[test]
fn select_next_noop_on_empty_table() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, vec![]);
    app.select_next();
    assert_eq!(active_page_selection(&app), None);
}

#[test]
fn select_next_triggers_fetch_when_near_bottom() {
    let mut app = stub_app_with_repos(1);
    // 6 items: positions 0-5. After two select_next calls we're at
    // position 1, and 1+5 = 6 >= 6 triggers the fetch.
    set_active_table_items(&mut app, (0..6).map(|i| issue_item(i.to_string())).collect());

    let repo = app.model.repo_order[0].clone();
    if let Some(rm) = app.model.repos.get_mut(&repo) {
        rm.issue_has_more = true;
        rm.issue_fetch_pending = false;
    }

    // Navigate to position 1 (next=1, 1+5=6 >= 6 triggers fetch)
    app.select_next(); // None -> 0
    app.select_next(); // 0 -> 1

    // At this point next=1, 1+5=6 >= 6, so it should trigger
    let entry = app.proto_commands.take_next();
    assert!(entry.is_some(), "expected FetchMoreIssues command");
    match entry.unwrap().0 {
        flotilla_protocol::Command {
            action: flotilla_protocol::CommandAction::FetchMoreIssues { repo: cmd_repo, desired_count }, ..
        } => {
            assert_eq!(cmd_repo, flotilla_protocol::RepoSelector::Path(app.model.repos[&repo].path.clone()));
            // providers.issues is empty (default), so desired = 0 + 50
            assert_eq!(desired_count, 50);
        }
        other => panic!("expected FetchMoreIssues, got {other:?}"),
    }

    // issue_fetch_pending should now be true
    assert!(app.model.repos[&repo].issue_fetch_pending);
}

// ── select_prev tests ────────────────────────────────────────────

#[test]
fn select_prev_from_none_selects_first() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..5).map(|i| issue_item(i.to_string())).collect());
    assert_eq!(active_page_selection(&app), None);
    app.select_prev();
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_prev_decrements_selection() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..5).map(|i| issue_item(i.to_string())).collect());
    // Navigate to position 2
    app.select_next(); // None -> 0
    app.select_next(); // 0 -> 1
    app.select_next(); // 1 -> 2
    app.select_prev(); // 2 -> 1
    assert_eq!(active_page_selection(&app), Some(1));
}

#[test]
fn select_prev_stays_at_zero() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, (0..5).map(|i| issue_item(i.to_string())).collect());
    app.select_next(); // None -> 0
    app.select_prev(); // 0 -> 0 (stays)
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_prev_noop_on_empty_table() {
    let mut app = stub_app_with_repos(1);
    set_active_table_items(&mut app, vec![]);
    app.select_prev();
    assert_eq!(active_page_selection(&app), None);
}

// ── row_at_mouse tests ───────────────────────────────────────────
// Note: mouse hit-testing now uses SplitTable's row_at_mouse(), which
// depends on section_areas populated during render. Since these tests
// don't render, they exercise the App-level row_at_mouse helper which
// works differently from the SplitTable's internal mouse handling.
// The mouse hit-testing at the SplitTable level is tested in
// repo_page/tests.rs and split_table/tests.rs.
