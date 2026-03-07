use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use ratatui::layout::Rect;
use ratatui::widgets::TableState;
use tui_input::Input;

use super::intent::Intent;
use flotilla_core::data::GroupedWorkItems;
use flotilla_protocol::CheckoutStatus;
use flotilla_protocol::WorkItemIdentity;

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
    pub is_added: bool,
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
        generating: bool,
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
    },
}

impl UiMode {
    pub fn is_config(&self) -> bool {
        matches!(self, UiMode::Config)
    }
}

/// Per-repo UI state (selection, table widget state, visual flags).
#[derive(Default)]
pub struct RepoUiState {
    pub table_view: GroupedWorkItems,
    pub table_state: TableState,
    pub selected_selectable_idx: Option<usize>,
    pub has_unseen_changes: bool,
    pub multi_selected: HashSet<WorkItemIdentity>,
    pub show_providers: bool,
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
    pub event_log_filter_area: Rect,
    pub file_picker_area: Rect,
    pub file_picker_list_area: Rect,
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
        Self {
            selected: None,
            count: 0,
            filter: tracing::Level::INFO,
        }
    }
}

pub struct UiState {
    pub mode: UiMode,
    pub repo_ui: HashMap<PathBuf, RepoUiState>,
    pub layout: LayoutAreas,
    pub drag: DragState,
    pub double_click: DoubleClickState,
    pub event_log: EventLogUiState,
    pub show_debug: bool,
}

impl UiState {
    pub fn new(repo_paths: &[PathBuf]) -> Self {
        let repo_ui = repo_paths
            .iter()
            .map(|p| (p.clone(), RepoUiState::default()))
            .collect();
        Self {
            mode: UiMode::default(),
            repo_ui,
            layout: LayoutAreas::default(),
            drag: DragState::default(),
            double_click: DoubleClickState::default(),
            event_log: EventLogUiState::default(),
            show_debug: false,
        }
    }

    pub fn active_repo_ui(&self, repo_order: &[PathBuf], active_repo: usize) -> &RepoUiState {
        &self.repo_ui[&repo_order[active_repo]]
    }
}
