pub mod executor;
mod file_picker;
pub mod intent;
pub mod issue_view;
mod key_handlers;
mod navigation;
pub mod open_views;
#[doc(hidden)]
pub mod test_builders;
#[cfg(test)]
pub(crate) mod test_support;
pub mod ui_state;
pub(crate) mod view_kind;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_core::{
    config::{ConfigStore, RepoViewLayoutConfig},
    daemon::{DaemonHandle, QuerySubscription},
};
use flotilla_protocol::{
    Command, CommandAction, CommandValue, DaemonEvent, EnvironmentId, HostName, HostSummary, NodeId, PeerConnectionState, ProviderData,
    ProviderError, ProvisioningTarget, RepoDelta, RepoIdentity, RepoInfo, RepoLabels, RepoSelector, RepoSnapshot, StepStatus, ViewAddress,
    WorkItem, WorkItemIdentity,
};
use indexmap::IndexMap;
pub use intent::Intent;
pub use open_views::{OpenView, OpenViews, ViewTarget};
use tokio::sync::mpsc;
use tui_input::Input;
use ui_state::PendingStatus;
pub use ui_state::{BranchInputKind, DirEntry, ProjectIssueStartContext, RepoViewLayout, TabId, UiState};

use crate::{
    convoy_model::{ConvoyId, ConvoySummary},
    keymap::Keymap,
    shared::Shared,
    theme::Theme,
    widgets::{
        repo_page::{RepoData, RepoPage},
        section_table::IssueRow,
        split_table::SelectedRow,
    },
};

/// Owned version of `SelectedRow` for use when the borrow can't be held.
pub(super) enum OwnedSelectedRow {
    WorkItem(Box<WorkItem>),
    IssueRow(Box<IssueRow>),
}

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
    pub environment_id: EnvironmentId,
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
    pub repository_key: Option<flotilla_protocol::RepositoryKey>,
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
    /// Registration/listing order of tracked repos (daemon list order).
    /// This is NOT tab order — tabs are `App::views` (`OpenViews`, ADR 0013).
    pub repo_order: Vec<RepoIdentity>,
    /// The repo the active tab is scoped to, when the active View is a repo
    /// view. Synced from `App::views` by `App::sync_active_view` — never set
    /// directly.
    pub active_repo: Option<RepoIdentity>,
    /// Per-repo, per-provider auth status from last refresh.
    /// Key: (repo_identity, provider_category, provider_name)
    pub provider_statuses: HashMap<(RepoIdentity, String, String), ProviderStatus>,
    pub status_message: Option<String>,
    /// All known host environments indexed by canonical environment identity.
    pub hosts: HashMap<EnvironmentId, TuiHostState>,
}

impl TuiModel {
    pub fn display_path(identity: &RepoIdentity, path: Option<PathBuf>) -> PathBuf {
        path.unwrap_or_else(|| PathBuf::from(identity.path.clone()))
    }

    pub fn from_repo_info(repos_info: Vec<RepoInfo>) -> Self {
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        for info in repos_info {
            let identity = info.identity;
            let path = Self::display_path(&identity, info.path);
            order.push(identity.clone());
            repos.insert(identity.clone(), TuiRepoModel {
                identity,
                repository_key: info.repository_key,
                path,
                providers: Arc::new(ProviderData::default()),
                labels: info.labels,
                provider_names: info.provider_names,
                provider_health: info.provider_health,
                loading: info.loading,
                has_unseen_changes: false,
            });
        }
        Self { repos, repo_order: order, active_repo: None, provider_statuses: HashMap::new(), status_message: None, hosts: HashMap::new() }
    }

    pub fn active(&self) -> &TuiRepoModel {
        self.active_opt().expect("active() requires the active tab to be a tracked repo view")
    }

    pub fn active_opt(&self) -> Option<&TuiRepoModel> {
        self.active_repo.as_ref().and_then(|identity| self.repos.get(identity))
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

    pub fn my_node_id(&self) -> Option<&NodeId> {
        self.hosts.values().find(|h| h.is_local).map(|h| &h.summary.node.node_id)
    }

    pub fn host_matches<'a>(&'a self, host: &HostName) -> Vec<&'a TuiHostState> {
        self.hosts.values().filter(|entry| &entry.host_name == host).collect()
    }

    pub fn resolve_host(&self, host: &HostName) -> Result<&TuiHostState, String> {
        let matches = self.host_matches(host);
        match matches.as_slice() {
            [] => Err(format!("unknown host: {host}")),
            [entry] => Ok(entry),
            _ => {
                let mut ids: Vec<_> = matches.iter().map(|entry| entry.environment_id.canonical_string()).collect();
                ids.sort();
                Err(format!("ambiguous host: {host} ({})", ids.join(", ")))
            }
        }
    }

    pub fn node_id_for_host(&self, host: &HostName) -> Option<&NodeId> {
        self.resolve_host(host).ok().map(|entry| &entry.summary.node.node_id)
    }

    pub fn resolve_environment_target(&self, environment_id: &EnvironmentId) -> Result<(NodeId, ProvisioningTarget), String> {
        if let Some(host) = self.hosts.get(environment_id) {
            let target = if environment_id.is_host() {
                ProvisioningTarget::Host { host: host.host_name.clone() }
            } else {
                ProvisioningTarget::ExistingEnvironment { host: host.host_name.clone(), env_id: environment_id.clone() }
            };
            return Ok((host.summary.node.node_id.clone(), target));
        }

        for host in self.hosts.values() {
            if host.summary.environments.iter().any(|environment| environment.environment_id() == environment_id) {
                return Ok((host.summary.node.node_id.clone(), ProvisioningTarget::ExistingEnvironment {
                    host: host.host_name.clone(),
                    env_id: environment_id.clone(),
                }));
            }
        }

        Err(format!("unknown environment: {environment_id}"))
    }

    pub fn host_for_node_id(&self, node_id: &NodeId) -> Option<&HostName> {
        self.hosts.values().find(|entry| entry.summary.node.node_id == *node_id).map(|entry| &entry.host_name)
    }

    pub fn peer_host_names(&self) -> Vec<HostName> {
        let mut peers: Vec<_> = self.hosts.values().filter(|h| !h.is_local).map(|h| h.host_name.clone()).collect();
        peers.sort();
        peers
    }

    pub fn home_dir_for_host(&self, host: &HostName) -> Option<&std::path::Path> {
        self.resolve_host(host).ok().and_then(|h| h.summary.system.home_dir.as_deref())
    }
}

/// Alias for the per-namespace map stored on `App` and passed into `RenderContext`.
pub type NamespaceMap = HashMap<String, NamespaceModel>;

/// Per-namespace convoy state tracked by the TUI. Populated from
/// `DaemonEvent::ResultSet` and updated by `DaemonEvent::ResultDelta`.
#[derive(Default)]
pub struct NamespaceModel {
    pub convoys: IndexMap<crate::convoy_model::ConvoyId, crate::convoy_model::ConvoySummary>,
    pub last_seq: u64,
}

/// Client-side projections of typed query result sets. The query identity is
/// retained so a scoped tab can select exactly the base or ephemeral-search
/// window it subscribed to.
#[derive(Default)]
pub struct QueryTableCache {
    pub independents: HashMap<flotilla_protocol::QueryId, QueryTableResult<flotilla_protocol::IndependentRow>>,
    pub issues: HashMap<flotilla_protocol::QueryId, QueryTableResult<flotilla_protocol::IssueRow>>,
    pub checkouts: HashMap<flotilla_protocol::QueryId, QueryTableResult<flotilla_protocol::CheckoutRow>>,
}

pub struct QueryTableResult<R> {
    pub rows: Vec<R>,
    pub state: flotilla_protocol::ResultSetState,
}

impl<R> Default for QueryTableResult<R> {
    fn default() -> Self {
        Self { rows: Vec::new(), state: flotilla_protocol::ResultSetState::default() }
    }
}

impl<R: Clone> QueryTableResult<R> {
    fn apply_delta<K: Eq>(
        &mut self,
        changed: &[R],
        removed: &[K],
        state: Option<&flotilla_protocol::ResultSetState>,
        key: impl Fn(&R) -> K,
        compare: impl FnMut(&R, &R) -> std::cmp::Ordering,
    ) {
        self.rows.retain(|row| !removed.contains(&key(row)));
        for changed_row in changed {
            let changed_key = key(changed_row);
            if let Some(existing) = self.rows.iter_mut().find(|row| key(row) == changed_key) {
                *existing = changed_row.clone();
            } else {
                self.rows.push(changed_row.clone());
            }
        }
        self.rows.sort_by(compare);
        if let Some(state) = state {
            self.state = state.clone();
        }
    }
}

pub fn table_rows<'a>(
    namespaces: &'a NamespaceMap,
    queries: &'a QueryTableCache,
    source_search: Option<&'a str>,
) -> crate::table_view::TableRows<'a> {
    crate::table_view::TableRows {
        convoys: namespaces.values().flat_map(|namespace| namespace.convoys.values()).collect(),
        independent_results: queries
            .independents
            .iter()
            .map(|(query, result)| crate::table_view::QueryRows { query, rows: &result.rows, state: &result.state })
            .collect(),
        issue_results: queries
            .issues
            .iter()
            .map(|(query, result)| crate::table_view::QueryRows { query, rows: &result.rows, state: &result.state })
            .collect(),
        checkout_results: queries
            .checkouts
            .iter()
            .map(|(query, result)| crate::table_view::QueryRows { query, rows: &result.rows, state: &result.state })
            .collect(),
        source_search,
    }
}

/// A command that has been dispatched to the daemon and is awaiting completion.
pub struct InFlightCommand {
    pub repo_identity: RepoIdentity,
    pub repo: PathBuf,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ProjectIssueStartBatch {
    total: usize,
    started: usize,
    rejected: Vec<String>,
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

#[derive(Debug)]
pub(crate) struct RecentCommandFinish {
    error_message: Option<String>,
    row_error_message: Option<String>,
}

pub struct App {
    pub daemon: Arc<dyn DaemonHandle>,
    pub config: Arc<ConfigStore>,
    pub model: TuiModel,
    /// The open tab set — the single source of tab truth (ADR 0013).
    /// Mutate only through the `App` tab methods so the active-repo sync and
    /// `open-views.toml` persistence stay coherent.
    pub views: OpenViews,
    pub ui: UiState,
    pub theme: Theme,
    pub keymap: Keymap,
    pub proto_commands: CommandQueue,
    pub in_flight: HashMap<u64, InFlightCommand>,
    /// Command IDs whose execute acknowledgement has arrived but whose
    /// CommandFinished event has not yet been handled.
    pub acknowledged_dispatches: HashSet<u64>,
    /// Number of this TUI's pending-action dispatches whose execute
    /// acknowledgement has not arrived yet.
    pub pending_dispatch_acks: usize,
    /// Commands that finished while at least one pending-action dispatch was
    /// awaiting its acknowledgement. Lifecycle events are system-wide, so
    /// unrelated finishes may appear here temporarily; the map is cleared as
    /// soon as this TUI has no acknowledgements left to reconcile.
    pub(crate) recent_command_finishes: HashMap<u64, RecentCommandFinish>,
    pub next_project_issue_start_batch_id: u64,
    pub project_issue_start_batches: HashMap<u64, ProjectIssueStartBatch>,
    pub command_project_issue_starts: HashMap<u64, ProjectIssueStartContext>,
    pub pending_cancel: Option<u64>,
    pub should_quit: bool,
    pub screen: crate::widgets::screen::Screen,
    /// Per-repo shared data handles. Written by `apply_snapshot()`/`apply_delta()`,
    /// read by `RepoPage` widgets during reconciliation and rendering.
    pub repo_data: HashMap<RepoIdentity, Shared<RepoData>>,
    /// Per-repo issue paging state, driven by stateless queries.
    pub issue_views: HashMap<RepoIdentity, issue_view::IssueViewState>,
    /// Demand-backed issue windows keyed by their fully parameterized query.
    /// Default issue sections consume this; stateless paging remains for
    /// ephemeral searches beyond the window.
    pub query_tables: QueryTableCache,
    pub pending_fetch_more: HashSet<flotilla_protocol::QueryId>,
    /// Sender half for background issue query tasks. Cloned into spawned tasks.
    pub issue_update_tx: mpsc::UnboundedSender<issue_view::IssueQueryUpdate>,
    /// Receiver half, drained each event-loop iteration.
    pub issue_update_rx: mpsc::UnboundedReceiver<issue_view::IssueQueryUpdate>,
    /// Client session ID. Passed to `execute_query` for query dispatch.
    pub session_id: uuid::Uuid,
    /// Drop guard that gives in-process subscribers the same teardown
    /// semantics socket subscribers get from connection close.
    _query_subscription: QuerySubscription,
    /// Per-namespace convoy state. Keyed by namespace string. Populated from
    /// `DaemonEvent::ResultSet` / `ResultDelta`.
    pub namespaces: HashMap<String, NamespaceModel>,
    /// Repo paths this TUI asked the daemon to track (the [+] flow), whose
    /// `RepoTracked` events should open a tab on arrival.
    pub pending_repo_opens: Vec<PathBuf>,
    /// Last seen seq per named query, for re-subscribe cursors.
    pub query_seqs: HashMap<flotilla_protocol::QueryId, u64>,
    /// Set when the open-view set changed and the daemon subscription no
    /// longer matches it. The event loop re-subscribes and clears this.
    pub subscriptions_dirty: bool,
    /// A resolved pane attach waiting for the event loop to leave raw mode,
    /// run it as a child process, and then restore the TUI.
    pub pending_attach_command: Option<String>,
}

impl App {
    pub fn new(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>, config: Arc<ConfigStore>, theme: Theme) -> Self {
        let mut model = TuiModel::from_repo_info(repos_info);
        // Open-view set: persisted if present, else seeded to match the
        // pre-View tab bar (overview + convoys + one tab per tracked repo).
        // The seed isn't written back here — it is deterministic, and any
        // tab mutation persists the set (scoped mode must never write it).
        let mut views = match config.load_open_views() {
            Some(entries) => OpenViews::from_entries(entries),
            None => OpenViews::seed_with_keys(
                model
                    .repo_order
                    .iter()
                    .filter_map(|identity| model.repos.get(identity).map(|repo| (identity.clone(), repo.repository_key.clone()))),
            ),
        };
        let repository_keys =
            model.repos.iter().filter_map(|(identity, repo)| repo.repository_key.clone().map(|key| (identity.clone(), key))).collect();
        views.bind_repository_keys(&repository_keys);
        model.active_repo = views.active_repo_identity().cloned();
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
                issue_rows: Vec::new(),
                issue_section_label: String::new(),
                loading: rm.loading,
            });
            let page = RepoPage::new(identity.clone(), shared.clone(), ui.view_layout);
            repo_data_map.insert(identity.clone(), shared);
            screen.repo_pages.insert(identity.clone(), page);
        }

        let (issue_update_tx, issue_update_rx) = mpsc::unbounded_channel();
        let session_id = uuid::Uuid::new_v4();
        let query_subscription = daemon.query_subscription(session_id);

        Self {
            daemon,
            config,
            model,
            views,
            ui,
            theme,
            keymap,
            proto_commands: Default::default(),
            in_flight: HashMap::new(),
            acknowledged_dispatches: HashSet::new(),
            pending_dispatch_acks: 0,
            recent_command_finishes: HashMap::new(),
            next_project_issue_start_batch_id: 1,
            project_issue_start_batches: HashMap::new(),
            command_project_issue_starts: HashMap::new(),
            pending_cancel: None,
            should_quit: false,
            screen,
            repo_data: repo_data_map,
            issue_views: HashMap::new(),
            query_tables: QueryTableCache::default(),
            pending_fetch_more: HashSet::new(),
            issue_update_tx,
            issue_update_rx,
            session_id,
            _query_subscription: query_subscription,
            namespaces: HashMap::new(),
            pending_repo_opens: Vec::new(),
            query_seqs: HashMap::new(),
            subscriptions_dirty: true,
            pending_attach_command: None,
        }
    }

    /// Construct an `App` in scoped mode: exactly the one View at `address`,
    /// never touching the persisted open-view set (ADR 0013).
    pub fn new_scoped(
        daemon: Arc<dyn DaemonHandle>,
        repos_info: Vec<RepoInfo>,
        config: Arc<ConfigStore>,
        theme: Theme,
        address: ViewAddress,
    ) -> Self {
        let mut app = Self::new(daemon, repos_info, config, theme);
        app.views = OpenViews::scoped(address);
        app.subscriptions_dirty = true;
        app.sync_active_view();
        app
    }

    /// The named queries the open Views consume, with re-subscribe cursors —
    /// the tab set IS the subscription set (ADR 0013). Repo views ride the
    /// Plane-A repo streams and stay outside this lifecycle.
    pub fn query_cursors(&self) -> Vec<flotilla_protocol::QueryCursor> {
        let mut queries = Vec::new();
        for view in self.views.iter() {
            let Some(address) = view.address() else { continue };
            for query in view_kind::queries(address, view.source_search()) {
                if !queries.contains(&query) {
                    queries.push(query);
                }
            }
        }
        queries.into_iter().map(|query| flotilla_protocol::QueryCursor { since: self.query_seqs.get(&query).copied(), query }).collect()
    }

    /// Returns true when the UI has in-progress work that should be animated.
    pub fn needs_animation(&self) -> bool {
        if !self.in_flight.is_empty() {
            return true;
        }
        if self.screen.repo_pages.values().any(|page| {
            page.pending_actions.values().any(|a| matches!(a.status, PendingStatus::Submitting | PendingStatus::InFlight { .. }))
        }) {
            return true;
        }
        if self.views.has_pending_rows() {
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

    pub(crate) fn begin_project_issue_start_batch(&mut self, total: usize) -> u64 {
        let batch_id = self.next_project_issue_start_batch_id;
        self.next_project_issue_start_batch_id += 1;
        self.project_issue_start_batches.insert(batch_id, ProjectIssueStartBatch { total, started: 0, rejected: Vec::new() });
        self.set_status_message(Some(format!("Starting {total} convoys...")));
        batch_id
    }

    pub(crate) fn set_project_issue_start_pending(&mut self, ctx: &ProjectIssueStartContext, status: PendingStatus, description: String) {
        let ViewAddress::Project { namespace, name } = &ctx.address else { return };
        let query = flotilla_protocol::QueryId::Issues { scope: flotilla_protocol::QueryScope::new(namespace, name), search: None };
        let Some(index) = self.views.find(&ctx.address) else { return };
        let Some(view) = self.views.get_mut(index) else { return };
        let state = view.project_table_state.table_mut(crate::table_view::ProjectPanelKind::Issues);
        match status {
            PendingStatus::Submitting => {
                let _ = state.begin_pending(query, ctx.row_id.clone(), description);
            }
            PendingStatus::InFlight { command_id } => {
                if state.row_state(&ctx.row_id).is_none() {
                    let _ = state.begin_pending(query, ctx.row_id.clone(), description);
                }
                state.mark_pending(&ctx.row_id, command_id);
            }
            PendingStatus::Failed(message) => {
                if state.row_state(&ctx.row_id).is_none() {
                    let _ = state.begin_pending(query, ctx.row_id.clone(), description);
                }
                state.mark_failed(&ctx.row_id, message);
            }
        }
    }

    pub(crate) fn clear_project_issue_start_pending(&mut self, ctx: &ProjectIssueStartContext) {
        let Some(index) = self.views.find(&ctx.address) else { return };
        let Some(view) = self.views.get_mut(index) else { return };
        view.project_table_state.table_mut(crate::table_view::ProjectPanelKind::Issues).clear_row_state(&ctx.row_id);
    }

    pub(crate) fn record_project_issue_start_result(&mut self, ctx: ProjectIssueStartContext, result: Result<Option<String>, String>) {
        match result {
            Ok(_) => {
                self.clear_project_issue_start_pending(&ctx);
                if let Some(batch) = self.project_issue_start_batches.get_mut(&ctx.batch_id) {
                    batch.started += 1;
                }
            }
            Err(message) => {
                self.set_project_issue_start_pending(&ctx, PendingStatus::Failed(message.clone()), "Start convoy".into());
                if let Some(batch) = self.project_issue_start_batches.get_mut(&ctx.batch_id) {
                    batch.rejected.push(format!("{}: {message}", ctx.issue.id));
                }
            }
        }
        self.update_project_issue_start_status(ctx.batch_id);
    }

    fn update_project_issue_start_status(&mut self, batch_id: u64) {
        let Some(batch) = self.project_issue_start_batches.get(&batch_id) else { return };
        let rejected = batch.rejected.len();
        let pending = batch.total.saturating_sub(batch.started + rejected);
        let convoy_word = if batch.started == 1 { "convoy" } else { "convoys" };
        let mut message = format!("{} {convoy_word} started", batch.started);
        if pending > 0 {
            message.push_str(&format!(", {pending} pending"));
        }
        if rejected > 0 {
            message.push_str(&format!(", {rejected} rejected"));
            if let Some(first) = batch.rejected.first() {
                message.push_str(&format!(": {first}"));
                if rejected > 1 {
                    message.push_str(&format!("; +{} more", rejected - 1));
                }
            }
        }
        self.set_status_message(Some(message));
        if pending == 0 {
            self.project_issue_start_batches.remove(&batch_id);
        }
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
        Command { node_id: None, provisioning_target: None, context_repo: None, action }
    }

    pub fn repo_command(&self, action: CommandAction) -> Command {
        Command {
            node_id: None,
            provisioning_target: None,
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn repo_command_for_identity(&self, repo_identity: RepoIdentity, action: CommandAction) -> Command {
        Command { node_id: None, provisioning_target: None, context_repo: Some(RepoSelector::Identity(repo_identity)), action }
    }

    /// Check that a provisioning target refers to a known host and (for NewEnvironment)
    /// an advertised environment provider. Returns `Err(message)` for display if invalid.
    fn validate_provisioning_target(&self, target: &ProvisioningTarget) -> Result<(), String> {
        let host = target.host();
        self.model.resolve_host(host)?;
        if let ProvisioningTarget::NewEnvironment { provider, .. } = target {
            let has_provider = self
                .model
                .resolve_host(host)
                .is_ok_and(|h| h.summary.providers.iter().any(|p| p.category == "environment_provider" && p.implementation == *provider));
            if !has_provider {
                return Err(format!("no {provider} environment provider on {host}"));
            }
        }
        Ok(())
    }

    pub fn targeted_command(&self, action: CommandAction) -> Command {
        let target = &self.ui.provisioning_target;
        let node_id = self
            .model
            .resolve_host(target.host())
            .expect("validated provisioning target should resolve to a unique host")
            .summary
            .node
            .node_id
            .clone();
        Command { node_id: Some(node_id), provisioning_target: Some(target.clone()), context_repo: None, action }
    }

    pub fn targeted_repo_command(&self, action: CommandAction) -> Command {
        let target = &self.ui.provisioning_target;
        let node_id = self
            .model
            .resolve_host(target.host())
            .expect("validated provisioning target should resolve to a unique host")
            .summary
            .node
            .node_id
            .clone();
        Command {
            node_id: Some(node_id),
            provisioning_target: Some(target.clone()),
            context_repo: Some(RepoSelector::Identity(self.model.active_repo_identity().clone())),
            action,
        }
    }

    pub fn item_host_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command { node_id: self.item_execution_host(item), provisioning_target: None, context_repo: None, action }
    }

    pub fn item_host_repo_command(&self, action: CommandAction, item: &WorkItem) -> Command {
        Command {
            node_id: self.item_execution_host(item),
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

    pub(super) fn work_item_for_issue_row(&self, row: &IssueRow) -> WorkItem {
        WorkItem {
            kind: flotilla_protocol::WorkItemKind::Issue,
            identity: WorkItemIdentity::Issue(row.id.clone()),
            // Issue rows are synthesized from the local repo view, so the local
            // node id should already be known; this fallback is only a safety net.
            node_id: self.model.my_node_id().cloned().unwrap_or_else(|| NodeId::new("issue-row")),
            branch: None,
            description: row.issue.title.clone(),
            checkout: None,
            change_request_key: None,
            session_key: None,
            issue_keys: vec![row.id.clone()],
            workspace_refs: vec![],
            is_main_checkout: false,
            debug_group: vec![],
            source: (!row.issue.provider_display_name.is_empty()).then(|| row.issue.provider_display_name.clone()),
            terminal_keys: vec![],
            attachable_set_id: None,
            agent_keys: vec![],
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

    fn item_execution_host(&self, item: &WorkItem) -> Option<NodeId> {
        match self.model.my_node_id() {
            Some(my_node_id) if item.node_id != *my_node_id => Some(item.node_id.clone()),
            _ => None,
        }
    }

    fn active_repo_is_remote_only(&self) -> bool {
        self.model.active_repo_root_opt().is_some_and(|p| p.starts_with(Path::new("<remote>")))
    }

    pub fn visible_status_items(&self) -> Vec<VisibleStatusItem> {
        collect_visible_status_items(&self.model, &self.ui)
    }

    /// Persist the open-view set after any tab mutation. Scoped sessions
    /// never touch the persisted set (ADR 0013).
    pub fn persist_open_views(&self) {
        if !self.views.is_scoped() {
            self.config.save_open_views(&self.views.to_entries());
        }
    }

    /// Re-derive tab-dependent state after any change to the active tab:
    /// the model's active-repo scope, the unseen-changes badge, and the
    /// status bar's layout indicator.
    pub fn sync_active_view(&mut self) {
        self.model.active_repo = self.views.active_repo_identity().cloned();
        if let Some(identity) = self.model.active_repo.clone() {
            if let Some(rm) = self.model.repos.get_mut(&identity) {
                rm.has_unseen_changes = false;
            }
            if let Some(page) = self.screen.repo_pages.get(&identity) {
                self.ui.view_layout = page.layout;
            }
        }
    }

    pub fn dismiss_status_item(&mut self, id: usize) {
        self.ui.status_bar.dismissed_status_ids.insert(id);
    }

    /// The only sanctioned way to write `model.status_message`. A dismissed
    /// error chip stays hidden only while the message is unchanged; any new
    /// message un-dismisses so it becomes visible. Writing the field directly
    /// bypasses that and leaves new errors invisible after one dismissal —
    /// which is how convoy admission errors vanished during the #796 dogfood.
    pub(crate) fn set_status_message(&mut self, status_message: Option<String>) {
        if self.model.status_message != status_message {
            self.ui.status_bar.dismissed_status_ids.remove(&0);
        }
        self.model.status_message = status_message;
    }

    pub(crate) fn drain_background_updates(&mut self) {
        use issue_view::{IssueFetchCompletion, IssueFetchFailure, IssueQueryUpdate};

        while let Ok(update) = self.issue_update_rx.try_recv() {
            match update {
                IssueQueryUpdate::PageFetched { repo, params, requested_page, page } => {
                    let Some(view) = self.issue_views.get_mut(&repo) else { continue };
                    match view.complete_fetch(&params, requested_page, page) {
                        IssueFetchCompletion::Ignored => continue,
                        IssueFetchCompletion::Applied => {}
                        IssueFetchCompletion::AppliedAndRefresh => {
                            self.spawn_query_page(repo.clone(), params, 1, 50);
                        }
                    }
                    self.push_issue_items_to_repo_data(&repo);
                }
                IssueQueryUpdate::QueryFailed { repo, params, requested_page, message } => {
                    tracing::warn!(%message, requested_page, is_search = params.search.is_some(), "issue query failed");
                    let Some(view) = self.issue_views.get_mut(&repo) else { continue };
                    let failure = view.fail_fetch(&params, requested_page);
                    if failure == IssueFetchFailure::Ignored {
                        continue;
                    }

                    let initial_failure = failure == IssueFetchFailure::Initial;
                    let refresh_after_failure = failure == IssueFetchFailure::ExistingAndRefresh;
                    self.set_status_message(Some(message));

                    if initial_failure && params.search.is_some() {
                        if let Some(view) = self.issue_views.get_mut(&repo) {
                            view.search = None;
                            view.search_query = None;
                        }
                        if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
                            page.active_search_query = None;
                        }
                        self.push_issue_items_to_repo_data(&repo);
                    } else if initial_failure {
                        if let Some(view) = self.issue_views.get_mut(&repo) {
                            view.default = None;
                        }
                    }

                    if refresh_after_failure {
                        self.spawn_query_page(repo, params, 1, 50);
                    }
                }
            }
        }
    }

    fn begin_issue_page_fetch(&mut self, repo: &RepoIdentity, params: &flotilla_protocol::issue_query::IssueQuery, page: u32) -> bool {
        let view = self.issue_views.entry(repo.clone()).or_default();
        view.begin_page_fetch(params, page)
    }

    fn begin_issue_refresh(&mut self, repo: &RepoIdentity, params: &flotilla_protocol::issue_query::IssueQuery) -> bool {
        let Some(view) = self.issue_views.get_mut(repo) else { return false };
        view.request_refresh(params) == issue_view::IssueRefreshRequest::Started
    }

    /// Spawn a background task to query one page of issue results.
    ///
    /// Currently always queries the local daemon (`host: None`). Remote-only
    /// repos are skipped in `maybe_fetch_default_issues` and the search widgets
    /// don't set a host. If future code paths need to query a remote host's
    /// issues, this method (or the executor interception) will need to forward
    /// the original `Command.host`.
    fn spawn_query_page(&self, repo: RepoIdentity, params: flotilla_protocol::issue_query::IssueQuery, page: u32, count: usize) {
        let daemon = self.daemon.clone();
        let tx = self.issue_update_tx.clone();
        let session_id = self.session_id;
        let params_clone = params.clone();
        tokio::spawn(async move {
            let cmd = Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssues {
                    repo: RepoSelector::Identity(repo.clone()),
                    params: params_clone.clone(),
                    page,
                    count,
                },
            };
            match daemon.execute_query(cmd, session_id).await {
                Ok(CommandValue::IssuePage(result_page)) => {
                    let _ = tx.send(issue_view::IssueQueryUpdate::PageFetched {
                        repo,
                        params: params_clone,
                        requested_page: page,
                        page: result_page,
                    });
                }
                Ok(other) => {
                    let _ = tx.send(issue_view::IssueQueryUpdate::QueryFailed {
                        repo,
                        params: params_clone,
                        requested_page: page,
                        message: format!("unexpected query result: {other:?}"),
                    });
                }
                Err(e) => {
                    let _ =
                        tx.send(issue_view::IssueQueryUpdate::QueryFailed { repo, params: params_clone, requested_page: page, message: e });
                }
            }
        });
    }

    /// Fetch default issues for a repo if they haven't been fetched yet.
    fn maybe_fetch_default_issues(&mut self, repo_identity: &RepoIdentity) {
        if self.model.repos.get(repo_identity).is_some_and(|r| r.path.starts_with("<remote>")) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let params = flotilla_protocol::issue_query::IssueQuery::default();
        if !self.begin_issue_page_fetch(repo_identity, &params, 1) {
            return;
        }
        self.spawn_query_page(repo_identity.clone(), params, 1, 50);
    }

    /// Refresh settled issue listings while preserving their current rows until
    /// replacement page-one results arrive. An initial fetch already in flight
    /// is left alone.
    fn refresh_issue_views(&mut self, repo_identity: &RepoIdentity) {
        if self.model.repos.get(repo_identity).is_some_and(|r| r.path.starts_with("<remote>")) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }

        self.maybe_fetch_default_issues(repo_identity);
        let params: Vec<_> = self
            .issue_views
            .get(repo_identity)
            .into_iter()
            .flat_map(|view| [view.default.as_ref(), view.search.as_ref()])
            .flatten()
            .map(|state| state.params.clone())
            .collect();
        for params in params {
            if self.begin_issue_refresh(repo_identity, &params) {
                self.spawn_query_page(repo_identity.clone(), params, 1, 50);
            }
        }
    }

    /// Push issue rows from `IssueViewState` into the `Shared<RepoData>` for
    /// a repo so the `SplitTable` can render them in the native issue section.
    fn push_issue_items_to_repo_data(&self, repo_identity: &RepoIdentity) {
        let Some(view) = self.issue_views.get(repo_identity) else { return };
        let issue_rows = view.active_issue_rows();
        let label = self.model.repos.get(repo_identity).map(|rm| rm.labels.issues.section.clone()).unwrap_or_else(|| "Issues".to_string());
        if let Some(handle) = self.repo_data.get(repo_identity) {
            handle.mutate(|d| {
                d.issue_rows = issue_rows;
                d.issue_section_label = label;
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
        let my_node_id = self.model.my_node_id().cloned();
        let active_repo_is_remote_only = self.active_repo_is_remote_only();
        crate::widgets::WidgetContext {
            model: &self.model,
            keymap: &self.keymap,
            config: &self.config,
            in_flight: &self.in_flight,
            provisioning_target: &self.ui.provisioning_target,
            my_host,
            my_node_id,
            views: &mut self.views,
            commands: &mut self.proto_commands,
            active_repo_is_remote_only,
            namespaces: &self.namespaces,
            query_tables: &self.query_tables,
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
                    if let Some(identity) = self.model.active_repo.clone() {
                        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
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
                    if let Some(identity) = self.model.active_repo.clone() {
                        if let Some(page) = self.screen.repo_pages.get_mut(&identity) {
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
                AppAction::SwitchToTab(i) => {
                    self.switch_tab(i);
                }
                AppAction::OpenView(address) => {
                    self.open_view(address);
                }
                AppAction::DrillView(address) => {
                    self.drill_view(address);
                }
                AppAction::BackView => {
                    self.back_view();
                }
                AppAction::ExecuteTableIntent(intent) => {
                    self.execute_table_intent(intent);
                }
                AppAction::SetTableFilter(filter) => {
                    self.views.active_table_state_mut().filter = filter;
                }
                AppAction::SetSourceSearch(search) => {
                    if self.views.active_table_state().source_search != search {
                        self.views.active_table_state_mut().source_search = search;
                        self.subscriptions_dirty = true;
                    }
                }
                AppAction::FetchMore(query) => {
                    if self.pending_fetch_more.insert(query.clone()) {
                        let daemon = Arc::clone(&self.daemon);
                        tokio::spawn(async move {
                            if let Err(error) = daemon.fetch_more(&query).await {
                                tracing::warn!(%error, %query, "fetch-more intent failed");
                            }
                        });
                    }
                }
                AppAction::SwitchToLastView => {
                    self.switch_to_last_view();
                }
                AppAction::CloseTab(i) => {
                    self.close_tab(i);
                }
                AppAction::CloseActiveTab => {
                    self.close_active_tab();
                }
                AppAction::ExpectRepoOpen(path) => {
                    self.pending_repo_opens.push(path);
                }
                AppAction::PersistOpenViews => {
                    self.persist_open_views();
                }
                AppAction::OpenFilePicker => {
                    self.open_file_picker_from_active_repo_parent();
                }
                AppAction::PrevTab => {
                    self.prev_tab();
                }
                AppAction::NextTab => {
                    self.next_tab();
                }
                AppAction::MoveTabLeft => {
                    self.move_tab(-1);
                }
                AppAction::MoveTabRight => {
                    self.move_tab(1);
                }
                AppAction::Refresh => {
                    let queries = self
                        .views
                        .active_address()
                        .map(|address| view_kind::queries(address, self.views.active_source_search()))
                        .unwrap_or_default();
                    if !queries.is_empty() {
                        for query in queries {
                            self.query_seqs.remove(&query);
                        }
                        self.subscriptions_dirty = true;
                    } else if let Some(repo) = self.model.active_repo_root_opt().cloned() {
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
                        page.active_search_query = Some(query.clone());
                    }
                    // Reset search paging state and set search_query so that
                    // in-flight results from a previous search are discarded.
                    let view = self.issue_views.entry(repo.clone()).or_default();
                    view.search = None;
                    view.search_query = Some(query);
                }
                AppAction::ClearSearchQuery { repo } => {
                    if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
                        page.active_search_query = None;
                    }
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
            DaemonEvent::RepoRefreshCompleted { repo_identity, .. } => self.refresh_issue_views(&repo_identity),
            DaemonEvent::RepoTracked(info) => self.handle_repo_added(*info),
            DaemonEvent::RepoUntracked { repo_identity, .. } => self.handle_repo_removed(&repo_identity),
            DaemonEvent::CommandStarted { command_id, node_id, repo_identity, repo, description, .. } => {
                tracing::info!(%command_id, %node_id, %description, repo = %repo_identity.path, "command started");
                let repo = repo
                    .or_else(|| self.model.repos.get(&repo_identity).map(|rm| rm.path.clone()))
                    .unwrap_or_else(|| TuiModel::display_path(&repo_identity, None));
                self.in_flight.insert(command_id, InFlightCommand { repo_identity, repo, description });
            }
            DaemonEvent::CommandFinished { command_id, node_id: _, repo_identity: _, repo: _, result, .. } => {
                if let Some(cmd) = self.in_flight.remove(&command_id) {
                    let error_message = match &result {
                        CommandValue::Error { message } => {
                            tracing::warn!(%command_id, description = %cmd.description, error = %message, "command failed");
                            Some(message.clone())
                        }
                        _ => {
                            tracing::info!(%command_id, description = %cmd.description, "command finished");
                            None
                        }
                    };
                    let row_error_message = match &result {
                        CommandValue::Error { message } => Some(message.clone()),
                        CommandValue::Cancelled => Some("Command cancelled".to_string()),
                        _ => None,
                    };
                    let project_issue_start = self.command_project_issue_starts.remove(&command_id);
                    executor::handle_result(result.clone(), self);
                    if let Some(ctx) = project_issue_start {
                        match result {
                            CommandValue::ConvoyStarted { name, .. } => {
                                self.record_project_issue_start_result(ctx, Ok(Some(name)));
                            }
                            CommandValue::Error { message } => {
                                self.record_project_issue_start_result(ctx, Err(message));
                            }
                            CommandValue::Cancelled => {
                                self.record_project_issue_start_result(ctx, Err("cancelled".into()));
                            }
                            _ => {}
                        }
                    }

                    // Find which repo+identity has this command_id
                    let found: Option<(RepoIdentity, WorkItemIdentity)> =
                        self.screen.repo_pages.iter().find_map(|(repo_identity, page)| {
                            page.pending_actions
                                .iter()
                                .find(|(_, a)| matches!(a.status, PendingStatus::InFlight { command_id: id } if id == command_id))
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

                    self.views.finish_pending_row_command(command_id, row_error_message.as_deref());

                    if !self.acknowledged_dispatches.remove(&command_id) && self.pending_dispatch_acks > 0 {
                        self.recent_command_finishes.insert(command_id, RecentCommandFinish { error_message, row_error_message });
                    }
                }
            }
            DaemonEvent::CommandStepUpdate { command_id, description, step_index, step_count, status, node_id, .. } => {
                if let Some(cmd) = self.in_flight.get_mut(&command_id) {
                    let step_label = format!("{}/{}", step_index + 1, step_count);
                    match status {
                        StepStatus::Started => {
                            tracing::info!(%command_id, %node_id, step = %step_label, %description, "step started");
                            cmd.description = format!("{} ({})", description, step_label);
                        }
                        StepStatus::Skipped => {
                            tracing::info!(%command_id, %node_id, step = %step_label, %description, "step skipped");
                        }
                        StepStatus::Succeeded => {
                            tracing::info!(%command_id, %node_id, step = %step_label, %description, "step succeeded");
                        }
                        StepStatus::Produced { .. } => {
                            tracing::info!(%command_id, %node_id, step = %step_label, %description, "step produced output");
                        }
                        StepStatus::Failed { ref message } => {
                            tracing::warn!(%command_id, %node_id, step = %step_label, %description, error = %message, "step failed");
                            self.set_status_message(Some(format!("{description}: {message}")));
                        }
                    }
                }
            }
            DaemonEvent::PeerStatusChanged { node_id, status } => {
                let peer_status = PeerStatus::from(status);
                let clear_target = matches!(peer_status, PeerStatus::Disconnected | PeerStatus::Rejected)
                    && self.model.node_id_for_host(self.ui.provisioning_target.host()).is_some_and(|target| *target == node_id);
                for entry in self.model.hosts.values_mut().filter(|entry| entry.summary.node.node_id == node_id) {
                    entry.status = peer_status;
                }
                if clear_target {
                    self.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::local() };
                }
            }
            DaemonEvent::HostSnapshot(snap) => {
                let status = PeerStatus::from(snap.connection_status);
                let environment_id = snap.environment_id.clone();
                let host_name = self
                    .model
                    .hosts
                    .get(&environment_id)
                    .map(|entry| entry.host_name.clone())
                    .or_else(|| snap.summary.host_name.clone())
                    .unwrap_or_else(|| HostName::new(&snap.node.display_name));
                self.model.hosts.insert(environment_id.clone(), TuiHostState {
                    environment_id,
                    host_name,
                    is_local: snap.is_local,
                    status,
                    summary: snap.summary,
                });
            }
            DaemonEvent::HostRemoved { environment_id, .. } => {
                let clear_target =
                    self.model.resolve_host(self.ui.provisioning_target.host()).is_ok_and(|target| target.environment_id == environment_id);
                self.model.hosts.remove(&environment_id);
                if clear_target {
                    self.ui.provisioning_target = ProvisioningTarget::Host { host: HostName::local() };
                }
            }
            DaemonEvent::ResultSet(result_set) => {
                let query = result_set.query();
                self.pending_fetch_more.remove(&query);
                self.query_seqs.insert(query.clone(), result_set.seq);
                self.views.reconcile_authoritative_rows(&query, &crate::table_view::AuthoritativeRowUpdate::Full);
                match &result_set.rows {
                    flotilla_protocol::Rows::Convoys(rows) => {
                        for namespace in self.namespaces.values_mut() {
                            namespace.convoys.clear();
                        }
                        for row in rows {
                            let convoy = ConvoySummary::from(row);
                            let namespace = convoy.namespace.clone();
                            let entry = self.namespaces.entry(namespace).or_default();
                            entry.convoys.insert(convoy.id.clone(), convoy);
                            entry.last_seq = result_set.seq;
                        }
                    }
                    flotilla_protocol::Rows::Independents { rows, .. } => {
                        self.query_tables
                            .independents
                            .insert(query.clone(), QueryTableResult { rows: rows.clone(), state: result_set.state.clone() });
                    }
                    flotilla_protocol::Rows::Issues { rows, .. } => {
                        self.query_tables
                            .issues
                            .insert(query.clone(), QueryTableResult { rows: rows.clone(), state: result_set.state.clone() });
                    }
                    flotilla_protocol::Rows::Checkouts { rows, .. } => {
                        self.query_tables.checkouts.insert(query, QueryTableResult { rows: rows.clone(), state: result_set.state.clone() });
                    }
                }
            }
            DaemonEvent::ResultDelta(delta) => {
                let query = delta.query();
                self.pending_fetch_more.remove(&query);
                self.query_seqs.insert(query.clone(), delta.seq);
                match &delta.changes {
                    flotilla_protocol::QueryChanges::Convoys { changed, removed } => {
                        let updated = changed
                            .iter()
                            .map(|row| crate::table_view::RowId::new(ConvoyId::for_resource(&row.resource).as_str()))
                            .chain(removed.iter().map(|resource| crate::table_view::RowId::new(ConvoyId::for_resource(resource).as_str())))
                            .collect();
                        self.views.reconcile_authoritative_rows(&query, &crate::table_view::AuthoritativeRowUpdate::Rows(updated));
                        for row in changed {
                            let convoy = ConvoySummary::from(row);
                            let namespace = convoy.namespace.clone();
                            let entry = self.namespaces.entry(namespace).or_default();
                            entry.convoys.insert(convoy.id.clone(), convoy);
                            entry.last_seq = delta.seq;
                        }
                        for removed in removed {
                            if let Some(entry) = self.namespaces.get_mut(&removed.namespace) {
                                entry.convoys.shift_remove(&ConvoyId::for_resource(removed));
                                entry.last_seq = delta.seq;
                            }
                        }
                    }
                    flotilla_protocol::QueryChanges::Independents { changed, removed, .. } => {
                        let result = self.query_tables.independents.entry(query).or_default();
                        result.apply_delta(
                            changed,
                            removed,
                            delta.state.as_ref(),
                            |row| row.resource.clone(),
                            |left, right| {
                                (&left.name, &left.host, &left.resource.namespace, &left.resource.name).cmp(&(
                                    &right.name,
                                    &right.host,
                                    &right.resource.namespace,
                                    &right.resource.name,
                                ))
                            },
                        );
                    }
                    flotilla_protocol::QueryChanges::Issues { changed, removed, .. } => {
                        let flotilla_protocol::QueryId::Issues { scope, .. } = &query else {
                            unreachable!("issue changes always have an issue query")
                        };
                        let updated = changed
                            .iter()
                            .map(|row| crate::table_view::issue_row_id(scope, &row.reference))
                            .chain(removed.iter().map(|reference| crate::table_view::issue_row_id(scope, reference)))
                            .collect();
                        self.views.reconcile_authoritative_rows(&query, &crate::table_view::AuthoritativeRowUpdate::Rows(updated));
                        let result = self.query_tables.issues.entry(query.clone()).or_default();
                        result.apply_delta(
                            changed,
                            removed,
                            delta.state.as_ref(),
                            |row| row.reference.clone(),
                            |left, right| left.reference.cmp_id_desc(&right.reference),
                        );
                    }
                    flotilla_protocol::QueryChanges::Checkouts { changed, removed, .. } => {
                        let result = self.query_tables.checkouts.entry(query).or_default();
                        result.apply_delta(
                            changed,
                            removed,
                            delta.state.as_ref(),
                            |row| row.resource.clone(),
                            |left, right| {
                                (&left.host, &left.path, &left.resource.name).cmp(&(&right.host, &right.path, &right.resource.name))
                            },
                        );
                    }
                }
            }
        }
    }

    fn apply_snapshot(&mut self, snap: RepoSnapshot) {
        let repo_identity = snap.repo_identity.clone();
        let path = snap
            .repo
            .clone()
            .or_else(|| self.model.repos.get(&repo_identity).map(|rm| rm.path.clone()))
            .unwrap_or_else(|| TuiModel::display_path(&repo_identity, None));
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
        if self.model.active_repo.as_ref() != Some(&repo_identity) && *old_providers != *rm.providers {
            if let Some(repo_model) = self.model.repos.get_mut(&repo_identity) {
                repo_model.has_unseen_changes = true;
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

        self.maybe_fetch_default_issues(&repo_identity);
    }

    fn apply_delta(&mut self, delta: RepoDelta) {
        let repo_identity = delta.repo_identity.clone();
        let path = delta
            .repo
            .clone()
            .or_else(|| self.model.repos.get(&repo_identity).map(|rm| rm.path.clone()))
            .unwrap_or_else(|| TuiModel::display_path(&repo_identity, None));
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
        if has_data_changes && self.model.active_repo.as_ref() != Some(&repo_identity) {
            if let Some(repo_model) = self.model.repos.get_mut(&repo_identity) {
                repo_model.has_unseen_changes = true;
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
        if let Some(existing) = self.model.repos.get_mut(&identity) {
            if existing.repository_key.is_none() {
                if let Some(key) = info.repository_key {
                    existing.repository_key = Some(key.clone());
                    self.views.bind_repository_keys(&HashMap::from([(identity, key)]));
                    self.subscriptions_dirty = true;
                }
            }
            return;
        }
        let path = TuiModel::display_path(&identity, info.path.clone());

        // Create Shared<RepoData> and RepoPage for the new repo
        let shared = Shared::new(RepoData {
            path: path.clone(),
            providers: Arc::new(ProviderData::default()),
            labels: info.labels.clone(),
            provider_names: info.provider_names.clone(),
            provider_health: info.provider_health.clone(),
            work_items: Vec::new(),
            issue_rows: Vec::new(),
            issue_section_label: String::new(),
            loading: info.loading,
        });
        let page = RepoPage::new(identity.clone(), shared.clone(), self.ui.view_layout);
        self.repo_data.insert(identity.clone(), shared);
        self.screen.repo_pages.insert(identity.clone(), page);

        self.model.repos.insert(identity.clone(), TuiRepoModel {
            identity: info.identity,
            repository_key: info.repository_key.clone(),
            path: path.clone(),
            providers: Arc::new(ProviderData::default()),
            labels: info.labels,
            provider_names: info.provider_names,
            provider_health: info.provider_health,
            loading: info.loading,
            has_unseen_changes: false,
        });
        self.model.repo_order.push(identity.clone());

        // Open a tab only when this TUI asked for the add (the [+] flow).
        // A repo registered by another session or the CLI is listed on the
        // overview but does not force a tab here — and a scoped pane never
        // grows tabs (ADR 0013).
        if let Some(pos) = self.pending_repo_opens.iter().position(|pending| pending == &path) {
            self.pending_repo_opens.remove(pos);
            if !self.views.is_scoped() {
                self.open_view(match info.repository_key {
                    Some(key) => ViewAddress::repo_with_key(identity, key),
                    None => ViewAddress::repo(identity),
                });
            }
        }
    }

    /// The repo's data leaves the model; any tab open on it stays, rendering
    /// its dangling-repo error state until the user closes it (ADR 0013 —
    /// no silent pruning of the open-view set).
    fn handle_repo_removed(&mut self, repo_identity: &RepoIdentity) {
        self.model.repos.remove(repo_identity);
        self.model.repo_order.retain(|repo| repo != repo_identity);
        self.repo_data.remove(repo_identity);
        self.issue_views.remove(repo_identity);
        self.screen.repo_pages.remove(repo_identity);
        // A modal opened against this repo (action menu, delete/close
        // confirm) must not outlive it: those widgets read the active repo's
        // labels unconditionally during render, which panics once the repo
        // is gone. RepoUntracked can arrive from another session at any
        // moment, so dismiss modals when the active tab loses its repo.
        if self.model.active_repo.as_ref() == Some(repo_identity) {
            self.dismiss_modals();
        }
    }

    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let identity = self.model.active_repo.as_ref()?;
        self.screen.repo_pages.get(identity).and_then(|page| page.table.selected_work_item())
    }

    /// Get the selected row (WorkItem or IssueRow) as an owned enum.
    pub(super) fn selected_row_cloned(&self) -> Option<OwnedSelectedRow> {
        let identity = self.model.active_repo.as_ref()?;
        let page = self.screen.repo_pages.get(identity)?;
        match page.table.selected_row()? {
            SelectedRow::WorkItem(item) => Some(OwnedSelectedRow::WorkItem(Box::new(item.clone()))),
            SelectedRow::Issue(row) => Some(OwnedSelectedRow::IssueRow(Box::new(row.clone()))),
        }
    }

    /// Build a repo command for an issue-row action (where no WorkItem context exists).
    pub(super) fn provider_repo_command_for_issue(&self, action: CommandAction) -> Command {
        self.repo_command(action)
    }

    /// Returns convoys for a namespace in daemon-provided order.
    pub fn convoys(&self, namespace: &str) -> Vec<&crate::convoy_model::ConvoySummary> {
        self.namespaces.get(namespace).map(|model| model.convoys.values().collect()).unwrap_or_default()
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
