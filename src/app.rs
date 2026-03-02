use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::ListState;
use strum::{Display, EnumIter, FromRepr, IntoEnumIterator};

use crate::data::DataStore;
use std::path::PathBuf;

#[derive(Default)]
pub enum PendingAction {
    #[default]
    None,
    SwitchWorktree(usize),
    CreateWorktree(String),
    RemoveWorktree(usize),
    OpenPr(i64),
    Refresh,
}

#[derive(Default, Clone, Copy, Display, FromRepr, EnumIter, PartialEq)]
pub enum Tab {
    #[default]
    #[strum(to_string = "Worktrees")]
    Worktrees,
    #[strum(to_string = "PRs")]
    Prs,
    #[strum(to_string = "Issues")]
    Issues,
    #[strum(to_string = "Sessions")]
    Sessions,
}

impl Tab {
    pub fn next(self) -> Self {
        let i = (self as usize + 1) % Self::iter().count();
        Self::from_repr(i).unwrap_or(self)
    }
    pub fn prev(self) -> Self {
        let count = Self::iter().count();
        let i = (self as usize + count - 1) % count;
        Self::from_repr(i).unwrap_or(self)
    }
}

#[derive(Default)]
pub struct App {
    pub should_quit: bool,
    pub current_tab: Tab,
    pub data: DataStore,
    pub repo_root: PathBuf,
    pub list_state: ListState,
    pub pending_action: PendingAction,
}

impl App {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            ..Default::default()
        }
    }

    pub async fn refresh_data(&mut self) {
        self.data.refresh(&self.repo_root).await;
        if self.list_state.selected().is_none() && self.current_list_len() > 0 {
            self.list_state.select(Some(0));
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => {
                self.current_tab = self.current_tab.next();
                self.list_state.select(Some(0));
            }
            KeyCode::BackTab => {
                self.current_tab = self.current_tab.prev();
                self.list_state.select(Some(0));
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('r') => {} // refresh handled in main loop
            KeyCode::Enter => {
                if let Some(i) = self.list_state.selected() {
                    match self.current_tab {
                        Tab::Worktrees => self.pending_action = PendingAction::SwitchWorktree(i),
                        Tab::Prs => {
                            if let Some(pr) = self.data.prs.get(i) {
                                self.pending_action = PendingAction::OpenPr(pr.number);
                            }
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Char('d') if self.current_tab == Tab::Worktrees => {
                if let Some(i) = self.list_state.selected() {
                    self.pending_action = PendingAction::RemoveWorktree(i);
                }
            }
            KeyCode::Char('p') => {
                if let Some(i) = self.list_state.selected() {
                    match self.current_tab {
                        Tab::Worktrees => {
                            // Find PR for this worktree's branch
                            if let Some(wt) = self.data.worktrees.get(i) {
                                if let Some(pr) = self.data.prs.iter().find(|pr| pr.head_ref_name == wt.branch) {
                                    self.pending_action = PendingAction::OpenPr(pr.number);
                                }
                            }
                        }
                        Tab::Prs => {
                            if let Some(pr) = self.data.prs.get(i) {
                                self.pending_action = PendingAction::OpenPr(pr.number);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    pub fn take_pending_action(&mut self) -> PendingAction {
        std::mem::take(&mut self.pending_action)
    }

    pub fn tick(&mut self) {
        // Will be used for auto-refresh
    }

    fn select_next(&mut self) {
        if self.current_list_len() > 0 {
            self.list_state.select_next();
        }
    }

    fn select_prev(&mut self) {
        if self.current_list_len() > 0 {
            self.list_state.select_previous();
        }
    }

    fn current_list_len(&self) -> usize {
        match self.current_tab {
            Tab::Worktrees => self.data.worktrees.len(),
            Tab::Prs => self.data.prs.len(),
            Tab::Issues => self.data.issues.len(),
            Tab::Sessions => 0,
        }
    }
}
