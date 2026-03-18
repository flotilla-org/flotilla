pub mod executor;
mod file_picker;
pub mod intent;
mod key_handlers;
mod navigation;
#[doc(hidden)]
pub mod test_builders;
#[cfg(test)]
pub(crate) mod test_support;
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
    Command, CommandAction, CommandResult, DaemonEvent, HostName, HostSummary, PeerConnectionState, ProviderData, ProviderError, RepoDelta,
    RepoIdentity, RepoInfo, RepoLabels, RepoSelector, RepoSnapshot, StepStatus, WorkItem, WorkItemIdentity,
};
pub use intent::Intent;
use tui_input::Input;
use ui_state::PendingStatus;
pub use ui_state::{BranchInputKind, DirEntry, RepoUiState, RepoViewLayout, TabId, UiMode, UiState};

use crate::{keymap::Keymap, theme::Theme};

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
    Rejected,
}

impl From<PeerConnectionState> for PeerStatus {
    fn from(state: PeerConnectionState) -> Self {
        match state {
            PeerConnectionState::Connected => PeerStatus::Connected,
            PeerConnectionState::Disconnected => PeerStatus::Disconnected,
            PeerConnectionState::Connecting => PeerStatus::Connecting,
            PeerConnectionState::Reconnecting => PeerStatus::Reconnecting,
            PeerConnectionState::Rejected { .. } => PeerStatus::Rejected,
        }
    }
}

/// Combined host state for display in the TUI.
#[derive(Debug, Clone)]
pub struct TuiHostState {
    pub host_name: HostName,
    pub is_local: bool,
    pub status: PeerStatus,
    pub summary: HostSummary,
}

#[derive(Default)]
pub struct CommandQueue {
    queue: VecDeque<(Command, Option<ui_state::PendingActionContext>)>,
}

impl CommandQueue {
    /// Push a command without pending-action tracking. Use `push_with_context`
    /// for user-visible actions that should show a row indicator.
    pub fn push(&mut self, cmd: Command) {
        self.queue.push_back((cmd, None));
    }
    pub fn push_with_context(&mut self, cmd: Command, ctx: Option<ui_state::PendingActionContext>) {
        self.queue.push_back((cmd, ctx));
    }
    pub fn take_next(&mut self) -> Option<(Command, Option<ui_state::PendingActionContext>)> {
        self.queue.pop_front()
    }
}

/// Per-repo view-model state for the TUI. Contains only what the UI needs
/// to render — no provider registry, no refresh handle.
pub struct TuiRepoModel {
    pub identity: RepoIdentity,
    pub path: PathBuf,
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
    pub repos: HashMap<RepoIdentity, TuiRepoModel>,
    pub repo_order: Vec<RepoIdentity>,
    pub active_repo: usize,
    /// Per-repo, per-provider auth status from last refresh.
    /// Key: (repo_identity, provider_category, provider_name)
    pub provider_statuses: HashMap<(RepoIdentity, String, String), ProviderStatus>,
    pub status_message: Option<String>,
    /// All known hosts — local + peers — indexed by hostname.
    pub hosts: HashMap<HostName, TuiHostState>,
}

impl TuiModel {
    pub fn from_repo_info(repos_info: Vec<RepoInfo>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for info in repos_info {
            let identity = info.identity;
            order.push(identity.clone());
            repos.insert(identity.clone(), TuiRepoModel {
                identity,
                path: info.path,
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
        }
        Self { repos, repo_order: order, active_repo: 0, provider_statuses: HashMap::new(), status_message: None, hosts: HashMap::new() }
    }

    pub fn active(&self) -> &TuiRepoModel {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    pub fn active_repo_root(&self) -> &PathBuf {
        &self.active().path
    }

    pub fn active_repo_identity(&self) -> &RepoIdentity {
        &self.active().identity
    }

    pub fn active_labels(&self) -> &RepoLabels {
        &self.active().labels
    }

    pub fn repo_name(path: &Path) -> String {
        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    pub fn my_host(&self) -> Option<&HostName> {
        self.hosts.values().find(|h| h.is_local).map(|h| &h.host_name)
    }

    pub fn peer_host_names(&self) -> Vec<HostName> {
        let mut peers: Vec<_> = self.hosts.values().filter(|h| !h.is_local).map(|h| h.host_name.clone()).collect();
        peers.sort();
        peers
    }

    pub fn home_dir_for_host(&self, host: &HostName) -> Option<&std::path::Path> {
        self.hosts.get(host).and_then(|h| h.summary.system.home_dir.as_deref())
    }
}

/// A command that has been dispatched to the daemon and is awaiting completion.
pub struct InFlightCommand {
    pub repo_identity: RepoIdentity,
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

fn peer_status_item(index: usize, host: &TuiHostState) -> Option<VisibleStatusItem> {
    let label = match host.status {
        PeerStatus::Disconnected => "HOST DOWN",
        PeerStatus::Connecting => "HOST CONNECTING",
        PeerStatus::Reconnecting => "HOST RECONNECTING",
        PeerStatus::Connected => return None,
        PeerStatus::Rejected => "HOST REJECTED",
    };
    Some(VisibleStatusItem { id: index + 1, text: format!("{label} {}", host.host_name) })
}

pub fn collect_visible_status_items(model: &TuiModel, ui: &UiState) -> Vec<VisibleStatusItem> {
    let mut items = vec![];

    if let Some(message) = &model.status_message {
        items.push(error_status_item(message));
    }

    let mut peers: Vec<_> = model.hosts.values().filter(|h| !h.is_local).collect();
    peers.sort_by(|a, b| a.host_name.cmp(&b.host_name));
    for (index, host) in peers.iter().enumerate() {
        if let Some(item) = peer_status_item(index, host) {
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
    pub theme: Theme,
    pub keymap: Keymap,
    pub proto_commands: CommandQueue,
    pub in_flight: HashMap<u64, InFlightCommand>,
    pub pending_cancel: Option<u64>,
    pub should_quit: bool,
    pub widget_stack: Vec<Box<dyn crate::widgets::InteractiveWidget>>,
}

impl App {
    pub fn new(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>, config: Arc<ConfigStore>, theme: Theme) -> Self {
        let model = TuiModel::from_repo_info(repos_info);
        let mut ui = UiState::new(&model.repo_order);
        let loaded_config = config.load_config();
        ui.view_layout = match loaded_config.ui.preview.layout {
            RepoViewLayoutConfig::Auto => RepoViewLayout::Auto,
            RepoViewLayoutConfig::Zoom => RepoViewLayout::Zoom,
            RepoViewLayoutConfig::Right => RepoViewLayout::Right,
            RepoViewLayoutConfig::Below => RepoViewLayout::Below,
        };
        let keymap = Keymap::from_config(&loaded_config.ui.keys);
        Self {
            daemon,
            config,
            model,
            ui,
            theme,
            keymap,
            proto_commands: Default::default(),
            in_flight: HashMap::new(),
            pending_cancel: None,
            should_quit: false,
            widget_stack: vec![],
        }
    }

    /// Returns true when the UI has in-progress work that should be animated.
    pub fn needs_animation(&self) -> bool {
        if !self.in_flight.is_empty() {
            return true;
        }
        if self.ui.repo_ui.values().any(|rui| rui.pending_actions.values().any(|a| matches!(a.status, PendingStatus::InFlight))) {
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

    pub fn command(&self, action: CommandAction) -> Command {
        Command { host: None, context_repo: None, action }
    }

    pub fn repo_command(&self, action: CommandAction) -> Command {
        Command { host: None, context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())), action }
    }

    pub fn repo_command_for_identity(&self, repo_identity: RepoIdentity, action: CommandAction) -> Command {
        Command { host: None, context_repo: Some(RepoSelector::Identity(repo_identity)), action }
    }

    pub fn targeted_command(&self, action: CommandAction) -> Command {
        Command { host: self.ui.target_host.clone(), context_repo: None, action }
    }

    pub fn targeted_repo_command(&self, action: CommandAction) -> Command {
        Command {
            host: self.ui.target_host.clone(),
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn item_host_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command { host: self.item_execution_host(item), context_repo: None, action }
    }

    pub fn item_host_repo_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command {
            host: self.item_execution_host(item),
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn provider_repo_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        if self.active_repo_is_remote_only() {
            self.item_host_repo_command(action, item)
        } else {
            self.repo_command(action)
        }
    }

    pub fn repo_path_for_identity(&self, identity: &RepoIdentity) -> Option<PathBuf> {
        self.model.repos.get(identity).map(|repo| repo.path.clone())
    }

    /// Resolve the local workspace template into role→command pairs.
    /// Used to tell the remote host what commands to prepare.
    pub fn local_template_commands(&self) -> Vec<flotilla_protocol::PreparedTerminalCommand> {
        flotilla_core::template::resolve_template_commands(self.model.active_repo_root(), self.config.base_path())
    }

    fn item_execution_host(&self, item: &WorkItem) -> Option<HostName> {
        match self.model.my_host() {
            Some(my_host) if item.host != *my_host => Some(item.host.clone()),
            _ => None,
        }
    }

    fn active_repo_is_remote_only(&self) -> bool {
        self.model.active_repo_root().starts_with(Path::new("<remote>"))
    }

    pub fn visible_status_items(&self) -> Vec<VisibleStatusItem> {
        collect_visible_status_items(&self.model, &self.ui)
    }

    pub fn persisted_tab_order_paths(&self) -> Vec<PathBuf> {
        self.model.repo_order.iter().filter_map(|repo_identity| self.model.repos.get(repo_identity).map(|repo| repo.path.clone())).collect()
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

    // ── Widget stack helpers ──

    pub fn build_widget_context(&mut self) -> crate::widgets::WidgetContext<'_> {
        crate::widgets::WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &self.config,
            in_flight: &self.in_flight,
            target_host: self.ui.target_host.as_ref(),
            active_repo: self.model.active_repo,
            repo_order: &self.model.repo_order,
            commands: &mut self.proto_commands,
            repo_ui: &mut self.ui.repo_ui,
            should_quit: false,
            pending_cancel: None,
        }
    }

    pub fn apply_outcome(&mut self, index: usize, outcome: crate::widgets::Outcome) {
        match outcome {
            crate::widgets::Outcome::Consumed => {}
            crate::widgets::Outcome::Ignored => {}
            crate::widgets::Outcome::Finished => {
                self.widget_stack.remove(index);
            }
            crate::widgets::Outcome::Push(widget) => {
                self.widget_stack.push(widget);
            }
            crate::widgets::Outcome::Swap(widget) => {
                self.widget_stack.remove(index);
                self.widget_stack.insert(index, widget);
            }
        }
    }

    pub fn apply_context_signals(&mut self, ctx: crate::widgets::WidgetContext) {
        if ctx.should_quit {
            self.should_quit = true;
        }
        if let Some(command_id) = ctx.pending_cancel {
            self.pending_cancel = Some(command_id);
        }
    }

    // ── Daemon event handling ──

    pub fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::RepoSnapshot(snap) => self.apply_snapshot(*snap),
            DaemonEvent::RepoDelta(delta) => self.apply_delta(*delta),
            DaemonEvent::RepoTracked(info) => self.handle_repo_added(*info),
            DaemonEvent::RepoUntracked { repo_identity, .. } => self.handle_repo_removed(&repo_identity),
            DaemonEvent::CommandStarted { command_id, repo_identity, repo, description, .. } => {
                tracing::info!(%command_id, %description, "command started");
                self.in_flight.insert(command_id, InFlightCommand { repo_identity, repo, description });
            }
            DaemonEvent::CommandFinished { command_id, host: _, repo_identity: _, repo: _, result, .. } => {
                if let Some(_cmd) = self.in_flight.remove(&command_id) {
                    tracing::info!(%command_id, "command finished");
                    let error_message = match &result {
                        CommandResult::Error { message } => Some(message.clone()),
                        _ => None,
                    };
                    executor::handle_result(result, self);

                    // Find which repo+identity has this command_id
                    let found: Option<(RepoIdentity, WorkItemIdentity)> = self.ui.repo_ui.iter().find_map(|(repo_identity, rui)| {
                        rui.pending_actions
                            .iter()
                            .find(|(_, a)| a.command_id == command_id)
                            .map(|(id, _)| (repo_identity.clone(), id.clone()))
                    });

                    if let Some((repo_identity, identity)) = found {
                        let rui = self.ui.repo_ui.get_mut(&repo_identity).expect("repo exists");
                        if let Some(message) = error_message {
                            if let Some(entry) = rui.pending_actions.get_mut(&identity) {
                                entry.status = PendingStatus::Failed(message);
                            }
                        } else {
                            rui.pending_actions.remove(&identity);
                        }
                    }
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
                            self.set_status_message(Some(format!("{description}: {message}")));
                        }
                    }
                }
            }
            DaemonEvent::PeerStatusChanged { host, status } => {
                let peer_status = PeerStatus::from(status);
                let clear_target =
                    matches!(peer_status, PeerStatus::Disconnected | PeerStatus::Rejected) && self.ui.target_host.as_ref() == Some(&host);
                if let Some(entry) = self.model.hosts.get_mut(&host) {
                    entry.status = peer_status;
                }
                if clear_target {
                    self.ui.target_host = None;
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                let status = PeerStatus::from(snap.connection_status);
                self.model.hosts.insert(snap.host_name.clone(), TuiHostState {
                    host_name: snap.host_name,
                    is_local: snap.is_local,
                    status,
                    summary: snap.summary,
                });
            }
            DaemonEvent::HostRemoved { host, .. } => {
                let clear_target = self.ui.target_host.as_ref() == Some(&host);
                self.model.hosts.remove(&host);
                if clear_target {
                    self.ui.target_host = None;
                }
            }
        }
    }

    fn apply_snapshot(&mut self, snap: RepoSnapshot) {
        let repo_identity = snap.repo_identity.clone();
        let path = snap.repo.clone();
        let rm = match self.model.repos.get_mut(&repo_identity) {
            Some(rm) => rm,
            None => return,
        };
        rm.path = path.clone();

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
            change_requests: rm.labels.change_requests.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.cloud_agents.section.clone(),
        };
        let table_view = data::group_work_items(&snap.work_items, &rm.providers, &section_labels, &path);

        // Provider health -> model-level statuses (now 1:1)
        for (category, providers) in &rm.provider_health {
            for (provider_name, &healthy) in providers {
                let status = if healthy { ProviderStatus::Ok } else { ProviderStatus::Error };
                let key = (repo_identity.clone(), category.clone(), provider_name.clone());
                self.model.provider_statuses.insert(key, status);
            }
        }

        // Remove stale provider_statuses entries for providers no longer in health map
        self.model
            .provider_statuses
            .retain(|k, _| k.0 != repo_identity || rm.provider_health.get(&k.1).is_some_and(|ps| ps.contains_key(&k.2)));

        // Change detection badge for inactive tabs
        let active_idx = self.model.active_repo;
        let i = self.model.repo_order.iter().position(|repo| repo == &repo_identity);
        if let Some(idx) = i {
            if idx != active_idx && *old_providers != *rm.providers {
                if let Some(rui) = self.ui.repo_ui.get_mut(&repo_identity) {
                    rui.has_unseen_changes = true;
                }
            }
        }

        if let Some(rui) = self.ui.repo_ui.get_mut(&repo_identity) {
            rui.update_table_view(table_view);
        }

        // Log and display errors (clears status when errors resolve)
        self.set_status_message(format_error_status(&snap.errors, &path));

        // Request initial issue fetch once per repo (on first snapshot received)
        let rm = self.model.repos.get_mut(&repo_identity).unwrap();
        if !rm.issue_initial_requested {
            rm.issue_initial_requested = true;
            let visible = self.ui.layout.table_area.height.saturating_sub(2) as usize;
            self.proto_commands.push(self.command(CommandAction::SetIssueViewport {
                repo: flotilla_protocol::RepoSelector::Path(path),
                visible_count: visible.max(20),
            }));
        }
    }

    fn apply_delta(&mut self, delta: RepoDelta) {
        let repo_identity = delta.repo_identity.clone();
        let path = delta.repo;
        let mut status_message_update = None;
        let rm = match self.model.repos.get_mut(&repo_identity) {
            Some(rm) => rm,
            None => return,
        };
        rm.path = path.clone();

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
            change_requests: rm.labels.change_requests.section.clone(),
            issues: rm.labels.issues.section.clone(),
            sessions: rm.labels.cloud_agents.section.clone(),
        };
        let table_view = data::group_work_items(&delta.work_items, &rm.providers, &section_labels, &path);

        // Provider health -> model-level statuses (now 1:1)
        for (category, providers) in &rm.provider_health {
            for (provider_name, &healthy) in providers {
                let status = if healthy { ProviderStatus::Ok } else { ProviderStatus::Error };
                let key = (repo_identity.clone(), category.clone(), provider_name.clone());
                self.model.provider_statuses.insert(key, status);
            }
        }

        // Remove stale provider_statuses entries for providers no longer in health map
        self.model
            .provider_statuses
            .retain(|k, _| k.0 != repo_identity || rm.provider_health.get(&k.1).is_some_and(|ps| ps.contains_key(&k.2)));

        // Change detection badge — any non-empty delta on inactive tab
        let has_data_changes = delta
            .changes
            .iter()
            .any(|c| !matches!(c, flotilla_protocol::Change::ProviderHealth { .. } | flotilla_protocol::Change::ErrorsChanged(_)));
        if has_data_changes {
            let active_idx = self.model.active_repo;
            let i = self.model.repo_order.iter().position(|repo| repo == &repo_identity);
            if let Some(idx) = i {
                if idx != active_idx {
                    if let Some(rui) = self.ui.repo_ui.get_mut(&repo_identity) {
                        rui.has_unseen_changes = true;
                    }
                }
            }
        }

        if let Some(rui) = self.ui.repo_ui.get_mut(&repo_identity) {
            rui.update_table_view(table_view);
        }

        if let Some(status_message) = status_message_update {
            self.set_status_message(status_message);
        }
    }

    fn handle_repo_added(&mut self, info: RepoInfo) {
        let identity = info.identity.clone();
        if self.model.repos.contains_key(&identity) {
            return;
        }
        self.model.repos.insert(identity.clone(), TuiRepoModel {
            identity: info.identity,
            path: info.path,
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
        self.model.repo_order.push(identity.clone());
        self.ui.repo_ui.insert(identity, RepoUiState::default());
    }

    fn handle_repo_removed(&mut self, repo_identity: &RepoIdentity) {
        self.model.repos.remove(repo_identity);
        self.model.repo_order.retain(|repo| repo != repo_identity);
        self.ui.repo_ui.remove(repo_identity);
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
            self.proto_commands.push(self.command(CommandAction::ClearIssueSearch { repo: flotilla_protocol::RepoSelector::Path(repo) }));
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
    use flotilla_protocol::WorkItemIdentity;
    use tempfile::tempdir;
    use test_support::*;

    use super::*;

    fn insert_local_host(model: &mut TuiModel, name: &str) {
        let host_name = HostName::new(name);
        model.hosts.insert(host_name.clone(), TuiHostState {
            host_name: host_name.clone(),
            is_local: true,
            status: PeerStatus::Connected,
            summary: HostSummary {
                host_name,
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
            },
        });
    }

    fn insert_peer_host(model: &mut TuiModel, name: &str, status: PeerStatus) {
        let host_name = HostName::new(name);
        model.hosts.insert(host_name.clone(), TuiHostState {
            host_name: host_name.clone(),
            is_local: false,
            status,
            summary: HostSummary {
                host_name,
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
            },
        });
    }

    // -- CommandQueue --

    #[test]
    fn command_queue_push_and_take_fifo() {
        let mut q = CommandQueue::default();
        q.push(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
        q.push(Command {
            host: None,
            context_repo: Some(RepoSelector::Path(PathBuf::from("/repo"))),
            action: CommandAction::OpenChangeRequest { id: "1".into() },
        });
        assert!(matches!(q.take_next(), Some((Command { action: CommandAction::Refresh { .. }, .. }, _))));
        assert!(matches!(q.take_next(), Some((Command { action: CommandAction::OpenChangeRequest { .. }, .. }, _))));
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
        assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-a")));
        assert!(model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/repo-b")));
        assert!(model.status_message.is_none());
    }

    #[test]
    fn from_repo_info_preserves_order() {
        let repos_info = vec![repo_info("/z", "z", RepoLabels::default()), repo_info("/a", "a", RepoLabels::default())];
        let model = TuiModel::from_repo_info(repos_info);
        assert_eq!(model.repos[&model.repo_order[0]].path, PathBuf::from("/z"));
        assert_eq!(model.repos[&model.repo_order[1]].path, PathBuf::from("/a"));
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
        let app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

        assert_eq!(app.ui.view_layout, RepoViewLayout::Below);
    }

    #[test]
    fn persist_layout_writes_current_ui_state() {
        let dir = tempdir().unwrap();
        let daemon: Arc<dyn DaemonHandle> = Arc::new(test_support::StubDaemon::new());
        let config = Arc::new(ConfigStore::with_base(dir.path()));
        let mut app = App::new(daemon, vec![repo_info("/tmp/repo-a", "repo-a", RepoLabels::default())], config, Theme::classic());

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
        let errors = vec![provider_error("change_request", "github", "rate limited")];
        let msg = format_error_status(&errors, Path::new("/tmp/my-repo")).unwrap();
        assert!(msg.contains("my-repo"));
        assert!(msg.contains("change_request"));
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
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        let snap = snapshot(&repo_path);
        app.apply_snapshot(snap);
        assert!(!app.model.repos[&repo].loading);
    }

    #[test]
    fn apply_snapshot_updates_issue_metadata() {
        let mut app = stub_app();
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        let mut snap = snapshot(&repo_path);
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
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        let mut snap = snapshot(&repo_path);
        snap.provider_health.insert("vcs".into(), HashMap::from([("git".into(), true), ("wt".into(), false)]));
        app.apply_snapshot(snap);

        assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "git".into())], ProviderStatus::Ok,);
        assert_eq!(app.model.provider_statuses[&(repo.clone(), "vcs".into(), "wt".into())], ProviderStatus::Error,);
    }

    #[test]
    fn apply_snapshot_sets_error_status_message() {
        let mut app = stub_app();
        let repo_path = active_repo_path(&app);

        let mut snap = snapshot(&repo_path);
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
        insert_peer_host(&mut app.model, "host-a", PeerStatus::Disconnected);

        assert_eq!(app.visible_status_items(), vec![VisibleStatusItem { id: 0, text: "ERROR boom".into() }, VisibleStatusItem {
            id: 1,
            text: "HOST DOWN host-a".into()
        },]);
    }

    #[test]
    fn apply_snapshot_clears_status_on_no_errors() {
        let mut app = stub_app();
        let repo_path = active_repo_path(&app);
        app.set_status_message(Some("old error".into()));

        let snap = snapshot(&repo_path);
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
        let repo_path = active_repo_path(&app);

        let snap = snapshot(&repo_path);
        app.apply_snapshot(snap);

        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some((Command { action: CommandAction::SetIssueViewport { .. }, .. }, _))));
        // Second snapshot should NOT queue another
        let snap2 = snapshot(&repo_path);
        app.apply_snapshot(snap2);
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn apply_snapshot_sets_unseen_changes_for_inactive_tab() {
        let mut app = stub_app_with_repos(2);
        let inactive_repo = app.model.repo_order[1].clone();
        let inactive_path = app.model.repos[&inactive_repo].path.clone();

        // First snapshot to establish baseline providers
        let snap1 = snapshot(&inactive_path);
        app.apply_snapshot(snap1);

        // Second snapshot with different providers
        let mut snap2 = snapshot(&inactive_path);
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
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        let mut change = delta(&repo_path, vec![]);
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
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
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
        let repo = app.model.active_repo_identity().clone();
        let repo_path = active_repo_path(&app);

        app.model.repos.get_mut(&repo).unwrap().provider_health.entry("vcs".into()).or_default().insert("git".into(), true);

        let change = delta(&repo_path, vec![flotilla_protocol::Change::ProviderHealth {
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
        let repo_path = active_repo_path(&app);

        let change = delta(&repo_path, vec![flotilla_protocol::Change::ErrorsChanged(vec![provider_error("cr", "gh", "broken")])]);
        app.apply_delta(change);

        assert!(app.model.status_message.as_ref().unwrap().contains("broken"));
    }

    #[test]
    fn apply_delta_data_change_on_inactive_tab_sets_unseen() {
        let mut app = stub_app_with_repos(2);
        let inactive_repo = app.model.repo_order[1].clone();
        let inactive_path = app.model.repos[&inactive_repo].path.clone();

        let change = delta(&inactive_path, vec![flotilla_protocol::Change::Session {
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
        let inactive_path = app.model.repos[&inactive_repo].path.clone();

        let change = delta(&inactive_path, vec![flotilla_protocol::Change::ProviderHealth {
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
        assert!(app.model.repos.values().any(|repo| repo.path.as_path() == Path::new("/tmp/new-repo")));
        assert_eq!(app.model.repos[app.model.repo_order.last().unwrap()].path, PathBuf::from("/tmp/new-repo"));
        // Adding a repo should not switch to it (it may arrive asynchronously)
        assert_eq!(app.model.active_repo, 0);
    }

    #[test]
    fn handle_repo_added_duplicate_is_noop() {
        let mut app = stub_app();
        let existing_path = app.model.repo_order[0].clone();
        let info = repo_info(app.model.repos[&existing_path].path.clone(), "dup", RepoLabels::default());
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
        let repo = app.model.active_repo_root().clone();

        app.handle_daemon_event(DaemonEvent::CommandStarted {
            command_id: 99,
            host: HostName::local(),
            repo_identity: app.model.active_repo_identity().clone(),
            repo: repo.clone(),
            description: "test cmd".into(),
        });

        assert!(app.in_flight.contains_key(&99));
        assert_eq!(app.in_flight[&99].description, "test cmd");
    }

    #[test]
    fn step_failure_surfaces_error_in_status_message() {
        let mut app = stub_app();
        let repo_identity = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo_identity].path.clone();

        app.in_flight.insert(42, InFlightCommand {
            repo_identity: repo_identity.clone(),
            repo: repo_path.clone(),
            description: "Creating checkout...".into(),
        });

        app.handle_daemon_event(DaemonEvent::CommandStepUpdate {
            command_id: 42,
            host: HostName::local(),
            repo_identity,
            repo: repo_path,
            step_index: 0,
            step_count: 1,
            description: "Create checkout for branch my-branch".into(),
            status: StepStatus::Failed { message: "branch already exists: my-branch".into() },
        });

        let msg = app.model.status_message.as_deref().expect("status_message should be set");
        assert!(msg.contains("branch already exists"), "expected error detail in status message, got: {msg}");
    }

    #[test]
    fn peer_disconnect_clears_selected_target_host() {
        let mut app = stub_app();
        app.ui.target_host = Some(HostName::new("alpha"));
        insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

        app.handle_daemon_event(DaemonEvent::PeerStatusChanged { host: HostName::new("alpha"), status: PeerConnectionState::Disconnected });

        assert_eq!(app.ui.target_host, None);
        assert_eq!(app.model.hosts.get(&HostName::new("alpha")).unwrap().status, PeerStatus::Disconnected);
    }

    #[test]
    fn host_removed_event_deletes_host_and_clears_selected_target_host() {
        let mut app = stub_app();
        app.ui.target_host = Some(HostName::new("alpha"));
        insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);

        app.handle_daemon_event(DaemonEvent::HostRemoved { host: HostName::new("alpha"), seq: 2 });

        assert_eq!(app.ui.target_host, None);
        assert!(!app.model.hosts.contains_key(&HostName::new("alpha")));
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
        app.ui.mode = UiMode::CloseConfirm {
            id: "42".into(),
            title: "Test PR".into(),
            identity: WorkItemIdentity::Session("test".into()),
            command: Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "42".into() } },
        };
        app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
    }

    #[test]
    fn close_confirm_enter_dispatches_command() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm {
            id: "42".into(),
            title: "Test PR".into(),
            identity: WorkItemIdentity::Session("test".into()),
            command: Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "42".into() } },
        };
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        let cmd = app.proto_commands.take_next();
        assert!(matches!(cmd, Some((Command { action: CommandAction::CloseChangeRequest { id }, .. }, _)) if id == "42"));
    }

    #[test]
    fn close_confirm_esc_cancels() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm {
            id: "42".into(),
            title: "Test PR".into(),
            identity: WorkItemIdentity::Session("test".into()),
            command: Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "42".into() } },
        };
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    #[test]
    fn close_confirm_n_cancels() {
        let mut app = stub_app();
        app.ui.mode = UiMode::CloseConfirm {
            id: "42".into(),
            title: "Test PR".into(),
            identity: WorkItemIdentity::Session("test".into()),
            command: Command { host: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "42".into() } },
        };
        app.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(app.ui.mode, UiMode::Normal));
        assert!(app.proto_commands.take_next().is_none());
    }

    // -- CommandQueue with PendingActionContext --

    #[test]
    fn command_queue_push_with_context() {
        use crate::app::ui_state::PendingActionContext;

        let mut q = CommandQueue::default();
        let ctx = PendingActionContext {
            identity: WorkItemIdentity::Session("s1".into()),
            description: "Archive session".into(),
            repo_identity: RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() },
        };
        q.push_with_context(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } }, Some(ctx));
        let (cmd, ctx) = q.take_next().expect("should have one entry");
        assert!(matches!(cmd.action, CommandAction::Refresh { .. }));
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().description, "Archive session");
    }

    #[test]
    fn command_queue_push_without_context() {
        let mut q = CommandQueue::default();
        q.push(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
        let (_, ctx) = q.take_next().expect("should have one entry");
        assert!(ctx.is_none());
    }

    // -- Pending action lifecycle on CommandFinished --

    #[test]
    fn command_finished_ok_clears_pending_action() {
        use crate::app::ui_state::{PendingAction, PendingStatus};

        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo].path.clone();
        let identity = WorkItemIdentity::Session("s1".into());

        app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
            command_id: 42,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });
        app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::local(),
            repo_identity: repo.clone(),
            repo: repo_path,
            result: CommandResult::Ok,
        });

        assert!(!app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
    }

    #[test]
    fn command_finished_error_transitions_to_failed() {
        use crate::app::ui_state::{PendingAction, PendingStatus};

        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo].path.clone();
        let identity = WorkItemIdentity::Session("s1".into());

        app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
            command_id: 42,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });
        app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::local(),
            repo_identity: repo.clone(),
            repo: repo_path,
            result: CommandResult::Error { message: "boom".into() },
        });

        let pending = &app.ui.repo_ui[&repo].pending_actions[&identity];
        assert!(matches!(pending.status, PendingStatus::Failed(ref msg) if msg == "boom"));
    }

    #[test]
    fn command_finished_cancelled_clears_pending_action() {
        use crate::app::ui_state::{PendingAction, PendingStatus};

        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo].path.clone();
        let identity = WorkItemIdentity::Session("s1".into());

        app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
            command_id: 42,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });
        app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::local(),
            repo_identity: repo.clone(),
            repo: repo_path,
            result: CommandResult::Cancelled,
        });

        assert!(!app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
    }

    #[test]
    fn orphaned_command_finished_harmlessly_ignored() {
        use crate::app::ui_state::{PendingAction, PendingStatus};

        let mut app = stub_app();
        let repo = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo].path.clone();
        let identity = WorkItemIdentity::Session("s1".into());

        // Insert pending action with command_id 99 (different from finished event)
        app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(identity.clone(), PendingAction {
            command_id: 99,
            status: PendingStatus::InFlight,
            description: "test".into(),
        });
        app.in_flight.insert(42, InFlightCommand { repo_identity: repo.clone(), repo: repo_path.clone(), description: "test".into() });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::local(),
            repo_identity: repo.clone(),
            repo: repo_path,
            result: CommandResult::Ok,
        });

        // The pending action with command_id 99 should still be there
        assert!(app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
    }

    #[test]
    fn local_checkout_created_does_not_queue_workspace() {
        let mut app = stub_app();
        insert_local_host(&mut app.model, "my-desktop");
        let repo_identity = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo_identity].path.clone();

        app.in_flight.insert(42, InFlightCommand {
            repo_identity: repo_identity.clone(),
            repo: repo_path.clone(),
            description: "test".into(),
        });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::new("my-desktop"),
            repo_identity,
            repo: repo_path,
            result: CommandResult::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/tmp/repo/wt-feat") },
        });

        assert!(app.proto_commands.take_next().is_none(), "workspace creation is now handled by checkout plan, not TUI");
    }

    #[test]
    fn remote_checkout_created_does_not_queue_workspace() {
        let mut app = stub_app();
        insert_local_host(&mut app.model, "my-desktop");
        let repo_identity = app.model.repo_order[0].clone();
        let repo_path = app.model.repos[&repo_identity].path.clone();

        app.in_flight.insert(42, InFlightCommand {
            repo_identity: repo_identity.clone(),
            repo: repo_path.clone(),
            description: "test".into(),
        });

        app.handle_daemon_event(DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::new("remote-a"),
            repo_identity,
            repo: repo_path,
            result: CommandResult::CheckoutCreated { branch: "feat".into(), path: PathBuf::from("/remote/wt-feat") },
        });

        assert!(app.proto_commands.take_next().is_none(), "remote checkout should not auto-create local workspace");
    }

    // -- TuiHostState / hosts map --

    #[test]
    fn host_snapshot_event_populates_hosts_map() {
        let mut app = stub_app();
        app.handle_daemon_event(DaemonEvent::HostSnapshot(Box::new(flotilla_protocol::HostSnapshot {
            seq: 1,
            host_name: HostName::new("desktop"),
            is_local: true,
            connection_status: PeerConnectionState::Connected,
            summary: HostSummary {
                host_name: HostName::new("desktop"),
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
            },
        })));
        assert_eq!(app.model.my_host(), Some(&HostName::new("desktop")));
        assert!(app.model.hosts.get(&HostName::new("desktop")).unwrap().is_local);
    }

    #[test]
    fn my_host_returns_none_before_host_snapshot() {
        let app = stub_app();
        assert!(app.model.my_host().is_none());
    }

    #[test]
    fn peer_host_names_returns_sorted_non_local() {
        let mut app = stub_app();
        insert_local_host(&mut app.model, "local");
        insert_peer_host(&mut app.model, "beta", PeerStatus::Connected);
        insert_peer_host(&mut app.model, "alpha", PeerStatus::Connected);
        assert_eq!(app.model.peer_host_names(), vec![HostName::new("alpha"), HostName::new("beta")]);
    }
}
