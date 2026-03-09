use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use tui_input::backend::crossterm::EventHandler as InputEventHandler;
use tui_input::Input;

use flotilla_core::data::GroupEntry;
use flotilla_protocol::{Command, WorkItem};

use super::{App, Intent, UiMode};

impl App {
    // ── Key handling ──

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.model.status_message = None;

        // Toggle help from Normal or Help modes
        if key.code == KeyCode::Char('?') {
            match self.ui.mode {
                UiMode::Normal => {
                    self.ui.mode = UiMode::Help;
                    return;
                }
                UiMode::Help => {
                    self.ui.mode = UiMode::Normal;
                    return;
                }
                _ => {}
            }
        }

        match self.ui.mode {
            UiMode::Help => {
                if key.code == KeyCode::Esc {
                    self.ui.mode = UiMode::Normal;
                }
            }
            UiMode::DeleteConfirm { .. } => self.handle_delete_confirm_key(key),
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
                if self.active_ui().show_providers {
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
            KeyCode::Char('.') => self.open_action_menu(),
            KeyCode::Enter => self.action_enter(),
            KeyCode::Char('n') => {
                self.ui.mode = UiMode::BranchInput {
                    input: Input::default(),
                    generating: false,
                    pending_issue_ids: Vec::new(),
                };
            }
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
                self.ui.mode = UiMode::IssueSearch {
                    input: Input::default(),
                };
            }
            KeyCode::Char('c') => {
                let sp = self.active_ui().show_providers;
                self.active_ui_mut().show_providers = !sp;
            }
            KeyCode::Char('a') => {
                let mut input = Input::default();
                if let Some(parent) = self.model.active_repo_root().parent() {
                    let parent_str = format!("{}/", parent.display());
                    input = Input::from(parent_str.as_str());
                }
                self.ui.mode = UiMode::FilePicker {
                    input,
                    dir_entries: Vec::new(),
                    selected: 0,
                };
                self.refresh_dir_listing();
            }
            _ => {}
        }
    }

    // ── Mouse handling ──

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.model.status_message = None;
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
                    let is_double_click = self
                        .ui
                        .double_click
                        .last_time
                        .map(|t| now.duration_since(t).as_millis() < 400)
                        .unwrap_or(false)
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

        for &intent in Intent::enter_priority() {
            if intent.is_available(&item) {
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
            self.ui.mode = UiMode::BranchInput {
                input: Input::default(),
                generating: true,
                pending_issue_ids: Vec::new(),
            };
            self.proto_commands.push(Command::GenerateBranchName {
                issue_keys: all_issue_keys,
            });
        }
        self.active_ui_mut().multi_selected.clear();
    }

    fn dispatch_if_available(&mut self, intent: Intent) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        if intent.is_available(&item) {
            self.resolve_and_push(intent, &item);
        }
    }

    fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
        if let Some(cmd) = intent.resolve(item, self) {
            match intent {
                Intent::RemoveCheckout => {
                    self.ui.mode = UiMode::DeleteConfirm {
                        info: None,
                        loading: true,
                    };
                }
                Intent::GenerateBranchName => {
                    self.ui.mode = UiMode::BranchInput {
                        input: Input::default(),
                        generating: true,
                        pending_issue_ids: Vec::new(),
                    };
                }
                _ => {}
            }
            self.proto_commands.push(cmd);
        }
    }

    fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let items: Vec<Intent> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter(|a| a.is_available(&item) && a.resolve(&item, self).is_some())
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
        let UiMode::ActionMenu {
            ref items,
            ref mut index,
        } = self.ui.mode
        else {
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
        if matches!(
            self.ui.mode,
            UiMode::BranchInput {
                generating: true,
                ..
            }
        ) {
            return;
        }

        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            let (branch, issue_ids) = if let UiMode::BranchInput {
                ref input,
                ref mut pending_issue_ids,
                ..
            } = self.ui.mode
            {
                (input.value().to_string(), std::mem::take(pending_issue_ids))
            } else {
                return;
            };
            if !branch.is_empty() {
                self.proto_commands.push(Command::CreateCheckout {
                    branch,
                    create_branch: true,
                    issue_ids,
                });
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
                let repo = self.model.active_repo_root().clone();
                self.proto_commands.push(Command::ClearIssueSearch { repo });
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
                    self.proto_commands
                        .push(Command::SearchIssues { repo, query });
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
                    if let UiMode::DeleteConfirm {
                        info: Some(ref info),
                        ..
                    } = self.ui.mode
                    {
                        self.proto_commands.push(Command::RemoveCheckout {
                            branch: info.branch.clone(),
                        });
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
