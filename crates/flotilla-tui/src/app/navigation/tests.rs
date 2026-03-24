use flotilla_core::data::{GroupEntry, GroupedWorkItems};
use ratatui::layout::Rect;

use crate::app::{
    test_support::{issue_item, issue_table_entries, set_active_table_view, stub_app_with_repos},
    App,
};

/// Read the selected selectable index from the active RepoPage.
fn active_page_selection(app: &App) -> Option<usize> {
    let identity = &app.model.repo_order[app.model.active_repo];
    app.screen.repo_pages.get(identity).and_then(|p| p.table.selected_selectable_idx)
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
    set_active_table_view(&mut app, issue_table_entries(5));
    assert_eq!(active_page_selection(&app), None);
    app.select_next();
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_next_advances_selection() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(5));
    app.select_next(); // None -> 0
    app.select_next(); // 0 -> 1
    assert_eq!(active_page_selection(&app), Some(1));
}

#[test]
fn select_next_stays_at_end() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(3));
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
    set_active_table_view(&mut app, issue_table_entries(0));
    app.select_next();
    assert_eq!(active_page_selection(&app), None);
}

#[test]
fn select_next_triggers_fetch_when_near_bottom() {
    let mut app = stub_app_with_repos(1);
    // 6 items: positions 0-5. After two select_next calls we're at
    // position 1, and 1+5 = 6 >= 6 triggers the fetch.
    set_active_table_view(&mut app, issue_table_entries(6));

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
    set_active_table_view(&mut app, issue_table_entries(5));
    assert_eq!(active_page_selection(&app), None);
    app.select_prev();
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_prev_decrements_selection() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(5));
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
    set_active_table_view(&mut app, issue_table_entries(5));
    app.select_next(); // None -> 0
    app.select_prev(); // 0 -> 0 (stays)
    assert_eq!(active_page_selection(&app), Some(0));
}

#[test]
fn select_prev_noop_on_empty_table() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(0));
    app.select_prev();
    assert_eq!(active_page_selection(&app), None);
}

// ── row_at_mouse tests ───────────────────────────────────────────

#[test]
fn row_at_mouse_outside_table_returns_none() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(5));
    // Set a known table area
    app.ui.layout.table_area = Rect::new(10, 10, 50, 20);
    // Click outside: x before table
    assert!(app.row_at_mouse(5, 15).is_none());
    // Click outside: y before table
    assert!(app.row_at_mouse(15, 5).is_none());
    // Click outside: x after table
    assert!(app.row_at_mouse(60, 15).is_none());
    // Click outside: y after table
    assert!(app.row_at_mouse(15, 30).is_none());
}

#[test]
fn row_at_mouse_header_rows_return_none() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(5));
    app.ui.layout.table_area = Rect::new(10, 10, 50, 20);
    // Row 0 and row 1 (header rows, y=10 and y=11) should return None
    assert!(app.row_at_mouse(15, 10).is_none());
    assert!(app.row_at_mouse(15, 11).is_none());
}

#[test]
fn row_at_mouse_valid_row_returns_selectable_index() {
    let mut app = stub_app_with_repos(1);
    set_active_table_view(&mut app, issue_table_entries(5));
    app.ui.layout.table_area = Rect::new(10, 10, 50, 20);
    // y=12 means row_in_table=2, data_row=0, offset=0, actual_row=0
    // selectable_indices = [0,1,2,3,4], so position of 0 is 0
    assert_eq!(app.row_at_mouse(15, 12), Some(0));
    // y=13 means data_row=1, actual_row=1 -> selectable index 1
    assert_eq!(app.row_at_mouse(15, 13), Some(1));
    // y=16 means data_row=4, actual_row=4 -> selectable index 4
    assert_eq!(app.row_at_mouse(15, 16), Some(4));
}

#[test]
fn row_at_mouse_non_selectable_row_returns_none() {
    let mut app = stub_app_with_repos(1);
    // Create entries with a gap: selectable indices are [0, 2] (skip 1 = header)
    let table_view = GroupedWorkItems {
        table_entries: vec![
            GroupEntry::Item(Box::new(issue_item("0"))),
            GroupEntry::Header(flotilla_core::data::SectionHeader("Section".into())),
            GroupEntry::Item(Box::new(issue_item("2"))),
        ],
        selectable_indices: vec![0, 2],
    };
    set_active_table_view(&mut app, table_view);
    app.ui.layout.table_area = Rect::new(10, 10, 50, 20);
    // y=12 -> data_row=0 -> actual_row=0, which IS in selectable_indices
    assert_eq!(app.row_at_mouse(15, 12), Some(0));
    // y=13 -> data_row=1 -> actual_row=1, which is NOT in selectable_indices (it's a header)
    assert!(app.row_at_mouse(15, 13).is_none());
    // y=14 -> data_row=2 -> actual_row=2, which IS in selectable_indices at position 1
    assert_eq!(app.row_at_mouse(15, 14), Some(1));
}
