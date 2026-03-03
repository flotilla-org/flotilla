use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as InputEventHandler;

use crate::data::{DataStore, DeleteConfirmInfo, TableEntry, WorkItem, WorkItemKind};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Default, PartialEq)]
pub enum InputMode {
    #[default]
    Normal,
    BranchName,
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
    OpenPr(i64),
    OpenIssueBrowser(i64),
    ArchiveSession(usize),
    GenerateBranchName(Vec<usize>),
    /// Teleport into a web session (creates worktree + workspace as needed)
    TeleportSession { session_id: String, branch: Option<String>, worktree_idx: Option<usize> },
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
                if let Some(si) = app.selected_selectable_idx {
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
                    if let Some(pr) = app.data.prs.get(pr_idx) {
                        app.pending_action = PendingAction::OpenPr(pr.number);
                    }
                }
            }
            Action::OpenIssue => {
                if let Some(&issue_idx) = item.issue_idxs.first() {
                    if let Some(issue) = app.data.issues.get(issue_idx) {
                        app.pending_action = PendingAction::OpenIssueBrowser(issue.number);
                    }
                }
            }
            Action::TeleportSession => {
                if let Some(ses_idx) = item.session_idx {
                    if let Some(session) = app.data.sessions.get(ses_idx) {
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
    pub data: DataStore,
    pub repo_root: PathBuf,
    pub table_state: TableState,
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
    // Track which selectable index is selected (index into data.selectable_indices)
    selected_selectable_idx: Option<usize>,
    // Popup area for mouse hit-testing (set by UI render)
    pub menu_area: Rect,
    // Multi-select
    pub multi_selected: BTreeSet<usize>,
    // Double-click detection
    last_click_time: Option<Instant>,
    last_click_selectable_idx: Option<usize>,
    // Branch generation loading
    pub generating_branch: bool,
    // Transient status/error message (cleared on next action)
    pub status_message: Option<String>,
}

impl App {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            ..Default::default()
        }
    }

    pub async fn refresh_data(&mut self) -> Vec<String> {
        let errors = self.data.refresh(&self.repo_root).await;
        // Restore selection or pick first
        if self.data.selectable_indices.is_empty() {
            self.selected_selectable_idx = None;
            self.table_state.select(None);
        } else if self.selected_selectable_idx.is_none() {
            self.selected_selectable_idx = Some(0);
            self.table_state.select(Some(self.data.selectable_indices[0]));
        } else if let Some(si) = self.selected_selectable_idx {
            // Clamp to bounds
            let clamped = si.min(self.data.selectable_indices.len() - 1);
            self.selected_selectable_idx = Some(clamped);
            self.table_state.select(Some(self.data.selectable_indices[clamped]));
        }
        errors
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let table_idx = self.table_state.selected()?;
        match self.data.table_entries.get(table_idx)? {
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
        if self.input_mode == InputMode::BranchName {
            self.handle_input_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if !self.multi_selected.is_empty() {
                    self.multi_selected.clear();
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
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // When popups are open, intercept clicks
        if self.show_action_menu {
            self.handle_menu_mouse(mouse);
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
                        self.selected_selectable_idx = Some(si);
                        self.table_state.select(Some(self.data.selectable_indices[si]));
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

                    self.selected_selectable_idx = Some(si);
                    self.table_state.select(Some(self.data.selectable_indices[si]));

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
                    self.selected_selectable_idx = Some(si);
                    self.table_state.select(Some(self.data.selectable_indices[si]));
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
        // Click outside menu → close it
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            self.show_action_menu = false;
            return;
        }
        // Click inside menu → select and execute
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
            let offset = self.table_state.offset();
            let actual_row = data_row + offset;
            self.data
                .selectable_indices
                .iter()
                .position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    fn toggle_multi_select(&mut self) {
        if let Some(si) = self.selected_selectable_idx {
            if self.multi_selected.contains(&si) {
                self.multi_selected.remove(&si);
            } else {
                self.multi_selected.insert(si);
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
        if !self.multi_selected.is_empty() {
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
        for &si in &self.multi_selected {
            if let Some(&table_idx) = self.data.selectable_indices.get(si) {
                if let Some(TableEntry::Item(item)) = self.data.table_entries.get(table_idx) {
                    all_issue_idxs.extend(&item.issue_idxs);
                }
            }
        }
        // Include current selection too
        if let Some(si) = self.selected_selectable_idx {
            if !self.multi_selected.contains(&si) {
                if let Some(&table_idx) = self.data.selectable_indices.get(si) {
                    if let Some(TableEntry::Item(item)) = self.data.table_entries.get(table_idx) {
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
        self.multi_selected.clear();
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
        let indices = &self.data.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let next = match self.selected_selectable_idx {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si, // stay at end
            None => 0,
        };
        self.selected_selectable_idx = Some(next);
        self.table_state.select(Some(indices[next]));
    }

    fn select_prev(&mut self) {
        let indices = &self.data.selectable_indices;
        if indices.is_empty() {
            return;
        }
        let prev = match self.selected_selectable_idx {
            Some(si) if si > 0 => si - 1,
            Some(si) => si, // stay at start
            None => 0,
        };
        self.selected_selectable_idx = Some(prev);
        self.table_state.select(Some(indices[prev]));
    }
}
