use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::GroupEntry;
use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, RepoSelector, WorkItem};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{
    ui_state::{FocusTarget, PendingActionContext},
    App, BranchInputKind, ClearDispatch, Intent, UiMode,
};
use crate::{
    keymap::{Action, ModeId},
    status_bar::StatusBarAction,
    theme,
};

impl App {
    // ── Key handling ──

    fn resolve_action(&self, key: KeyEvent) -> Option<Action> {
        let mode_id = ModeId::from(&self.ui.mode);

        // Text input modes: only Esc and Enter are intercepted.
        // All other keys pass through to tui_input in handle_key.
        match mode_id {
            ModeId::BranchInput | ModeId::IssueSearch => {
                return match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    _ => None,
                };
            }
            ModeId::CommandPalette => {
                return match key.code {
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Down => Some(Action::SelectNext),
                    _ => None,
                };
            }
            // FilePicker has both a text input and a navigation list.
            // Hardcoded rather than routed through the keymap because shared
            // bindings (e.g. `?` → ToggleHelp) would intercept keys the user
            // intends to type into the path field.
            ModeId::FilePicker => {
                return match key.code {
                    KeyCode::Char('j') | KeyCode::Down => Some(Action::SelectNext),
                    KeyCode::Char('k') | KeyCode::Up => Some(Action::SelectPrev),
                    KeyCode::Esc => Some(Action::Dismiss),
                    KeyCode::Enter => Some(Action::Confirm),
                    _ => None,
                };
            }
            _ => {}
        }

        self.keymap.resolve(mode_id, crokey::KeyCombination::from(key))
    }

    fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::SelectNext => match self.ui.mode.focus_target() {
                FocusTarget::WorkItemTable => self.select_next(),
                FocusTarget::EventLog => {
                    if let Some(sel) = self.ui.event_log.selected {
                        if sel + 1 < self.ui.event_log.count {
                            self.ui.event_log.selected = Some(sel + 1);
                        }
                    } else if self.ui.event_log.count > 0 {
                        self.ui.event_log.selected = Some(self.ui.event_log.count - 1);
                    }
                }
                FocusTarget::HelpText => {
                    self.ui.help_scroll = self.ui.help_scroll.saturating_add(1);
                }
                FocusTarget::ActionMenu => {
                    if let UiMode::ActionMenu { ref items, ref mut index } = self.ui.mode {
                        if *index < items.len().saturating_sub(1) {
                            *index += 1;
                        }
                    }
                }
                FocusTarget::FilePickerList => self.file_picker_select_next(),
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, entries, ref mut selected, ref mut scroll_top, .. } = self.ui.mode {
                        let count = crate::palette::filter_entries(entries, input.value()).len();
                        if count > 0 {
                            *selected = (*selected + 1) % count;
                            let max_visible = crate::palette::MAX_PALETTE_ROWS;
                            if *selected >= *scroll_top + max_visible {
                                *scroll_top = selected.saturating_sub(max_visible - 1);
                            } else if *selected < *scroll_top {
                                *scroll_top = *selected;
                            }
                        }
                    }
                }
                FocusTarget::BranchInput
                | FocusTarget::IssueSearchInput
                | FocusTarget::DeleteConfirmDialog
                | FocusTarget::CloseConfirmDialog => {}
            },
            Action::SelectPrev => match self.ui.mode.focus_target() {
                FocusTarget::WorkItemTable => self.select_prev(),
                FocusTarget::EventLog => {
                    if let Some(sel) = self.ui.event_log.selected {
                        if sel > 0 {
                            self.ui.event_log.selected = Some(sel - 1);
                        }
                    }
                }
                FocusTarget::HelpText => {
                    self.ui.help_scroll = self.ui.help_scroll.saturating_sub(1);
                }
                FocusTarget::ActionMenu => {
                    if let UiMode::ActionMenu { ref mut index, .. } = self.ui.mode {
                        *index = index.saturating_sub(1);
                    }
                }
                FocusTarget::FilePickerList => self.file_picker_select_prev(),
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, entries, ref mut selected, ref mut scroll_top, .. } = self.ui.mode {
                        let count = crate::palette::filter_entries(entries, input.value()).len();
                        if count > 0 {
                            *selected = if *selected == 0 { count - 1 } else { *selected - 1 };
                            let max_visible = crate::palette::MAX_PALETTE_ROWS;
                            if *selected >= *scroll_top + max_visible {
                                *scroll_top = selected.saturating_sub(max_visible - 1);
                            } else if *selected < *scroll_top {
                                *scroll_top = *selected;
                            }
                        }
                    }
                }
                FocusTarget::BranchInput
                | FocusTarget::IssueSearchInput
                | FocusTarget::DeleteConfirmDialog
                | FocusTarget::CloseConfirmDialog => {}
            },
            Action::Confirm => match self.ui.mode.focus_target() {
                FocusTarget::WorkItemTable => self.action_enter(),
                FocusTarget::ActionMenu => {
                    self.execute_menu_action();
                    if matches!(self.ui.mode, UiMode::ActionMenu { .. }) {
                        self.ui.mode = UiMode::Normal;
                    }
                }
                FocusTarget::BranchInput => {
                    if !matches!(self.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }) {
                        let (branch, issue_ids) = if let UiMode::BranchInput { ref input, ref mut pending_issue_ids, .. } = self.ui.mode {
                            (input.value().to_string(), std::mem::take(pending_issue_ids))
                        } else {
                            return;
                        };
                        if !branch.is_empty() {
                            self.proto_commands.push(self.targeted_command(CommandAction::Checkout {
                                repo: flotilla_protocol::RepoSelector::Identity(self.model.active_repo_identity().clone()),
                                target: CheckoutTarget::FreshBranch(branch),
                                issue_ids,
                            }));
                        }
                        self.ui.mode = UiMode::Normal;
                    }
                }
                FocusTarget::IssueSearchInput => {
                    let query = if let UiMode::IssueSearch { ref input } = self.ui.mode {
                        input.value().to_string()
                    } else {
                        return;
                    };
                    if !query.is_empty() {
                        let repo = self.model.active_repo_root().clone();
                        self.proto_commands.push(self.command(CommandAction::SearchIssues {
                            repo: flotilla_protocol::RepoSelector::Path(repo),
                            query: query.clone(),
                        }));
                        self.active_ui_mut().active_search_query = Some(query);
                    }
                    self.ui.mode = UiMode::Normal;
                }
                FocusTarget::FilePickerList => self.activate_dir_entry(),
                FocusTarget::DeleteConfirmDialog => {
                    let loading = matches!(self.ui.mode, UiMode::DeleteConfirm { loading: true, .. });
                    if !loading {
                        if let UiMode::DeleteConfirm { info: Some(ref info), ref terminal_keys, ref identity, ref remote_host, .. } =
                            self.ui.mode
                        {
                            let ctx = PendingActionContext {
                                identity: identity.clone(),
                                description: format!("Remove {}", info.branch),
                                repo_identity: self.model.active_repo_identity().clone(),
                            };
                            let action = CommandAction::RemoveCheckout {
                                checkout: CheckoutSelector::Query(info.branch.clone()),
                                terminal_keys: terminal_keys.clone(),
                            };
                            let command = Command {
                                host: remote_host.clone(),
                                context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
                                action,
                            };
                            self.proto_commands.push_with_context(command, Some(ctx));
                        }
                        self.ui.mode = UiMode::Normal;
                    }
                }
                FocusTarget::CloseConfirmDialog => {
                    if let UiMode::CloseConfirm { ref id, ref identity, ref command, .. } = self.ui.mode {
                        let ctx = PendingActionContext {
                            identity: identity.clone(),
                            description: format!("Close {}", id),
                            repo_identity: self.model.active_repo_identity().clone(),
                        };
                        self.proto_commands.push_with_context(command.clone(), Some(ctx));
                    }
                    self.ui.mode = UiMode::Normal;
                }
                FocusTarget::CommandPalette => {
                    if let UiMode::CommandPalette { ref input, entries, selected, .. } = self.ui.mode {
                        let text = input.value().to_string();

                        // "search <terms>" — apply filter directly, empty clears
                        if let Some(query) = text.strip_prefix("search ") {
                            let query = query.trim().to_string();
                            self.ui.mode = UiMode::Normal;
                            if query.is_empty() {
                                self.clear_active_issue_search(crate::app::ClearDispatch::Always);
                            } else {
                                let repo = self.model.active_repo_root().clone();
                                self.proto_commands.push(self.command(CommandAction::SearchIssues {
                                    repo: flotilla_protocol::RepoSelector::Path(repo),
                                    query: query.clone(),
                                }));
                                self.active_ui_mut().active_search_query = Some(query);
                            }
                            return;
                        }

                        // Otherwise dispatch the selected entry's action
                        let filtered = crate::palette::filter_entries(entries, &text);
                        if let Some(entry) = filtered.get(selected) {
                            let action = entry.action;
                            self.ui.mode = UiMode::Normal;
                            self.dispatch_action(action);
                            return;
                        }
                    }
                    self.ui.mode = UiMode::Normal;
                }
                FocusTarget::HelpText | FocusTarget::EventLog => {}
            },
            Action::Refresh => {} // handled in the main event loop
            Action::PrevTab => self.prev_tab(),
            Action::NextTab => self.next_tab(),
            Action::MoveTabLeft => {
                if !self.ui.mode.is_config() && self.move_tab(-1) {
                    self.config.save_tab_order(&self.persisted_tab_order_paths());
                }
            }
            Action::MoveTabRight => {
                if !self.ui.mode.is_config() && self.move_tab(1) {
                    self.config.save_tab_order(&self.persisted_tab_order_paths());
                }
            }
            Action::ToggleHelp => match self.ui.mode {
                UiMode::Normal => self.ui.mode = UiMode::Help,
                UiMode::Help => {
                    self.ui.mode = UiMode::Normal;
                    self.ui.help_scroll = 0;
                }
                _ => {}
            },
            Action::ToggleMultiSelect => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.toggle_multi_select();
                }
            }
            Action::ToggleProviders => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    let sp = self.active_ui().show_providers;
                    self.active_ui_mut().show_providers = !sp;
                }
            }
            Action::ToggleDebug => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.ui.show_debug = !self.ui.show_debug;
                }
            }
            Action::ToggleStatusBarKeys => {
                self.ui.status_bar.show_keys = !self.ui.status_bar.show_keys;
            }
            Action::CycleHost => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    let peer_hosts = self.model.peer_host_names();
                    self.ui.cycle_target_host(&peer_hosts);
                }
            }
            Action::CycleLayout => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.ui.cycle_layout();
                    self.persist_layout();
                }
            }
            Action::CycleTheme => {
                let themes = theme::available_themes();
                let current = self.theme.name;
                let idx = themes.iter().position(|(name, _)| *name == current).unwrap_or(0);
                let next = (idx + 1) % themes.len();
                self.theme = (themes[next].1)();
            }
            Action::OpenActionMenu => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.open_action_menu();
                }
            }
            Action::OpenBranchInput => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.enter_branch_input(BranchInputKind::Manual);
                }
            }
            Action::OpenIssueSearch => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.ui.mode = UiMode::IssueSearch { input: Input::default() };
                }
            }
            Action::OpenFilePicker => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.open_file_picker_from_active_repo_parent();
                }
            }
            Action::OpenCommandPalette => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.ui.mode = UiMode::CommandPalette {
                        input: Input::default(),
                        entries: crate::palette::all_entries(),
                        selected: 0,
                        scroll_top: 0,
                    };
                }
            }
            Action::Dismiss => match self.ui.mode.focus_target() {
                FocusTarget::WorkItemTable => {
                    // Cancellation takes priority over other dismiss actions while a command is running.
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
                FocusTarget::EventLog => {
                    self.ui.mode = UiMode::Normal;
                }
                FocusTarget::HelpText => {
                    self.ui.mode = UiMode::Normal;
                    self.ui.help_scroll = 0;
                }
                FocusTarget::ActionMenu
                | FocusTarget::BranchInput
                | FocusTarget::FilePickerList
                | FocusTarget::DeleteConfirmDialog
                | FocusTarget::CloseConfirmDialog
                | FocusTarget::CommandPalette => {
                    self.ui.mode = UiMode::Normal;
                }
                FocusTarget::IssueSearchInput => {
                    self.clear_active_issue_search(ClearDispatch::Always);
                    self.ui.mode = UiMode::Normal;
                }
            },
            Action::Quit => {
                self.should_quit = true;
            }
            Action::Dispatch(intent) => {
                if matches!(self.ui.mode.focus_target(), FocusTarget::WorkItemTable) {
                    self.dispatch_if_available(intent);
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some(action) = self.resolve_action(key) {
            self.dispatch_action(action);
            return;
        }

        // Unresolved keys in text input modes pass through to tui_input
        match self.ui.mode {
            UiMode::FilePicker { .. } => self.handle_file_picker_key(key),
            UiMode::BranchInput { .. } => self.handle_branch_input_key(key),
            UiMode::IssueSearch { .. } => self.handle_issue_search_key(key),
            UiMode::CommandPalette { ref mut input, entries, ref mut selected, ref mut scroll_top, .. } => {
                // Tab / Right arrow: fill selected command name into input
                if matches!(key.code, KeyCode::Tab | KeyCode::Right) {
                    let filtered = crate::palette::filter_entries(entries, input.value());
                    if let Some(entry) = filtered.get(*selected) {
                        let filled = format!("{} ", entry.name);
                        *input = Input::from(filled.as_str());
                        *selected = 0;
                        *scroll_top = 0;
                    }
                    return;
                }

                // Backspace on empty input closes the palette
                if matches!(key.code, KeyCode::Backspace) && input.value().is_empty() {
                    self.ui.mode = UiMode::Normal;
                    return;
                }

                input.handle_event(&crossterm::event::Event::Key(key));
                // Shortcut: typing / when input is empty fills "search "
                if input.value() == "/" {
                    *input = Input::from("search ");
                    *selected = 0;
                    *scroll_top = 0;
                    return;
                }
                *selected = 0;
                *scroll_top = 0;
            }
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
            | UiMode::IssueSearch { .. }
            | UiMode::CommandPalette { .. } => {
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

        let my_host = self.model.my_host().cloned();
        for &intent in Intent::enter_priority() {
            if intent.is_available(&item) && intent.is_allowed_for_host(&item, &my_host) {
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
            self.proto_commands.push(self.targeted_repo_command(CommandAction::GenerateBranchName { issue_keys: all_issue_keys }));
        }
        self.active_ui_mut().multi_selected.clear();
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
                    self.ui.mode = UiMode::DeleteConfirm {
                        info: None,
                        loading: true,
                        terminal_keys: item.terminal_keys.clone(),
                        identity: item.identity.clone(),
                        remote_host: self.item_execution_host(item),
                    };
                }
                Intent::GenerateBranchName => {
                    self.enter_branch_input(BranchInputKind::Generating);
                }
                Intent::CloseChangeRequest => {
                    self.ui.mode = UiMode::CloseConfirm {
                        id: match &cmd {
                            Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                            _ => return,
                        },
                        title: item.description.clone(),
                        identity: item.identity.clone(),
                        command: cmd,
                    };
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

    fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let my_host = self.model.my_host().cloned();
        let items: Vec<Intent> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter(|a| a.is_available(&item) && a.is_allowed_for_host(&item, &my_host) && a.resolve(&item, self).is_some())
            .collect();

        if items.is_empty() {
            return;
        }

        self.ui.mode = UiMode::ActionMenu { items, index: 0 };
    }

    fn handle_branch_input_key(&mut self, key: KeyEvent) {
        // Ignore keys while generating
        if matches!(self.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }) {
            return;
        }

        if let UiMode::BranchInput { ref mut input, .. } = self.ui.mode {
            input.handle_event(&crossterm::event::Event::Key(key));
        }
    }

    fn handle_issue_search_key(&mut self, key: KeyEvent) {
        if let UiMode::IssueSearch { ref mut input } = self.ui.mode {
            input.handle_event(&crossterm::event::Event::Key(key));
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
            remote_host: None,
        }
    }

    fn left_click(x: u16, y: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE }
    }

    // ── handle_key — top-level dispatch ──────────────────────────────

    #[test]
    fn dispatch_action_select_next_moves_work_item_selection() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a"), make_work_item("b")]);

        app.dispatch_action(Action::SelectNext);

        assert_eq!(app.active_ui().selected_selectable_idx, Some(1));
    }

    #[test]
    fn dispatch_action_select_next_moves_config_event_log_selection() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.ui.event_log.count = 3;
        app.ui.event_log.selected = Some(0);

        app.dispatch_action(Action::SelectNext);

        assert_eq!(app.ui.event_log.selected, Some(1));
    }

    #[test]
    fn dispatch_action_select_next_scrolls_help() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Help;

        app.dispatch_action(Action::SelectNext);

        assert_eq!(app.ui.help_scroll, 1);
    }

    #[test]
    fn dispatch_action_select_next_advances_menu_index() {
        let mut app = stub_app();
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest, Intent::SwitchToWorkspace], index: 0 };

        app.dispatch_action(Action::SelectNext);

        match app.ui.mode {
            UiMode::ActionMenu { index, .. } => assert_eq!(index, 1),
            _ => panic!("expected ActionMenu"),
        }
    }

    #[test]
    fn dispatch_action_select_next_advances_file_picker_selection() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![dir_entry("alpha", false, false), dir_entry("beta", false, false)]);

        app.dispatch_action(Action::SelectNext);

        match app.ui.mode {
            UiMode::FilePicker { selected, .. } => assert_eq!(selected, 1),
            _ => panic!("expected FilePicker"),
        }
    }

    #[test]
    fn dispatch_action_confirm_executes_action_menu_selection() {
        let mut app = stub_app();
        setup_table(&mut app, vec![make_work_item("a")]);
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::RemoveCheckout], index: 0 };

        app.dispatch_action(Action::Confirm);

        assert!(matches!(app.ui.mode, UiMode::DeleteConfirm { loading: true, .. }));
    }

    #[test]
    fn dispatch_action_confirm_submits_delete_confirm() {
        let mut app = stub_app();
        app.ui.mode = delete_confirm_mode("feat/delete");

        app.dispatch_action(Action::Confirm);

        assert!(matches!(app.ui.mode, UiMode::Normal));
        let (cmd, _) = app.proto_commands.take_next().expect("expected remove checkout command");
        match cmd {
            Command { action: CommandAction::RemoveCheckout { checkout, .. }, .. } => {
                assert_eq!(checkout, CheckoutSelector::Query("feat/delete".into()));
            }
            other => panic!("expected RemoveCheckout, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_action_confirm_submits_branch_input() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::from("feature/test"), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };

        app.dispatch_action(Action::Confirm);

        assert!(matches!(app.ui.mode, UiMode::Normal));
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
        let mut app = stub_app();
        app.ui.mode = UiMode::IssueSearch { input: Input::from("bug fix") };

        app.dispatch_action(Action::Confirm);

        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.active_ui().active_search_query.as_deref(), Some("bug fix"));
        let (cmd, _) = app.proto_commands.take_next().expect("expected search command");
        match cmd {
            Command { action: CommandAction::SearchIssues { query, .. }, .. } => {
                assert_eq!(query, "bug fix");
            }
            other => panic!("expected SearchIssues, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_action_confirm_activates_file_picker_selection() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let repo_dir = tmp.path().join("my-repo");
        std::fs::create_dir(&repo_dir).expect("create repo dir");
        std::fs::create_dir(repo_dir.join(".git")).expect("create git dir");

        let mut app = stub_app();
        let parent_path = format!("{}/", tmp.path().to_string_lossy());
        let entries = vec![DirEntry { name: "my-repo".to_string(), is_dir: true, is_git_repo: true, is_added: false }];
        enter_file_picker(&mut app, &parent_path, entries);

        app.dispatch_action(Action::Confirm);

        assert!(matches!(app.ui.mode, UiMode::Normal));
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

        app.ui.mode = UiMode::Config;
        assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), Some(Action::Dismiss));

        app.ui.mode = UiMode::Help;
        assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), Some(Action::Dismiss));
    }

    #[test]
    fn resolve_action_maps_file_picker_navigation_keys() {
        let mut app = stub_app();
        enter_file_picker(&mut app, "/tmp/", vec![dir_entry("alpha", false, false), dir_entry("beta", false, false)]);

        assert_eq!(app.resolve_action(key(KeyCode::Char('j'))), Some(Action::SelectNext));
        assert_eq!(app.resolve_action(key(KeyCode::Char('k'))), Some(Action::SelectPrev));
        assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), None);
    }

    #[test]
    fn resolve_action_does_not_intercept_manual_branch_input_text() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::default(), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };

        assert_eq!(app.resolve_action(key(KeyCode::Char('q'))), None);
    }

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
    fn config_q_dismisses_to_normal() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(!app.should_quit);
    }

    #[test]
    fn config_esc_dismisses_to_normal() {
        let mut app = stub_app();
        app.ui.mode = UiMode::Config;
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(!app.should_quit);
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

    #[test]
    fn brackets_do_not_switch_tabs_from_action_menu() {
        let mut app = stub_app_with_repos(2);
        app.ui.mode = UiMode::ActionMenu { items: vec![Intent::OpenChangeRequest], index: 0 };

        app.handle_key(key(KeyCode::Char(']')));

        assert!(matches!(app.ui.mode, UiMode::ActionMenu { .. }));
        assert_eq!(app.model.active_repo, 0);
    }

    #[test]
    fn brackets_do_not_switch_tabs_while_branch_input_generating() {
        let mut app = stub_app_with_repos(2);
        app.ui.mode = UiMode::BranchInput { input: Input::from("partial"), kind: BranchInputKind::Generating, pending_issue_ids: vec![] };

        app.handle_key(key(KeyCode::Char(']')));

        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
        assert_eq!(app.model.active_repo, 0);
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
        app.ui.mode = UiMode::Help;
        app.ui.help_scroll = 7;

        app.handle_key(key(KeyCode::Char('q')));

        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.ui.help_scroll, 0);
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
    fn normal_slash_opens_command_palette() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
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
        assert!(matches!(app.ui.mode, UiMode::ActionMenu { .. }));
    }

    #[test]
    fn clicking_search_status_target_opens_command_palette() {
        let mut app = stub_app();
        app.ui.layout.status_bar.key_targets =
            vec![StatusBarTarget::new(Rect::new(10, 29, 12, 1), StatusBarAction::key(KeyCode::Char('/')))];

        app.handle_mouse(left_click(12, 29));

        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
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
    fn clicking_host_status_target_cycles_target_host() {
        let mut app = stub_app();
        insert_peer_host(&mut app.model, "alpha");
        app.ui.layout.status_bar.key_targets =
            vec![StatusBarTarget::new(Rect::new(0, 29, 16, 1), StatusBarAction::key(KeyCode::Char('h')))];

        app.handle_mouse(left_click(4, 29));

        assert_eq!(app.ui.target_host, Some(HostName::new("alpha")));
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
    fn branch_input_generating_ignores_confirm_but_allows_dismiss() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::from("partial"), kind: BranchInputKind::Generating, pending_issue_ids: vec![] };
        // Enter should be ignored
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. }));
        assert!(app.proto_commands.take_next().is_none());

        // Esc should dismiss the generating prompt
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn branch_input_manual_q_types_character() {
        let mut app = stub_app();
        app.ui.mode = UiMode::BranchInput { input: Input::default(), kind: BranchInputKind::Manual, pending_issue_ids: vec![] };

        app.handle_key(key(KeyCode::Char('q')));

        match app.ui.mode {
            UiMode::BranchInput { ref input, .. } => assert_eq!(input.value(), "q"),
            _ => panic!("expected BranchInput"),
        }
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
                assert_eq!(repo, flotilla_protocol::RepoSelector::Path(PathBuf::from("/tmp/test-repo")));
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
                assert_eq!(repo, flotilla_protocol::RepoSelector::Path(PathBuf::from("/tmp/test-repo")));
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
    fn delete_confirm_attaches_pending_context() {
        let mut app = stub_app();
        let item = make_work_item("a");
        app.ui.mode = UiMode::DeleteConfirm {
            info: Some(CheckoutStatus {
                branch: "feat/a".into(),
                change_request_status: None,
                merge_commit_sha: None,
                unpushed_commits: vec![],
                has_uncommitted: false,
                uncommitted_files: vec![],
                base_detection_warning: None,
            }),
            loading: false,
            terminal_keys: vec![],
            identity: item.identity.clone(),
            remote_host: None,
        };
        app.handle_key(key(KeyCode::Char('y')));
        let (_, ctx) = app.proto_commands.take_next().expect("should have command");
        let ctx = ctx.expect("should have pending context");
        assert_eq!(ctx.identity, item.identity);
    }

    #[test]
    fn delete_confirm_routes_to_remote_host_when_set() {
        let mut app = stub_app();
        let hostname = HostName::new("feta");
        app.ui.mode = UiMode::DeleteConfirm {
            info: Some(CheckoutStatus {
                branch: "feat/x".into(),
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
            remote_host: Some(hostname.clone()),
        };
        app.handle_key(key(KeyCode::Char('y')));
        let (cmd, _) = app.proto_commands.take_next().expect("command");
        assert_eq!(cmd.host, Some(hostname));
        assert!(matches!(cmd.action, CommandAction::RemoveCheckout { .. }));
    }

    #[test]
    fn delete_confirm_ignores_while_loading() {
        let mut app = stub_app();
        app.ui.mode = UiMode::DeleteConfirm {
            info: None,
            loading: true,
            terminal_keys: vec![],
            identity: WorkItemIdentity::Session("test".into()),
            remote_host: None,
        };
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
        app.ui.mode = UiMode::DeleteConfirm {
            info: None,
            loading: false,
            terminal_keys: vec![],
            identity: WorkItemIdentity::Session("test".into()),
            remote_host: None,
        };
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
        app.ui.mode = UiMode::CloseConfirm {
            id: "PR-1".into(),
            title: "test".into(),
            identity: item.identity.clone(),
            command: Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "PR-1".into() } },
        };
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
        app.ui.mode = UiMode::CloseConfirm {
            id: "PR-1".into(),
            title: "test".into(),
            identity: WorkItemIdentity::ChangeRequest("PR-1".into()),
            command: expected.clone(),
        };

        app.handle_key(key(KeyCode::Char('y')));

        let (command, _) = app.proto_commands.take_next().expect("should have command");
        assert_eq!(command, expected);
    }

    // ── command palette key handling ────────────────────────────────

    #[test]
    fn double_slash_fills_search() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
        app.handle_key(key(KeyCode::Char('/')));
        if let UiMode::CommandPalette { ref input, .. } = app.ui.mode {
            assert_eq!(input.value(), "search ");
        } else {
            panic!("expected CommandPalette");
        }
    }

    #[test]
    fn command_palette_tab_fills_command_name() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // First entry is "search" — Tab should fill it
        app.handle_key(key(KeyCode::Tab));
        if let UiMode::CommandPalette { ref input, selected, .. } = app.ui.mode {
            assert_eq!(input.value(), "search ");
            assert_eq!(selected, 0);
        } else {
            panic!("expected CommandPalette");
        }
    }

    #[test]
    fn command_palette_search_with_args_applies_filter() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "search auth".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.active_ui().active_search_query.as_deref(), Some("auth"));
    }

    #[test]
    fn command_palette_search_empty_term_clears() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "search ".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert_eq!(app.active_ui().active_search_query, None);
    }

    #[test]
    fn command_palette_enter_dispatches_action() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // First entry is "search" which dispatches OpenIssueSearch
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn command_palette_esc_dismisses() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(matches!(app.ui.mode, UiMode::CommandPalette { .. }));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
    }

    #[test]
    fn command_palette_arrow_navigation_wraps() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        // Down from 0 → 1
        app.handle_key(key(KeyCode::Down));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 1);
        } else {
            panic!("expected CommandPalette");
        }
        // Up from 1 → 0
        app.handle_key(key(KeyCode::Up));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected CommandPalette");
        }
        // Up from 0 → wraps to last
        app.handle_key(key(KeyCode::Up));
        if let UiMode::CommandPalette { selected, entries, ref input, .. } = &app.ui.mode {
            assert_eq!(*selected, crate::palette::filter_entries(entries, input.value()).len() - 1);
        } else {
            panic!("expected CommandPalette");
        }
    }

    #[test]
    fn command_palette_typing_resets_selection() {
        let mut app = stub_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        // Now type a char — selection resets
        app.handle_key(key(KeyCode::Char('h')));
        if let UiMode::CommandPalette { selected, .. } = app.ui.mode {
            assert_eq!(selected, 0);
        } else {
            panic!("expected CommandPalette");
        }
    }
}
