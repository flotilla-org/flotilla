use std::collections::{BTreeMap, HashSet};

use flotilla_protocol::{HostName, RepoIdentity, WorkItemIdentity};
use ratatui::layout::Rect;

use crate::status_bar::StatusBarTarget;

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
    pub is_added: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BranchInputKind {
    /// User is manually typing a branch name.
    #[default]
    Manual,
    /// AI is generating a branch name from issue context.
    Generating,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepoViewLayout {
    #[default]
    Auto,
    Zoom,
    Right,
    Below,
}

#[derive(Clone, Debug)]
pub enum PendingStatus {
    InFlight,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PendingAction {
    pub command_id: u64,
    pub status: PendingStatus,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct PendingActionContext {
    pub identity: WorkItemIdentity,
    pub description: String,
    pub repo_identity: RepoIdentity,
}

/// Identifies a clickable tab in the tab bar.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TabId {
    /// The main flotilla app tab (config/home).
    Flotilla,
    /// A repository tab, identified by index in repo_order.
    Repo(usize),
    /// The [+] button for adding repos.
    Add,
}

impl TabId {
    /// Label for the flotilla app tab.
    pub const FLOTILLA_LABEL: &str = " ⚓ flotilla ";
    /// Display width of the label (⚓ is 1 column, not 3 bytes).
    pub const FLOTILLA_LABEL_WIDTH: u16 = 13;
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
    pub is_config: bool,
    pub target_host: Option<HostName>,
    pub view_layout: RepoViewLayout,
    pub status_bar: StatusBarUiState,
    pub layout: LayoutAreas,
    pub show_debug: bool,
    pub help_scroll: u16,
}

impl UiState {
    pub fn new(_repo_ids: &[RepoIdentity]) -> Self {
        Self {
            is_config: false,
            target_host: None,
            view_layout: RepoViewLayout::default(),
            status_bar: StatusBarUiState::default(),
            layout: LayoutAreas::default(),
            show_debug: false,
            help_scroll: 0,
        }
    }

    pub fn cycle_layout(&mut self) {
        self.view_layout = match self.view_layout {
            RepoViewLayout::Auto => RepoViewLayout::Zoom,
            RepoViewLayout::Zoom => RepoViewLayout::Right,
            RepoViewLayout::Right => RepoViewLayout::Below,
            RepoViewLayout::Below => RepoViewLayout::Auto,
        };
    }

    /// Cycle through currently connected peer hosts, then back to local.
    ///
    /// If the current target is no longer present in `peer_hosts`, cycling
    /// restarts from the first available peer. Peer status updates are
    /// responsible for clearing a stale selection when the chosen host
    /// disconnects.
    pub fn cycle_target_host(&mut self, peer_hosts: &[HostName]) {
        self.target_host = match self.target_host.as_ref() {
            None => peer_hosts.first().cloned(),
            Some(current) => peer_hosts.iter().position(|host| host == current).and_then(|index| peer_hosts.get(index + 1).cloned()),
        };
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
        assert!(!state.is_config);
        assert!(!state.show_debug);
        assert_eq!(state.view_layout, RepoViewLayout::Auto);
    }

    #[test]
    fn ui_state_defaults_to_auto_layout() {
        let state = UiState::new(&[]);
        assert_eq!(state.view_layout, RepoViewLayout::Auto);
    }

    #[test]
    fn ui_state_defaults_to_showing_status_bar_keys() {
        let state = UiState::new(&[]);
        assert!(state.status_bar.show_keys);
    }

    #[test]
    fn ui_state_defaults_target_host_to_local() {
        let state = UiState::new(&[]);
        assert_eq!(state.target_host, None);
    }

    #[test]
    fn status_bar_ui_state_defaults_to_showing_keys() {
        assert!(StatusBarUiState::default().show_keys);
    }

    #[test]
    fn layout_cycles_auto_zoom_right_below_auto() {
        let mut state = UiState::new(&[]);

        state.cycle_layout();
        assert_eq!(state.view_layout, RepoViewLayout::Zoom);

        state.cycle_layout();
        assert_eq!(state.view_layout, RepoViewLayout::Right);

        state.cycle_layout();
        assert_eq!(state.view_layout, RepoViewLayout::Below);

        state.cycle_layout();
        assert_eq!(state.view_layout, RepoViewLayout::Auto);
    }

    #[test]
    fn cycle_target_host_advances_through_known_peers_and_back_to_local() {
        let mut state = UiState::new(&[]);
        let peers = vec![HostName::new("alpha"), HostName::new("beta")];

        state.cycle_target_host(&peers);
        assert_eq!(state.target_host, Some(HostName::new("alpha")));

        state.cycle_target_host(&peers);
        assert_eq!(state.target_host, Some(HostName::new("beta")));

        state.cycle_target_host(&peers);
        assert_eq!(state.target_host, None);
    }

    #[test]
    fn cycle_target_host_ignores_empty_peer_list() {
        let mut state = UiState::new(&[]);

        state.cycle_target_host(&[]);

        assert_eq!(state.target_host, None);
    }
}
