use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use flotilla_protocol::{HostName, IssueRef, ProvisioningTarget, RepoIdentity, ViewAddress};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::{
    status_bar::StatusBarTarget,
    table_view::{PendingRowContext, RowId},
};

#[derive(Clone, bon::Builder)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_git_repo: bool,
    pub is_added: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingStatus {
    Submitting,
    InFlight { command_id: u64 },
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PendingActionContext {
    pub description: String,
    pub target: PendingActionTarget,
}

impl PendingActionContext {
    pub fn table_row(target: PendingRowContext, description: String) -> Self {
        Self { description, target: PendingActionTarget::TableRow(target) }
    }

    pub fn project_issue_start(target: ProjectIssueStartContext, description: String) -> Self {
        Self { description, target: PendingActionTarget::ProjectIssueStart(target) }
    }

    pub fn project_issue_start_context(&self) -> Option<&ProjectIssueStartContext> {
        match &self.target {
            PendingActionTarget::ProjectIssueStart(context) => Some(context),
            _ => None,
        }
    }

    pub fn table_row_context(&self) -> Option<&PendingRowContext> {
        match &self.target {
            PendingActionTarget::TableRow(context) => Some(context),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PendingActionTarget {
    TableRow(PendingRowContext),
    ProjectIssueStart(ProjectIssueStartContext),
}

#[derive(Clone, Debug)]
pub struct ProjectIssueStartContext {
    pub address: ViewAddress,
    pub row_id: RowId,
    pub issue: IssueRef,
    pub batch_id: u64,
}

/// Identifies a clickable segment in the tab bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TabId {
    /// An open View, by index into `App::views`.
    View(usize),
    /// The [+] button for adding repos.
    Add,
}

#[derive(Default)]
pub struct LayoutAreas {
    pub table_area: Rect,
    pub menu_area: Rect,
    pub tab_areas: BTreeMap<TabId, Rect>,
    pub status_bar: StatusBarLayout,
    pub file_picker_area: Rect,
    pub file_picker_list_area: Rect,
}

#[derive(Default)]
pub struct StatusBarLayout {
    pub area: Rect,
    pub key_targets: Vec<StatusBarTarget>,
    pub dismiss_targets: Vec<StatusBarTarget>,
}

pub struct StatusBarUiState {
    pub show_keys: bool,
    pub dismissed_status_ids: HashSet<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationKind {
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: u64,
    pub kind: NotificationKind,
    pub text: String,
}

/// In-session message history, independent of whichever surface renders it.
const MAX_NOTIFICATION_HISTORY: usize = 100;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Notifications {
    entries: Vec<Notification>,
    next_id: u64,
    pub expanded: bool,
    selected: usize,
}

impl Notifications {
    pub fn push(&mut self, kind: NotificationKind, text: String) {
        if self.entries.first().is_some_and(|entry| entry.kind == kind && entry.text == text) {
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.insert(0, Notification { id, kind, text });
        self.entries.truncate(MAX_NOTIFICATION_HISTORY);
        self.selected = 0;
    }

    pub fn entries(&self) -> &[Notification] {
        &self.entries
    }

    pub fn latest(&self) -> Option<&Notification> {
        self.entries.first()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.entries.len().saturating_sub(1));
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn clear_selected(&mut self) {
        if self.selected < self.entries.len() {
            self.entries.remove(self.selected);
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
            if self.entries.is_empty() {
                self.expanded = false;
            }
        }
    }

    pub fn clear_all(&mut self) {
        self.entries.clear();
        self.selected = 0;
        self.expanded = false;
    }
}

impl Default for StatusBarUiState {
    fn default() -> Self {
        Self { show_keys: true, dismissed_status_ids: HashSet::new() }
    }
}

#[derive(Default)]
pub struct DragState {
    pub dragging_tab: Option<usize>,
    pub start_x: u16,
    pub active: bool,
}

pub struct UiState {
    pub provisioning_target: ProvisioningTarget,
    pub status_bar: StatusBarUiState,
    pub notifications: Notifications,
    pub layout: LayoutAreas,
    pub show_debug: bool,
    pub help_scroll: u16,
}

impl UiState {
    pub fn new(_repo_ids: &[RepoIdentity]) -> Self {
        Self {
            provisioning_target: ProvisioningTarget::Host { host: HostName::local() },
            status_bar: StatusBarUiState::default(),
            notifications: Notifications::default(),
            layout: LayoutAreas::default(),
            show_debug: false,
            help_scroll: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostName;

    use super::*;

    // ── UiState::new tests ────────────────────────────────────────────

    #[test]
    fn new_with_empty_paths() {
        let state = UiState::new(&[]);
        assert!(!state.show_debug);
    }

    #[test]
    fn ui_state_defaults_to_showing_status_bar_keys() {
        let state = UiState::new(&[]);
        assert!(state.status_bar.show_keys);
    }

    #[test]
    fn ui_state_defaults_provisioning_target_to_local_host() {
        let state = UiState::new(&[]);
        assert_eq!(state.provisioning_target, ProvisioningTarget::Host { host: HostName::local() });
    }

    #[test]
    fn status_bar_ui_state_defaults_to_showing_keys() {
        assert!(StatusBarUiState::default().show_keys);
    }

    #[test]
    fn notifications_keep_results_until_cleared() {
        let mut state = UiState::new(&[]);
        state.notifications.push(NotificationKind::Info, "Convoy created".into());
        state.notifications.push(NotificationKind::Error, "Dispatch failed".into());

        assert_eq!(state.notifications.entries().len(), 2);
        assert_eq!(state.notifications.latest().expect("latest notification").text, "Dispatch failed");
        state.notifications.clear_selected();
        assert_eq!(state.notifications.latest().expect("remaining notification").text, "Convoy created");
        state.notifications.clear_all();
        assert!(state.notifications.entries().is_empty());
    }

    #[test]
    fn repeating_the_same_notification_does_not_fill_history() {
        let mut notifications = Notifications::default();
        notifications.push(NotificationKind::Error, "Host unavailable".into());
        notifications.push(NotificationKind::Error, "Host unavailable".into());
        notifications.push(NotificationKind::Info, "Host unavailable".into());
        assert_eq!(notifications.entries().len(), 2);
    }

    #[test]
    fn notification_history_keeps_the_newest_hundred_entries() {
        let mut notifications = Notifications::default();
        for number in 0..101 {
            notifications.push(NotificationKind::Info, format!("result {number}"));
        }
        assert_eq!(notifications.entries().len(), 100);
        assert_eq!(notifications.latest().expect("latest result").text, "result 100");
        assert_eq!(notifications.entries().last().expect("oldest retained result").text, "result 1");
    }

    #[test]
    fn notification_selection_clamps_and_closes_when_the_last_entry_is_cleared() {
        let mut notifications = Notifications::default();
        notifications.push(NotificationKind::Info, "old".into());
        notifications.push(NotificationKind::Info, "new".into());
        notifications.expanded = true;
        notifications.select_next();
        notifications.select_next();
        assert_eq!(notifications.selected(), 1);
        notifications.clear_selected();
        assert_eq!(notifications.selected(), 0);
        notifications.select_previous();
        assert_eq!(notifications.selected(), 0);
        notifications.clear_selected();
        assert!(!notifications.expanded);
    }
}
