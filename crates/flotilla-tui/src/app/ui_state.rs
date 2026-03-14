use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

use flotilla_core::data::{GroupEntry, GroupedWorkItems};
use flotilla_protocol::{CheckoutStatus, HostName, RepoIdentity, WorkItemIdentity};
use ratatui::{layout::Rect, widgets::TableState};
use tui_input::Input;

use super::intent::Intent;
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

#[derive(Default)]
pub enum UiMode {
    #[default]
    Normal,
    Help,
    Config,
    ActionMenu {
        items: Vec<Intent>,
        index: usize,
    },
    BranchInput {
        input: Input,
        kind: BranchInputKind,
        /// Issue IDs to link to the branch when created (provider_name, issue_id).
        pending_issue_ids: Vec<(String, String)>,
    },
    FilePicker {
        input: Input,
        dir_entries: Vec<DirEntry>,
        selected: usize,
    },
    DeleteConfirm {
        info: Option<CheckoutStatus>,
        loading: bool,
        terminal_keys: Vec<flotilla_protocol::ManagedTerminalId>,
        identity: WorkItemIdentity,
    },
    CloseConfirm {
        id: String,
        title: String,
        identity: WorkItemIdentity,
        command: flotilla_protocol::Command,
    },
    IssueSearch {
        input: Input,
    },
}

impl UiMode {
    pub fn is_config(&self) -> bool {
        matches!(self, UiMode::Config)
    }
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

/// Per-repo UI state (selection, table widget state, visual flags).
#[derive(Default)]
pub struct RepoUiState {
    pub table_view: GroupedWorkItems,
    pub table_state: TableState,
    pub selected_selectable_idx: Option<usize>,
    pub has_unseen_changes: bool,
    pub multi_selected: HashSet<WorkItemIdentity>,
    pub pending_actions: HashMap<WorkItemIdentity, PendingAction>,
    pub show_providers: bool,
    pub active_search_query: Option<String>,
}

impl RepoUiState {
    /// Replace the table view and restore selection by work item identity.
    pub fn update_table_view(&mut self, table_view: GroupedWorkItems) {
        let prev_identity =
            self.selected_selectable_idx.and_then(|si| self.table_view.selectable_indices.get(si).copied()).and_then(|ti| {
                match self.table_view.table_entries.get(ti) {
                    Some(GroupEntry::Item(item)) => Some(item.identity.clone()),
                    _ => None,
                }
            });

        self.table_view = table_view;

        if self.table_view.selectable_indices.is_empty() {
            self.selected_selectable_idx = None;
            self.table_state.select(None);
        } else if let Some(ref identity) = prev_identity {
            let found = self.table_view.selectable_indices.iter().enumerate().find(|(_, &ti)| {
                matches!(
                    self.table_view.table_entries.get(ti),
                    Some(GroupEntry::Item(item)) if item.identity == *identity
                )
            });
            if let Some((si, &ti)) = found {
                self.selected_selectable_idx = Some(si);
                self.table_state.select(Some(ti));
            } else {
                self.selected_selectable_idx = Some(0);
                self.table_state.select(Some(self.table_view.selectable_indices[0]));
            }
        } else {
            self.selected_selectable_idx = Some(0);
            self.table_state.select(Some(self.table_view.selectable_indices[0]));
        }

        // Clean up stale multi-select identities
        let current_identities: HashSet<WorkItemIdentity> = self
            .table_view
            .table_entries
            .iter()
            .filter_map(|e| match e {
                GroupEntry::Item(item) => Some(item.identity.clone()),
                _ => None,
            })
            .collect();
        self.multi_selected.retain(|id| current_identities.contains(id));
        self.pending_actions.retain(|id, _| current_identities.contains(id));
    }
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
    /// The gear/settings icon.
    Gear,
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
    pub event_log_filter_area: Rect,
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

#[derive(Default)]
pub struct DoubleClickState {
    pub last_time: Option<Instant>,
    pub last_selectable_idx: Option<usize>,
}

pub struct EventLogUiState {
    pub selected: Option<usize>,
    pub count: usize,
    pub filter: tracing::Level,
}

impl Default for EventLogUiState {
    fn default() -> Self {
        Self { selected: None, count: 0, filter: tracing::Level::INFO }
    }
}

pub struct UiState {
    pub mode: UiMode,
    pub repo_ui: HashMap<RepoIdentity, RepoUiState>,
    pub target_host: Option<HostName>,
    pub view_layout: RepoViewLayout,
    pub status_bar: StatusBarUiState,
    pub layout: LayoutAreas,
    pub drag: DragState,
    pub double_click: DoubleClickState,
    pub event_log: EventLogUiState,
    pub show_debug: bool,
    pub help_scroll: u16,
}

impl UiState {
    pub fn new(repo_ids: &[RepoIdentity]) -> Self {
        let repo_ui = repo_ids.iter().map(|repo_id| (repo_id.clone(), RepoUiState::default())).collect();
        Self {
            mode: UiMode::default(),
            repo_ui,
            target_host: None,
            view_layout: RepoViewLayout::default(),
            status_bar: StatusBarUiState::default(),
            layout: LayoutAreas::default(),
            drag: DragState::default(),
            double_click: DoubleClickState::default(),
            event_log: EventLogUiState::default(),
            show_debug: false,
            help_scroll: 0,
        }
    }

    pub fn active_repo_ui(&self, repo_order: &[RepoIdentity], active_repo: usize) -> &RepoUiState {
        &self.repo_ui[&repo_order[active_repo]]
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

    fn repo_id(path: &str) -> RepoIdentity {
        RepoIdentity { authority: "local".into(), path: path.into() }
    }

    // ── UiMode tests ──────────────────────────────────────────────────

    #[test]
    fn is_config_returns_true_only_for_config_variant() {
        let cases: Vec<(UiMode, bool)> = vec![
            (UiMode::Normal, false),
            (UiMode::Help, false),
            (UiMode::Config, true),
            (UiMode::ActionMenu { items: vec![], index: 0 }, false),
            (UiMode::BranchInput { input: Input::default(), kind: BranchInputKind::Manual, pending_issue_ids: vec![] }, false),
            (UiMode::FilePicker { input: Input::default(), dir_entries: vec![], selected: 0 }, false),
            (
                UiMode::DeleteConfirm {
                    info: None,
                    loading: false,
                    terminal_keys: vec![],
                    identity: WorkItemIdentity::Session("test".into()),
                },
                false,
            ),
            (
                UiMode::CloseConfirm {
                    id: "42".into(),
                    title: "test".into(),
                    identity: WorkItemIdentity::Session("test".into()),
                    command: flotilla_protocol::Command {
                        host: None,
                        context_repo: None,
                        action: flotilla_protocol::CommandAction::CloseChangeRequest { id: "42".into() },
                    },
                },
                false,
            ),
            (UiMode::IssueSearch { input: Input::default() }, false),
        ];
        for (mode, expected) in &cases {
            assert_eq!(mode.is_config(), *expected, "failed for mode variant");
        }
    }

    #[test]
    fn ui_mode_default_is_normal() {
        assert!(matches!(UiMode::default(), UiMode::Normal));
    }

    // ── UiState::new tests ────────────────────────────────────────────

    #[test]
    fn new_with_empty_paths() {
        let state = UiState::new(&[]);
        assert!(state.repo_ui.is_empty());
        assert!(matches!(state.mode, UiMode::Normal));
        assert!(!state.show_debug);
        assert_eq!(state.view_layout, RepoViewLayout::Auto);
    }

    #[test]
    fn new_with_single_path_creates_one_repo() {
        let paths = vec![repo_id("/repo/a")];
        let state = UiState::new(&paths);
        assert_eq!(state.repo_ui.len(), 1);
        assert!(state.repo_ui.contains_key(&repo_id("/repo/a")));
    }

    #[test]
    fn new_with_multiple_paths_creates_correct_count() {
        let paths = vec![repo_id("/repo/a"), repo_id("/repo/b"), repo_id("/repo/c")];
        let state = UiState::new(&paths);
        assert_eq!(state.repo_ui.len(), 3);
        for p in &paths {
            assert!(state.repo_ui.contains_key(p));
        }
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

    // ── active_repo_ui tests ──────────────────────────────────────────

    #[test]
    fn active_repo_ui_returns_repos_for_valid_indices() {
        let paths = vec![repo_id("/repo/a"), repo_id("/repo/b")];
        let state = UiState::new(&paths);
        for idx in 0..paths.len() {
            let repo_ui = state.active_repo_ui(&paths, idx);
            assert_eq!(repo_ui.selected_selectable_idx, None);
            assert!(!repo_ui.has_unseen_changes);
        }
    }

    #[test]
    #[should_panic]
    fn active_repo_ui_panics_on_out_of_bounds_index() {
        let paths = vec![repo_id("/repo/a")];
        let state = UiState::new(&paths);
        let _ = state.active_repo_ui(&paths, 5);
    }

    // ── RepoUiState default tests ─────────────────────────────────────

    #[test]
    fn repo_ui_state_default() {
        let state = RepoUiState::default();
        assert_eq!(state.selected_selectable_idx, None);
        assert!(!state.has_unseen_changes);
        assert!(state.multi_selected.is_empty());
        assert!(!state.show_providers);
    }

    // ── EventLogUiState default tests ─────────────────────────────────

    #[test]
    fn event_log_ui_state_default_values() {
        let state = EventLogUiState::default();
        assert!(state.selected.is_none());
        assert_eq!(state.count, 0);
        assert_eq!(state.filter, tracing::Level::INFO);
    }

    // ── PendingAction tests ──────────────────────────────────────────

    #[test]
    fn pending_actions_default_is_empty() {
        let state = RepoUiState::default();
        assert!(state.pending_actions.is_empty());
    }

    #[test]
    fn pending_actions_cleaned_on_table_view_update() {
        use flotilla_protocol::{HostName, HostPath};

        let mut state = RepoUiState::default();

        let identity_a = WorkItemIdentity::Checkout(HostPath { host: HostName::new("local"), path: "/tmp/a".into() });
        let identity_b = WorkItemIdentity::Checkout(HostPath { host: HostName::new("local"), path: "/tmp/b".into() });

        state.pending_actions.insert(identity_a.clone(), PendingAction {
            command_id: 1,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });
        state.pending_actions.insert(identity_b.clone(), PendingAction {
            command_id: 2,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });

        // Build a table view that only contains identity_a
        let mut table_view = GroupedWorkItems::default();
        let item_a = flotilla_protocol::WorkItem {
            kind: flotilla_protocol::WorkItemKind::Checkout,
            identity: identity_a.clone(),
            host: HostName::new("local"),
            branch: None,
            description: String::new(),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: Vec::new(),
            workspace_refs: Vec::new(),
            is_main_checkout: false,
            debug_group: Vec::new(),
            source: None,
            terminal_keys: Vec::new(),
        };
        table_view.table_entries.push(GroupEntry::Item(Box::new(item_a)));
        table_view.selectable_indices.push(0);

        state.update_table_view(table_view);

        assert!(state.pending_actions.contains_key(&identity_a));
        assert!(!state.pending_actions.contains_key(&identity_b));
    }
}
