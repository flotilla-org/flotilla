pub mod executor;
pub mod intent;
pub mod ui_state;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use tui_input::backend::crossterm::EventHandler as InputEventHandler;
use tui_input::Input;

use flotilla_core::daemon::DaemonHandle;
use flotilla_core::data::{self, GroupEntry, SectionLabels};
use flotilla_protocol::{
    Command, DaemonEvent, ProviderData, RepoInfo, RepoLabels, Snapshot, WorkItem,
};
use std::collections::VecDeque;

pub use intent::Intent;
pub use ui_state::{DirEntry, RepoUiState, TabId, UiMode, UiState};

/// Per-provider auth/health status from last refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    Error,
}

#[derive(Default)]
pub struct CommandQueue {
    queue: VecDeque<Command>,
}

impl CommandQueue {
    pub fn push(&mut self, cmd: Command) {
        self.queue.push_back(cmd);
    }
    pub fn take_next(&mut self) -> Option<Command> {
        self.queue.pop_front()
    }
}

/// Per-repo view-model state for the TUI. Contains only what the UI needs
/// to render — no provider registry, no refresh handle.
pub struct TuiRepoModel {
    pub providers: Arc<ProviderData>,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, String>,
    pub provider_health: HashMap<String, bool>,
    pub loading: bool,
}

/// TUI-side domain model. Mirrors the shape of core's `AppModel` but without
/// daemon-internal fields (registry, refresh handles). Populated from
/// `DaemonHandle::list_repos()` and updated by `DaemonEvent::Snapshot`.
pub struct TuiModel {
    pub repos: HashMap<PathBuf, TuiRepoModel>,
    pub repo_order: Vec<PathBuf>,
    pub active_repo: usize,
    /// Per-repo, per-provider auth status from last refresh.
    /// Key: (repo_path, provider_category, provider_name)
    pub provider_statuses: HashMap<(PathBuf, String, String), ProviderStatus>,
    pub status_message: Option<String>,
}

impl TuiModel {
    pub fn from_repo_info(repos_info: Vec<RepoInfo>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for info in repos_info {
            repos.insert(
                info.path.clone(),
                TuiRepoModel {
                    providers: Arc::new(ProviderData::default()),
                    labels: info.labels,
                    provider_names: info.provider_names,
                    provider_health: info.provider_health,
                    loading: info.loading,
                },
            );
            order.push(info.path);
        }
        Self {
            repos,
            repo_order: order,
            active_repo: 0,
            provider_statuses: HashMap::new(),
            status_message: None,
        }
    }

    pub fn active(&self) -> &TuiRepoModel {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    pub fn active_repo_root(&self) -> &PathBuf {
        &self.repo_order[self.active_repo]
    }

    pub fn active_labels(&self) -> &RepoLabels {
        &self.active().labels
    }

    pub fn repo_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    }
}

pub struct App {
    pub daemon: Arc<dyn DaemonHandle>,
    pub model: TuiModel,
    pub ui: UiState,
    pub proto_commands: CommandQueue,
    pub should_quit: bool,
}

impl App {
    pub fn new(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>) -> Self {
        let model = TuiModel::from_repo_info(repos_info);
        let ui = UiState::new(&model.repo_order);
        Self {
            daemon,
            model,
            ui,
            proto_commands: Default::default(),
            should_quit: false,
        }
    }

    // ── Daemon event handling ──

    pub fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::Snapshot(snap) => self.apply_snapshot(*snap),
            DaemonEvent::RepoAdded(info) => self.handle_repo_added(*info),
            DaemonEvent::RepoRemoved { path } => self.handle_repo_removed(&path),
            DaemonEvent::CommandResult { result, .. } => {
                // Not used in-process (results returned directly from execute)
                executor::handle_result(result, self);
            }
        }
    }

    fn apply_snapshot(&mut self, snap: Snapshot) {
        let path = snap.repo.clone();
        let rm = match self.model.repos.get_mut(&path) {
            Some(rm) => rm,
            None => return,
        };

        let old_providers = std::mem::replace(&mut rm.providers, Arc::new(snap.providers));
        rm.provider_health = snap.provider_health.clone();
        rm.loading = false;

        // Build table view
        let section_labels = SectionLabels {
            checkouts: rm.labels.checkouts.section.clone(),
            code_review: rm.labels.code_review.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.sessions.section.clone(),
        };
        let table_view = data::group_work_items(&snap.work_items, &rm.providers, &section_labels);

        // Provider health -> model-level statuses
        for (kind, healthy) in &rm.provider_health {
            let provider_name = rm.provider_names.get(kind.as_str()).cloned();
            if let Some(pname) = provider_name {
                let key = (path.clone(), kind.clone(), pname);
                let status = if *healthy {
                    ProviderStatus::Ok
                } else {
                    ProviderStatus::Error
                };
                self.model.provider_statuses.insert(key, status);
            }
        }

        // Change detection badge for inactive tabs
        let active_idx = self.model.active_repo;
        let i = self.model.repo_order.iter().position(|p| p == &path);
        if let Some(idx) = i {
            if idx != active_idx && *old_providers != *rm.providers {
                if let Some(rui) = self.ui.repo_ui.get_mut(&path) {
                    rui.has_unseen_changes = true;
                }
            }
        }

        // Store table view on UI state and restore selection by identity
        if let Some(rui) = self.ui.repo_ui.get_mut(&path) {
            // Save current selection identity
            let prev_identity = rui
                .selected_selectable_idx
                .and_then(|si| rui.table_view.selectable_indices.get(si).copied())
                .and_then(|ti| match rui.table_view.table_entries.get(ti) {
                    Some(GroupEntry::Item(item)) => Some(item.identity.clone()),
                    _ => None,
                });

            rui.table_view = table_view;

            // Restore selection by identity
            if rui.table_view.selectable_indices.is_empty() {
                rui.selected_selectable_idx = None;
                rui.table_state.select(None);
            } else if let Some(ref identity) = prev_identity {
                let found =
                    rui.table_view
                        .selectable_indices
                        .iter()
                        .enumerate()
                        .find(|(_, &ti)| {
                            matches!(
                                rui.table_view.table_entries.get(ti),
                                Some(GroupEntry::Item(item)) if item.identity == *identity
                            )
                        });
                if let Some((si, &ti)) = found {
                    rui.selected_selectable_idx = Some(si);
                    rui.table_state.select(Some(ti));
                } else {
                    // Item was removed — select first
                    rui.selected_selectable_idx = Some(0);
                    rui.table_state
                        .select(Some(rui.table_view.selectable_indices[0]));
                }
            } else {
                rui.selected_selectable_idx = Some(0);
                rui.table_state
                    .select(Some(rui.table_view.selectable_indices[0]));
            }

            // Clean up stale multi-select identities
            let current_identities: std::collections::HashSet<flotilla_protocol::WorkItemIdentity> =
                rui.table_view
                    .table_entries
                    .iter()
                    .filter_map(|e| match e {
                        GroupEntry::Item(item) => Some(item.identity.clone()),
                        _ => None,
                    })
                    .collect();
            rui.multi_selected
                .retain(|id| current_identities.contains(id));
        }

        // Log errors, suppressing "issues disabled" since the daemon handles that
        if !snap.errors.is_empty() {
            let name = TuiModel::repo_name(&path);
            let mut all_errors: Vec<String> = Vec::new();
            for e in &snap.errors {
                if e.category == "issues" && e.message.contains("has disabled issues") {
                    continue;
                }
                tracing::error!("{name}: {}: {}", e.category, e.message);
                all_errors.push(format!("{name}: {}: {}", e.category, e.message));
            }
            if !all_errors.is_empty() {
                self.model.status_message = Some(all_errors.join("; "));
            }
        }
    }

    fn handle_repo_added(&mut self, info: RepoInfo) {
        let path = info.path.clone();
        if self.model.repos.contains_key(&path) {
            return;
        }
        self.model.repos.insert(
            path.clone(),
            TuiRepoModel {
                providers: Arc::new(ProviderData::default()),
                labels: info.labels,
                provider_names: info.provider_names,
                provider_health: info.provider_health,
                loading: info.loading,
            },
        );
        self.model.repo_order.push(path.clone());
        self.ui.repo_ui.insert(path, RepoUiState::default());
        self.switch_tab(self.model.repo_order.len() - 1);
    }

    fn handle_repo_removed(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.model.repos.remove(&path);
        self.model.repo_order.retain(|p| p != &path);
        self.ui.repo_ui.remove(&path);
        if self.model.active_repo >= self.model.repo_order.len()
            && !self.model.repo_order.is_empty()
        {
            self.model.active_repo = self.model.repo_order.len() - 1;
        }
    }

    // ── Convenience accessors ──

    pub fn active_ui(&self) -> &RepoUiState {
        self.ui
            .active_repo_ui(&self.model.repo_order, self.model.active_repo)
    }

    pub fn active_ui_mut(&mut self) -> &mut RepoUiState {
        let key = &self.model.repo_order[self.model.active_repo];
        self.ui.repo_ui.get_mut(key).unwrap()
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let table_idx = self.active_ui().table_state.selected()?;
        match self.active_ui().table_view.table_entries.get(table_idx)? {
            GroupEntry::Item(item) => Some(item),
            GroupEntry::Header(_) => None,
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

    pub fn prefill_branch_input(
        &mut self,
        branch_name: &str,
        pending_issue_ids: Vec<(String, String)>,
    ) {
        self.ui.mode = UiMode::BranchInput {
            input: Input::from(branch_name),
            generating: false,
            pending_issue_ids,
        };
    }

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
            KeyCode::Char('d') => self.dispatch_if_available(Intent::RemoveWorktree),
            KeyCode::Char('D') => self.ui.show_debug = !self.ui.show_debug,
            KeyCode::Char('p') => self.dispatch_if_available(Intent::OpenPr),
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            KeyCode::Char('{') => {
                if !self.ui.mode.is_config() && self.move_tab(-1) {
                    flotilla_core::config::save_tab_order(&self.model.repo_order);
                }
            }
            KeyCode::Char('}') => {
                if !self.ui.mode.is_config() && self.move_tab(1) {
                    flotilla_core::config::save_tab_order(&self.model.repo_order);
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
            if let Some(&table_idx) = self.active_ui().table_view.selectable_indices.get(si) {
                if let Some(GroupEntry::Item(item)) =
                    self.active_ui().table_view.table_entries.get(table_idx)
                {
                    let identity = item.identity.clone();
                    let rui = self.active_ui_mut();
                    if !rui.multi_selected.remove(&identity) {
                        rui.multi_selected.insert(identity);
                    }
                }
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
                self.proto_commands.push(Command::CreateWorktree {
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
            let UiMode::FilePicker {
                ref mut input,
                ref mut dir_entries,
                ref mut selected,
            } = self.ui.mode
            else {
                return;
            };
            match key.code {
                KeyCode::Down | KeyCode::Char('j')
                    if key.modifiers.is_empty() || key.code == KeyCode::Down =>
                {
                    if !dir_entries.is_empty() {
                        *selected = (*selected + 1).min(dir_entries.len() - 1);
                    }
                    false
                }
                KeyCode::Up | KeyCode::Char('k')
                    if key.modifiers.is_empty() || key.code == KeyCode::Up =>
                {
                    *selected = selected.saturating_sub(1);
                    false
                }
                KeyCode::Tab => {
                    if let Some(entry) = dir_entries.get(*selected).cloned() {
                        let current = input.value().to_string();
                        let base = if current.ends_with('/') {
                            current.clone()
                        } else {
                            current
                                .rsplit_once('/')
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
            let UiMode::FilePicker {
                ref input,
                ref dir_entries,
                selected,
            } = self.ui.mode
            else {
                return;
            };
            let Some(entry) = dir_entries.get(selected).cloned() else {
                return;
            };
            let current = input.value().to_string();
            let base = if current.ends_with('/') {
                current
            } else {
                current
                    .rsplit_once('/')
                    .map(|(prefix, _)| format!("{prefix}/"))
                    .unwrap_or_default()
            };
            (entry, base)
        };

        if entry.is_git_repo && !entry.is_added {
            let path = PathBuf::from(format!("{}{}", base, entry.name));
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            self.proto_commands
                .push(Command::AddRepo { path: canonical });
            self.ui.mode = UiMode::Normal;
        } else if entry.is_dir {
            let new_path = format!("{}{}/", base, entry.name);
            if let UiMode::FilePicker {
                ref mut input,
                ref mut selected,
                ..
            } = self.ui.mode
            {
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
            let len = if let UiMode::FilePicker {
                ref dir_entries, ..
            } = self.ui.mode
            {
                dir_entries.len()
            } else {
                return;
            };
            if row < len {
                if let UiMode::FilePicker {
                    ref mut selected, ..
                } = self.ui.mode
                {
                    *selected = row;
                }
                self.activate_dir_entry();
            }
        }
    }

    pub fn refresh_dir_listing(&mut self) {
        let Self { model, ui, .. } = self;
        let UiMode::FilePicker {
            ref input,
            ref mut dir_entries,
            ..
        } = ui.mode
        else {
            return;
        };

        let path_str = input.value().to_string();
        let dir = if path_str.ends_with('/') {
            PathBuf::from(&path_str)
        } else {
            PathBuf::from(&path_str)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
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
                    // Extract branch from DeleteInfo and send RemoveCheckout
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
