use flotilla_protocol::ViewAddress;

use crate::app::{
    test_support::{activate_repo_tab, issue_item, set_active_table_items, stub_app_with_repos},
    App,
};

/// Read the selected flat index from the active RepoPage.
fn active_page_selection(app: &App) -> Option<usize> {
    let identity = app.model.active_repo.as_ref().expect("active tab should be a repo view");
    app.screen.repo_pages.get(identity).and_then(|p| p.table.selected_flat_index())
}

/// The parsed address of the active tab.
fn active_address(app: &App) -> ViewAddress {
    app.views.active_address().expect("active tab should have a parsed address").clone()
}

fn convoys_address() -> ViewAddress {
    ViewAddress::Convoys { namespace: "flotilla".to_string() }
}

// Seeded tab layout for `stub_app_with_repos(n)` is
// `[overview(0), convoys(1), repo-0(2), .., repo-(n-1)(n+1)]` with repo-0 active.

// ── switch_tab tests ─────────────────────────────────────────────

#[test]
fn switch_tab_sets_active_repo() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(0);
    assert_eq!(app.model.active_repo, None, "overview tab has no active repo");
    activate_repo_tab(&mut app, 2);
    assert_eq!(app.views.active_index(), 4);
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[2].clone()));
}

#[test]
fn switch_tab_clears_unseen_changes() {
    let mut app = stub_app_with_repos(2);
    // Mark repo-1 as having unseen changes
    let key = app.model.repo_order[1].clone();
    app.model.repos.get_mut(&key).unwrap().has_unseen_changes = true;
    activate_repo_tab(&mut app, 1);
    assert!(!app.model.repos[&key].has_unseen_changes);
}

#[test]
fn switch_tab_noop_for_out_of_range() {
    let mut app = stub_app_with_repos(2);
    app.switch_tab(9);
    // Should remain on the seeded active tab (repo-0)
    assert_eq!(app.views.active_index(), 2);
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[0].clone()));
}

#[test]
fn switch_tab_from_overview_to_repo() {
    let mut app = stub_app_with_repos(2);
    app.switch_tab(0);
    activate_repo_tab(&mut app, 1);
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[1].clone()));
    assert_ne!(app.views.active_index(), 0, "should have left the overview tab");
}

// ── next_tab tests ───────────────────────────────────────────────

#[test]
fn next_tab_advances_to_next_repo_tab() {
    let mut app = stub_app_with_repos(3);
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[0].clone()));
    app.next_tab();
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[1].clone()));
}

#[test]
fn next_tab_wraps_to_overview_after_last_tab() {
    let mut app = stub_app_with_repos(2);
    activate_repo_tab(&mut app, 1); // go to last repo tab
    app.next_tab();
    assert_eq!(active_address(&app), ViewAddress::Overview);
    assert_eq!(app.model.active_repo, None);
}

#[test]
fn next_tab_from_overview_goes_to_convoys() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(0);
    app.next_tab();
    assert_eq!(active_address(&app), convoys_address(), "expected Convoys tab after next from overview");
}

#[test]
fn next_tab_from_convoys_goes_to_first_repo() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(1);
    app.next_tab();
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[0].clone()));
}

#[test]
fn next_tab_cycles_with_no_repos() {
    let mut app = stub_app_with_repos(0);
    // Layout is [overview, convoys] with convoys active — should not panic.
    app.next_tab();
    assert_eq!(active_address(&app), ViewAddress::Overview);
}

// ── prev_tab tests ───────────────────────────────────────────────

#[test]
fn prev_tab_steps_back_to_previous_repo_tab() {
    let mut app = stub_app_with_repos(3);
    activate_repo_tab(&mut app, 2);
    app.prev_tab();
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[1].clone()));
}

#[test]
fn prev_tab_wraps_to_convoys_from_first_repo() {
    let mut app = stub_app_with_repos(2);
    // repo-0 is active — prev goes to Convoys, not the overview directly
    app.prev_tab();
    assert_eq!(active_address(&app), convoys_address());
    assert_eq!(app.model.active_repo, None);
}

#[test]
fn prev_tab_wraps_to_overview_from_convoys() {
    let mut app = stub_app_with_repos(2);
    app.switch_tab(1);
    app.prev_tab();
    assert_eq!(active_address(&app), ViewAddress::Overview);
}

#[test]
fn prev_tab_from_overview_goes_to_last_tab() {
    let mut app = stub_app_with_repos(3);
    app.switch_tab(0);
    app.prev_tab();
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[2].clone()));
}

#[test]
fn prev_tab_cycles_with_no_repos() {
    let mut app = stub_app_with_repos(0);
    // Layout is [overview, convoys] with convoys active — should not panic.
    app.prev_tab();
    assert_eq!(active_address(&app), ViewAddress::Overview);
}

// ── move_tab tests ───────────────────────────────────────────────

#[test]
fn move_tab_moves_active_tab_right_and_persists() {
    let mut app = stub_app_with_repos(3);
    // Active is repo-0 at tab index 2.
    let repo0 = ViewAddress::Repo(app.model.repo_order[0].clone());
    let repo1 = ViewAddress::Repo(app.model.repo_order[1].clone());
    let order_before = app.model.repo_order.clone();

    assert!(app.move_tab(1));

    assert_eq!(app.views.active_index(), 3, "active follows its tab");
    assert_eq!(app.views.get(2).and_then(|v| v.address()), Some(&repo1));
    assert_eq!(app.views.get(3).and_then(|v| v.address()), Some(&repo0));
    assert_eq!(app.model.repo_order, order_before, "registration order is not tab order");

    // The new tab order is persisted to open-views.toml.
    let saved = app.config.load_open_views().expect("open views should be persisted after a move");
    let addresses: Vec<String> = saved.iter().map(|e| e.address.clone()).collect();
    assert_eq!(addresses[2], repo1.to_string());
    assert_eq!(addresses[3], repo0.to_string());
}

#[test]
fn move_tab_moves_active_tab_left() {
    let mut app = stub_app_with_repos(3);
    activate_repo_tab(&mut app, 1); // tab index 3
    let repo1 = ViewAddress::Repo(app.model.repo_order[1].clone());

    assert!(app.move_tab(-1));

    assert_eq!(app.views.active_index(), 2);
    assert_eq!(app.views.get(2).and_then(|v| v.address()), Some(&repo1));
    assert_eq!(app.model.active_repo, Some(app.model.repo_order[1].clone()), "moving a tab keeps it active");
}

#[test]
fn move_tab_returns_false_at_boundaries() {
    let mut app = stub_app_with_repos(3);
    // The pinned overview never moves.
    app.switch_tab(0);
    assert!(!app.move_tab(1));
    assert!(!app.move_tab(-1));
    // Convoys at index 1 cannot displace the pinned overview.
    app.switch_tab(1);
    assert!(!app.move_tab(-1));
    // The last tab cannot move further right.
    activate_repo_tab(&mut app, 2);
    assert!(!app.move_tab(1));
}

#[test]
fn move_tab_stops_at_pinned_overview_with_single_repo() {
    let mut app = stub_app_with_repos(1);
    // [overview, convoys, repo-0] with repo-0 active.
    assert!(!app.move_tab(1), "already the last tab");
    assert!(app.move_tab(-1), "repo tab can swap left past convoys");
    assert_eq!(app.views.active_index(), 1);
    assert!(!app.move_tab(-1), "nothing displaces the pinned overview");
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
