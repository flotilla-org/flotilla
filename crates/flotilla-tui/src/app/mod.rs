pub mod executor;
mod file_picker;
pub mod intent;
mod key_handlers;
mod navigation;
#[doc(hidden)]
pub mod test_builders;
#[cfg(test)]
mod test_support;
pub mod ui_state;

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_core::{
    config::{ConfigStore, RepoViewLayoutConfig},
    daemon::DaemonHandle,
    data::{self, GroupEntry, SectionLabels},
};
use flotilla_protocol::{
    Command, DaemonEvent, HostName, PeerConnectionState, ProviderData, ProviderError, RepoInfo, RepoLabels, Snapshot, SnapshotDelta,
    StepStatus, WorkItem,
};
pub use intent::Intent;
use tui_input::Input;
pub use ui_state::{BranchInputKind, DirEntry, RepoUiState, RepoViewLayout, TabId, UiMode, UiState};

/// Per-provider auth/health status from last refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    Error,
}

/// Connection status for a remote peer host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    Connected,
    Disconnected,
    Connecting,
    Reconnecting,
}

impl From<PeerConnectionState> for PeerStatus {
    fn from(state: PeerConnectionState) -> Self {
        match state {
            PeerConnectionState::Connected => PeerStatus::Connected,
            PeerConnectionState::Disconnected => PeerStatus::Disconnected,
            PeerConnectionState::Connecting => PeerStatus::Connecting,
            PeerConnectionState::Reconnecting => PeerStatus::Reconnecting,
        }
    }
}

/// Status of a configured remote peer host, for display in the config view.
#[derive(Debug, Clone)]
pub struct PeerHostStatus {
    pub name: HostName,
    pub status: PeerStatus,
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
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub loading: bool,
    pub issue_has_more: bool,
    pub issue_total: Option<u32>,
    pub issue_search_active: bool,
    pub issue_fetch_pending: bool,
    /// Whether the initial issue fetch has been requested for this repo.
    pub issue_initial_requested: bool,
}

/// TUI-side domain model. Mirrors the shape of core's `AppModel` but without
/// daemon-internal fields (registry, refresh handles). Populated from
/// `DaemonHandle::list_repos()` and updated by daemon snapshot events.
pub struct TuiModel {
    pub repos: HashMap<PathBuf, TuiRepoModel>,
    pub repo_order: Vec<PathBuf>,
    pub active_repo: usize,
    /// Per-repo, per-provider auth status from last refresh.
    /// Key: (repo_path, provider_category, provider_name)
    pub provider_statuses: HashMap<(PathBuf, String, String), ProviderStatus>,
    pub status_message: Option<String>,
    /// The daemon's hostname, set from the first Snapshot received.
    pub my_host: Option<HostName>,
    /// Status of configured remote peer hosts.
    pub peer_hosts: Vec<PeerHostStatus>,
}

impl TuiModel {
    pub fn from_repo_info(repos_info: Vec<RepoInfo>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for info in repos_info {
            repos.insert(info.path.clone(), TuiRepoModel {
                providers: Arc::new(ProviderData::default()),
                labels: info.labels,
                provider_names: info.provider_names,
                provider_health: info.provider_health,
                loading: info.loading,
                issue_has_more: false,
                issue_total: None,
                issue_search_active: false,
                issue_fetch_pending: false,
                issue_initial_requested: false,
            });
            order.push(info.path);
        }
        Self {
            repos,
            repo_order: order,
            active_repo: 0,
            provider_statuses: HashMap::new(),
            status_message: None,
            my_host: None,
            peer_hosts: Vec::new(),
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
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
    }
}

/// A command that has been dispatched to the daemon and is awaiting completion.
pub struct InFlightCommand {
    pub repo: PathBuf,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleStatusItem {
    pub id: usize,
    pub text: String,
}

fn error_status_item(message: &str) -> VisibleStatusItem {
    VisibleStatusItem { id: 0, text: format!("ERROR {}", message) }
}

fn peer_status_item(index: usize, peer: &PeerHostStatus) -> Option<VisibleStatusItem> {
    let label = match peer.status {
        PeerStatus::Disconnected => "HOST DOWN",
        PeerStatus::Connecting => "HOST CONNECTING",
        PeerStatus::Reconnecting => "HOST RECONNECTING",
        PeerStatus::Connected => return None,
    };
    Some(VisibleStatusItem { id: index + 1, text: format!("{label} {}", peer.name) })
}

pub fn collect_visible_status_items(model: &TuiModel, ui: &UiState) -> Vec<VisibleStatusItem> {
    let mut items = vec![];

    if let Some(message) = &model.status_message {
        items.push(error_status_item(message));
    }

    for (index, peer) in model.peer_hosts.iter().enumerate() {
        if let Some(item) = peer_status_item(index, peer) {
            items.push(item);
        }
    }

    items.into_iter().filter(|item| !ui.status_bar.dismissed_status_ids.contains(&item.id)).collect()
}

/// Log provider errors and format them into a status message.
///
/// Suppresses "issues disabled" messages since the daemon handles those.
/// Returns `None` when there are no displayable errors.
fn format_error_status(errors: &[ProviderError], repo_path: &Path) -> Option<String> {
    let name = TuiModel::repo_name(repo_path);
    let mut all_errors: Vec<String> = Vec::new();
    for e in errors {
        if e.category == "issues" && e.message.contains("has disabled issues") {
            continue;
        }
        let provider_suffix = if e.provider.is_empty() { String::new() } else { format!(" ({})", e.provider) };
        tracing::error!(%name, category = %e.category, provider = %e.provider, message = %e.message, "provider error");
        all_errors.push(format!("{name}: {}{provider_suffix}: {}", e.category, e.message));
    }
    if all_errors.is_empty() {
        None
    } else {
        Some(all_errors.join("; "))
    }
}

pub struct App {
    pub daemon: Arc<dyn DaemonHandle>,
    pub config: Arc<ConfigStore>,
    pub model: TuiModel,
    pub ui: UiState,
    pub proto_commands: CommandQueue,
    pub in_flight: HashMap<u64, InFlightCommand>,
    pub pending_cancel: Option<u64>,
    pub should_quit: bool,
}

impl App {
    pub fn new(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>, config: Arc<ConfigStore>) -> Self {
        let model = TuiModel::from_repo_info(repos_info);
        let mut ui = UiState::new(&model.repo_order);
        let loaded_config = config.load_config();
        ui.view_layout = match loaded_config.ui.preview.layout {
            RepoViewLayoutConfig::Auto => RepoViewLayout::Auto,
            RepoViewLayoutConfig::Zoom => RepoViewLayout::Zoom,
            RepoViewLayoutConfig::Right => RepoViewLayout::Right,
            RepoViewLayoutConfig::Below => RepoViewLayout::Below,
        };
        Self {
            daemon,
            config,
            model,
            ui,
            proto_commands: Default::default(),
            in_flight: HashMap::new(),
            pending_cancel: None,
            should_quit: false,
        }
    }

    /// Returns true when the UI has in-progress work that should be animated.
    pub fn needs_animation(&self) -> bool {
        if !self.in_flight.is_empty() {
            return true;
        }
        matches!(self.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. } | UiMode::DeleteConfirm { loading: true, .. })
    }

    pub fn persist_layout(&self) {
        let layout = match self.ui.view_layout {
            RepoViewLayout::Auto => RepoViewLayoutConfig::Auto,
            RepoViewLayout::Zoom => RepoViewLayoutConfig::Zoom,
            RepoViewLayout::Right => RepoViewLayoutConfig::Right,
            RepoViewLayout::Below => RepoViewLayoutConfig::Below,
        };
        self.config.save_layout(layout);
    }

    pub fn visible_status_items(&self) -> Vec<VisibleStatusItem> {
        collect_visible_status_items(&self.model, &self.ui)
    }

    pub fn dismiss_status_item(&mut self, id: usize) {
        self.ui.status_bar.dismissed_status_ids.insert(id);
    }

    fn set_status_message(&mut self, status_message: Option<String>) {
        if self.model.status_message != status_message {
            self.ui.status_bar.dismissed_status_ids.remove(&0);
        }
        self.model.status_message = status_message;
    }

    // ── Daemon event handling ──

    pub fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::SnapshotFull(snap) => self.apply_snapshot(*snap),
            DaemonEvent::SnapshotDelta(delta) => self.apply_delta(*delta),
            DaemonEvent::RepoAdded(info) => self.handle_repo_added(*info),
            DaemonEvent::RepoRemoved { path } => self.handle_repo_removed(&path),
            DaemonEvent::CommandStarted { command_id, repo, description } => {
                tracing::info!(%command_id, %description, "command started");
                self.in_flight.insert(command_id, InFlightCommand { repo, description });
            }
            DaemonEvent::CommandFinished { command_id, result, .. } => {
                if let Some(_cmd) = self.in_flight.remove(&command_id) {
                    tracing::info!(%command_id, "command finished");
                    executor::handle_result(result, self);
                }
            }
            DaemonEvent::CommandStepUpdate { command_id, description, step_index, step_count, status, .. } => {
                if let Some(cmd) = self.in_flight.get_mut(&command_id) {
                    match status {
                        StepStatus::Started => {
                            cmd.description = format!("{} ({}/{})", description, step_index + 1, step_count);
                        }
                        StepStatus::Skipped => {
                            tracing::info!(%command_id, %description, "step skipped");
                        }
                        StepStatus::Succeeded => {
                            tracing::info!(%command_id, %description, "step succeeded");
                        }
                        StepStatus::Failed { ref message } => {
                            tracing::warn!(%command_id, %description, error = %message, "step failed");
                        }
                    }
                }
            }
            DaemonEvent::PeerStatusChanged { host, status } => {
                let peer_status = PeerStatus::from(status);
                if let Some(existing) = self.model.peer_hosts.iter_mut().find(|p| p.name == host) {
                    existing.status = peer_status;
                } else {
                    self.model.peer_hosts.push(PeerHostStatus { name: host, status: peer_status });
                }
                self.model.peer_hosts.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }

    fn apply_snapshot(&mut self, snap: Snapshot) {
        if self.model.my_host.is_none() {
            self.model.my_host = Some(snap.host_name.clone());
        }

        let path = snap.repo.clone();
        let rm = match self.model.repos.get_mut(&path) {
            Some(rm) => rm,
            None => return,
        };

        let old_providers = std::mem::replace(&mut rm.providers, Arc::new(snap.providers));
        rm.provider_health = snap.provider_health.clone();
        rm.loading = false;
        rm.issue_has_more = snap.issue_has_more;
        rm.issue_total = snap.issue_total;
        rm.issue_search_active = snap.issue_search_results.is_some();
        rm.issue_fetch_pending = false;

        // Build table view
        let section_labels = SectionLabels {
            checkouts: rm.labels.checkouts.section.clone(),
            code_review: rm.labels.code_review.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.cloud_agents.section.clone(),
        };
        let table_view = data::group_work_items(&snap.work_items, &rm.providers, &section_labels, &path);

        // Provider health -> model-level statuses (now 1:1)
        for (category, providers) in &rm.provider_health {
            for (provider_name, &healthy) in providers {
                let status = if healthy { ProviderStatus::Ok } else { ProviderStatus::Error };
                let key = (path.clone(), category.clone(), provider_name.clone());
                self.model.provider_statuses.insert(key, status);
            }
        }

        // Remove stale provider_statuses entries for providers no longer in health map
        self.model.provider_statuses.retain(|k, _| k.0 != path || rm.provider_health.get(&k.1).is_some_and(|ps| ps.contains_key(&k.2)));

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

        if let Some(rui) = self.ui.repo_ui.get_mut(&path) {
            rui.update_table_view(table_view);
        }

        // Log and display errors (clears status when errors resolve)
        self.set_status_message(format_error_status(&snap.errors, &path));

        // Request initial issue fetch once per repo (on first snapshot received)
        let rm = self.model.repos.get_mut(&path).unwrap();
        if !rm.issue_initial_requested {
            rm.issue_initial_requested = true;
            let visible = self.ui.layout.table_area.height.saturating_sub(2) as usize;
            self.proto_commands.push(Command::SetIssueViewport { repo: path, visible_count: visible.max(20) });
        }
    }

    fn apply_delta(&mut self, delta: SnapshotDelta) {
        let path = delta.repo;
        let mut status_message_update = None;
        let rm = match self.model.repos.get_mut(&path) {
            Some(rm) => rm,
            None => return,
        };

        // Apply provider data changes
        let mut providers = (*rm.providers).clone();
        flotilla_core::delta::apply_changes(&mut providers, delta.changes.clone());
        rm.providers = Arc::new(providers);

        // Update issue metadata
        rm.issue_has_more = delta.issue_has_more;
        rm.issue_total = delta.issue_total;
        rm.issue_search_active = delta.issue_search_results.is_some();
        rm.issue_fetch_pending = false;

        // Apply provider health and error changes from the delta
        for change in &delta.changes {
            match change {
                flotilla_protocol::Change::ProviderHealth {
                    category,
                    provider,
                    op: flotilla_protocol::EntryOp::Added(v) | flotilla_protocol::EntryOp::Updated(v),
                } => {
                    rm.provider_health.entry(category.clone()).or_default().insert(provider.clone(), *v);
                }
                flotilla_protocol::Change::ProviderHealth { category, provider, op: flotilla_protocol::EntryOp::Removed } => {
                    if let Some(providers) = rm.provider_health.get_mut(category) {
                        providers.remove(provider);
                        if providers.is_empty() {
                            rm.provider_health.remove(category);
                        }
                    }
                }
                flotilla_protocol::Change::ErrorsChanged(errors) => {
                    status_message_update = Some(format_error_status(errors, &path));
                }
                _ => {}
            }
        }

        // Use daemon's pre-correlated work items directly (no re-correlation)
        let section_labels = SectionLabels {
            checkouts: rm.labels.checkouts.section.clone(),
            code_review: rm.labels.code_review.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.cloud_agents.section.clone(),
        };
        let table_view = data::group_work_items(&delta.work_items, &rm.providers, &section_labels, &path);

        // Provider health -> model-level statuses (now 1:1)
        for (category, providers) in &rm.provider_health {
            for (provider_name, &healthy) in providers {
                let status = if healthy { ProviderStatus::Ok } else { ProviderStatus::Error };
                let key = (path.clone(), category.clone(), provider_name.clone());
                self.model.provider_statuses.insert(key, status);
            }
        }

        // Remove stale provider_statuses entries for providers no longer in health map
        self.model.provider_statuses.retain(|k, _| k.0 != path || rm.provider_health.get(&k.1).is_some_and(|ps| ps.contains_key(&k.2)));

        // Change detection badge — any non-empty delta on inactive tab
        let has_data_changes = delta
            .changes
            .iter()
            .any(|c| !matches!(c, flotilla_protocol::Change::ProviderHealth { .. } | flotilla_protocol::Change::ErrorsChanged(_)));
        if has_data_changes {
            let active_idx = self.model.active_repo;
            let i = self.model.repo_order.iter().position(|p| p == &path);
            if let Some(idx) = i {
                if idx != active_idx {
                    if let Some(rui) = self.ui.repo_ui.get_mut(&path) {
                        rui.has_unseen_changes = true;
                    }
                }
            }
        }

        if let Some(rui) = self.ui.repo_ui.get_mut(&path) {
            rui.update_table_view(table_view);
        }

        if let Some(status_message) = status_message_update {
            self.set_status_message(status_message);
        }
    }

    fn handle_repo_added(&mut self, info: RepoInfo) {
        let path = info.path.clone();
        if self.model.repos.contains_key(&path) {
            return;
        }
        self.model.repos.insert(path.clone(), TuiRepoModel {
            providers: Arc::new(ProviderData::default()),
            labels: info.labels,
            provider_names: info.provider_names,
            provider_health: info.provider_health,
            loading: info.loading,
            issue_has_more: false,
            issue_total: None,
            issue_search_active: false,
            issue_fetch_pending: false,
            issue_initial_requested: false,
        });
        self.model.repo_order.push(path.clone());
        self.ui.repo_ui.insert(path, RepoUiState::default());
    }

    fn handle_repo_removed(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.model.repos.remove(&path);
        self.model.repo_order.retain(|p| p != &path);
        self.ui.repo_ui.remove(&path);
        if self.model.repo_order.is_empty() {
            self.should_quit = true;
            return;
        }
        if self.model.active_repo >= self.model.repo_order.len() {
            self.model.active_repo = self.model.repo_order.len() - 1;
        }
    }

    // ── Convenience accessors ──

    pub fn active_ui(&self) -> &RepoUiState {
        self.ui.active_repo_ui(&self.model.repo_order, self.model.active_repo)
    }

    pub fn active_ui_mut(&mut self) -> &mut RepoUiState {
        let key = &self.model.repo_order[self.model.active_repo];
        self.ui.repo_ui.get_mut(key).expect("active repo must have UI state")
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let table_idx = self.active_ui().table_state.selected()?;
        match self.active_ui().table_view.table_entries.get(table_idx)? {
            GroupEntry::Item(item) => Some(item),
            GroupEntry::Header(_) => None,
        }
    }

    pub fn prefill_branch_input(&mut self, branch_name: &str, pending_issue_ids: Vec<(String, String)>) {
        self.ui.mode = UiMode::BranchInput { input: Input::from(branch_name), kind: BranchInputKind::Manual, pending_issue_ids };
    }

    pub(super) fn enter_branch_input(&mut self, kind: BranchInputKind) {
        self.ui.mode = UiMode::BranchInput { input: Input::default(), kind, pending_issue_ids: Vec::new() };
    }

    pub(super) fn open_file_picker_from_active_repo_parent(&mut self) {
        let mut input = Input::default();
        if let Some(parent) = self.model.active_repo_root().parent() {
            let parent_str = format!("{}/", parent.display());
            input = Input::from(parent_str.as_str());
        }
        self.ui.mode = UiMode::FilePicker { input, dir_entries: Vec::new(), selected: 0 };
        self.refresh_dir_listing();
    }

    pub(super) fn clear_active_issue_search(&mut self, dispatch: ClearDispatch) {
        if dispatch == ClearDispatch::Always || self.active_ui().active_search_query.is_some() {
            let repo = self.model.active_repo_root().clone();
            self.proto_commands.push(Command::ClearIssueSearch { repo });
        }
        self.active_ui_mut().active_search_query = None;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClearDispatch {
    /// Always dispatch the clear command, even if no search is active.
    Always,
    /// Only dispatch if there is an active search query.
    OnlyIfActive,
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;
    use tempfile::tempdir;
    use test_support::*;

    use super::*;

    // -- CommandQueue --

    #[test]
    fn command_queue_push_and_take_fifo() {
        let mut q = CommandQueue::default();
        q.push(Command::Refresh);
        q.push(Command::OpenChangeRequest { id: "1".into() });
        assert!(matches!(q.take_next(), Some(Command::Refresh)));
        assert!(matches!(q.take_next(), Some(Command::OpenChangeRequest { .. })));
    }

    #[test]
    fn command_queue_empty_returns_none() {
        let mut q = CommandQueue::default();
        assert!(q.take_next().is_none());
    }

    // -- TuiModel::repo_name --

    #[test]
    fn repo_name_extracts_directory_name() {
        assert_eq!(TuiModel::repo_name(Path::new("/home/user/project")), "project");
    }

    #[test]
    fn repo_name_root_path() {
        let name = TuiModel::repo_name(Path::new("/"));
        assert_eq!(name, "/");
    }

    // -- TuiModel::from_repo_info --

    #[test]
    fn from_repo_info_builds_correct_model() {
        let repos_info =
            vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default()), repo_info("/tmp/repo-b", "repo-b", RepoLabels::default())];
        let model = TuiModel::from_repo_info(repos_info);
        assert_eq!(model.repos.len(), 2);
        assert_eq!(model.repo_order.len(), 2);
        assert_eq!(model.active_repo, 0);
        assert!(model.repos.contains_key(Path::new("/tmp/repo-a")));
        assert!(model.repos.contains_key(Path::new("/tmp/repo-b")));
        assert!(model.status_message.is_none());
    }

    #[test]
    fn from_repo_info_preserves_order() {
        let repos_info = vec![repo_info("/z", "z", RepoLabels::default()), repo_info("/a", "a", RepoLabels::default())];
        let model = TuiModel::from_repo_info(repos_info);
        assert_eq!(model.repo_order[0], PathBuf::from("/z"));
        assert_eq!(model.repo_order[1], PathBuf::from("/a"));
    }

    #[test]
    fn from_repo_info_empty() {
        let model = TuiModel::from_repo_info(vec![]);
        assert!(model.repos.is_empty());
        assert!(model.repo_order.is_empty());
    }

    #[test]
    fn app_new_loads_layout_from_config() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[ui.preview]\nlayout = \"below\"\n").unwrap();

        let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
        let config = Arc::new(ConfigStore::with_base(dir.path()));
        let app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config);

        assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
    }

    #[test]
    fn persist_layout_writes_current_ui_state() {
        let dir = tempdir().unwrap();
        let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
        let config = Arc::new(ConfigStore::with_base(dir.path()));
        let mut app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config);

        app.ui.view_layout = RepoViewLayout::Right;
        app.persist_layout();

        let reloaded = ConfigStore::with_base(dir.path());
        let cfg = reloaded.load_config();
        assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
    }

    // -- format_error_status --

    #[test]
    fn format_error_status_no_errors() {
        assert!(format_error_status(&[], Path::new("/repo")).is_none());
    }

    #[test]
    fn format_error_status_single_error() {
        let errors = vec![provider_error("code_review", "github", "rate limited")];
        let msg = format_error_status(&errors, Path::new("/tmp/my-repo")).unwrap();
        assert!(msg.contains("my-repo"));
        assert!(msg.contains("code_review"));
        assert!(msg.contains("rate limited"));
        assert!(msg.contains("(github)"));
    }

    #[test]
    fn format_error_status_suppresses_issues_disabled() {
        let errors = vec![provider_error("issues", "github", "repo has disabled issues")];
        assert!(format_error_status(&errors, Path::new("/repo")).is_none());
    }

    #[test]
    fn format_error_status_mixed_suppressed_and_real() {
        let errors = vec![provider_error("issues", "github", "repo has disabled issues"), provider_error("vcs", "git", "not a git repo")];
        let msg = format_error_status(&errors, Path::new("/repo")).unwrap();
        assert!(msg.contains("not a git repo"));
        assert!(!msg.contains("disabled issues"));
    }

    #[test]
    fn format_error_status_empty_provider_no_suffix() {
        let errors = vec![provider_error("vcs", "", "error")];
        let msg = format_error_status(&errors, Path::new("/r")).unwrap();
        assert!(!msg.contains("()"));
    }

    #[test]
    fn format_error_status_multiple_errors_joined() {
        let errors = vec![provider_error("vcs", "git", "err1"), provider_error("cr", "gh", "err2")];
        let msg = format_error_status(&errors, Path::new("/r")).unwrap();
        assert!(msg.contains("; "));
    }

    #[test]
    fn apply_snapshot_updates_provider_data() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let snap = snapshot(&repo);
        app.apply_snapshot(snap);
        assert!(!app.model.repos[&repo].loading);
    }

    #[test]
    fn apply_snapshot_updates_issue_metadata() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let mut snap = snapshot(&repo);
        snap.issue_has_more = true;
        snap.issue_total = Some(42);
        snap.issue_search_results = Some(vec![]);
        app.apply_snapshot(snap);

        let rm = &app.model.repos[&repo];
        assert!(rm.issue_has_more);
        assert_eq!(rm.issue_total, Some(42));
        assert!(rm.issue_search_active);
    }

    #[test]
    fn apply_snapshot_maps_provider_health_to_statuses() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let mut snap = snapshot(&repo);
        snap.provider_health.insert("vcs".into(), HashMap::from([("git".into(), true), ("wt".into(), false)]));
        app.apply_snapshot(snap);

        assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
        assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "wt".into())], ProviderStatus::Error,);
    }

    #[test]
    fn apply_snapshot_sets_error_status_message() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let mut snap = snapshot(&repo);
        snap.errors = vec![provider_error("cr", "gh", "fail")];
        app.apply_snapshot(snap);

        assert!(app.model.status_message.is_some());
        assert!(app.model.status_message.as_ref().unwrap().contains("fail"));
    }

    #[test]
    fn dismissing_status_message_hides_only_that_message() {
        let mut app = stub_app();
        app.set_status_message(Some("rate limit exceeded".into()));

        let id = app.visible_status_items()[0].id;
        app.dismiss_status_item(id);

        assert!(app.visible_status_items().is_empty());
    }

    #[test]
    fn new_status_message_reappears_after_dismissing_old_one() {
        let mut app = stub_app();
        app.set_status_message(Some("old error".into()));
        app.dismiss_status_item(0);

        app.set_status_message(Some("new error".into()));

        assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR new error".into() }]);
    }

    #[test]
    fn visible_status_items_use_shared_error_and_peer_labels() {
        let mut app = stub_app();
        app.set_status_message(Some("boom".into()));
        app.model.peer_hosts = vec![PeerHostStatus { name: flotilla_protocol::HostName::new("host-a"), status: PeerStatus::Disconnected }];

        assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR boom".into() }, VisibleStatusItem {
            id: 1,
            text: "HOST DOWN host-a".into()
        },]);
    }

    #[test]
    fn apply_snapshot_clears_status_on_no_errors() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);
        app.set_status_message(Some("old error".into()));

        let snap = snapshot(&repo);
        app.apply_snapshot(snap);

        assert!(app.model.status_message.is_none());
    }

    #[test]
    fn apply_snapshot_unknown_repo_is_noop() {
        let mut app = stub_app();
        let snap = snapshot(Path::new("/nonexistent"));
        app.apply_snapshot(snap);
    }

    #[test]
    fn apply_snapshot_requests_initial_issue_fetch() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let snap = snapshot(&repo);
        app.apply_snapshot(snap);

        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some(Command::SetIssueViewport { .. })));
        // Second snapshot should NOT queue another
        let snap2 = snapshot(&repo);
        app.apply_snapshot(snap2);
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn apply_snapshot_sets_unseen_changes_for_inactive_tab() {
        let mut app = stub_app_with_repos(2);
        let inactive_repo = app.model.repo_order[1].clone();

        // First snapshot to establish baseline providers
        let snap1 = snapshot(&inactive_repo);
        app.apply_snapshot(snap1);

        // Second snapshot with different providers
        let mut snap2 = snapshot(&inactive_repo);
        snap2.seq = 2;
        snap2.work_items = vec![checkout_item("feat", "/wt", false)];
        let mut different_providers = ProviderData::default();
        different_providers.checkouts.insert(
            flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from("/wt")),
            flotilla_protocol::Checkout {
                branch: "feat".into(),
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
            },
        );
        snap2.providers = different_providers;
        app.apply_snapshot(snap2);

        assert!(app.ui.repo_ui[&inactive_repo].has_unseen_changes);
    }

    // -- apply_delta --

    #[test]
    fn apply_delta_updates_issue_metadata() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let mut change = delta(&repo, vec![]);
        change.issue_total = Some(10);
        change.issue_has_more = true;
        app.apply_delta(change);

        let rm = &app.model.repos[&repo];
        assert_eq!(rm.issue_total, Some(10));
        assert!(rm.issue_has_more);
        assert!(!rm.issue_fetch_pending);
    }

    #[test]
    fn apply_delta_unknown_repo_is_noop() {
        let mut app = stub_app();
        let mut change = delta(Path::new("/nonexistent"), vec![]);
        change.seq = 1;
        change.prev_seq = 0;
        app.apply_delta(change);
    }

    #[test]
    fn apply_delta_provider_health_added() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let change = delta(&repo, vec![flotilla_protocol::Change::ProviderHealth {
            category: "vcs".into(),
            provider: "git".into(),
            op: flotilla_protocol::EntryOp::Added(true),
        }]);
        app.apply_delta(change);

        assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
        assert!(app.model.repos[&repo].provider_health["vcs"]["git"]);
    }

    #[test]
    fn apply_delta_provider_health_removed() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        app.model.repos.get_mut(&repo).unwrap().provider_health.entry("vcs".into()).or_default().insert("git".into(), true);

        let change = delta(&repo, vec![flotilla_protocol::Change::ProviderHealth {
            category: "vcs".into(),
            provider: "git".into(),
            op: flotilla_protocol::EntryOp::Removed,
        }]);
        app.apply_delta(change);

        assert!(!app.model.repos[&repo].provider_health.contains_key("vcs"));
    }

    #[test]
    fn apply_delta_errors_changed_updates_status() {
        let mut app = stub_app();
        let repo = active_repo_path(&app);

        let change = delta(&repo, vec![flotilla_protocol::Change::ErrorsChanged(vec![provider_error("cr", "gh", "broken")])]);
        app.apply_delta(change);

        assert!(app.model.status_message.as_ref().unwrap().contains("broken"));
    }

    #[test]
    fn apply_delta_data_change_on_inactive_tab_sets_unseen() {
        let mut app = stub_app_with_repos(2);
        let inactive_repo = app.model.repo_order[1].clone();

        let change = delta(&inactive_repo, vec![flotilla_protocol::Change::Session {
            key: "s1".into(),
            op: flotilla_protocol::EntryOp::Added(flotilla_protocol::CloudAgentSession {
                title: "new session".into(),
                status: flotilla_protocol::SessionStatus::Running,
                model: None,
                updated_at: None,
                correlation_keys: vec![],
                provider_name: String::new(),
                provider_display_name: String::new(),
                item_noun: String::new(),
            }),
        }]);
        app.apply_delta(change);

        assert!(app.ui.repo_ui[&inactive_repo].has_unseen_changes);
    }

    #[test]
    fn apply_delta_health_only_change_does_not_set_unseen() {
        let mut app = stub_app_with_repos(2);
        let inactive_repo = app.model.repo_order[1].clone();

        let change = delta(&inactive_repo, vec![flotilla_protocol::Change::ProviderHealth {
            category: "vcs".into(),
            provider: "git".into(),
            op: flotilla_protocol::EntryOp::Added(true),
        }]);
        app.apply_delta(change);

        assert!(!app.ui.repo_ui[&inactive_repo].has_unseen_changes);
    }

    // -- handle_repo_added / handle_repo_removed --

    #[test]
    fn handle_repo_added_adds_new_repo() {
        let mut app = stub_app();
        assert_eq!(app.model.repos.len(), 1);

        let info = repo_info("/tmp/new-repo", "new-repo", RepoLabels::default());
        app.handle_repo_added(info);

        assert_eq!(app.model.repos.len(), 2);
        assert!(app.model.repos.contains_key(Path::new("/tmp/new-repo")));
        assert_eq!(app.model.repo_order.last().unwrap(), Path::new("/tmp/new-repo"));
        // Adding a repo should not switch to it (it may arrive asynchronously)
        assert_eq!(app.model.active_repo, 0);
    }

    #[test]
    fn handle_repo_added_duplicate_is_noop() {
        let mut app = stub_app();
        let existing_path = app.model.repo_order[0].clone();
        let info = repo_info(existing_path.to_str().unwrap(), "dup", RepoLabels::default());
        app.handle_repo_added(info);
        assert_eq!(app.model.repos.len(), 1);
    }

    #[test]
    fn handle_repo_removed_removes_repo() {
        let mut app = stub_app_with_repos(2);
        let path = app.model.repo_order[0].clone();

        app.handle_repo_removed(&path);

        assert_eq!(app.model.repos.len(), 1);
        assert!(!app.model.repos.contains_key(&path));
        assert!(!app.model.repo_order.contains(&path));
    }

    #[test]
    fn handle_repo_removed_last_repo_sets_quit() {
        let mut app = stub_app();
        let path = app.model.repo_order[0].clone();

        app.handle_repo_removed(&path);

        assert!(app.should_quit);
    }

    #[test]
    fn handle_repo_removed_adjusts_active_index() {
        let mut app = stub_app_with_repos(3);
        app.model.active_repo = 2;
        let last_path = app.model.repo_order[2].clone();

        app.handle_repo_removed(&last_path);

        assert_eq!(app.model.active_repo, 1);
    }

    // -- handle_daemon_event --

    #[test]
    fn handle_daemon_event_command_started_tracked() {
        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();

        app.handle_daemon_event(DaemonEvent::CommandStarted { command_id: 99, repo: repo.clone(), description: "test cmd".into() });

        assert!(app.in_flight.contains_key(&99));
        assert_eq!(app.in_flight[&99].description, "test cmd");
    }

    // -- Convenience accessors --

    #[test]
    fn selected_work_item_none_when_no_selection() {
        let app = stub_app();
        assert!(app.selected_work_item().is_none());
    }

    #[test]
    fn selected_work_item_returns_item() {
        let mut app = stub_app();
        setup_selectable_table(&mut app, vec![checkout_item("feat", "/wt", false)]);
        let item = app.selected_work_item();
        assert!(item.is_some());
        assert_eq!(item.unwrap().branch.as_deref(), Some("feat"));
    }

    #[test]
    fn prefill_branch_input_sets_mode() {
        let mut app = stub_app();
        app.prefill_branch_input("my-branch", vec![("gh".into(), "1".into())]);
        match &app.ui.mode {
            UiMode::BranchInput { input, kind, pending_issue_ids } => {
                assert_eq!(input.value(), "my-branch");
                assert_eq!(*kind, BranchInputKind::Manual);
                assert_eq!(pending_issue_ids.len(), 1);
            }
            _ => panic!("expected BranchInput mode"),
        }
    }

    // -- CloseConfirm flow --

    #[test]
    fn close_confirm_y_dispatches_command() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm { id: "42".into(), title: "Test PR".into() };
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some(Command::CloseChangeRequest { id }) if id == "42"));
    }

    #[test]
    fn close_confirm_enter_dispatches_command() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm { id: "42".into(), title: "Test PR".into() };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some(Command::CloseChangeRequest { id }) if id == "42"));
    }

    #[test]
    fn close_confirm_esc_cancels() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm { id: "42".into(), title: "Test PR".into() };
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn close_confirm_n_cancels() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm { id: "42".into(), title: "Test PR".into() };
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }
}
