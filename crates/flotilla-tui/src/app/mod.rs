pub mod executor;
mod file_picker;
pub mod intent;
pub mod issue_view;
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
};
use flotilla_protocol::{
    Command, CommandAction, CommandValue, DaemonEvent, HostName, HostSummary, PeerConnectionState, ProviderData, ProviderError,
    ProvisioningTarget, RepoDelta, RepoIdentity, RepoInfo, RepoLabels, RepoSelector, RepoSnapshot, StepStatus, WorkItem, WorkItemIdentity,
};
pub use intent::Intent;
use tokio::sync::mpsc;
use tui_input::Input;
use ui_state::PendingStatus;
pub use ui_state::{BranchInputKind, DirEntry, RepoViewLayout, TabId, UiState};

use crate::{
    keymap::Keymap,
    shared::Shared,
    theme::Theme,
    widgets::repo_page::{RepoData, RepoPage},
};

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
    /// Whether this inactive tab has received data updates since last viewed.
    pub has_unseen_changes: bool,
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
                has_unseen_changes: false,
            });
        }
        Self { repos, repo_order: order, active_repo: 0, provider_statuses: HashMap::new(), status_message: None, hosts: HashMap::new() }
    }

    pub fn active(&self) -> &TuiRepoModel {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    pub fn active_opt(&self) -> Option<&TuiRepoModel> {
        self.repo_order.get(self.active_repo).and_then(|identity| self.repos.get(identity))
    }

    pub fn active_repo_root(&self) -> &PathBuf {
        &self.active().path
    }

    pub fn active_repo_root_opt(&self) -> Option<&PathBuf> {
        self.active_opt().map(|repo| &repo.path)
    }

    pub fn active_repo_identity(&self) -> &RepoIdentity {
        &self.active().identity
    }

    pub fn active_repo_identity_opt(&self) -> Option<&RepoIdentity> {
        self.active_opt().map(|repo| &repo.identity)
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
    pub screen: crate::widgets::screen::Screen,
    /// Per-repo shared data handles. Written by `apply_snapshot()`/`apply_delta()`,
    /// read by `RepoPage` widgets during reconciliation and rendering.
    pub repo_data: HashMap<RepoIdentity, Shared<RepoData>>,
    /// Per-repo issue cursor state, driven by `IssueQueryService`.
    pub issue_views: HashMap<RepoIdentity, issue_view::IssueViewState>,
    /// Sender half for background issue query tasks. Cloned into spawned tasks.
    pub issue_update_tx: mpsc::UnboundedSender<issue_view::IssueQueryUpdate>,
    /// Receiver half, drained each event-loop iteration.
    pub issue_update_rx: mpsc::UnboundedReceiver<issue_view::IssueQueryUpdate>,
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

        // Create Shared<RepoData> handles and RepoPage instances for each repo
        let mut repo_data_map = HashMap::new();
        let mut screen = crate::widgets::screen::Screen::new();
        for (identity, rm) in &model.repos {
            let shared = Shared::new(RepoData {
                path: rm.path.clone(),
                providers: Arc::new(ProviderData::default()),
                labels: rm.labels.clone(),
                provider_names: rm.provider_names.clone(),
                provider_health: rm.provider_health.clone(),
                work_items: Vec::new(),
                issue_items: Vec::new(),
                loading: rm.loading,
            });
            let page = RepoPage::new(identity.clone(), shared.clone(), ui.view_layout);
            repo_data_map.insert(identity.clone(), shared);
            screen.repo_pages.insert(identity.clone(), page);
        }

        let (issue_update_tx, issue_update_rx) = mpsc::unbounded_channel();

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
            screen,
            repo_data: repo_data_map,
            issue_views: HashMap::new(),
            issue_update_tx,
            issue_update_rx,
        }
    }

    /// Returns true when the UI has in-progress work that should be animated.
    pub fn needs_animation(&self) -> bool {
        if !self.in_flight.is_empty() {
            return true;
        }
        if self.screen.repo_pages.values().any(|page| page.pending_actions.values().any(|a| matches!(a.status, PendingStatus::InFlight))) {
            return true;
        }
        // Check modal stack for loading states
        if let Some(widget) = self.screen.modal_stack.last() {
            if let Some(biw) = widget.as_any().downcast_ref::<crate::widgets::branch_input::BranchInputWidget>() {
                if biw.is_generating() {
                    return true;
                }
            }
            if let Some(dcw) = widget.as_any().downcast_ref::<crate::widgets::delete_confirm::DeleteConfirmWidget>() {
                if dcw.loading {
                    return true;
                }
            }
        }
        false
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
        Command { host: None, provisioning_target: None, context_repo: None, action }
    }

    pub fn repo_command(&self, action: CommandAction) -> Command {
        Command {
            host: None,
            provisioning_target: None,
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn repo_command_for_identity(&self, repo_identity: RepoIdentity, action: CommandAction) -> Command {
        Command { host: None, provisioning_target: None, context_repo: Some(RepoSelector::Identity(repo_identity)), action }
    }

    /// Check that a provisioning target refers to a known host and (for NewEnvironment)
    /// an advertised environment provider. Returns `Err(message)` for display if invalid.
    fn validate_provisioning_target(&self, target: &ProvisioningTarget) -> Result<(), String> {
        let host = target.host();
        let is_local = self.model.my_host().is_some_and(|h| h == host);
        let host_known = is_local || self.model.hosts.contains_key(host);
        if !host_known {
            return Err(format!("unknown host: {host}"));
        }
        if let ProvisioningTarget::NewEnvironment { provider, .. } = target {
            let has_provider =
                self.model.hosts.get(host).is_some_and(|h| {
                    h.summary.providers.iter().any(|p| p.category == "environment_provider" && p.implementation == *provider)
                });
            if !has_provider {
                return Err(format!("no {provider} environment provider on {host}"));
            }
        }
        Ok(())
    }

    pub fn targeted_command(&self, action: CommandAction) -> Command {
        let target = &self.ui.provisioning_target;
        Command { host: Some(target.host().clone()), provisioning_target: Some(target.clone()), context_repo: None, action }
    }

    pub fn targeted_repo_command(&self, action: CommandAction) -> Command {
        let target = &self.ui.provisioning_target;
        Command {
            host: Some(target.host().clone()),
            provisioning_target: Some(target.clone()),
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn item_host_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command { host: self.item_execution_host(item), provisioning_target: None, context_repo: None, action }
    }

    pub fn item_host_repo_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command {
            host: self.item_execution_host(item),
            provisioning_target: None,
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
        flotilla_core::template::resolve_template_commands(self.model.active_repo_root(), self.config.base_path().as_path())
    }

    fn item_execution_host(&self, item: &WorkItem) -> Option<HostName> {
        match self.model.my_host() {
            Some(my_host) if item.host != *my_host => Some(item.host.clone()),
            _ => None,
        }
    }

    fn active_repo_is_remote_only(&self) -> bool {
        self.model.active_repo_root_opt().is_some_and(|p| p.starts_with(Path::new("<remote>")))
    }

    pub fn visible_status_items(&self) -> Vec<VisibleStatusItem> {
        collect_visible_status_items(&self.model, &self.ui)
    }

    pub fn persisted_tab_order_paths(&self) -> Vec<flotilla_core::path_context::ExecutionEnvironmentPath> {
        self.model
            .repo_order
            .iter()
            .filter_map(|repo_identity| {
                self.model.repos.get(repo_identity).map(|repo| flotilla_core::path_context::ExecutionEnvironmentPath::new(&repo.path))
            })
            .collect()
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

    pub(crate) fn drain_background_updates(&mut self) {
        use issue_view::{IssueCursorState, IssueQueryUpdate};

        while let Ok(update) = self.issue_update_rx.try_recv() {
            match update {
                IssueQueryUpdate::DefaultCursorOpened { repo, cursor } => {
                    let view = self.issue_views.entry(repo.clone()).or_default();
                    view.default = Some(IssueCursorState {
                        cursor: cursor.clone(),
                        items: Vec::new(),
                        total: None,
                        has_more: true,
                        fetch_pending: true,
                    });
                    // Immediately fetch the first page.
                    self.spawn_fetch_page(repo, cursor, 50);
                }
                IssueQueryUpdate::SearchCursorOpened { repo, cursor, query } => {
                    let view = self.issue_views.entry(repo.clone()).or_default();
                    view.search = Some(IssueCursorState {
                        cursor: cursor.clone(),
                        items: Vec::new(),
                        total: None,
                        has_more: true,
                        fetch_pending: true,
                    });
                    view.search_query = Some(query);
                    // Immediately fetch the first page.
                    self.spawn_fetch_page(repo, cursor, 50);
                }
                IssueQueryUpdate::PageFetched { repo, cursor, page } => {
                    if let Some(view) = self.issue_views.get_mut(&repo) {
                        // Match cursor to search or default.
                        let target = if view.search.as_ref().is_some_and(|s| s.cursor == cursor) {
                            view.search.as_mut()
                        } else if view.default.as_ref().is_some_and(|d| d.cursor == cursor) {
                            view.default.as_mut()
                        } else {
                            None
                        };
                        if let Some(cursor_state) = target {
                            cursor_state.append_page(page);
                        }
                        // Push updated issue items into Shared<RepoData>.
                        self.push_issue_items_to_repo_data(&repo);
                    }
                }
                IssueQueryUpdate::QueryFailed { repo: _, message } => {
                    tracing::warn!(%message, "issue query failed");
                    self.set_status_message(Some(message));
                }
            }
        }
    }

    /// Spawn a background task to fetch one page of issue results.
    fn spawn_fetch_page(&self, repo: RepoIdentity, cursor: flotilla_protocol::issue_query::CursorId, count: usize) {
        let daemon = self.daemon.clone();
        let tx = self.issue_update_tx.clone();
        tokio::spawn(async move {
            let cmd = Command {
                host: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssueFetchPage { cursor: cursor.clone(), count },
            };
            match daemon.execute_query(cmd).await {
                Ok(CommandValue::IssuePage(page)) => {
                    let _ = tx.send(issue_view::IssueQueryUpdate::PageFetched { repo, cursor, page });
                }
                Ok(other) => {
                    let _ =
                        tx.send(issue_view::IssueQueryUpdate::QueryFailed { repo, message: format!("unexpected query result: {other:?}") });
                }
                Err(e) => {
                    let _ = tx.send(issue_view::IssueQueryUpdate::QueryFailed { repo, message: e });
                }
            }
        });
    }

    /// Open a default issue cursor for a repo if one hasn't been opened yet.
    fn maybe_open_default_issue_cursor(&self, repo_identity: &RepoIdentity) {
        if self.issue_views.contains_key(repo_identity) {
            return;
        }
        // Guard: only spawn when inside a tokio runtime (unit tests that call
        // apply_snapshot directly may not have one).
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let daemon = self.daemon.clone();
        let tx = self.issue_update_tx.clone();
        let repo_id = repo_identity.clone();
        let cmd = Command {
            host: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::QueryIssueOpen {
                repo: RepoSelector::Identity(repo_id.clone()),
                params: flotilla_protocol::issue_query::IssueQuery::default(),
            },
        };
        tokio::spawn(async move {
            match daemon.execute_query(cmd).await {
                Ok(CommandValue::IssueQueryOpened { cursor }) => {
                    let _ = tx.send(issue_view::IssueQueryUpdate::DefaultCursorOpened { repo: repo_id, cursor });
                }
                Ok(_) => { /* ignore unexpected result for default cursor */ }
                Err(e) => {
                    tracing::debug!(repo = %repo_id.path, %e, "default issue cursor open failed (expected when no issue provider)");
                }
            }
        });
    }

    /// Push issue items from `IssueViewState` into the `Shared<RepoData>` for
    /// a repo so the `SplitTable` can render them.
    fn push_issue_items_to_repo_data(&self, repo_identity: &RepoIdentity) {
        let Some(view) = self.issue_views.get(repo_identity) else { return };
        let host_name = self.model.my_host().cloned().unwrap_or_else(HostName::local);
        let issue_work_items = view.active_work_items(&host_name);
        if let Some(handle) = self.repo_data.get(repo_identity) {
            handle.mutate(|d| {
                d.issue_items = issue_work_items;
            });
        }
    }

    // ── Widget stack helpers ──

    /// Pop all modal widgets from the stack.
    /// Called when the user switches tabs or navigates away, so stale modals
    /// don't linger across context changes.
    pub fn dismiss_modals(&mut self) {
        self.screen.dismiss_modals();
    }

    /// Returns true if a modal widget is on the stack above the base layer.
    pub fn has_modal(&self) -> bool {
        self.screen.has_modal()
    }

    pub fn build_widget_context(&mut self) -> crate::widgets::WidgetContext<'_> {
        let my_host = self.model.my_host().cloned();
        let active_repo_is_remote_only = self.active_repo_is_remote_only();
        crate::widgets::WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &self.config,
            in_flight: &self.in_flight,
            provisioning_target: &self.ui.provisioning_target,
            my_host,
            active_repo: self.model.active_repo,
            repo_order: &self.model.repo_order,
            commands: &mut self.proto_commands,
            is_config: &mut self.ui.is_config,
            active_repo_is_remote_only,
            app_actions: Vec::new(),
        }
    }

    pub fn process_app_actions(&mut self, actions: Vec<crate::widgets::AppAction>) {
        use crate::widgets::AppAction;
        for action in actions {
            match action {
                AppAction::Quit => self.should_quit = true,
                AppAction::CancelCommand(id) => self.pending_cancel = Some(id),
                AppAction::CycleTheme => {
                    let themes = crate::theme::available_themes();
                    let current = self.theme.name;
                    let idx = themes.iter().position(|(name, _)| *name == current).unwrap_or(0);
                    let next = (idx + 1) % themes.len();
                    self.theme = (themes[next].1)();
                }
                AppAction::SetTheme(name) => {
                    self.theme = crate::theme::theme_by_name(&name);
                }
                AppAction::CycleLayout => {
                    // Cycle the active page's layout (handles both the direct
                    // repo_page path where the page already cycled, and the
                    // command palette path where only the AppAction was emitted).
                    if !self.model.repo_order.is_empty() {
                        let identity = &self.model.repo_order[self.model.active_repo];
                        if let Some(page) = self.screen.repo_pages.get_mut(identity) {
                            page.cycle_layout();
                            self.ui.view_layout = page.layout;
                        }
                    }
                    self.persist_layout();
                }
                AppAction::SetLayout(name) => {
                    let layout = match name.as_str() {
                        "auto" => RepoViewLayout::Auto,
                        "zoom" => RepoViewLayout::Zoom,
                        "right" => RepoViewLayout::Right,
                        "below" => RepoViewLayout::Below,
                        _ => {
                            self.set_status_message(Some(format!("unknown layout: {name}")));
                            continue;
                        }
                    };
                    self.ui.view_layout = layout;
                    if !self.model.repo_order.is_empty() {
                        let identity = &self.model.repo_order[self.model.active_repo];
                        if let Some(page) = self.screen.repo_pages.get_mut(identity) {
                            page.layout = layout;
                        }
                    }
                    self.persist_layout();
                }
                AppAction::CycleHost => {
                    // CycleHost is no longer the primary way to set targets;
                    // the `target` command in the command palette is. Keep the
                    // action as a no-op to avoid breaking any remaining callers.
                }
                AppAction::SetTarget(name) => {
                    // Try full syntax first. Only fall back to bare hostname (@-prefix)
                    // for inputs that don't start with a target prefix — otherwise a
                    // malformed +docker@ would silently become Host { host: "+docker@" }.
                    let has_target_prefix = name.starts_with('@') || name.starts_with('+') || name.starts_with('=');
                    let result = name.parse::<ProvisioningTarget>().or_else(|orig_err| {
                        if has_target_prefix {
                            Err(orig_err)
                        } else {
                            format!("@{name}").parse::<ProvisioningTarget>()
                        }
                    });
                    match result {
                        Ok(target) => match self.validate_provisioning_target(&target) {
                            Ok(()) => self.ui.provisioning_target = target,
                            Err(msg) => self.set_status_message(Some(msg)),
                        },
                        Err(e) => {
                            tracing::warn!(%name, %e, "invalid provisioning target");
                            self.set_status_message(Some(format!("invalid target: {name}")));
                        }
                    }
                }
                AppAction::ToggleDebug => {
                    self.ui.show_debug = !self.ui.show_debug;
                }
                AppAction::ToggleStatusBarKeys => {
                    self.ui.status_bar.show_keys = !self.ui.status_bar.show_keys;
                }
                AppAction::ToggleProviders => {
                    if let Some(identity) = self.model.active_repo_identity_opt() {
                        if let Some(page) = self.screen.repo_pages.get_mut(identity) {
                            page.show_providers = !page.show_providers;
                        }
                    } else {
                        self.set_status_message(Some("No active repo".into()));
                    }
                }
                AppAction::ToggleMultiSelect => {
                    if let Some(repo_identity) = self.model.active_repo_identity_opt().cloned() {
                        if let Some(page) = self.screen.repo_pages.get_mut(&repo_identity) {
                            if let Some(item) = page.table.selected_work_item() {
                                let item_identity = item.identity.clone();
                                if !page.multi_selected.remove(&item_identity) {
                                    page.multi_selected.insert(item_identity);
                                }
                            }
                        }
                    } else {
                        self.set_status_message(Some("No active repo".into()));
                    }
                }
                AppAction::OpenActionMenu => {
                    self.open_action_menu();
                }
                AppAction::ActionEnter => {
                    self.action_enter();
                }
                AppAction::StatusBarKeyPress { code, modifiers } => {
                    self.handle_key(crossterm::event::KeyEvent::new(code, modifiers));
                }
                AppAction::ClearError(id) => {
                    self.dismiss_status_item(id);
                }
                AppAction::SwitchToConfig => {
                    self.dismiss_modals();
                    self.ui.is_config = true;
                }
                AppAction::SwitchToRepo(i) => {
                    self.dismiss_modals();
                    self.switch_tab(i);
                }
                AppAction::SaveTabOrder => {
                    self.config.save_tab_order(&self.persisted_tab_order_paths());
                }
                AppAction::OpenFilePicker => {
                    self.open_file_picker_from_active_repo_parent();
                }
                AppAction::PrevTab => {
                    self.dismiss_modals();
                    self.prev_tab();
                }
                AppAction::NextTab => {
                    self.dismiss_modals();
                    self.next_tab();
                }
                AppAction::MoveTabLeft => {
                    if !self.ui.is_config && self.move_tab(-1) {
                        self.config.save_tab_order(&self.persisted_tab_order_paths());
                    }
                }
                AppAction::MoveTabRight => {
                    if !self.ui.is_config && self.move_tab(1) {
                        self.config.save_tab_order(&self.persisted_tab_order_paths());
                    }
                }
                AppAction::Refresh => {
                    if let Some(repo) = self.model.active_repo_root_opt().cloned() {
                        self.proto_commands.push(self.command(CommandAction::Refresh { repo: Some(RepoSelector::Path(repo)) }));
                    } else {
                        self.set_status_message(Some("No active repo".into()));
                    }
                }
                AppAction::ShowStatus(message) => {
                    self.set_status_message(Some(message));
                }
                AppAction::SetSearchQuery { repo, query } => {
                    if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
                        page.active_search_query = Some(query);
                    }
                }
                AppAction::ClearSearchQuery { repo } => {
                    if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
                        page.active_search_query = None;
                    }
                    // Clear the search cursor, reverting to the default listing.
                    if let Some(view) = self.issue_views.get_mut(&repo) {
                        view.search = None;
                        view.search_query = None;
                    }
                    self.push_issue_items_to_repo_data(&repo);
                }
            }
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
                        CommandValue::Error { message } => Some(message.clone()),
                        _ => None,
                    };
                    executor::handle_result(result, self);

                    // Find which repo+identity has this command_id
                    let found: Option<(RepoIdentity, WorkItemIdentity)> =
                        self.screen.repo_pages.iter().find_map(|(repo_identity, page)| {
                            page.pending_actions
                                .iter()
                                .find(|(_, a)| a.command_id == command_id)
                                .map(|(id, _)| (repo_identity.clone(), id.clone()))
                        });

                    if let Some((repo_identity, identity)) = found {
                        if let Some(ref message) = error_message {
                            if let Some(page) = self.screen.repo_pages.get_mut(&repo_identity) {
                                if let Some(entry) = page.pending_actions.get_mut(&identity) {
                                    entry.status = PendingStatus::Failed(message.clone());
                                }
                            }
                        } else if let Some(page) = self.screen.repo_pages.get_mut(&repo_identity) {
                            page.pending_actions.remove(&identity);
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
                    matches!(peer_status, PeerStatus::Disconnected | PeerStatus::Rejected) && self.ui.provisioning_target.host() == &host;
                if let Some(entry) = self.model.hosts.get_mut(&host) {
                    entry.status = peer_status;
                }
                if clear_target {
                    self.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::local() };
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
                let clear_target = self.ui.provisioning_target.host() == &host;
                self.model.hosts.remove(&host);
                if clear_target {
                    self.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::local() };
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
                if let Some(repo_model) = self.model.repos.get_mut(&repo_identity) {
                    repo_model.has_unseen_changes = true;
                }
            }
        }

        // Feed data into Shared<RepoData> for RepoPage rendering
        if let Some(handle) = self.repo_data.get(&repo_identity) {
            let rm = &self.model.repos[&repo_identity];
            handle.mutate(|d| {
                d.path = path.clone();
                d.providers = rm.providers.clone();
                d.labels = rm.labels.clone();
                d.provider_names = rm.provider_names.clone();
                d.provider_health = rm.provider_health.clone();
                d.work_items = snap.work_items;
                d.loading = false;
            });
        }

        // Log and display errors (clears status when errors resolve)
        self.set_status_message(format_error_status(&snap.errors, &path));

        // On first snapshot for a repo, open a default issue cursor.
        self.maybe_open_default_issue_cursor(&repo_identity);
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
                    if let Some(repo_model) = self.model.repos.get_mut(&repo_identity) {
                        repo_model.has_unseen_changes = true;
                    }
                }
            }
        }

        // Feed data into Shared<RepoData> for RepoPage rendering
        if let Some(handle) = self.repo_data.get(&repo_identity) {
            let rm = &self.model.repos[&repo_identity];
            handle.mutate(|d| {
                d.path = path.clone();
                d.providers = rm.providers.clone();
                d.labels = rm.labels.clone();
                d.provider_names = rm.provider_names.clone();
                d.provider_health = rm.provider_health.clone();
                d.work_items = delta.work_items;
                d.loading = false;
            });
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

        // Create Shared<RepoData> and RepoPage for the new repo
        let shared = Shared::new(RepoData {
            path: info.path.clone(),
            providers: Arc::new(ProviderData::default()),
            labels: info.labels.clone(),
            provider_names: info.provider_names.clone(),
            provider_health: info.provider_health.clone(),
            work_items: Vec::new(),
            issue_items: Vec::new(),
            loading: info.loading,
        });
        let page = RepoPage::new(identity.clone(), shared.clone(), self.ui.view_layout);
        self.repo_data.insert(identity.clone(), shared);
        self.screen.repo_pages.insert(identity.clone(), page);

        self.model.repos.insert(identity.clone(), TuiRepoModel {
            identity: info.identity,
            path: info.path,
            providers: Arc::new(ProviderData::default()),
            labels: info.labels,
            provider_names: info.provider_names,
            provider_health: info.provider_health,
            loading: info.loading,
            has_unseen_changes: false,
        });
        self.model.repo_order.push(identity);
    }

    fn handle_repo_removed(&mut self, repo_identity: &RepoIdentity) {
        self.model.repos.remove(repo_identity);
        self.model.repo_order.retain(|repo| repo != repo_identity);
        self.repo_data.remove(repo_identity);
        self.issue_views.remove(repo_identity);
        self.screen.repo_pages.remove(repo_identity);
        if self.model.repo_order.is_empty() {
            self.should_quit = true;
            return;
        }
        if self.model.active_repo >= self.model.repo_order.len() {
            self.model.active_repo = self.model.repo_order.len() - 1;
        }
        // Sync layout from the now-active page so the status bar indicator
        // reflects the correct repo after removal.
        let identity = &self.model.repo_order[self.model.active_repo];
        if let Some(page) = self.screen.repo_pages.get(identity) {
            self.ui.view_layout = page.layout;
        }
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        if self.model.repo_order.is_empty() {
            return None;
        }
        let identity = &self.model.repo_order[self.model.active_repo];
        self.screen.repo_pages.get(identity).and_then(|page| page.table.selected_work_item())
    }

    pub(super) fn open_file_picker_from_active_repo_parent(&mut self) {
        let start_dir = self
            .model
            .active_repo_root_opt()
            .and_then(|r| r.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        let input = Input::from(format!("{}/", start_dir.display()).as_str());
        let dir_entries = crate::widgets::command_palette::refresh_dir_listing_standalone(input.value(), &self.model);
        self.screen.modal_stack.push(Box::new(crate::widgets::file_picker::FilePickerWidget::new(input, dir_entries)));
    }
}

#[cfg(test)]
mod tests;
