use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::GroupEntry;
use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, WorkItem};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{ui_state::PendingActionContext, App, BranchInputKind, ClearDispatch, Intent, UiMode};
use crate::status_bar::StatusBarAction;

impl App {
    // ── Key handling ──

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('K')
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(
                self.ui.mode,
                UiMode::BranchInput { kind: BranchInputKind::Manual, .. } | UiMode::IssueSearch { .. } | UiMode::FilePicker { .. }
            )
        {
            self.ui.status_bar.show_keys = !self.ui.status_bar.show_keys;
            return;
        }

        // Toggle help from Normal or Help modes
        if key.code == KeyCode::Char('?') {
            match self.ui.mode {
                UiMode::Normal => {
                    self.ui.mode = UiMode::Help;
                    return;
                }
                UiMode::Help => {
                    self.ui.mode = UiMode::Normal;
                    self.ui.help_scroll = 0;
                    return;
                }
                _ => {}
            }
        }

        match self.ui.mode {
            UiMode::Help => match key.code {
                KeyCode::Esc => {
                    self.ui.mode = UiMode::Normal;
                    self.ui.help_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.ui.help_scroll = self.ui.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.ui.help_scroll = self.ui.help_scroll.saturating_sub(1);
                }
                _ => {}
            },
            UiMode::DeleteConfirm { .. } => self.handle_delete_confirm_key(key),
            UiMode::CloseConfirm { .. } => self.handle_close_confirm_key(key),
            UiMode::ActionMenu { .. } => self.handle_menu_key(key),
            UiMode::FilePicker { .. } => self.handle_file_picker_key(key),
            UiMode::BranchInput { .. } => self.handle_branch_input_key(key),
            UiMode::IssueSearch { .. } => self.handle_issue_search_key(key),
            UiMode::Config => self.handle_config_key(key),
            UiMode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_config_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(sel) = self.ui.event_log.selected {
                    if sel + 1 < self.ui.event_log.count {
                        self.ui.event_log.selected = Some(sel + 1);
                    }
                } else if self.ui.event_log.count > 0 {
                    self.ui.event_log.selected = Some(self.ui.event_log.count - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(sel) = self.ui.event_log.selected {
                    if sel > 0 {
                        self.ui.event_log.selected = Some(sel - 1);
                    }
                }
            }
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                // Cancellation takes priority over other Esc actions while a command is running.
                if let Some(&command_id) = self.in_flight.keys().next() {
                    self.pending_cancel = Some(command_id);
                } else if self.active_ui().active_search_query.is_some() {
                    self.clear_active_issue_search(ClearDispatch::OnlyIfActive);
                } else if self.active_ui().show_providers {
                    self.active_ui_mut().show_providers = false;
                } else if !self.active_ui().multi_selected.is_empty() {
                    self.active_ui_mut().multi_selected.clear();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('r') => {} // refresh handled in main loop
            KeyCode::Char(' ') => self.toggle_multi_select(),
            KeyCode::Char('l') => {
                self.ui.cycle_layout();
                self.persist_layout();
            }
            KeyCode::Char('.') => self.open_action_menu(),
            KeyCode::Enter => self.action_enter(),
            KeyCode::Char('n') => self.enter_branch_input(BranchInputKind::Manual),
            KeyCode::Char('d') => self.dispatch_if_available(Intent::RemoveCheckout),
            KeyCode::Char('D') => self.ui.show_debug = !self.ui.show_debug,
            KeyCode::Char('p') => self.dispatch_if_available(Intent::OpenChangeRequest),
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            KeyCode::Char('{') => {
                if !self.ui.mode.is_config() && self.move_tab(-1) {
                    self.config.save_tab_order(&self.model.repo_order);
                }
            }
            KeyCode::Char('}') => {
                if !self.ui.mode.is_config() && self.move_tab(1) {
                    self.config.save_tab_order(&self.model.repo_order);
                }
            }
            KeyCode::Char('/') => {
                self.ui.mode = UiMode::IssueSearch { input: Input::default() };
            }
            KeyCode::Char('c') => {
                let sp = self.active_ui().show_providers;
                self.active_ui_mut().show_providers = !sp;
            }
            KeyCode::Char('a') => self.open_file_picker_from_active_repo_parent(),
            _ => {}
        }
    }

    // ── Mouse handling ──

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.handle_status_bar_mouse(mouse) {
            return;
        }

        match self.ui.mode {
            UiMode::ActionMenu { .. } => {
                self.handle_menu_mouse(mouse);
                return;
            }
            UiMode::FilePicker { .. } => {
                self.handle_file_picker_mouse(mouse);
                return;
            }
            UiMode::Help
            | UiMode::DeleteConfirm { .. }
            | UiMode::CloseConfirm { .. }
            | UiMode::BranchInput { .. }
            | UiMode::IssueSearch { .. } => {
                return;
            }
            UiMode::Config | UiMode::Normal => {}
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self.ui.double_click.last_time.map(|t| now.duration_since(t).as_millis() < 400).unwrap_or(false)
                        && self.ui.double_click.last_selectable_idx == Some(si);

                    let table_idx = self.active_ui().table_view.selectable_indices[si];
                    self.active_ui_mut().selected_selectable_idx = Some(si);
                    self.active_ui_mut().table_state.select(Some(table_idx));

                    if is_double_click {
                        self.action_enter();
                        self.ui.double_click.last_time = None;
                        self.ui.double_click.last_selectable_idx = None;
                    } else {
                        self.ui.double_click.last_time = Some(now);
                        self.ui.double_click.last_selectable_idx = Some(si);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                    let table_idx = self.active_ui().table_view.selectable_indices[si];
                    self.active_ui_mut().selected_selectable_idx = Some(si);
                    self.active_ui_mut().table_state.select(Some(table_idx));
                    self.open_action_menu();
                }
            }
            MouseEventKind::ScrollDown => self.select_next(),
            MouseEventKind::ScrollUp => self.select_prev(),
            _ => {}
        }
    }

    // ── Private helpers ──

    fn handle_status_bar_mouse(&mut self, mouse: MouseEvent) -> bool {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }

        for target in &self.ui.layout.status_bar.dismiss_targets {
            if target.contains(mouse.column, mouse.row) {
                if let StatusBarAction::ClearError(id) = target.action {
                    self.dismiss_status_item(id);
                    return true;
                }
            }
        }

        for target in &self.ui.layout.status_bar.key_targets {
            if target.contains(mouse.column, mouse.row) {
                self.dispatch_status_bar_action(target.action.clone());
                return true;
            }
        }

        false
    }

    fn dispatch_status_bar_action(&mut self, action: StatusBarAction) {
        match action {
            StatusBarAction::KeyPress { code, modifiers } => self.handle_key(KeyEvent::new(code, modifiers)),
            StatusBarAction::ClearError(id) => self.dismiss_status_item(id),
        }
    }

    fn handle_menu_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let x = mouse.column;
        let y = mouse.row;
        let a = self.ui.layout.menu_area;
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.ui.mode = UiMode::Normal;
            return;
        }
        let row = (y - a.y) as usize;
        if row < 1 {
            return;
        }
        let item_idx = row - 1;
        let menu_len = if let UiMode::ActionMenu { ref items, .. } = self.ui.mode {
            items.len()
        } else {
            return;
        };
        if item_idx < menu_len {
            if let UiMode::ActionMenu { ref mut index, .. } = self.ui.mode {
                *index = item_idx;
            }
            self.execute_menu_action();
            if matches!(self.ui.mode, UiMode::ActionMenu { .. }) {
                self.ui.mode = UiMode::Normal;
            }
        }
    }

    fn action_enter(&mut self) {
        if !self.active_ui().multi_selected.is_empty() {
            self.action_enter_multi_select();
            return;
        }

        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = &self.model.my_host;
        for &intent in Intent::enter_priority() {
            if intent.is_available(&item) && intent.is_allowed_for_host(&item, my_host) {
                self.resolve_and_push(intent, &item);
                return;
            }
        }
    }

    fn action_enter_multi_select(&mut self) {
        let multi_selected = self.active_ui().multi_selected.clone();
        let mut all_issue_keys: Vec<String> = Vec::new();

        // Collect issues from multi-selected items
        for entry in &self.active_ui().table_view.table_entries {
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
            self.enter_branch_input(BranchInputKind::Generating);
            self.proto_commands.push(self.repo_command(CommandAction::GenerateBranchName { issue_keys: all_issue_keys }));
        }
        self.active_ui_mut().multi_selected.clear();
    }

    fn dispatch_if_available(&mut self, intent: Intent) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        if intent.is_available(&item) && intent.is_allowed_for_host(&item, &self.model.my_host) {
            self.resolve_and_push(intent, &item);
        }
    }

    fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
        // Safety net: block filesystem operations on remote items even if
        // the caller somehow bypassed the menu/availability filter.
        if !intent.is_allowed_for_host(item, &self.model.my_host) {
            tracing::warn!(?intent, host = %item.host, "blocked intent on remote item");
            self.model.status_message = Some("Cannot perform this action on a remote item".to_string());
            return;
        }

        if let Some(cmd) = intent.resolve(item, self) {
            match intent {
                Intent::RemoveCheckout => {
                    self.ui.mode = UiMode::DeleteConfirm {
                        info: None,
                        loading: true,
                        terminal_keys: item.terminal_keys.clone(),
                        identity: item.identity.clone(),
                    };
                }
                Intent::GenerateBranchName => {
                    self.enter_branch_input(BranchInputKind::Generating);
                }
                Intent::CloseChangeRequest => {
                    // CloseChangeRequest doesn't push now — the confirm handler
                    // creates its own PendingActionContext when the user confirms.
                    self.ui.mode = UiMode::CloseConfirm {
                        id: match &cmd {
                            Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                            _ => return,
                        },
                        title: item.description.clone(),
                        identity: item.identity.clone(),
                    };
                    return;
                }
                _ => {}
            }
            let pending_ctx = PendingActionContext {
                identity: item.identity.clone(),
                description: intent.label(self.model.active_labels()),
                repo_path: self.model.active_repo_root().clone(),
            };
            self.proto_commands.push_with_context(cmd, Some(pending_ctx));
        }
    }

    fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = &self.model.my_host;
        let items: Vec<Intent> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter(|a| a.is_available(&item) && a.is_allowed_for_host(&item, my_host) && a.resolve(&item, self).is_some())
            .collect();

        if items.is_empty() {
            return;
        }

        self.ui.mode = UiMode::ActionMenu { items, index: 0 };
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            self.execute_menu_action();
            // Only reset to Normal if the action didn't set a different mode
            // (e.g. DeleteConfirm, BranchInput)
            if matches!(self.ui.mode, UiMode::ActionMenu { .. }) {
                self.ui.mode = UiMode::Normal;
            }
            return;
        }
        let UiMode::ActionMenu { ref items, ref mut index } = self.ui.mode else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if *index < items.len().saturating_sub(1) {
                    *index += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                *index = index.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn handle_branch_input_key(&mut self, key: KeyEvent) {
        // Ignore keys while generating
        if matches!(self.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }) {
            return;
        }

        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            let (branch, issue_ids) = if let UiMode::BranchInput { ref input, ref mut pending_issue_ids, .. } = self.ui.mode {
                (input.value().to_string(), std::mem::take(pending_issue_ids))
            } else {
                return;
            };
            if !branch.is_empty() {
                self.proto_commands.push(self.command(CommandAction::Checkout {
                    repo: flotilla_protocol::RepoSelector::Path(self.model.active_repo_root().clone()),
                    target: CheckoutTarget::FreshBranch(branch),
                    issue_ids,
                }));
            }
            self.ui.mode = UiMode::Normal;
            return;
        }
        if let UiMode::BranchInput { ref mut input, .. } = self.ui.mode {
            input.handle_event(&crossterm::event::Event::Key(key));
        }
    }

    fn handle_issue_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.clear_active_issue_search(ClearDispatch::Always);
                self.ui.mode = UiMode::Normal;
            }
            KeyCode::Enter => {
                let query = if let UiMode::IssueSearch { ref input } = self.ui.mode {
                    input.value().to_string()
                } else {
                    return;
                };
                if !query.is_empty() {
                    let repo = self.model.active_repo_root().clone();
                    self.proto_commands.push(self.command(CommandAction::SearchIssues { repo, query: query.clone() }));
                    self.active_ui_mut().active_search_query = Some(query);
                }
                self.ui.mode = UiMode::Normal;
            }
            _ => {
                if let UiMode::IssueSearch { ref mut input } = self.ui.mode {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
            }
        }
    }

    fn handle_delete_confirm_key(&mut self, key: KeyEvent) {
        let loading = matches!(self.ui.mode, UiMode::DeleteConfirm { loading: true, .. });
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if !loading {
                    // Extract branch from CheckoutStatus and send RemoveCheckout
                    if let UiMode::DeleteConfirm { info: Some(ref info), ref terminal_keys, ref identity, .. } = self.ui.mode {
                        let ctx = PendingActionContext {
                            identity: identity.clone(),
                            description: format!("Remove {}", info.branch),
                            repo_path: self.model.active_repo_root().clone(),
                        };
                        self.proto_commands.push_with_context(
                            self.command(CommandAction::RemoveCheckout {
                                checkout: CheckoutSelector::Query(info.branch.clone()),
                                terminal_keys: terminal_keys.clone(),
                            }),
                            Some(ctx),
                        );
                    }
                    self.ui.mode = UiMode::Normal;
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.ui.mode = UiMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_close_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let UiMode::CloseConfirm { ref id, ref identity, .. } = self.ui.mode {
                    let ctx = PendingActionContext {
                        identity: identity.clone(),
                        description: format!("Close {}", id),
                        repo_path: self.model.active_repo_root().clone(),
                    };
                    self.proto_commands
                        .push_with_context(self.repo_command(CommandAction::CloseChangeRequest { id: id.clone() }), Some(ctx));
                }
                self.ui.mode = UiMode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.ui.mode = UiMode::Normal;
            }
            _ => {}
        }
    }

    fn execute_menu_action(&mut self) {
        let (intent, item) = {
            let UiMode::ActionMenu { ref items, index } = self.ui.mode else {
                return;
            };
            let Some(&intent) = items.get(index) else {
                return;
            };
            let Some(item) = self.selected_work_item().cloned() else {
                return;
            };
            (intent, item)
        };
        self.resolve_and_push(intent, &item);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use flotilla_protocol::{CheckoutStatus, Command, HostName, HostPath, WorkItemIdentity};
    use ratatui::layout::Rect;

    use super::{super::RepoViewLayout, *};
    use crate::{
        app::test_support::{checkout_item, key, setup_selectable_table as setup_table, stub_app},
        status_bar::{StatusBarAction, StatusBarTarget},
    };

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::local(), PathBuf::from(path))
    }
    use tui_input::Input;

    fn make_work_item(id: &str) -> flotilla_protocol::WorkItem {
        checkout_item(&format!("feat/{id}"), &format!("/tmp/{id}"), false)
    }

    fn delete_confirm_mode(branch: &str) -> UiMode {
        UiMode::DeleteConfirm {
            info: Some(CheckoutStatus {
                branch: branch.into(),
                change_request_status: None,
                merge_commit_sha: None,
                unpushed_commits: vec![],
                has_uncommitted: false,
                uncommitted_files: vec![],
                base_detection_warning: None,
            }),
            loading: false,
            terminal_keys: vec![],
            identity: WorkItemIdentity::Session("test".into()),
        }
    }

    fn left_click(x: u16, y: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
    }

    // ── handle_key — top-level dispatch ──────────────────────────────

    #[test]
    fn question_mark_toggles_help_from_normal() {
        let mut app = stub_app();
        assert!(matches!(app.ui.mode, UiMode::Normal));
        app.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(app.ui.mode, UiMode::Help));
    }

    #[test]
    fn question_mark_toggles_help_back_to_normal() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Help;
        app.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn question_mark_in_other_modes_does_not_toggle() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest], index: 0 };
        app.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(app.ui.mode, UiMode::ActionMenu { .. }));
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
        app.ui.mode = UiMode::Help;
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    // ── handle_config_key ────────────────────────────────────────────

    #[test]
    fn config_q_quits() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn config_esc_quits() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn config_j_navigates_event_log_down() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 5;
        app.ui.event_log.selected = Some(0);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.ui.event_log.selected, Some(1));
    }

    #[test]
    fn config_k_navigates_event_log_up() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 5;
        app.ui.event_log.selected = Some(3);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.ui.event_log.selected, Some(2));
    }

    #[test]
    fn config_j_when_no_selection_jumps_to_last() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 5;
        app.ui.event_log.selected = None;
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.ui.event_log.selected, Some(4));
    }

    #[test]
    fn config_j_at_end_stays() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 3;
        app.ui.event_log.selected = Some(2);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.ui.event_log.selected, Some(2));
    }

    #[test]
    fn config_k_at_zero_stays() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 5;
        app.ui.event_log.selected = Some(0);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.ui.event_log.selected, Some(0));
    }

    #[test]
    fn config_bracket_switches_tabs() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        // ] in Config mode should switch to Normal mode + first repo
        app.handle_key(key(KeyCode::Char(']')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.model.active_repo, 0);

        // [ from first repo (index 0) goes back to Config
        app.handle_key(key(KeyCode::Char('[')));
        assert!(matches!(app.ui.mode, UiMode::Config));
    }

    // ── handle_normal_key ────────────────────────────────────────────

    #[test]
    fn normal_q_quits() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn normal_esc_clears_providers_first() {
        let mut app = stub_app();
        app.active_ui_mut().show_providers = true;
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.active_ui().show_providers);
        assert!(!app.should_quit);
    }

    #[test]
    fn normal_esc_clears_multi_select_second() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a")]);
        app.active_ui_mut().multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));
        assert!(!app.active_ui().multi_selected.is_empty());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.active_ui().multi_selected.is_empty());
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
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Manual, .. }));
    }

    #[test]
    fn normal_d_dispatches_remove_checkout() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a")]);
        app.handle_key(key(KeyCode::Char('d')));
        // RemoveCheckout resolves to FetchCheckoutStatus, then sets DeleteConfirm mode
        assert!(matches!(app.ui.mode, UiMode::DeleteConfirm { loading: true, .. }));
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
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
    fn normal_slash_enters_issue_search() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn normal_c_toggles_providers() {
        let mut app = stub_app();
        assert!(!app.active_ui().show_providers);
        app.handle_key(key(KeyCode::Char('c')));
        assert!(app.active_ui().show_providers);
        app.handle_key(key(KeyCode::Char('c')));
        assert!(!app.active_ui().show_providers);
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
        assert!(matches!(app.ui.mode, UiMode::ActionMenu { .. }));
    }

    #[test]
    fn clicking_search_status_target_enters_issue_search_mode() {
        let mut app = stub_app();
        app.ui.layout.status_bar.key_targets =
            vec![StatusBarTarget::new(Rect::new(10, 29, 12, 1), StatusBarAction::key(KeyCode::Char('/')))];

        app.handle_mouse(left_click(12, 29));

        assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn clicking_layout_status_cycles_layout() {
        let mut app = stub_app();
        assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);
        app.ui.layout.status_bar.key_targets =
            vec![StatusBarTarget::new(Rect::new(0, 29, 12, 1), StatusBarAction::key(KeyCode::Char('l')))];

        app.handle_mouse(left_click(4, 29));

        assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
    }

    #[test]
    fn clicking_dismiss_status_target_hides_visible_error() {
        let mut app = stub_app();
        app.model.status_message = Some("boom".into());
        app.ui.layout.status_bar.dismiss_targets = vec![StatusBarTarget::new(Rect::new(20, 29, 1, 1), StatusBarAction::ClearError(0))];

        app.handle_mouse(left_click(20, 29));

        assert!(app.visible_status_items().is_empty());
    }

    // ── handle_menu_key ──────────────────────────────────────────────

    #[test]
    fn menu_esc_returns_to_normal() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 0 };
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn menu_j_advances_index() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 0 };
        app.handle_key(key(KeyCode::Char('j')));
        match app.ui.mode {
            UiMode::ActionMenu { index, .. } => assert_eq!(index, 1),
            _ => panic!("expected ActionMenu"),
        }
    }

    #[test]
    fn menu_k_decrements_index() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 1 };
        app.handle_key(key(KeyCode::Char('k')));
        match app.ui.mode {
            UiMode::ActionMenu { index, .. } => assert_eq!(index, 0),
            _ => panic!("expected ActionMenu"),
        }
    }

    #[test]
    fn menu_j_stays_at_end() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 1 };
        app.handle_key(key(KeyCode::Char('j')));
        match app.ui.mode {
            UiMode::ActionMenu { index, .. } => assert_eq!(index, 1),
            _ => panic!("expected ActionMenu"),
        }
    }

    #[test]
    fn menu_k_stays_at_zero() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 0 };
        app.handle_key(key(KeyCode::Char('k')));
        match app.ui.mode {
            UiMode::ActionMenu { index, .. } => assert_eq!(index, 0),
            _ => panic!("expected ActionMenu"),
        }
    }

    // ── handle_branch_input_key ──────────────────────────────────────

    #[test]
    fn branch_input_esc_returns_to_normal() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::from("my-branch"), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn branch_input_enter_creates_checkout() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::from("my-branch"), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
        app.ui.mode = UiMode::BranchInput {
            input: Input::from("feat/issue-42"),
            kind: BranchInputKind::Manual,
            pending_issue_ids: vec![("github".into(), "42".into())],
        };
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
        app.ui.mode = UiMode::BranchInput { input: Input::default(), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn branch_input_ignored_while_generating() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::from("partial"), kind: BranchInputKind::Generating, pending_issue_ids: vec![] };
        // Enter should be ignored
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
        assert!(app.proto_commands.take_next().is_none());

        // Esc should also be ignored
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
    }

    // ── handle_issue_search_key ──────────────────────────────────────

    #[test]
    fn issue_search_esc_clears_and_returns() {
        let mut app = stub_app();
        app.ui.mode = UiMode::IssueSearch { input: Input::from("some query") };
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::ClearIssueSearch { repo }, .. } => {
                assert_eq!(repo, PathBuf::from("/tmp/test-repo"));
            }
            other => panic!("expected ClearIssueSearch, got {:?}", other),
        }
    }

    #[test]
    fn issue_search_enter_submits_query() {
        let mut app = stub_app();
        app.ui.mode = UiMode::IssueSearch { input: Input::from("bug fix") };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::SearchIssues { repo, query }, .. } => {
                assert_eq!(repo, PathBuf::from("/tmp/test-repo"));
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
        }
    }

    #[test]
    fn issue_search_enter_empty_no_command() {
        let mut app = stub_app();
        app.ui.mode = UiMode::IssueSearch { input: Input::default() };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    // ── handle_delete_confirm_key ────────────────────────────────────

    #[test]
    fn delete_confirm_y_sends_remove_checkout() {
        let mut app = stub_app();
        app.ui.mode = delete_confirm_mode("feat/x");
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
        app.ui.mode = delete_confirm_mode("feat/y");
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
                assert_eq!(checkout, CheckoutSelector::Query("feat/y".into()));
            }
            other => panic!("expected RemoveCheckout, got {:?}", other),
        }
    }

    #[test]
    fn delete_confirm_ignores_while_loading() {
        let mut app = stub_app();
        app.ui.mode =
            UiMode::DeleteConfirm { info: None, loading: true, terminal_keys: vec![], identity: WorkItemIdentity::Session("test".into()) };
        app.handle_key(key(KeyCode::Char('y')));
        // Should still be in DeleteConfirm mode
        assert!(matches!(app.ui.mode, UiMode::DeleteConfirm { loading: true, .. }));
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn delete_confirm_esc_cancels() {
        let mut app = stub_app();
        app.ui.mode = delete_confirm_mode("feat/z");
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn delete_confirm_n_cancels() {
        let mut app = stub_app();
        app.ui.mode = delete_confirm_mode("feat/z");
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    // ── open_action_menu ─────────────────────────────────────────────

    #[test]
    fn open_action_menu_builds_filtered_list() {
        let mut app = stub_app();
        // A checkout item without workspace — CreateWorkspace + RemoveCheckout should be available
        let item = make_work_item("a");
        setup_table(&mut app, vec![item]);
        app.open_action_menu();
        match &app.ui.mode {
            UiMode::ActionMenu { items, index } => {
                assert_eq!(*index, 0);
                assert!(!items.is_empty());
                // CreateWorkspace should be available (checkout with no workspace)
                assert!(items.contains(&Intent::CreateWorkspace));
                // RemoveCheckout should be available (non-main checkout with branch)
                assert!(items.contains(&Intent::RemoveCheckout));
                // SwitchToWorkspace should NOT be available (no workspace_refs)
                assert!(!items.contains(&Intent::SwitchToWorkspace));
            }
            other => panic!("expected ActionMenu, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[test]
    fn open_action_menu_noop_when_no_selection() {
        let mut app = stub_app();
        // No items in table, no selection
        assert!(matches!(app.ui.mode, UiMode::Normal));
        app.open_action_menu();
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
            Command { action: CommandAction::CreateWorkspaceForCheckout { checkout_path }, .. } => {
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
    fn resolve_and_push_sets_delete_confirm_for_remove_checkout() {
        let mut app = stub_app();
        let item = make_work_item("a");
        app.resolve_and_push(Intent::RemoveCheckout, &item);
        assert!(matches!(app.ui.mode, UiMode::DeleteConfirm { loading: true, .. }));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        assert!(matches!(cmd, Command { action: CommandAction::FetchCheckoutStatus { .. }, .. }));
    }

    #[test]
    fn resolve_and_push_sets_branch_input_for_generate_branch_name() {
        let mut app = stub_app();
        let mut item = make_work_item("a");
        item.issue_keys = vec!["ISSUE-1".into()];
        app.resolve_and_push(Intent::GenerateBranchName, &item);
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
                assert_eq!(issue_keys, vec!["ISSUE-1".to_string()]);
            }
            other => panic!("expected GenerateBranchName, got {:?}", other),
        }
    }

    // ── execute_menu_action ──────────────────────────────────────────

    #[test]
    fn execute_menu_action_dispatches_selected_intent() {
        let mut app = stub_app();
        let item = make_work_item("a");
        setup_table(&mut app, vec![item]);
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::CreateWorkspace, Intent::RemoveCheckout], index: 0 };
        app.execute_menu_action();
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        assert!(matches!(cmd, Command { action: CommandAction::CreateWorkspaceForCheckout { .. }, .. }));
    }

    #[test]
    fn menu_enter_resets_to_normal_for_simple_actions() {
        let mut app = stub_app();
        let item = make_work_item("a");
        setup_table(&mut app, vec![item]);
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::CreateWorkspace], index: 0 };
        app.handle_key(key(KeyCode::Enter));
        // CreateWorkspace doesn't change the mode itself, so handle_menu_key resets to Normal
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn menu_enter_preserves_delete_confirm_mode() {
        let mut app = stub_app();
        let item = make_work_item("a");
        setup_table(&mut app, vec![item]);
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::RemoveCheckout], index: 0 };
        app.handle_key(key(KeyCode::Enter));
        // RemoveCheckout sets DeleteConfirm mode, which should be preserved
        assert!(matches!(app.ui.mode, UiMode::DeleteConfirm { loading: true, .. }));
    }

    // ── j/k navigation in normal mode ────────────────────────────────

    #[test]
    fn normal_j_selects_next() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a"), make_work_item("b"), make_work_item("c")]);
        assert_eq!(app.active_ui().selected_selectable_idx, Some(0));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.active_ui().selected_selectable_idx, Some(1));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.active_ui().selected_selectable_idx, Some(2));
    }

    #[test]
    fn normal_k_selects_prev() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);
        // Move to second item first
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.active_ui().selected_selectable_idx, Some(1));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.active_ui().selected_selectable_idx, Some(0));
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

        // Multi-select both items
        app.active_ui_mut().multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));
        app.active_ui_mut().multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/b")));

        app.action_enter();

        // Should set BranchInput with generating=true
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
        let (cmd, _) = app.proto_commands.take_next().unwrap();
        match cmd {
            Command { action: CommandAction::GenerateBranchName { issue_keys }, .. } => {
                assert!(issue_keys.contains(&"ISSUE-1".to_string()));
                assert!(issue_keys.contains(&"ISSUE-2".to_string()));
            }
            other => panic!("expected GenerateBranchName, got {:?}", other),
        }
        // Multi-select should be cleared
        assert!(app.active_ui().multi_selected.is_empty());
    }

    #[test]
    fn action_enter_multi_select_without_issues_clears() {
        let mut app = stub_app();
        let item_a = make_work_item("a"); // no issue_keys
        setup_table(&mut app, vec![item_a]);

        app.active_ui_mut().multi_selected.insert(WorkItemIdentity::Checkout(hp("/tmp/a")));

        app.action_enter();

        // No issues, so no GenerateBranchName — stays in Normal, multi_selected cleared
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
        assert!(app.active_ui().multi_selected.is_empty());
    }

    // ── delete_confirm_y_with_no_info ────────────────────────────────

    #[test]
    fn delete_confirm_y_with_no_info_does_not_push_command() {
        let mut app = stub_app();
        app.ui.mode =
            UiMode::DeleteConfirm { info: None, loading: false, terminal_keys: vec![], identity: WorkItemIdentity::Session("test".into()) };
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
        match &app.ui.mode {
            UiMode::ActionMenu { items, .. } => {
                assert!(items.contains(&Intent::OpenChangeRequest));
            }
            other => panic!("expected ActionMenu, got {:?}", std::mem::discriminant(other)),
        }
    }

    // ── space toggles multi-select ───────────────────────────────────

    #[test]
    fn space_toggles_multi_select() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a")]);
        assert!(app.active_ui().multi_selected.is_empty());
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(!app.active_ui().multi_selected.is_empty());
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.active_ui().multi_selected.is_empty());
    }

    #[test]
    fn l_cycles_layout_in_normal_mode() {
        let mut app = stub_app();
        assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.ui.view_layout, RepoViewLayout::Zoom);
        assert!(matches!(app.ui.mode, UiMode::Normal));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.ui.view_layout, RepoViewLayout::Right);
        assert!(matches!(app.ui.mode, UiMode::Normal));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
        assert!(matches!(app.ui.mode, UiMode::Normal));

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.ui.view_layout, RepoViewLayout::Auto);
        assert!(matches!(app.ui.mode, UiMode::Normal));
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
        // Set up CloseConfirm mode with the item's identity
        app.ui.mode = UiMode::CloseConfirm { id: "PR-1".into(), title: "test".into(), identity: item.identity.clone() };
        // Simulate pressing 'y' to confirm
        app.handle_key(key(KeyCode::Char('y')));
        let (_, ctx) = app.proto_commands.take_next().expect("should have command");
        let ctx = ctx.expect("should have pending context");
        assert_eq!(ctx.identity, item.identity);
    }
}
