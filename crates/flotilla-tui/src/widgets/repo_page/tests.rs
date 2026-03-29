use flotilla_protocol::{CloudAgentSession, ProviderData, RepoLabels, SessionStatus, WorkItemIdentity};

use super::*;
use crate::app::test_support::{issue_item, session_item, TestWidgetHarness};

fn test_repo_identity() -> RepoIdentity {
    RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() }
}

fn test_repo_data(items: Vec<WorkItem>) -> Shared<RepoData> {
    Shared::new(RepoData {
        path: PathBuf::from("/tmp/test-repo"),
        providers: Arc::new(ProviderData::default()),
        labels: RepoLabels::default(),
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        work_items: items,
        issue_has_more: false,
        issue_total: None,
        issue_search_active: false,
        loading: false,
    })
}

fn page_with_items(items: Vec<WorkItem>) -> RepoPage {
    let data = test_repo_data(items);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);
    page.reconcile_if_changed();
    page
}

/// Shared data containing one archived session ("s1") and one issue ("i1").
fn repo_data_with_archived_session() -> Shared<RepoData> {
    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), CloudAgentSession {
        title: String::new(),
        status: SessionStatus::Archived,
        model: None,
        updated_at: None,
        correlation_keys: Vec::new(),
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    });
    Shared::new(RepoData {
        path: PathBuf::from("/tmp/test-repo"),
        providers: Arc::new(providers),
        labels: RepoLabels::default(),
        provider_names: HashMap::new(),
        provider_health: HashMap::new(),
        work_items: vec![session_item("s1"), issue_item("i1")],
        issue_has_more: false,
        issue_total: None,
        issue_search_active: false,
        loading: false,
    })
}

// ── reconcile_if_changed ──

#[test]
fn reconcile_rebuilds_table_on_data_change() {
    let data = test_repo_data(vec![issue_item("1"), issue_item("2")]);
    let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);

    // First reconciliation should pick up initial data.
    page.reconcile_if_changed();
    assert_eq!(page.table.total_item_count(), 2);

    // Mutate the shared data to add a third item.
    data.mutate(|d| d.work_items.push(issue_item("3")));

    page.reconcile_if_changed();
    assert_eq!(page.table.total_item_count(), 3);
}

#[test]
fn reconcile_is_noop_when_unchanged() {
    let data = test_repo_data(vec![issue_item("1")]);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

    page.reconcile_if_changed();
    let gen_after_first = page.last_seen_generation;

    // Second call should not update generation.
    page.reconcile_if_changed();
    assert_eq!(page.last_seen_generation, gen_after_first);
}

#[test]
fn reconcile_prunes_stale_multi_select() {
    let data = test_repo_data(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
    let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);
    page.reconcile_if_changed();

    // Multi-select items 1 and 3.
    page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));
    page.multi_selected.insert(WorkItemIdentity::Issue("3".into()));
    assert_eq!(page.multi_selected.len(), 2);

    // Remove item 3 from the data.
    data.mutate(|d| d.work_items.retain(|i| i.identity != WorkItemIdentity::Issue("3".into())));

    page.reconcile_if_changed();
    assert_eq!(page.multi_selected.len(), 1);
    assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));
    assert!(!page.multi_selected.contains(&WorkItemIdentity::Issue("3".into())));
}

#[test]
fn reconcile_prunes_stale_pending_actions() {
    let data = test_repo_data(vec![issue_item("1"), issue_item("2")]);
    let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);
    page.reconcile_if_changed();

    page.pending_actions.insert(WorkItemIdentity::Issue("1".into()), PendingAction {
        command_id: 1,
        status: crate::app::ui_state::PendingStatus::InFlight,
        description: "test".into(),
    });
    page.pending_actions.insert(WorkItemIdentity::Issue("2".into()), PendingAction {
        command_id: 2,
        status: crate::app::ui_state::PendingStatus::InFlight,
        description: "test".into(),
    });

    // Remove item 2.
    data.mutate(|d| d.work_items.retain(|i| i.identity != WorkItemIdentity::Issue("2".into())));

    page.reconcile_if_changed();
    assert!(page.pending_actions.contains_key(&WorkItemIdentity::Issue("1".into())));
    assert!(!page.pending_actions.contains_key(&WorkItemIdentity::Issue("2".into())));
}

// ── dismiss cascade ──

#[test]
fn dismiss_cascade_cancels_in_flight_first() {
    let mut page = page_with_items(vec![issue_item("1")]);
    let mut harness = TestWidgetHarness::new();
    harness.in_flight.insert(42, crate::app::InFlightCommand {
        repo_identity: harness.model.repo_order[0].clone(),
        repo: PathBuf::from("/tmp/test-repo"),
        description: "test".into(),
    });
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(42))));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
}

#[test]
fn dismiss_cancels_most_recent_command() {
    let mut page = page_with_items(vec![issue_item("1")]);
    let mut harness = TestWidgetHarness::new();
    let repo_identity = harness.model.repo_order[0].clone();
    harness.in_flight.insert(10, crate::app::InFlightCommand {
        repo_identity: repo_identity.clone(),
        repo: PathBuf::from("/tmp/test-repo"),
        description: "older".into(),
    });
    harness.in_flight.insert(20, crate::app::InFlightCommand {
        repo_identity,
        repo: PathBuf::from("/tmp/test-repo"),
        description: "newer".into(),
    });
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(
        ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(20))),
        "should cancel the most recent command (highest ID)"
    );
}

#[test]
fn dismiss_ignores_commands_for_other_repos() {
    let mut page = page_with_items(vec![issue_item("1")]);
    let mut harness = TestWidgetHarness::new();
    // Insert a command for a different repo — dismiss should not cancel it.
    harness.in_flight.insert(42, crate::app::InFlightCommand {
        repo_identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "other/repo".into() },
        repo: PathBuf::from("/tmp/other-repo"),
        description: "other repo command".into(),
    });
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    // Should fall through to quit since no in-flight command matches active repo.
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(_))), "should not cancel other repo's command");
}

#[test]
fn dismiss_cascade_clears_search_second() {
    let mut page = page_with_items(vec![issue_item("1")]);
    page.active_search_query = Some("test".into());

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    assert!(page.active_search_query.is_none());
}

#[test]
fn dismiss_cascade_clears_providers_third() {
    let mut page = page_with_items(vec![issue_item("1")]);
    page.show_providers = true;

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    assert!(!page.show_providers);
}

#[test]
fn dismiss_cascade_clears_multi_select_fourth() {
    let mut page = page_with_items(vec![issue_item("1")]);
    page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    assert!(page.multi_selected.is_empty());
}

#[test]
fn dismiss_cascade_clears_selection_fifth() {
    let mut page = page_with_items(vec![issue_item("1")]);
    // After page_with_items, the first item is auto-selected.
    assert!(page.table.selected_work_item().is_some());

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    assert!(page.table.selected_work_item().is_none());
}

#[test]
fn dismiss_cascade_quits_when_nothing_to_clear() {
    // Empty table: no selection, no search, no providers — should quit.
    let mut page = page_with_items(vec![]);

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Dismiss, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
}

// ── cycle_layout ──

#[test]
fn cycle_layout_is_page_scoped() {
    let mut page = page_with_items(vec![]);
    assert_eq!(page.layout, RepoViewLayout::Auto);

    page.cycle_layout();
    assert_eq!(page.layout, RepoViewLayout::Zoom);

    page.cycle_layout();
    assert_eq!(page.layout, RepoViewLayout::Right);

    page.cycle_layout();
    assert_eq!(page.layout, RepoViewLayout::Below);

    page.cycle_layout();
    assert_eq!(page.layout, RepoViewLayout::Auto);
}

#[test]
fn cycle_layout_action_emits_app_action() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::CycleLayout, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CycleLayout)));
    // The page does NOT cycle its own layout — process_app_actions does that.
    assert_eq!(page.layout, RepoViewLayout::Auto);
}

// ── select_next / select_prev ──

#[test]
fn select_next_advances() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
    let mut harness = TestWidgetHarness::new();

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::SelectNext, &mut ctx);
    }

    assert_eq!(page.table.selected_flat_index(), Some(1));
}

#[test]
fn select_prev_decrements() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
    let mut harness = TestWidgetHarness::new();

    // Move to index 2.
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::SelectNext, &mut ctx);
    }
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::SelectNext, &mut ctx);
    }
    assert_eq!(page.table.selected_flat_index(), Some(2));

    // Move back to 1.
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::SelectPrev, &mut ctx);
    }
    assert_eq!(page.table.selected_flat_index(), Some(1));
}

// ── toggle_multi_select ──

#[test]
fn toggle_multi_select_adds_and_removes() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2")]);
    let mut harness = TestWidgetHarness::new();

    // Toggle on.
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleMultiSelect, &mut ctx);
    }
    assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));

    // Toggle off.
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleMultiSelect, &mut ctx);
    }
    assert!(page.multi_selected.is_empty());
}

// ── select_all ──

#[test]
fn select_all_selects_all_items() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);

    page.select_all();

    assert_eq!(page.multi_selected.len(), 3);
    assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));
    assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("2".into())));
    assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("3".into())));
}

// ── toggle_providers ──

#[test]
fn toggle_providers_toggles() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleProviders, &mut ctx);
    }
    assert!(page.show_providers);

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleProviders, &mut ctx);
    }
    assert!(!page.show_providers);
}

// ── toggle_archived ──

#[test]
fn toggle_archived_flips_show_archived() {
    let mut harness = TestWidgetHarness::new();
    let data = test_repo_data(vec![]);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

    assert!(!page.show_archived);
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleArchived, &mut ctx);
    }
    assert!(page.show_archived);
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleArchived, &mut ctx);
    }
    assert!(!page.show_archived);
}

#[test]
fn toggle_archived_rebuilds_table_immediately() {
    let mut page = RepoPage::new(test_repo_identity(), repo_data_with_archived_session(), RepoViewLayout::Auto);
    page.reconcile_if_changed();

    // With show_archived=false, the archived session row is filtered out.
    let count_before = page.table.total_item_count();

    let mut harness = TestWidgetHarness::new();
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleArchived, &mut ctx);
    }

    // After toggling on, the archived session should appear immediately.
    let count_after = page.table.total_item_count();
    assert!(count_after > count_before, "toggle on should reveal archived session row");

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleArchived, &mut ctx);
    }

    // After toggling off, the archived session should be hidden again.
    assert_eq!(page.table.total_item_count(), count_before, "toggle off should re-hide archived session row");
}

#[test]
fn dismiss_rebuilds_table_when_clearing_archived() {
    let mut page = RepoPage::new(test_repo_identity(), repo_data_with_archived_session(), RepoViewLayout::Auto);
    page.reconcile_if_changed();

    let hidden_count = page.table.total_item_count();

    // Toggle archived on, then dismiss to turn it back off.
    let mut harness = TestWidgetHarness::new();
    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::ToggleArchived, &mut ctx);
    }
    assert!(page.show_archived);
    let visible_count = page.table.total_item_count();
    assert!(visible_count > hidden_count);

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::Dismiss, &mut ctx);
    }
    assert!(!page.show_archived);
    assert_eq!(page.table.total_item_count(), hidden_count, "dismiss should re-hide archived session rows");
}

#[test]
fn dismiss_clears_show_archived_before_multi_select() {
    let mut harness = TestWidgetHarness::new();
    let data = test_repo_data(vec![issue_item("1")]);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

    page.show_archived = true;
    page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::Dismiss, &mut ctx);
    }
    assert!(!page.show_archived, "archived cleared first");
    assert!(!page.multi_selected.is_empty(), "multi-select not yet cleared");

    {
        let mut ctx = harness.ctx();
        page.handle_action(Action::Dismiss, &mut ctx);
    }
    assert!(page.multi_selected.is_empty(), "now multi-select cleared");
}

// ── quit ──

#[test]
fn quit_pushes_app_action() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Quit, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
}

// ── push modal widgets ──

#[test]
fn toggle_help_pushes_widget() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::ToggleHelp, &mut ctx);
    assert!(matches!(outcome, Outcome::Push(_)));
}

#[test]
fn open_branch_input_pushes_widget() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::OpenBranchInput, &mut ctx);
    assert!(matches!(outcome, Outcome::Push(_)));
}

#[test]
fn open_issue_search_pushes_widget() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::OpenIssueSearch, &mut ctx);
    assert!(matches!(outcome, Outcome::Push(_)));
}

#[test]
fn open_command_palette_pushes_widget() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::OpenCommandPalette, &mut ctx);
    assert!(matches!(outcome, Outcome::Push(_)));
}

// ── ignored actions ──

#[test]
fn confirm_returns_ignored() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::Confirm, &mut ctx);
    assert!(matches!(outcome, Outcome::Ignored));
}

#[test]
fn open_action_menu_returns_ignored() {
    let mut page = page_with_items(vec![]);
    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let outcome = page.handle_action(Action::OpenActionMenu, &mut ctx);
    assert!(matches!(outcome, Outcome::Ignored));
}

// ── binding_mode ──

#[test]
fn binding_mode_normal_when_no_search() {
    let page = page_with_items(vec![]);
    assert_eq!(page.binding_mode(), KeyBindingMode::from(BindingModeId::Normal));
}

#[test]
fn binding_mode_composed_when_search_active() {
    let mut page = page_with_items(vec![]);
    page.active_search_query = Some("test".into());
    match page.binding_mode() {
        KeyBindingMode::Composed(ids) => {
            assert_eq!(ids, vec![BindingModeId::Normal, BindingModeId::SearchActive]);
        }
        other => panic!("expected Composed, got {:?}", other),
    }
}

// ── status_fragment ──

#[test]
fn status_fragment_default_when_nothing_active() {
    let page = page_with_items(vec![]);
    assert!(page.status_fragment().status.is_none());
}

#[test]
fn status_fragment_shows_selected_count() {
    let mut page = page_with_items(vec![issue_item("1")]);
    page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));
    let fragment = page.status_fragment();
    match fragment.status {
        Some(StatusContent::Label(s)) => assert!(s.contains("SELECTED")),
        other => panic!("expected Label with SELECTED, got {:?}", other),
    }
}

#[test]
fn status_fragment_shows_search_query() {
    let mut page = page_with_items(vec![]);
    page.active_search_query = Some("auth".into());
    let fragment = page.status_fragment();
    match fragment.status {
        Some(StatusContent::Label(s)) => assert!(s.contains("auth")),
        other => panic!("expected Label with query, got {:?}", other),
    }
}

#[test]
fn status_fragment_shows_providers() {
    let mut page = page_with_items(vec![]);
    page.show_providers = true;
    let fragment = page.status_fragment();
    match fragment.status {
        Some(StatusContent::Label(s)) => assert_eq!(s, "PROVIDERS"),
        other => panic!("expected Label PROVIDERS, got {:?}", other),
    }
}

// ── preview position resolution ──

#[test]
fn auto_layout_prefers_right_when_wide() {
    let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Auto);
    assert_eq!(position, Some(ResolvedPreviewPosition::Right));
}

#[test]
fn auto_layout_prefers_below_when_tall() {
    let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Auto);
    assert_eq!(position, Some(ResolvedPreviewPosition::Below));
}

#[test]
fn explicit_right_layout() {
    let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Right);
    assert_eq!(position, Some(ResolvedPreviewPosition::Right));
}

#[test]
fn explicit_below_layout() {
    let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Below);
    assert_eq!(position, Some(ResolvedPreviewPosition::Below));
}

#[test]
fn zoom_layout_returns_none() {
    let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Zoom);
    assert_eq!(position, None);
}

// ── Mouse selection regression tests ──
//
// SplitTable mouse hit-testing requires `section_areas` to be populated
// during render, so direct mouse dispatch without rendering won't produce
// hits. Instead, we test that the select_by_mouse API works and that
// scroll events trigger navigation.

#[test]
fn select_by_mouse_selects_correct_item() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
    assert_eq!(page.table.selected_flat_index(), Some(0));

    // Select the last item via direct API.
    page.table.select_flat_index(2);
    assert_eq!(page.table.selected_flat_index(), Some(2));
    assert_eq!(page.table.selected_work_item().expect("selected").description, "Item 3");
}

#[test]
fn scroll_down_advances_selection() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2")]);
    assert_eq!(page.table.selected_flat_index(), Some(0));

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: 5,
        row: 5,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    let outcome = page.handle_mouse(mouse, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert_eq!(page.table.selected_flat_index(), Some(1), "scroll down should advance selection");
}

#[test]
fn scroll_up_retreats_selection() {
    let mut page = page_with_items(vec![issue_item("1"), issue_item("2")]);
    // Move to second item.
    page.table.select_next();
    assert_eq!(page.table.selected_flat_index(), Some(1));

    let mut harness = TestWidgetHarness::new();
    let mut ctx = harness.ctx();

    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollUp,
        column: 5,
        row: 5,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    let outcome = page.handle_mouse(mouse, &mut ctx);
    assert!(matches!(outcome, Outcome::Consumed));
    assert_eq!(page.table.selected_flat_index(), Some(0), "scroll up should retreat selection");
}
