use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as InputEventHandler;

use crate::data::{DataStore, DeleteConfirmInfo, TableEntry, WorkItem, WorkItemKind};
use crate::providers::registry::ProviderRegistry;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Default, PartialEq)]
pub enum InputMode {
    #[default]
    Normal,
    BranchName,
    AddRepo,
}

pub struct RepoState {
    #[allow(dead_code)]
    pub repo_root: PathBuf,
    pub registry: ProviderRegistry,
    pub data: DataStore,
    pub table_state: TableState,
    pub selected_selectable_idx: Option<usize>,
    pub has_unseen_changes: bool,
    pub multi_selected: BTreeSet<usize>,
}

impl RepoState {
    pub fn new(repo_root: PathBuf, registry: ProviderRegistry) -> Self {
        Self {
            repo_root,
            registry,
            data: DataStore::default(),
            table_state: TableState::default(),
            selected_selectable_idx: None,
            has_unseen_changes: false,
            multi_selected: BTreeSet::new(),
        }
    }

    /// Snapshot for change detection: (worktrees, change_requests, sessions, branches, issues)
    pub fn data_snapshot(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.data.worktrees.len(),
            self.data.change_requests.len(),
            self.data.sessions.len(),
            self.data.remote_branches.len(),
            self.data.issues.len(),
        )
    }
}

pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
    pub is_added: bool,
}

impl Clone for DirEntry {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            is_dir: self.is_dir,
            is_git_repo: self.is_git_repo,
            is_added: self.is_added,
        }
    }
}

#[derive(Default)]
pub enum PendingAction {
    #[default]
    None,
    SwitchWorktree(usize),
    SelectWorkspace(String),
    CreateWorktree(String),
    FetchDeleteInfo(usize),
    ConfirmDelete,
    OpenPr(String),
    OpenIssueBrowser(String),
    ArchiveSession(usize),
    GenerateBranchName(Vec<usize>),
    /// Teleport into a web session (creates worktree + workspace as needed)
    TeleportSession { session_id: String, branch: Option<String>, worktree_idx: Option<usize> },
    AddRepo(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SwitchToWorkspace,
    CreateWorkspace,
    RemoveWorktree,
    CreateWorktreeAndWorkspace,
    GenerateBranchName,
    OpenPr,
    OpenIssue,
    TeleportSession,
    ArchiveSession,
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::SwitchToWorkspace => "Switch to workspace",
            Action::CreateWorkspace => "Create workspace",
            Action::RemoveWorktree => "Remove worktree",
            Action::CreateWorktreeAndWorkspace => "Create worktree + workspace",
            Action::GenerateBranchName => "Generate branch name",
            Action::OpenPr => "Open PR in browser",
            Action::OpenIssue => "Open issue in browser",
            Action::TeleportSession => "Teleport session",
            Action::ArchiveSession => "Archive session",
        }
    }

    pub fn is_available(&self, item: &WorkItem) -> bool {
        match self {
            Action::SwitchToWorkspace => !item.workspace_refs.is_empty(),
            Action::CreateWorkspace => item.worktree_idx.is_some() && item.workspace_refs.is_empty(),
            Action::RemoveWorktree => item.worktree_idx.is_some() && !item.is_main_worktree,
            Action::CreateWorktreeAndWorkspace => item.worktree_idx.is_none() && item.branch.is_some(),
            Action::GenerateBranchName => item.branch.is_none() && !item.issue_idxs.is_empty(),
            Action::OpenPr => item.pr_idx.is_some(),
            Action::OpenIssue => !item.issue_idxs.is_empty(),
            Action::TeleportSession => item.session_idx.is_some(),
            Action::ArchiveSession => item.session_idx.is_some(),
        }
    }

    pub fn shortcut_hint(&self) -> Option<&'static str> {
        match self {
            Action::RemoveWorktree => Some("d:remove"),
            Action::OpenPr => Some("p:show PR"),
            _ => None,
        }
    }

    pub fn dispatch(&self, item: &WorkItem, app: &mut App) {
        match self {
            Action::SwitchToWorkspace => {
                if let Some(ws_ref) = item.workspace_refs.first() {
                    app.pending_action = PendingAction::SelectWorkspace(ws_ref.clone());
                }
            }
            Action::CreateWorkspace => {
                if let Some(wt_idx) = item.worktree_idx {
                    app.pending_action = PendingAction::SwitchWorktree(wt_idx);
                }
            }
            Action::RemoveWorktree => {
                if item.kind != WorkItemKind::Worktree || item.is_main_worktree {
                    return;
                }
                if let Some(si) = app.active().selected_selectable_idx {
                    app.delete_confirm_loading = true;
                    app.show_delete_confirm = true;
                    app.pending_action = PendingAction::FetchDeleteInfo(si);
                }
            }
            Action::CreateWorktreeAndWorkspace => {
                if let Some(branch) = &item.branch {
                    app.pending_action = PendingAction::CreateWorktree(branch.clone());
                }
            }
            Action::GenerateBranchName => {
                if !item.issue_idxs.is_empty() {
                    app.generating_branch = true;
                    app.pending_action = PendingAction::GenerateBranchName(item.issue_idxs.clone());
                }
            }
            Action::OpenPr => {
                if let Some(pr_idx) = item.pr_idx {
                    if let Some(cr) = app.active().data.change_requests.get(pr_idx) {
                        app.pending_action = PendingAction::OpenPr(cr.id.clone());
                    }
                }
            }
            Action::OpenIssue => {
                if let Some(&issue_idx) = item.issue_idxs.first() {
                    if let Some(issue) = app.active().data.issues.get(issue_idx) {
                        app.pending_action = PendingAction::OpenIssueBrowser(issue.id.clone());
                    }
                }
            }
            Action::TeleportSession => {
                if let Some(ses_idx) = item.session_idx {
                    if let Some(session) = app.active().data.sessions.get(ses_idx) {
                        app.pending_action = PendingAction::TeleportSession {
                            session_id: session.id.clone(),
                            branch: item.branch.clone(),
                            worktree_idx: item.worktree_idx,
                        };
                    }
                }
            }
            Action::ArchiveSession => {
                if let Some(ses_idx) = item.session_idx {
                    app.pending_action = PendingAction::ArchiveSession(ses_idx);
                }
            }
        }
    }

    pub fn all_in_menu_order() -> &'static [Action] {
        &[
            Action::SwitchToWorkspace,
            Action::CreateWorkspace,
            Action::RemoveWorktree,
            Action::CreateWorktreeAndWorkspace,
            Action::GenerateBranchName,
            Action::OpenPr,
            Action::OpenIssue,
            Action::TeleportSession,
            Action::ArchiveSession,
        ]
    }

    pub fn enter_priority() -> &'static [Action] {
        &[
            Action::SwitchToWorkspace,
            Action::TeleportSession,
            Action::CreateWorkspace,
            Action::CreateWorktreeAndWorkspace,
            Action::GenerateBranchName,
        ]
    }
}

#[derive(Default)]
pub struct App {
    pub should_quit: bool,
    pub repos: HashMap<PathBuf, RepoState>,
    pub repo_order: Vec<PathBuf>,
    pub active_repo: usize,
    pub pending_action: PendingAction,
    pub show_action_menu: bool,
    pub action_menu_items: Vec<Action>,
    pub action_menu_index: usize,
    pub input_mode: InputMode,
    pub input: Input,
    pub show_help: bool,
    pub table_area: Rect,
    // Delete confirmation
    pub show_delete_confirm: bool,
    pub delete_confirm_info: Option<DeleteConfirmInfo>,
    pub delete_confirm_loading: bool,
    // Popup area for mouse hit-testing (set by UI render)
    pub menu_area: Rect,
    // Tab bar areas for mouse hit-testing (set by UI render)
    pub tab_areas: Vec<Rect>,
    pub add_tab_area: Rect,
    // Double-click detection
    last_click_time: Option<Instant>,
    last_click_selectable_idx: Option<usize>,
    // Branch generation loading
    pub generating_branch: bool,
    // Transient status/error message (cleared on next action)
    pub status_message: Option<String>,
    // File picker state
    pub dir_entries: Vec<DirEntry>,
    pub dir_selected: usize,
    pub file_picker_area: Rect,
    pub file_picker_list_area: Rect,
}

impl App {
    pub fn new(repos: Vec<PathBuf>) -> Self {
        let mut map = HashMap::new();
        let mut order = Vec::new();
        for path in repos {
            if !map.contains_key(&path) {
                let registry = crate::providers::discovery::detect_providers(&path);
                map.insert(path.clone(), RepoState::new(path.clone(), registry));
                order.push(path);
            }
        }
        Self {
            repos: map,
            repo_order: order,
            ..Default::default()
        }
    }

    /// Reference to the active repo state.
    pub fn active(&self) -> &RepoState {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    /// Mutable reference to the active repo state.
    pub fn active_mut(&mut self) -> &mut RepoState {
        let key = &self.repo_order[self.active_repo];
        self.repos.get_mut(key).unwrap()
    }

    /// Reference to the active repo's provider registry.
    #[allow(dead_code)]
    pub fn active_registry(&self) -> &ProviderRegistry {
        &self.active().registry
    }

    /// Path of the active repo.
    pub fn active_repo_root(&self) -> &PathBuf {
        &self.repo_order[self.active_repo]
    }

    /// Repo display name (directory basename).
    pub fn repo_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    pub fn add_repo(&mut self, path: PathBuf) {
        if !self.repos.contains_key(&path) {
            let registry = crate::providers::discovery::detect_providers(&path);
            self.repos.insert(path.clone(), RepoState::new(path.clone(), registry));
            self.repo_order.push(path);
        }
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.repo_order.len() {
            self.active_repo = idx;
            let key = &self.repo_order[idx];
            self.repos.get_mut(key).unwrap().has_unseen_changes = false;
        }
    }

    pub fn next_tab(&mut self) {
        if !self.repo_order.is_empty() {
            self.switch_tab((self.active_repo + 1) % self.repo_order.len());
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.repo_order.is_empty() {
            self.switch_tab(
                self.active_repo
                    .checked_sub(1)
                    .unwrap_or(self.repo_order.len() - 1),
            );
        }
    }

    #[allow(dead_code)]
    pub async fn refresh_data(&mut self) -> Vec<String> {
        let key = self.repo_order[self.active_repo].clone();
        let rs = self.repos.get_mut(&key).unwrap();
        let mut ds = std::mem::take(&mut rs.data);
        let reg = std::mem::take(&mut rs.registry);
        let errors = ds.refresh(&key, &reg).await;
        let rs = self.repos.get_mut(&key).unwrap();
        rs.registry = reg;
        rs.data = ds;
        // Restore selection or pick first
        if rs.data.selectable_indices.is_empty() {
            rs.selected_selectable_idx = None;
            rs.table_state.select(None);
        } else if rs.selected_selectable_idx.is_none() {
            rs.selected_selectable_idx = Some(0);
            rs.table_state.select(Some(rs.data.selectable_indices[0]));
        } else if let Some(si) = rs.selected_selectable_idx {
            let clamped = si.min(rs.data.selectable_indices.len() - 1);
            rs.selected_selectable_idx = Some(clamped);
            rs.table_state.select(Some(rs.data.selectable_indices[clamped]));
        }
        errors
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let rs = self.active();
        let table_idx = rs.table_state.selected()?;
        match rs.data.table_entries.get(table_idx)? {
            TableEntry::Item(item) => Some(item),
            TableEntry::Header(_) => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Help toggle works everywhere
        if key.code == KeyCode::Char('?') {
            self.show_help = !self.show_help;
            return;
        }
        if self.show_help {
            if key.code == KeyCode::Esc {
                self.show_help = false;
            }
            return;
        }
        if self.show_delete_confirm {
            self.handle_delete_confirm_key(key);
            return;
        }
        if self.show_action_menu {
            self.handle_menu_key(key);
            return;
        }
        if self.input_mode == InputMode::AddRepo {
            self.handle_add_repo_key(key);
            return;
        }
        if self.input_mode == InputMode::BranchName {
            self.handle_input_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if !self.active().multi_selected.is_empty() {
                    self.active_mut().multi_selected.clear();
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
                self.input_mode = InputMode::BranchName;
                self.input.reset();
            }
            KeyCode::Char('d') => self.dispatch_if_available(Action::RemoveWorktree),
            KeyCode::Char('p') => self.dispatch_if_available(Action::OpenPr),
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            KeyCode::Char('a') => {
                self.input_mode = InputMode::AddRepo;
                self.input.reset();
                // Pre-fill with parent of active repo
                if let Some(parent) = self.active_repo_root().parent() {
                    let parent_str = format!("{}/", parent.display());
                    self.input = Input::from(parent_str.as_str());
                }
                self.dir_entries = Vec::new();
                self.dir_selected = 0;
                self.refresh_dir_listing();
            }
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // When popups are open, intercept clicks
        if self.show_action_menu {
            self.handle_menu_mouse(mouse);
            return;
        }
        if self.input_mode == InputMode::AddRepo {
            self.handle_file_picker_mouse(mouse);
            return;
        }
        if self.show_help || self.show_delete_confirm || self.generating_branch
            || self.input_mode == InputMode::BranchName
        {
            return; // ignore mouse when other popups are open
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    // Shift+Click: toggle multi-select
                    if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                        let table_idx = self.active().data.selectable_indices[si];
                        self.active_mut().selected_selectable_idx = Some(si);
                        self.active_mut().table_state.select(Some(table_idx));
                        self.toggle_multi_select();
                    }
                    return;
                }

                if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                    let now = Instant::now();
                    let is_double_click = self
                        .last_click_time
                        .map(|t| now.duration_since(t).as_millis() < 400)
                        .unwrap_or(false)
                        && self.last_click_selectable_idx == Some(si);

                    let table_idx = self.active().data.selectable_indices[si];
                    self.active_mut().selected_selectable_idx = Some(si);
                    self.active_mut().table_state.select(Some(table_idx));

                    if is_double_click {
                        self.action_enter();
                        self.last_click_time = None;
                        self.last_click_selectable_idx = None;
                    } else {
                        self.last_click_time = Some(now);
                        self.last_click_selectable_idx = Some(si);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(si) = self.row_at_mouse(mouse.column, mouse.row) {
                    let table_idx = self.active().data.selectable_indices[si];
                    self.active_mut().selected_selectable_idx = Some(si);
                    self.active_mut().table_state.select(Some(table_idx));
                    self.open_action_menu();
                }
            }
            MouseEventKind::ScrollDown => self.select_next(),
            MouseEventKind::ScrollUp => self.select_prev(),
            _ => {}
        }
    }

    fn handle_menu_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let x = mouse.column;
        let y = mouse.row;
        let a = self.menu_area;
        // Click outside menu -> close it
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.show_action_menu = false;
            return;
        }
        // Click inside menu -> select and execute
        // Account for border (1 row top)
        let row = (y - a.y) as usize;
        if row < 1 {
            return; // border
        }
        let item_idx = row - 1;
        if item_idx < self.action_menu_items.len() {
            self.action_menu_index = item_idx;
            self.execute_menu_action();
            self.show_action_menu = false;
        }
    }

    fn row_at_mouse(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.table_area.x
            && x < self.table_area.x + self.table_area.width
            && y >= self.table_area.y
            && y < self.table_area.y + self.table_area.height
        {
            let row_in_table = (y - self.table_area.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let offset = self.active().table_state.offset();
            let actual_row = data_row + offset;
            self.active()
                .data
                .selectable_indices
                .iter()
                .position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    fn toggle_multi_select(&mut self) {
        if let Some(si) = self.active().selected_selectable_idx {
            if self.active().multi_selected.contains(&si) {
                self.active_mut().multi_selected.remove(&si);
            } else {
                self.active_mut().multi_selected.insert(si);
            }
        }
    }

    pub fn prefill_branch_input(&mut self, branch_name: &str) {
        self.input = Input::from(branch_name);
        self.input_mode = InputMode::BranchName;
        self.generating_branch = false;
    }

    fn action_enter(&mut self) {
        // Multi-select flow: combine selected issues
        if !self.active().multi_selected.is_empty() {
            self.action_enter_multi_select();
            return;
        }

        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        for &action in Action::enter_priority() {
            if action.is_available(&item) {
                action.dispatch(&item, self);
                return;
            }
        }
    }

    fn action_enter_multi_select(&mut self) {
        let mut all_issue_idxs: Vec<usize> = Vec::new();
        let multi_selected: BTreeSet<usize> = self.active().multi_selected.clone();
        for &si in &multi_selected {
            if let Some(&table_idx) = self.active().data.selectable_indices.get(si) {
                if let Some(TableEntry::Item(item)) = self.active().data.table_entries.get(table_idx) {
                    all_issue_idxs.extend(&item.issue_idxs);
                }
            }
        }
        // Include current selection too
        if let Some(si) = self.active().selected_selectable_idx {
            if !multi_selected.contains(&si) {
                if let Some(&table_idx) = self.active().data.selectable_indices.get(si) {
                    if let Some(TableEntry::Item(item)) = self.active().data.table_entries.get(table_idx) {
                        all_issue_idxs.extend(&item.issue_idxs);
                    }
                }
            }
        }
        all_issue_idxs.sort();
        all_issue_idxs.dedup();
        if !all_issue_idxs.is_empty() {
            self.generating_branch = true;
            self.pending_action = PendingAction::GenerateBranchName(all_issue_idxs);
        }
        self.active_mut().multi_selected.clear();
    }

    fn dispatch_if_available(&mut self, action: Action) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        if action.is_available(&item) {
            action.dispatch(&item, self);
        }
    }

    fn open_action_menu(&mut self) {
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };

        let items: Vec<Action> = Action::all_in_menu_order()
            .iter()
            .copied()
            .filter(|a| a.is_available(&item))
            .collect();

        if items.is_empty() {
            return;
        }

        self.action_menu_items = items;
        self.action_menu_index = 0;
        self.show_action_menu = true;
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.show_action_menu = false,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.action_menu_index < self.action_menu_items.len().saturating_sub(1) {
                    self.action_menu_index += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.action_menu_index = self.action_menu_index.saturating_sub(1);
            }
            KeyCode::Enter => {
                self.execute_menu_action();
                self.show_action_menu = false;
            }
            _ => {}
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input.reset();
            }
            KeyCode::Enter => {
                let branch = self.input.value().to_string();
                if !branch.is_empty() {
                    self.pending_action = PendingAction::CreateWorktree(branch);
                }
                self.input_mode = InputMode::Normal;
                self.input.reset();
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
            }
        }
    }

    fn handle_add_repo_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input.reset();
                self.dir_entries.clear();
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() || key.code == KeyCode::Down => {
                if !self.dir_entries.is_empty() {
                    self.dir_selected = (self.dir_selected + 1).min(self.dir_entries.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() || key.code == KeyCode::Up => {
                self.dir_selected = self.dir_selected.saturating_sub(1);
            }
            KeyCode::Tab => {
                // Complete selected entry into input
                if let Some(entry) = self.dir_entries.get(self.dir_selected) {
                    let current = self.input.value().to_string();
                    // Find the directory prefix
                    let base = if current.ends_with('/') {
                        current.clone()
                    } else {
                        // Go up to last /
                        current.rsplit_once('/')
                            .map(|(prefix, _)| format!("{prefix}/"))
                            .unwrap_or_default()
                    };
                    let new_path = format!("{}{}/", base, entry.name);
                    self.input = Input::from(new_path.as_str());
                    self.dir_selected = 0;
                    self.refresh_dir_listing();
                }
            }
            KeyCode::Enter => {
                self.activate_dir_entry();
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
                self.dir_selected = 0;
                self.refresh_dir_listing();
            }
        }
    }

    fn activate_dir_entry(&mut self) {
        if let Some(entry) = self.dir_entries.get(self.dir_selected).cloned() {
            let current = self.input.value().to_string();
            let base = if current.ends_with('/') {
                current
            } else {
                current.rsplit_once('/')
                    .map(|(prefix, _)| format!("{prefix}/"))
                    .unwrap_or_default()
            };
            if entry.is_git_repo && !entry.is_added {
                let path = PathBuf::from(format!("{}{}", base, entry.name));
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                self.pending_action = PendingAction::AddRepo(canonical);
                self.input_mode = InputMode::Normal;
                self.input.reset();
                self.dir_entries.clear();
            } else if entry.is_dir {
                let new_path = format!("{}{}/", base, entry.name);
                self.input = Input::from(new_path.as_str());
                self.dir_selected = 0;
                self.refresh_dir_listing();
            }
        }
    }

    pub fn handle_file_picker_mouse(&mut self, mouse: MouseEvent) {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let x = mouse.column;
        let y = mouse.row;
        let a = self.file_picker_area;
        // Click outside picker → close it
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.input_mode = InputMode::Normal;
            self.input.reset();
            self.dir_entries.clear();
            return;
        }
        // Click in the list area → select and activate
        let la = self.file_picker_list_area;
        if x >= la.x && x < la.x + la.width && y >= la.y && y < la.y + la.height {
            let row = (y - la.y) as usize;
            if row < self.dir_entries.len() {
                self.dir_selected = row;
                self.activate_dir_entry();
            }
        }
    }

    pub fn refresh_dir_listing(&mut self) {
        let path_str = self.input.value().to_string();
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
                    continue; // skip hidden
                }
                if !filter.is_empty() && !name.to_lowercase().starts_with(&filter) {
                    continue;
                }
                let path = entry.path();
                let is_dir = path.is_dir();
                if !is_dir {
                    continue; // only show directories
                }
                let is_git_repo = path.join(".git").exists();
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                let is_added = self.repos.contains_key(&canonical);
                entries.push(DirEntry {
                    name,
                    is_dir,
                    is_git_repo,
                    is_added,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.dir_entries = entries;
    }

    fn handle_delete_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if !self.delete_confirm_loading {
                    self.pending_action = PendingAction::ConfirmDelete;
                    self.show_delete_confirm = false;
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.show_delete_confirm = false;
                self.delete_confirm_info = None;
            }
            _ => {}
        }
    }

    fn execute_menu_action(&mut self) {
        let Some(&action) = self.action_menu_items.get(self.action_menu_index) else {
            return;
        };
        let Some(item) = self.selected_work_item().cloned() else {
            return;
        };
        action.dispatch(&item, self);
    }

    pub fn take_pending_action(&mut self) -> PendingAction {
        std::mem::take(&mut self.pending_action)
    }

    fn select_next(&mut self) {
        let indices = &self.active().data.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active().selected_selectable_idx;
        let next = match current_si {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si, // stay at end
            None => 0,
        };
        let table_idx = self.active().data.selectable_indices[next];
        self.active_mut().selected_selectable_idx = Some(next);
        self.active_mut().table_state.select(Some(table_idx));
    }

    fn select_prev(&mut self) {
        let indices = &self.active().data.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let current_si = self.active().selected_selectable_idx;
        let prev = match current_si {
            Some(si) if si > 0 => si - 1,
            Some(si) => si, // stay at start
            None => 0,
        };
        let table_idx = self.active().data.selectable_indices[prev];
        self.active_mut().selected_selectable_idx = Some(prev);
        self.active_mut().table_state.select(Some(table_idx));
    }
}
