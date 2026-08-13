use std::collections::{BTreeMap, HashSet};

use flotilla_protocol::{HostName, IssueRef, ProvisioningTarget, RepoIdentity, ViewAddress};
use ratatui::layout::Rect;

use crate::{
    status_bar::StatusBarTarget,
    table_view::{PendingRowContext, RowId},
};

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
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
    pub layout: LayoutAreas,
    pub show_debug: bool,
    pub help_scroll: u16,
    /// Transient echo of the last dispatched command, shown dim at the left of the status bar.
    /// Examples: `"cr 42 open"`, `"/cr 42 ?"`.
    pub command_echo: Option<String>,
}

impl UiState {
    pub fn new(_repo_ids: &[RepoIdentity]) -> Self {
        Self {
            provisioning_target: ProvisioningTarget::Host { host: HostName::local() },
            status_bar: StatusBarUiState::default(),
            layout: LayoutAreas::default(),
            show_debug: false,
            help_scroll: 0,
            command_echo: None,
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
}
