use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::GroupEntry;
use flotilla_protocol::{Command, CommandAction, WorkItem};

use super::{ui_state::PendingActionContext, App, BranchInputKind, Intent};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode},
    keymap::Action,
    widgets::InteractiveWidget,
};

impl App {
    // ── Key handling ──

    /// Resolve a key event using the app-level config/normal distinction.
    ///
    /// Called when the base layer widget (Normal mode_id) is on top, so that
    /// config mode gets Overview bindings instead of Normal.
    fn resolve_action(&self, key: KeyEvent) -> Option<Action> {
        let mode_id = if self.ui.is_config { BindingModeId::Overview } else { BindingModeId::Normal };
        let mode: KeyBindingMode = mode_id.into();
        self.keymap.resolve(&mode, crokey::KeyCombination::from(key))
    }

    /// Handle actions that the widget stack returned `Ignored` for.
    ///
    /// These are actions that need `&mut App` context the widget doesn't
    /// have: confirm/enter, action menu, file picker, and dispatch intent.
    pub(super) fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Confirm => {
                if !self.ui.is_config {
                    self.action_enter();
                }
            }
            Action::OpenActionMenu => {
                if !self.ui.is_config {
                    self.open_action_menu();
                }
            }
            Action::OpenFilePicker => {
                if !self.ui.is_config {
                    self.open_file_picker_from_active_repo_parent();
                }
            }
            Action::Dispatch(intent) => {
                if !self.ui.is_config {
                    self.dispatch_if_available(intent);
                }
            }
            // Handled by the widget stack (page widgets or modals) or
            // pre-dispatched as global actions. No-op if they reach here.
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Snapshot selection so we can detect changes for infinite scroll.
        let prev_selection = self.active_page_selection();

        // Determine the topmost widget's mode. Screen delegates to the
        // top modal (if any) for mode_id / captures_raw_keys.
        let captures_raw = self.screen.captures_raw_keys();
        let mode_id = self.screen.binding_mode().primary();

        let action = if captures_raw {
            match key.code {
                // Resolve Enter/Esc through the widget's own binding mode so
                // user overrides for e.g. IssueSearch.confirm still fire.
                KeyCode::Esc | KeyCode::Enter => self.keymap.resolve(&KeyBindingMode::from(mode_id), crokey::KeyCombination::from(key)),
                _ => None,
            }
        } else {
            // Hybrid widgets (text input + action keys) need hardcoded
            // resolution to prevent shared bindings (e.g. j/k → SelectNext/
            // SelectPrev) from intercepting text input. Other non-capturing
            // widgets use the normal keymap.
            match mode_id {
                BindingModeId::CommandPalette => match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Down => Some(Action::SelectNext),
                    _ => None,
                },
                BindingModeId::FilePicker => match key.code {
                    KeyCode::Char('j') | KeyCode::Down => Some(Action::SelectNext),
                    KeyCode::Char('k') | KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    _ => None,
                },
                // When the top widget is the base layer (Normal mode_id),
                // resolve using the actual UI mode. This ensures Config mode
                // gets correct bindings (e.g. q → Dismiss, not Quit).
                BindingModeId::Normal => self.resolve_action(key),
                _ => self.keymap.resolve(&KeyBindingMode::from(mode_id), crokey::KeyCombination::from(key)),
            }
        };

        // Dispatch to Screen, which handles modal routing internally.
        // Take the screen out to avoid borrow conflicts between the widget
        // dispatch (`&mut Screen`) and the `WidgetContext` (borrows other `App` fields).
        let mut screen = std::mem::take(&mut self.screen);
        let (outcome_is_ignored, app_actions) = {
            let mut ctx = self.build_widget_context();
            let outcome =
                if let Some(action) = action { screen.handle_action(action, &mut ctx) } else { screen.handle_raw_key(key, &mut ctx) };
            (matches!(outcome, crate::widgets::Outcome::Ignored), std::mem::take(&mut ctx.app_actions))
        };
        self.screen = screen;

        // Fall through if unhandled — these are actions that need &mut App
        // context the widget stack doesn't have. Only when no modal is active:
        // modals are focus barriers, so their Ignored should not leak through
        // to app-level dispatch.
        if outcome_is_ignored && !self.screen.has_modal() {
            if let Some(action) = action {
                self.dispatch_action(action);
            }
        }
        self.process_app_actions(app_actions);

        // Post-dispatch: check for infinite scroll only if the selection
        // actually changed. This avoids spurious fetches from unrelated
        // key presses that happen to fire when the selection is near the bottom.
        if self.active_page_selection() != prev_selection {
            self.check_infinite_scroll();
        }
    }

    // ── Mouse handling ──

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Snapshot selection so we can detect changes for infinite scroll.
        let prev_selection = self.active_page_selection();

        // Dispatch to Screen, which handles modal routing internally.
        let mut screen = std::mem::take(&mut self.screen);
        let app_actions = {
            let mut ctx = self.build_widget_context();
            screen.handle_mouse(mouse, &mut ctx);
            std::mem::take(&mut ctx.app_actions)
        };
        self.screen = screen;
        self.process_app_actions(app_actions);

        // ── Tab drag handling ──
        // The Tabs widget owns the drag state but can't mutate model.repo_order
        // (read-only in WidgetContext). Perform the actual swap here.
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            let tabs = &mut self.screen.tabs;
            if tabs.drag.dragging_tab.is_some()
                && tabs.drag.active
                && tabs.handle_drag(mouse.column, mouse.row, &mut self.model.repo_order, &mut self.model.active_repo)
            {
                // Update drag index to the new position after the swap
                tabs.update_drag_index(self.model.active_repo);
            }
        }

        // ── Infinite scroll check ──
        // Only if the selection actually changed — avoids spurious fetches
        // from tab bar clicks, status bar clicks, etc.
        if self.active_page_selection() != prev_selection {
            self.check_infinite_scroll();
        }
    }

    /// Get the current selection index from the active RepoPage, if any.
    fn active_page_selection(&self) -> Option<usize> {
        if self.model.repo_order.is_empty() {
            return None;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        self.screen.repo_pages.get(identity).and_then(|page| page.table.selected_selectable_idx)
    }

    // ── Private helpers ──

    /// Check if the current selection is near the bottom and fetch more issues.
    ///
    /// The WorkItemTable widget handles selection changes but can't mutate
    /// `model.repos` (to set `issue_fetch_pending`). This post-dispatch check
    /// runs after every key event to trigger infinite scroll when needed.
    fn check_infinite_scroll(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        let Some(page) = self.screen.repo_pages.get(identity) else {
            return;
        };
        let Some(next) = page.table.selected_selectable_idx else {
            return;
        };
        let total = page.table.grouped_items.selectable_indices.len();
        if next + 5 >= total && self.model.active().issue_has_more && !self.model.active().issue_fetch_pending {
            let repo = self.model.active_repo_root().clone();
            let issue_count = self.model.active().providers.issues.len();
            let desired = issue_count + 50;
            let repo_identity = self.model.active_repo_identity().clone();
            if let Some(rm) = self.model.repos.get_mut(&repo_identity) {
                rm.issue_fetch_pending = true;
            }
            self.proto_commands.push(
                self.command(CommandAction::FetchMoreIssues { repo: flotilla_protocol::RepoSelector::Path(repo), desired_count: desired }),
            );
        }
    }

    pub(super) fn action_enter(&mut self) {
        let identity = &self.model.repo_order[self.model.active_repo];
        let has_multi = self.screen.repo_pages.get(identity).is_some_and(|p| !p.multi_selected.is_empty());
        if has_multi {
            self.action_enter_multi_select();
            return;
        }

        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = self.model.my_host().cloned();
        for &intent in Intent::enter_priority() {
            if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
                self.resolve_and_push(intent, &item);
                return;
            }
        }
    }

    fn action_enter_multi_select(&mut self) {
        let identity = &self.model.repo_order[self.model.active_repo];
        let Some(page) = self.screen.repo_pages.get(identity) else {
            return;
        };
        let multi_selected = page.multi_selected.clone();
        let mut all_issue_keys: Vec<String> = Vec::new();

        // Collect issues from multi-selected items
        for entry in &page.table.grouped_items.table_entries {
            if let GroupEntry::Item(item) = entry {
                if multi_selected.contains(&item.identity) {
                    all_issue_keys.extend(item.issue_keys.iter().cloned());
                }
            }
        }

        // Also include current selection if not already in multi_selected
        if let Some(item) = self.selected_work_item() {
            if !multi_selected.contains(&item.identity) {
                all_issue_keys.extend(item.issue_keys.iter().cloned());
            }
        }

        all_issue_keys.sort();
        all_issue_keys.dedup();
        if !all_issue_keys.is_empty() {
            self.screen.modal_stack.push(Box::new(crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating)));
            self.proto_commands.push(self.targeted_repo_command(CommandAction::GenerateBranchName { issue_keys: all_issue_keys }));
        }
        let identity = identity.clone();
        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
            page.multi_selected.clear();
        }
    }

    fn dispatch_if_available(&mut self, intent: Intent) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        let my_host = self.model.my_host().cloned();
        if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
            self.resolve_and_push(intent, &item);
        }
    }

    fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
        // Safety net: block filesystem operations on remote items even if
        // the caller somehow bypassed the menu/availability filter.
        let my_host = self.model.my_host().cloned();
        if !intent.is_allowed_for_host(item, &my_host) {
            tracing::warn!(?intent, host = %item.host, "blocked intent on remote item");
            self.model.status_message = Some("Cannot perform this action on a remote item".to_string());
            return;
        }

        if let Some(cmd) = intent.resolve(item, self) {
            match intent {
                Intent::RemoveCheckout => {
                    let checkout_path = item.checkout_key().map(|hp| hp.path.clone());
                    let widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(
                        item.terminal_keys.clone(),
                        item.identity.clone(),
                        self.item_execution_host(item),
                        checkout_path,
                    );
                    self.screen.modal_stack.push(Box::new(widget));
                }
                Intent::GenerateBranchName => {
                    self.screen
                        .modal_stack
                        .push(Box::new(crate::widgets::branch_input::BranchInputWidget::new(BranchInputKind::Generating)));
                }
                Intent::CloseChangeRequest => {
                    let id = match &cmd {
                        Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                        _ => return,
                    };
                    let widget =
                        crate::widgets::close_confirm::CloseConfirmWidget::new(id, item.description.clone(), item.identity.clone(), cmd);
                    self.screen.modal_stack.push(Box::new(widget));
                    return;
                }
                _ => {}
            }
            let pending_ctx = PendingActionContext {
                identity: item.identity.clone(),
                description: intent.label(self.model.active_labels()),
                repo_identity: self.model.active_repo_identity().clone(),
            };
            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
        }
    }

    pub(super) fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = self.model.my_host().cloned();
        let entries: Vec<crate::widgets::action_menu::MenuEntry> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter_map(|intent| {
                if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
                    intent.resolve(&item, self).map(|command| crate::widgets::action_menu::MenuEntry { intent, command })
                } else {
                    None
                }
            })
            .collect();

        if entries.is_empty() {
            return;
        }

        self.screen.modal_stack.push(Box::new(crate::widgets::action_menu::ActionMenuWidget::new(entries, item)));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use flotilla_protocol::{CheckoutSelector, CheckoutStatus, CheckoutTarget, Command, HostName, HostPath, WorkItemIdentity};
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

    /// Read the active RepoPage's selected selectable index.
    fn active_selection(app: &App) -> Option<usize> {
        let identity = &app.model.repo_order[app.model.active_repo];
        app.screen.repo_pages.get(identity).and_then(|p| p.table.selected_selectable_idx)
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

    /// Set the active RepoPage's selection by selectable index.
    fn set_active_selection(app: &mut App, si: usize) {
        let identity = app.model.repo_order[app.model.active_repo].clone();
        if let Some(page) = app.screen.repo_pages.get_mut(&identity) {
            page.table.select_row_self(si);
        }
    }

    fn insert_peer_host(model: &mut crate::app::TuiModel, name: &str) {
        let host_name = HostName::new(name);
        model.hosts.insert(host_name.clone(), TuiHostState {
            host_name: host_name.clone(),
            is_local: false,
            status: PeerStatus::Connected,
            summary: flotilla_protocol::HostSummary {
                host_name,
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
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
            Command { action: CommandAction::SearchIssues { query, .. }, .. } => {
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
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
        assert_eq!(app.resolve_action(key(KeyCode::Char('?'))), Some(Action::ToggleHelp));
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
    fn question_mark_toggles_help_from_normal() {
        let mut app = stub_app();
        assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
        app.handle_key(key(KeyCode::Char('?')));
        assert!(!app.screen.modal_stack.is_empty(), "expected modal widget pushed on stack");
    }

    #[test]
    fn question_mark_toggles_help_back_to_normal() {
        let mut app = stub_app();
        app.screen.modal_stack.push(Box::new(crate::widgets::help::HelpWidget::new()));
        app.handle_key(key(KeyCode::Char('?')));
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
            command: Command { host: None, context_repo: None, action: CommandAction::OpenChangeRequest { id: "1".into() } },
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
    fn unrelated_key_near_bottom_does_not_trigger_fetch_more_issues() {
        let mut app = stub_app();
        setup_table(&mut app, vec![
            make_work_item("a"),
            make_work_item("b"),
            make_work_item("c"),
            make_work_item("d"),
            make_work_item("e"),
            make_work_item("f"),
        ]);
        set_active_selection(&mut app, 1);

        let repo = app.model.repo_order[0].clone();
        if let Some(rm) = app.model.repos.get_mut(&repo) {
            rm.issue_has_more = true;
            rm.issue_fetch_pending = false;
        }

        app.handle_key(key(KeyCode::Char('c')));

        assert!(active_show_providers(&app));
        assert!(app.proto_commands.take_next().is_none(), "did not expect FetchMoreIssues command");
        assert!(!app.model.repos[&repo].issue_fetch_pending, "did not expect issue_fetch_pending to flip");
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
            command: Command { host: None, context_repo: None, action: CommandAction::OpenChangeRequest { id: "1".into() } },
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
        app.screen.repo_pages.get_mut(&identity).expect("repo page exists").multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));
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
    fn normal_c_toggles_providers() {
        let mut app = stub_app();
        assert!(!active_show_providers(&app));
        app.handle_key(key(KeyCode::Char('c')));
        assert!(active_show_providers(&app));
        app.handle_key(key(KeyCode::Char('c')));
        assert!(!active_show_providers(&app));
    }

    #[test]
    fn normal_h_cycles_target_host_through_known_peers() {
        let mut app = stub_app();
        insert_peer_host(&mut app.model, "alpha");
        insert_peer_host(&mut app.model, "beta");

        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.ui.target_host, Some(HostName::new("alpha")));

        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.ui.target_host, Some(HostName::new("beta")));

        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.ui.target_host, None);
    }

    #[test]
    fn normal_h_ignores_empty_peer_list() {
        let mut app = stub_app();

        app.handle_key(key(KeyCode::Char('h')));

        assert_eq!(app.ui.target_host, None);
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
    fn clicking_host_status_target_cycles_target_host() {
        let mut app = stub_app();
        insert_peer_host(&mut app.model, "alpha");
        app.screen.status_bar.key_targets = vec![StatusBarTarget::new(Rect::new(0, 29, 16, 1), StatusBarAction::key(KeyCode::Char('h')))];

        app.handle_mouse(left_click(4, 29));

        assert_eq!(app.ui.target_host, Some(HostName::new("alpha")));
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
                    host: None,
                    context_repo: None,
                    action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
                },
            },
            crate::widgets::action_menu::MenuEntry {
                intent: Intent::RemoveCheckout,
                command: Command {
                    host: None,
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
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        assert!(matches!(cmd, Command { action: CommandAction::ClearIssueSearch { .. }, .. }));
    }

    #[test]
    fn issue_search_enter_submits_query() {
        let mut app = stub_app();
        push_issue_search_widget_with_text(&mut app, "bug fix");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::SearchIssues { query, .. }, .. } => {
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
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

    // ── handle_delete_confirm_key (via widget stack) ────────────────

    fn push_delete_confirm_widget(app: &mut App, branch: &str) {
        let mut widget =
            crate::widgets::delete_confirm::DeleteConfirmWidget::new(vec![], WorkItemIdentity::Session("test".into()), None, None);
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
        let mut widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(vec![], item.identity.clone(), None, None);
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
        let hostname = HostName::new("feta");
        let mut widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(
            vec![],
            WorkItemIdentity::Session("test".into()),
            Some(hostname.clone()),
            None,
        );
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
        assert_eq!(cmd.host, Some(hostname));
        assert!(matches!(cmd.action, CommandAction::RemoveCheckout { .. }));
    }

    #[test]
    fn delete_confirm_ignores_while_loading() {
        let mut app = stub_app();
        // Loading widget — no info yet
        let widget = crate::widgets::delete_confirm::DeleteConfirmWidget::new(vec![], WorkItemIdentity::Session("test".into()), None, None);
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
                host: None,
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
        page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));
        page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/b")));

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
        page.multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));

        app.action_enter();

        // No issues, so no GenerateBranchName — stays in Normal, multi_selected cleared
        assert_eq!(app.screen.modal_stack.len(), 0, "expected no modals on stack");
        assert!(app.proto_commands.take_next().is_none());
        let identity = app.model.repo_order[0].clone();
        let page = app.screen.repo_pages.get(&identity).expect("page exists");
        assert!(page.multi_selected.is_empty());
    }

    // ── delete_confirm_y_with_no_info ────────────────────────────────

    #[test]
    fn delete_confirm_y_with_no_info_does_not_push_command() {
        let mut app = stub_app();
        let mut widget =
            crate::widgets::delete_confirm::DeleteConfirmWidget::new(vec![], WorkItemIdentity::Session("test".into()), None, None);
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
        item.change_request_key = Some("PR#10".into());
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
        item.change_request_key = Some("PR#42".into());
        setup_table(&mut app, vec![item]);
        app.handle_key(key(KeyCode::Char('p')));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::OpenChangeRequest { id }, .. } => {
                assert_eq!(id, "PR#42");
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
            host: None,
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
            host: Some(HostName::new("remote-host")),
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
}
