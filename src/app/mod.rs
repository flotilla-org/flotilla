pub mod command;
pub mod executor;
pub mod intent;
pub mod model;
pub mod ui_state;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as InputEventHandler;

use crate::data::{TableEntry, WorkItem};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

pub use command::{Command, CommandQueue};
pub use intent::Intent;
pub use model::{AppModel, ProviderStatus};
pub use ui_state::{DirEntry, TabId, UiMode, UiState, RepoUiState};

pub struct App {
    pub model: AppModel,
    pub ui: UiState,
    pub commands: CommandQueue,
    pub should_quit: bool,
    pub pending_issue_ids: Vec<(String, String)>,
}

impl App {
    pub fn new(repos: Vec<PathBuf>) -> Self {
        let model = AppModel::new(repos);
        let ui = UiState::new(&model.repo_order);
        Self {
            model,
            ui,
            commands: Default::default(),
            should_quit: false,
            pending_issue_ids: Vec::new(),
        }
    }

    // ── Convenience accessors ──

    pub fn active_ui(&self) -> &RepoUiState {
        self.ui.active_repo_ui(&self.model.repo_order, self.model.active_repo)
    }

    pub fn active_ui_mut(&mut self) -> &mut RepoUiState {
        let key = &self.model.repo_order[self.model.active_repo];
        self.ui.repo_ui.get_mut(key).unwrap()
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let table_idx = self.active_ui().table_state.selected()?;
        match self.active_ui().table_view.table_entries.get(table_idx)? {
            TableEntry::Item(item) => Some(item),
            TableEntry::Header(_) => None,
        }
    }

    pub fn add_repo(&mut self, path: PathBuf) {
        if !self.model.repos.contains_key(&path) {
            self.model.add_repo(path.clone());
            self.ui.repo_ui.insert(path, RepoUiState::default());
        }
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.model.repo_order.len() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = idx;
            let key = &self.model.repo_order[idx];
            self.ui.repo_ui.get_mut(key).unwrap().has_unseen_changes = false;
        }
    }

    pub fn next_tab(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        if self.ui.mode.is_config() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = 0;
        } else if self.model.active_repo < self.model.repo_order.len() - 1 {
            self.switch_tab(self.model.active_repo + 1);
        } else {
            self.ui.mode = UiMode::Config;
        }
    }

    pub fn prev_tab(&mut self) {
        if self.model.repo_order.is_empty() {
            return;
        }
        if self.ui.mode.is_config() {
            self.ui.mode = UiMode::Normal;
            self.model.active_repo = self.model.repo_order.len() - 1;
        } else if self.model.active_repo > 0 {
            self.switch_tab(self.model.active_repo - 1);
        } else {
            self.ui.mode = UiMode::Config;
        }
    }

    pub fn move_tab(&mut self, delta: isize) -> bool {
        let len = self.model.repo_order.len();
        if len < 2 {
            return false;
        }
        let cur = self.model.active_repo;
        let new_idx = cur as isize + delta;
        if new_idx < 0 || new_idx >= len as isize {
            return false;
        }
        let new_idx = new_idx as usize;
        self.model.repo_order.swap(cur, new_idx);
        self.model.active_repo = new_idx;
        true
    }

    pub fn prefill_branch_input(&mut self, branch_name: &str) {
        self.ui.mode = UiMode::BranchInput {
            input: Input::from(branch_name),
            generating: false,
        };
    }

    // ── Key handling ──

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.model.status_message = None;

        // Toggle help from Normal or Help modes
        if key.code == KeyCode::Char('?') {
            match self.ui.mode {
                UiMode::Normal => { self.ui.mode = UiMode::Help; return; }
                UiMode::Help => { self.ui.mode = UiMode::Normal; return; }
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
            KeyCode::Char(' ') => self.open_action_menu(),
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.toggle_multi_select();
                } else {
                    self.action_enter();
                }
            }
            KeyCode::Char('n') => {
                self.ui.mode = UiMode::BranchInput {
                    input: Input::default(),
                    generating: false,
                };
            }
            KeyCode::Char('d') => self.dispatch_if_available(Intent::RemoveWorktree),
            KeyCode::Char('D') => self.ui.show_debug = !self.ui.show_debug,
            KeyCode::Char('p') => self.dispatch_if_available(Intent::OpenPr),
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            KeyCode::Char('{') => {
                if !self.ui.mode.is_config() && self.move_tab(-1) {
                    crate::config::save_tab_order(&self.model.repo_order);
                }
            }
            KeyCode::Char('}') => {
                if !self.ui.mode.is_config() && self.move_tab(1) {
                    crate::config::save_tab_order(&self.model.repo_order);
                }
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
            UiMode::Help | UiMode::DeleteConfirm { .. } | UiMode::BranchInput { .. } => {
                return;
            }
            UiMode::Config | UiMode::Normal => {}
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                        let table_idx = self.active_ui().table_view.selectable_indices[si];
                        self.active_ui_mut().selected_selectable_idx = Some(si);
                        self.active_ui_mut().table_state.select(Some(table_idx));
                        self.toggle_multi_select();
                    }
                    return;
                }

                if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self.ui.double_click.last_time
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
            self.ui.mode = UiMode::Normal;
        }
    }

    fn row_at_mouse(&self, x: u16, y: u16) -> Option<usize> {
        let ta = self.ui.layout.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let offset = self.active_ui().table_state.offset();
            let actual_row = data_row + offset;
            self.active_ui()
                .table_view
                .selectable_indices
                .iter()
                .position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    fn toggle_multi_select(&mut self) {
        if let Some(si) = self.active_ui().selected_selectable_idx {
            if self.active_ui().multi_selected.contains(&si) {
                self.active_ui_mut().multi_selected.remove(&si);
            } else {
                self.active_ui_mut().multi_selected.insert(si);
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
        let mut all_issue_idxs: Vec<usize> = Vec::new();
        let multi_selected: BTreeSet<usize> = self.active_ui().multi_selected.clone();
        for &si in &multi_selected {
            if let Some(&table_idx) = self.active_ui().table_view.selectable_indices.get(si) {
                if let Some(TableEntry::Item(item)) = self.active_ui().table_view.table_entries.get(table_idx) {
                    all_issue_idxs.extend(&item.issue_idxs);
                }
            }
        }
        if let Some(si) = self.active_ui().selected_selectable_idx {
            if !multi_selected.contains(&si) {
                if let Some(&table_idx) = self.active_ui().table_view.selectable_indices.get(si) {
                    if let Some(TableEntry::Item(item)) = self.active_ui().table_view.table_entries.get(table_idx) {
                        all_issue_idxs.extend(&item.issue_idxs);
                    }
                }
            }
        }
        all_issue_idxs.sort();
        all_issue_idxs.dedup();
        if !all_issue_idxs.is_empty() {
            self.ui.mode = UiMode::BranchInput {
                input: Input::default(),
                generating: true,
            };
            self.commands.push(Command::GenerateBranchName(all_issue_idxs));
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
                Intent::RemoveWorktree => {
                    self.ui.mode = UiMode::DeleteConfirm {
                        info: None,
                        loading: true,
                    };
                }
                Intent::GenerateBranchName => {
                    self.ui.mode = UiMode::BranchInput {
                        input: Input::default(),
                        generating: true,
                    };
                }
                _ => {}
            }
            self.commands.push(cmd);
        }
    }

    fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let items: Vec<Intent> = Intent::all_in_menu_order()
            .iter()
            .copied()
            .filter(|a| a.is_available(&item))
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
            self.ui.mode = UiMode::Normal;
            return;
        }
        let UiMode::ActionMenu { ref items, ref mut index } = self.ui.mode else { return; };
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
        if matches!(self.ui.mode, UiMode::BranchInput { generating: true, .. }) {
            return;
        }

        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            let branch = if let UiMode::BranchInput { ref input, .. } = self.ui.mode {
                input.value().to_string()
            } else {
                return;
            };
            if !branch.is_empty() {
                let issue_ids = std::mem::take(&mut self.pending_issue_ids);
                self.commands.push(Command::CreateWorktree { branch, create_branch: true, issue_ids });
            }
            self.ui.mode = UiMode::Normal;
            return;
        }
        if let UiMode::BranchInput { ref mut input, .. } = self.ui.mode {
            input.handle_event(&crossterm::event::Event::Key(key));
        }
    }

    fn handle_file_picker_key(&mut self, key: KeyEvent) {
        // Keys that change mode
        if key.code == KeyCode::Esc {
            self.ui.mode = UiMode::Normal;
            return;
        }
        if key.code == KeyCode::Enter {
            self.activate_dir_entry();
            return;
        }

        let needs_refresh = {
            let UiMode::FilePicker { ref mut input, ref mut dir_entries, ref mut selected } = self.ui.mode else { return; };
            match key.code {
                KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() || key.code == KeyCode::Down => {
                    if !dir_entries.is_empty() {
                        *selected = (*selected + 1).min(dir_entries.len() - 1);
                    }
                    false
                }
                KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() || key.code == KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    false
                }
                KeyCode::Tab => {
                    if let Some(entry) = dir_entries.get(*selected).cloned() {
                        let current = input.value().to_string();
                        let base = if current.ends_with('/') {
                            current.clone()
                        } else {
                            current.rsplit_once('/')
                                .map(|(prefix, _)| format!("{prefix}/"))
                                .unwrap_or_default()
                        };
                        let new_path = format!("{}{}/", base, entry.name);
                        *input = Input::from(new_path.as_str());
                        *selected = 0;
                    }
                    true
                }
                _ => {
                    input.handle_event(&crossterm::event::Event::Key(key));
                    *selected = 0;
                    true
                }
            }
        };
        if needs_refresh {
            self.refresh_dir_listing();
        }
    }

    fn activate_dir_entry(&mut self) {
        let (entry, base) = {
            let UiMode::FilePicker { ref input, ref dir_entries, selected } = self.ui.mode else { return; };
            let Some(entry) = dir_entries.get(selected).cloned() else { return; };
            let current = input.value().to_string();
            let base = if current.ends_with('/') {
                current
            } else {
                current.rsplit_once('/')
                    .map(|(prefix, _)| format!("{prefix}/"))
                    .unwrap_or_default()
            };
            (entry, base)
        };

        if entry.is_git_repo && !entry.is_added {
            let path = PathBuf::from(format!("{}{}", base, entry.name));
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            self.commands.push(Command::AddRepo(canonical));
            self.ui.mode = UiMode::Normal;
        } else if entry.is_dir {
            let new_path = format!("{}{}/", base, entry.name);
            if let UiMode::FilePicker { ref mut input, ref mut selected, .. } = self.ui.mode {
                *input = Input::from(new_path.as_str());
                *selected = 0;
            }
            self.refresh_dir_listing();
        }
    }

    pub fn handle_file_picker_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let x = mouse.column;
        let y = mouse.row;
        let a = self.ui.layout.file_picker_area;
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.ui.mode = UiMode::Normal;
            return;
        }
        let la = self.ui.layout.file_picker_list_area;
        if x >= la.x && x < la.x + la.width && y >= la.y && y < la.y + la.height {
            let row = (y - la.y) as usize;
            let len = if let UiMode::FilePicker { ref dir_entries, .. } = self.ui.mode {
                dir_entries.len()
            } else {
                return;
            };
            if row < len {
                if let UiMode::FilePicker { ref mut selected, .. } = self.ui.mode {
                    *selected = row;
                }
                self.activate_dir_entry();
            }
        }
    }

    pub fn refresh_dir_listing(&mut self) {
        let Self { model, ui, .. } = self;
        let UiMode::FilePicker { ref input, ref mut dir_entries, .. } = ui.mode else { return; };

        let path_str = input.value().to_string();
        let dir = if path_str.ends_with('/') {
            PathBuf::from(&path_str)
        } else {
            PathBuf::from(&path_str).parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };

        let filter = if !path_str.ends_with('/') {
            PathBuf::from(&path_str)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if !filter.is_empty() && !name.to_lowercase().starts_with(&filter) {
                    continue;
                }
                let path = entry.path();
                let is_dir = path.is_dir();
                if !is_dir {
                    continue;
                }
                let is_git_repo = path.join(".git").exists();
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                let is_added = model.repos.contains_key(&canonical);
                entries.push(DirEntry {
                    name,
                    is_dir,
                    is_git_repo,
                    is_added,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        *dir_entries = entries;
    }

    fn handle_delete_confirm_key(&mut self, key: KeyEvent) {
        let loading = matches!(self.ui.mode, UiMode::DeleteConfirm { loading: true, .. });
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if !loading {
                    self.commands.push(Command::ConfirmDelete);
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
            let UiMode::ActionMenu { ref items, index } = self.ui.mode else { return; };
            let Some(&intent) = items.get(index) else { return; };
            let Some(item) = self.selected_work_item().cloned() else { return; };
            (intent, item)
        };
        self.resolve_and_push(intent, &item);
    }

    fn select_next(&mut self) {
        let indices = &self.active_ui().table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active_ui().selected_selectable_idx;
        let next = match current_si {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = self.active_ui().table_view.selectable_indices[next];
        self.active_ui_mut().selected_selectable_idx = Some(next);
        self.active_ui_mut().table_state.select(Some(table_idx));
    }

    fn select_prev(&mut self) {
        let indices = &self.active_ui().table_view.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active_ui().selected_selectable_idx;
        let prev = match current_si {
            Some(si) if si > 0 => si - 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = self.active_ui().table_view.selectable_indices[prev];
        self.active_ui_mut().selected_selectable_idx = Some(prev);
        self.active_ui_mut().table_state.select(Some(table_idx));
    }
}
