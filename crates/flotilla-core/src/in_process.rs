//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::{
    AssociationKey, Command, CorrelationKey, DaemonEvent, DeltaEntry, HostListResponse, HostName, HostPath, HostProvidersResponse,
    HostStatusResponse, HostSummary, Issue, PeerConnectionState, ProviderData, ProviderInfo, RepoDelta, RepoDetailResponse, RepoInfo,
    RepoProvidersResponse, RepoSnapshot, RepoSummary, RepoWorkResponse, StatusResponse, StreamKey, TopologyResponse, TopologyRoute,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    config::ConfigStore,
    convert::snapshot_to_proto,
    daemon::DaemonHandle,
    executor,
    host_registry::HostCounts,
    issue_cache::IssueCache,
    model::{provider_names_from_registry, repo_name, RepoModel},
    providers::discovery::{discover_providers, DiscoveryResult, DiscoveryRuntime, EnvironmentBag},
    refresh::RefreshSnapshot,
    repo_state::{RepoRootState, RepoState, SnapshotBuildContext},
    step::run_step_plan,
};

fn fallback_repo_identity(path: &Path) -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.to_string_lossy().into_owned() }
}

fn repo_identity_from_bag_or_path(path: &Path, bag: &EnvironmentBag) -> flotilla_protocol::RepoIdentity {
    bag.repo_identity().unwrap_or_else(|| fallback_repo_identity(path))
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn normalize_local_provider_hosts(mut providers: ProviderData, host_name: &HostName) -> ProviderData {
    providers.checkouts = providers
        .checkouts
        .into_iter()
        .map(|(host_path, mut checkout)| {
            checkout.correlation_keys = normalize_correlation_keys(checkout.correlation_keys, host_name);
            (HostPath::new(host_name.clone(), host_path.path), checkout)
        })
        .collect();

    for change_request in providers.change_requests.values_mut() {
        change_request.correlation_keys = normalize_correlation_keys(std::mem::take(&mut change_request.correlation_keys), host_name);
    }

    for session in providers.sessions.values_mut() {
        session.correlation_keys = normalize_correlation_keys(std::mem::take(&mut session.correlation_keys), host_name);
    }

    for workspace in providers.workspaces.values_mut() {
        workspace.correlation_keys = normalize_correlation_keys(std::mem::take(&mut workspace.correlation_keys), host_name);
    }

    providers
}

fn normalize_correlation_keys(keys: Vec<CorrelationKey>, host_name: &HostName) -> Vec<CorrelationKey> {
    keys.into_iter()
        .map(|key| match key {
            CorrelationKey::CheckoutPath(host_path) => CorrelationKey::CheckoutPath(HostPath::new(host_name.clone(), host_path.path)),
            other => other,
        })
        .collect()
}

fn merge_local_provider_data(base: &mut ProviderData, other: &ProviderData) {
    for (host_path, checkout) in &other.checkouts {
        // Preferred root data is merged first and remains authoritative on collisions.
        base.checkouts.entry(host_path.clone()).or_insert_with(|| checkout.clone());
    }
    for (name, terminal) in &other.managed_terminals {
        base.managed_terminals.entry(name.clone()).or_insert_with(|| terminal.clone());
    }
    for (name, branch) in &other.branches {
        base.branches.entry(name.clone()).or_insert_with(|| branch.clone());
    }
    for (name, workspace) in &other.workspaces {
        base.workspaces.entry(name.clone()).or_insert_with(|| workspace.clone());
    }
    for (id, set) in &other.attachable_sets {
        base.attachable_sets.entry(id.clone()).or_insert_with(|| set.clone());
    }
    for (key, cr) in &other.change_requests {
        base.change_requests.entry(key.clone()).or_insert_with(|| cr.clone());
    }
    for (key, issue) in &other.issues {
        base.issues.entry(key.clone()).or_insert_with(|| issue.clone());
    }
    for (key, session) in &other.sessions {
        base.sessions.entry(key.clone()).or_insert_with(|| session.clone());
    }
}

fn merge_provider_health(merged: &mut HashMap<(&'static str, String), bool>, next: &HashMap<(&'static str, String), bool>) {
    for (provider, healthy) in next {
        merged.entry(provider.clone()).and_modify(|existing| *existing &= *healthy).or_insert(*healthy);
    }
}

fn merge_provider_errors(merged: &mut Vec<crate::data::RefreshError>, next: &[crate::data::RefreshError]) {
    for err in next {
        if !merged
            .iter()
            .any(|existing| existing.category == err.category && existing.provider == err.provider && existing.message == err.message)
        {
            merged.push(err.clone());
        }
    }
}

/// Returned by `execute()` for commands that run inline without lifecycle events.
/// Callers must not treat this as a real command ID for in-flight tracking.
const INLINE_COMMAND_ID: u64 = 0;

/// Extract issue IDs referenced by association keys on change requests and checkouts.
fn collect_linked_issue_ids(providers: &ProviderData) -> Vec<String> {
    let mut ids = HashSet::new();
    for cr in providers.change_requests.values() {
        for key in &cr.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            ids.insert(issue_id.clone());
        }
    }
    for co in providers.checkouts.values() {
        for key in &co.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            ids.insert(issue_id.clone());
        }
    }
    ids.into_iter().collect()
}

/// Clone base providers and replace the issues field with cached issues or search results.
fn inject_issues(base_providers: &ProviderData, cache: &IssueCache, search_results: &Option<Vec<(String, Issue)>>) -> ProviderData {
    let mut providers = base_providers.clone();
    if let Some(ref results) = search_results {
        providers.issues = results.iter().map(|(id, i)| (id.clone(), i.clone())).collect();
    } else if !cache.is_empty() {
        providers.issues = (*cache.to_index_map()).clone();
    } else {
        providers.issues.clear();
    }
    providers
}

fn inject_issues_from_entries(
    base_providers: &ProviderData,
    issue_entries: &Arc<indexmap::IndexMap<String, Issue>>,
    search_results: &Option<Vec<(String, Issue)>>,
) -> ProviderData {
    let mut providers = base_providers.clone();
    if let Some(ref results) = search_results {
        providers.issues = results.iter().map(|(id, i)| (id.clone(), i.clone())).collect();
    } else if !issue_entries.is_empty() {
        providers.issues = (**issue_entries).clone();
    } else {
        providers.issues.clear();
    }
    providers
}

/// Build a proto RepoSnapshot, optionally merging peer provider data before correlation.
fn build_repo_snapshot_with_peers(
    ctx: SnapshotBuildContext<'_>,
    seq: u64,
    peer_overlay: Option<&[(HostName, ProviderData)]>,
) -> RepoSnapshot {
    let SnapshotBuildContext { repo_identity, path, local_providers, errors, provider_health, cache, search_results, host_name } = ctx;
    let local_providers = normalize_local_provider_hosts(inject_issues(local_providers, cache, search_results), host_name);

    // Merge peer provider data if any
    let providers = if let Some(peers) = peer_overlay {
        let peer_refs: Vec<(HostName, &ProviderData)> = peers.iter().map(|(h, d)| (h.clone(), d)).collect();
        Arc::new(crate::merge::merge_provider_data(&local_providers, host_name, &peer_refs))
    } else {
        Arc::new(local_providers)
    };

    let (work_items, correlation_groups) = crate::data::correlate(&providers);
    let re_snapshot =
        RefreshSnapshot { providers, work_items, correlation_groups, errors: errors.to_vec(), provider_health: provider_health.clone() };
    let mut snapshot = snapshot_to_proto(repo_identity, path, seq, &re_snapshot, host_name);
    snapshot.issue_total = cache.total_count;
    snapshot.issue_has_more = cache.has_more;
    snapshot.issue_search_results = search_results.clone();
    snapshot
}

/// Choose whether to broadcast a full snapshot or a delta.
///
/// Sends a full snapshot when:
/// - This is the first broadcast (prev_seq == 0)
/// - The delta has no changes (shouldn't happen, but avoids empty deltas)
/// - The serialized delta is larger than the serialized full snapshot
///
/// Otherwise sends a delta.
fn choose_event(snapshot: RepoSnapshot, delta: DeltaEntry) -> DaemonEvent {
    // First broadcast or empty delta → always send full
    if delta.prev_seq == 0 || delta.changes.is_empty() {
        return DaemonEvent::RepoSnapshot(Box::new(snapshot));
    }

    let snapshot_delta = RepoDelta {
        seq: delta.seq,
        prev_seq: delta.prev_seq,
        repo_identity: snapshot.repo_identity.clone(),
        repo: snapshot.repo.clone(),
        changes: delta.changes,
        work_items: snapshot.work_items.clone(),
        issue_total: snapshot.issue_total,
        issue_has_more: snapshot.issue_has_more,
        issue_search_results: snapshot.issue_search_results.clone(),
    };

    // Compare serialized sizes — if delta is larger, send full
    let delta_size = serde_json::to_string(&snapshot_delta).map(|s| s.len());
    let full_size = serde_json::to_string(&snapshot).map(|s| s.len());

    match (delta_size, full_size) {
        (Ok(d), Ok(f)) if d < f => {
            debug!(delta_bytes = d, full_bytes = f, "delta smaller than full, sending delta");
            DaemonEvent::RepoDelta(Box::new(snapshot_delta))
        }
        _ => {
            debug!("sending full snapshot (delta not smaller)");
            DaemonEvent::RepoSnapshot(Box::new(snapshot))
        }
    }
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<flotilla_protocol::RepoIdentity, RepoState>>,
    repo_order: RwLock<Vec<flotilla_protocol::RepoIdentity>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    next_command_id: AtomicU64,
    host_name: HostName,
    /// When true, only local providers (VCS, checkout manager, workspace
    /// manager, terminal pool) are registered. External providers (code
    /// review, issue tracker, cloud agents, AI utilities) are skipped
    /// because the follower receives that data from the leader via PeerData.
    follower: bool,
    /// Peer provider data overlay, keyed by repo identity.
    /// Set by the DaemonServer when peer snapshots arrive. Merged into
    /// the local snapshot during broadcast.
    peer_providers: RwLock<HashMap<flotilla_protocol::RepoIdentity, Vec<(HostName, ProviderData)>>>,
    /// Last applied overlay version per repo. `set_peer_providers` rejects
    /// applies whose version is older than the stored value, preventing stale
    /// data from overwriting fresher writes.
    peer_overlay_versions: RwLock<HashMap<flotilla_protocol::RepoIdentity, u64>>,
    /// Maps local tracked paths (including virtual synthetic paths) to RepoIdentity.
    // Lock ordering: do not hold path_identities across awaits that later take
    // repos/repo_order; add_repo intentionally takes it last while already
    // holding those write locks.
    path_identities: RwLock<HashMap<PathBuf, flotilla_protocol::RepoIdentity>>,
    host_registry: crate::host_registry::HostRegistry,
    /// Host-level environment assertions, computed once at startup and
    /// reused for each repo discovery.
    host_bag: EnvironmentBag,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: DiscoveryRuntime,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: crate::agents::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
}

impl InProcessDaemon {
    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because a background poll task is spawned that
    /// holds a reference. The poll loop checks every 100ms for new refresh
    /// snapshots and broadcasts delta or full events for each change.
    pub async fn new(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>, discovery: DiscoveryRuntime, host_name: HostName) -> Arc<Self> {
        use crate::providers::discovery::{self, DiscoveryResult};

        let follower = discovery.is_follower();
        let (event_tx, _) = broadcast::channel(256);
        let mut repos: HashMap<flotilla_protocol::RepoIdentity, RepoState> = HashMap::new();
        let mut order = Vec::new();
        let mut path_identities = HashMap::new();

        // Run host detection once before the repo loop
        let host_bag = discovery::run_host_detectors(&discovery.host_detectors, &*discovery.runner, &*discovery.env).await;
        let agent_state_store = crate::agents::shared_file_backed_agent_state_store(crate::config::flotilla_config_dir());

        for path in repo_paths {
            if path_identities.contains_key(&path) {
                continue;
            }
            let attachable_store = discovery.shared_attachable_store(&config);
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discovery::discover_providers(
                &host_bag,
                &path,
                &discovery.repo_detectors,
                &discovery.factories,
                &config,
                Arc::clone(&discovery.runner),
                Arc::clone(&attachable_store),
                &*discovery.env,
            )
            .await;
            if !unmet.is_empty() {
                debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
            }

            let identity = repo_identity_from_bag_or_path(&path, &host_repo_bag);
            let slug = repo_slug.clone();
            let mut model = RepoModel::new(path.clone(), registry, repo_slug, attachable_store, Arc::clone(&agent_state_store));
            model.data.loading = true;
            let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                order.push(identity.clone());
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
            }
            path_identities.insert(path.clone(), identity);
        }

        let local_host_summary = crate::host_summary::build_local_host_summary(
            &host_name,
            &host_bag,
            crate::host_summary::provider_statuses_from_registries(
                repos.values().map(|state| state.preferred_root().model.registry.as_ref()),
            ),
            &*discovery.env,
        );

        let daemon = Arc::new(Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
            config,
            next_command_id: AtomicU64::new(1),
            host_name: host_name.clone(),
            follower,
            peer_providers: RwLock::new(HashMap::new()),
            peer_overlay_versions: RwLock::new(HashMap::new()),
            path_identities: RwLock::new(path_identities),
            host_registry: crate::host_registry::HostRegistry::new(host_name.clone(), local_host_summary),
            host_bag,
            discovery,
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
        });

        // Spawn self-driving poll loop with a Weak reference.
        // The loop exits naturally when all external Arc owners drop.
        let weak = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                match weak.upgrade() {
                    Some(d) => d.poll_snapshots().await,
                    None => break,
                }
            }
        });

        daemon
    }

    /// Returns the host name for this daemon.
    pub fn host_name(&self) -> &HostName {
        &self.host_name
    }

    /// Returns the session ID for this daemon instance.
    ///
    /// Generated once at startup via `Uuid::new_v4()`. Used in peer Hello
    /// handshake so peers can detect daemon restarts.
    pub fn session_id(&self) -> uuid::Uuid {
        self.session_id
    }

    pub fn local_host_summary(&self) -> &HostSummary {
        self.host_registry.local_host_summary()
    }

    pub fn agent_state_store(&self) -> &crate::agents::SharedAgentStateStore {
        &self.agent_state_store
    }

    pub async fn set_daemon_socket_path(&self, path: PathBuf) {
        *self.daemon_socket_path.write().await = Some(path);
    }

    pub async fn daemon_socket_path(&self) -> Option<PathBuf> {
        self.daemon_socket_path.read().await.clone()
    }

    /// Returns the current connection status for a peer host.
    pub async fn peer_connection_status(&self, host: &HostName) -> PeerConnectionState {
        self.host_registry.peer_connection_status(host).await
    }

    pub async fn set_configured_peer_names(&self, peers: Vec<HostName>) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .set_configured_peer_names(peers, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_peer_host_summaries(&self, summaries: HashMap<HostName, HostSummary>) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .set_peer_host_summaries(summaries, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_connection_status(&self, host: &HostName, status: PeerConnectionState) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .publish_peer_connection_status(host, status, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_summary(&self, host: &HostName, summary: HostSummary) {
        self.host_registry
            .publish_peer_summary(host, summary, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_topology_routes(&self, routes: Vec<TopologyRoute>) {
        self.host_registry.set_topology_routes(routes).await;
    }

    async fn local_host_counts(&self) -> HostCounts {
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut counts = HostCounts::default();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            if !state.preferred_root().is_local {
                continue;
            }
            counts.repo_count += 1;
            if let Some(snapshot) = state.cached_snapshot() {
                counts.work_item_count += snapshot.work_items.len();
            }
        }

        counts
    }

    async fn remote_host_counts(&self) -> HashMap<HostName, HostCounts> {
        let peer_providers = self.peer_providers.read().await;
        let mut counts: HashMap<HostName, HostCounts> = HashMap::new();

        for peers in peer_providers.values() {
            for (host, providers) in peers {
                let entry = counts.entry(host.clone()).or_default();
                entry.repo_count += 1;
                entry.work_item_count += crate::data::correlate(providers).0.len();
            }
        }

        counts
    }

    /// Returns whether this daemon is running in follower mode.
    pub fn is_follower(&self) -> bool {
        self.follower
    }

    /// Resolve a repo identity to the preferred local path for execution or overlay updates.
    pub async fn preferred_local_path_for_identity(&self, identity: &flotilla_protocol::RepoIdentity) -> Option<PathBuf> {
        self.repos.read().await.get(identity).map(|state| state.preferred_path().to_path_buf())
    }

    /// Resolve a tracked local or synthetic repo path to its stable repo identity.
    pub async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        self.path_identities.read().await.get(repo_path).cloned()
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        let mut repo_bag = EnvironmentBag::new();
        let runner = &*self.discovery.runner;
        let env = &*self.discovery.env;
        for detector in &self.discovery.repo_detectors {
            repo_bag = repo_bag.extend(detector.detect(repo_path, runner, env).await);
        }
        let combined = self.host_bag.merge(&repo_bag);
        repo_identity_from_bag_or_path(repo_path, &combined)
    }

    /// Returns the paths of all locally tracked repos.
    ///
    /// Only local repo paths, not remote/virtual ones. Used by the outbound
    /// task to send local state to a newly connected peer.
    pub async fn tracked_repo_paths(&self) -> Vec<PathBuf> {
        self.repos.read().await.values().flat_map(RepoState::local_paths).collect()
    }

    async fn resolve_repo_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        match selector {
            flotilla_protocol::RepoSelector::Path(path) => {
                let identities = self.path_identities.read().await;
                if identities.contains_key(path) {
                    Ok(path.clone())
                } else {
                    Err(format!("repo not tracked: {}", path.display()))
                }
            }
            flotilla_protocol::RepoSelector::Query(query) => {
                let repos = self.repos.read().await;
                let entries: Vec<_> = repos.values().map(|state| (state.preferred_path(), state.slug())).collect();
                crate::resolve::resolve_repo(query, entries.into_iter()).map_err(|e| e.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => self
                .repos
                .read()
                .await
                .get(identity)
                .map(|state| state.preferred_path().to_path_buf())
                .ok_or_else(|| format!("repo not tracked: {identity}")),
        }
    }

    async fn resolve_checkout_selector(&self, selector: &flotilla_protocol::CheckoutSelector) -> Result<(PathBuf, String), String> {
        let repos = self.repos.read().await;
        let mut matches = Vec::new();
        for state in repos.values() {
            // The preferred root's provider view is the merged per-identity
            // checkout set after the first broadcast cycle. Before that first
            // broadcast, only the preferred root's own checkouts are present.
            for (host_path, checkout) in &state.preferred_root().model.data.providers.checkouts {
                if host_path.host != self.host_name {
                    continue;
                }
                let matched = match selector {
                    flotilla_protocol::CheckoutSelector::Path(path) => host_path.path == *path,
                    flotilla_protocol::CheckoutSelector::Query(query) => {
                        checkout.branch == *query || checkout.branch.contains(query) || host_path.path.to_string_lossy().contains(query)
                    }
                };
                if matched {
                    matches.push((state.preferred_path().to_path_buf(), checkout.branch.clone()));
                }
            }
        }
        match matches.len() {
            0 => Err("checkout not found".into()),
            1 => Ok(matches.remove(0)),
            _ => Err("checkout selector is ambiguous".into()),
        }
    }

    async fn resolve_repo_for_command(&self, command: &Command) -> Result<PathBuf, String> {
        use flotilla_protocol::CommandAction;

        match &command.action {
            CommandAction::Checkout { repo, .. } => self.resolve_repo_selector(repo).await,
            CommandAction::RemoveCheckout { checkout, .. } => self.resolve_checkout_selector(checkout).await.map(|(repo, _)| repo),
            CommandAction::Refresh { repo: Some(selector) } => self.resolve_repo_selector(selector).await,
            CommandAction::FetchCheckoutStatus { .. }
            | CommandAction::OpenChangeRequest { .. }
            | CommandAction::CloseChangeRequest { .. }
            | CommandAction::OpenIssue { .. }
            | CommandAction::LinkIssuesToChangeRequest { .. }
            | CommandAction::ArchiveSession { .. }
            | CommandAction::GenerateBranchName { .. }
            | CommandAction::TeleportSession { .. }
            | CommandAction::CreateWorkspaceForCheckout { .. }
            | CommandAction::CreateWorkspaceFromPreparedTerminal { .. }
            | CommandAction::PrepareTerminalForCheckout { .. }
            | CommandAction::SelectWorkspace { .. } => {
                let selector = command.context_repo.as_ref().ok_or_else(|| "command requires repo context".to_string())?;
                self.resolve_repo_selector(selector).await
            }
            _ => Err("command does not resolve to a single repo".to_string()),
        }
    }

    /// Get the local-only provider data for a repo (without peer overlay).
    ///
    /// Used by the outbound replication task to send only this host's
    /// authoritative data to peers, avoiding echo-back of merged peer data.
    pub async fn get_local_providers(&self, repo: &Path) -> Option<(ProviderData, u64)> {
        let identity = self.tracked_repo_identity_for_path(repo).await?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity)?;
        // add_root() keeps any local root ahead of synthetic remote-only
        // roots, so a non-local preferred root means this identity currently
        // has no executable local instance.
        if !state.preferred_root().is_local {
            return None;
        }
        // last_local_providers excludes peer overlay data; normalize after
        // injecting cached issues so outbound replication only sends this
        // host's authoritative state.
        let providers = normalize_local_provider_hosts(
            inject_issues(&state.last_local_providers, &state.issue_cache, &state.search_results),
            &self.host_name,
        );
        Some((providers, state.local_data_version()))
    }

    /// Update the peer provider data overlay for a repo and trigger re-broadcast.
    ///
    /// Called by the DaemonServer when PeerManager receives updated peer data.
    /// The peer data is merged into the local snapshot during the next broadcast.
    pub async fn set_peer_providers(&self, repo_path: &Path, peers: Vec<(HostName, ProviderData)>, overlay_version: u64) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo_path).await else {
            return;
        };
        {
            let mut versions = self.peer_overlay_versions.write().await;
            let stored = versions.entry(identity.clone()).or_insert(0);
            if overlay_version < *stored {
                return; // stale — a newer version has already been applied
            }
            *stored = overlay_version;
        }
        {
            let mut pp = self.peer_providers.write().await;
            if peers.is_empty() {
                pp.remove(&identity);
            } else {
                pp.insert(identity.clone(), peers);
            }
        }
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .sync_host_membership(&remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
        self.broadcast_snapshot_inner(repo_path, false).await;
    }

    /// Test accessor: return the current peer providers for a given repo identity.
    #[cfg(feature = "test-support")]
    pub async fn peer_providers_for_test(&self, identity: &flotilla_protocol::RepoIdentity) -> Vec<(HostName, ProviderData)> {
        self.peer_providers.read().await.get(identity).cloned().unwrap_or_default()
    }

    /// Test accessor: override the issue cache's `last_refreshed_at` timestamp
    /// for the given repo path. Useful for bypassing the MIN_INTERVAL_SECS
    /// guard in `refresh_issues_incremental`.
    #[cfg(feature = "test-support")]
    pub async fn set_issue_cache_refreshed_at_for_test(&self, repo: &Path, timestamp: &str) {
        let identity = self.tracked_repo_identity_for_path(repo).await.expect("set_issue_cache_refreshed_at_for_test: repo not tracked");
        let mut repos = self.repos.write().await;
        let state = repos.get_mut(&identity).expect("set_issue_cache_refreshed_at_for_test: repo state not found");
        state.issue_cache.mark_refreshed(timestamp.to_string());
    }

    /// Test accessor: directly invoke the incremental issue refresh cycle.
    #[cfg(feature = "test-support")]
    pub async fn refresh_issues_incremental_for_test(&self) {
        self.refresh_issues_incremental().await;
    }

    /// Poll all repos for new refresh snapshots.
    ///
    /// For each repo whose background refresh has produced a new snapshot,
    /// update internal state, increment the sequence number, and broadcast
    /// a `DaemonEvent::RepoSnapshot` or `DaemonEvent::RepoDelta`.
    ///
    /// Called automatically by the background poll loop spawned in `new()`.
    async fn poll_snapshots(&self) {
        // Collect changed snapshots under a brief write lock (need &mut for borrow_and_update),
        // then do correlation work outside the lock to avoid blocking other operations.
        let changed: Vec<_> = {
            let mut repos = self.repos.write().await;
            repos
                .iter_mut()
                .filter_map(|(identity, state)| {
                    let mut any_changed = false;
                    let mut snapshots = Vec::new();
                    for root in &mut state.roots {
                        let handle = &mut root.model.refresh_handle;
                        if handle.snapshot_rx.has_changed().unwrap_or(false) {
                            let _ = handle.snapshot_rx.borrow_and_update();
                            any_changed = true;
                        }
                        snapshots.push(handle.snapshot_rx.borrow().clone());
                    }
                    if !any_changed {
                        return None;
                    }
                    Some((
                        identity.clone(),
                        snapshots,
                        state.issue_cache.to_index_map(),
                        state.issue_cache.total_count,
                        state.issue_cache.has_more,
                        state.search_results.clone(),
                    ))
                })
                .collect()
        };
        // Write lock released here

        if changed.is_empty() {
            return;
        }

        // Read peer overlay once (brief read lock)
        let peer_overlay = self.peer_providers.read().await.clone();

        // Correlate and build proto snapshots outside any lock
        let mut updates = Vec::new();
        for (identity, snapshots, issue_entries, issue_total, issue_has_more, search_results) in changed {
            let mut local_providers = ProviderData::default();
            let mut provider_health = HashMap::new();
            let mut errors = Vec::new();
            let mut initialized = false;

            for snapshot in &snapshots {
                let providers = normalize_local_provider_hosts(
                    inject_issues_from_entries(&snapshot.providers, &issue_entries, &search_results),
                    &self.host_name,
                );
                if !initialized {
                    local_providers = providers;
                    initialized = true;
                } else {
                    merge_local_provider_data(&mut local_providers, &providers);
                }
                merge_provider_health(&mut provider_health, &snapshot.provider_health);
                merge_provider_errors(&mut errors, &snapshot.errors);
            }

            let last_local_providers = local_providers.clone();
            // Merge peer provider data if any
            let providers = if let Some(peers) = peer_overlay.get(&identity) {
                let peer_refs: Vec<(HostName, &ProviderData)> = peers.iter().map(|(h, d)| (h.clone(), d)).collect();
                Arc::new(crate::merge::merge_provider_data(&local_providers, &self.host_name, &peer_refs))
            } else {
                Arc::new(local_providers)
            };
            let (work_items, correlation_groups) = crate::data::correlate(&providers);

            let re_snapshot = RefreshSnapshot { providers, work_items, correlation_groups, errors, provider_health };
            updates.push((identity, last_local_providers, re_snapshot, issue_total, issue_has_more, search_results));
        }

        // Apply updates under write lock and broadcast
        let mut repos = self.repos.write().await;
        for (identity, last_local_providers, re_snapshot, issue_total, issue_has_more, search_results) in updates {
            let Some(state) = repos.get_mut(&identity) else {
                continue;
            };

            state.preferred_root_mut().model.data.providers = Arc::clone(&re_snapshot.providers);
            state.preferred_root_mut().model.data.correlation_groups = re_snapshot.correlation_groups.clone();
            state.preferred_root_mut().model.data.provider_health = re_snapshot.provider_health.clone();
            state.preferred_root_mut().model.data.loading = false;

            let mut proto_snapshot =
                snapshot_to_proto(state.identity().clone(), state.preferred_path(), state.seq() + 1, &re_snapshot, &self.host_name);
            proto_snapshot.provider_health = crate::convert::health_to_proto(&state.preferred_root().model.data.provider_health);
            proto_snapshot.issue_total = issue_total;
            proto_snapshot.issue_has_more = issue_has_more;
            proto_snapshot.issue_search_results = search_results;

            // Compute and log delta (also advances seq)
            let delta_entry = state.record_delta(
                &proto_snapshot.providers,
                &proto_snapshot.provider_health,
                &proto_snapshot.errors,
                proto_snapshot.work_items.clone(),
            );
            debug!(
                repo = %state.preferred_path().display(),
                prev_seq = delta_entry.prev_seq,
                seq = delta_entry.seq,
                change_count = delta_entry.changes.len(),
                "recorded repo delta"
            );

            state.mark_local_change();
            state.last_local_providers = last_local_providers;
            // Store a local-only snapshot (errors + health from the refresh,
            // providers from last_local_providers). Callers that need peer data
            // merge it on-demand via peer_providers; storing merged data here
            // would cause double-merge bugs in normalize_local_provider_hosts.
            state.last_snapshot = Arc::new(RefreshSnapshot {
                providers: Arc::new(state.last_local_providers.clone()),
                errors: re_snapshot.errors.clone(),
                provider_health: re_snapshot.provider_health.clone(),
                ..Default::default()
            });
            state.set_cached_snapshot(proto_snapshot.clone());

            let event = choose_event(proto_snapshot, delta_entry);
            let _ = self.event_tx.send(event);
        }

        // After broadcasting, check for linked issues that aren't cached yet
        // and fetch/pin them. This is a separate step so it doesn't block the
        // main snapshot broadcast path.
        drop(repos);
        self.fetch_missing_linked_issues().await;
        self.refresh_issues_incremental().await;
    }

    /// Fetch issue pages until the cache has at least `desired_count` entries
    /// (or no more pages are available).
    async fn ensure_issues_cached(&self, repo: &Path, desired_count: usize) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo).await else {
            return;
        };
        // Serialize fetches per-repo to prevent concurrent calls from reading the same
        // next_page and skipping pages.
        let mutex = {
            let repos = self.repos.read().await;
            match repos.get(&identity) {
                Some(state) => state.issue_fetch_mutex(),
                None => return,
            }
        };
        let _guard = mutex.lock().await;
        loop {
            // Check cache state and grab registry Arc (single read lock)
            let (page_num, registry) = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(&identity) else {
                    return;
                };
                let need = state.issue_cache.len() < desired_count && state.issue_cache.has_more;
                if !need {
                    break;
                }
                if state.registry().issue_trackers.is_empty() {
                    // No tracker — stop claiming more pages are available
                    drop(repos);
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(&identity) {
                        state.issue_cache.has_more = false;
                    }
                    break;
                }
                (state.issue_cache.next_page, state.registry())
            };

            // Fetch the next page outside any lock
            let page_result = {
                let tracker = registry.issue_trackers.preferred().unwrap();
                tracker.list_issues_page(repo, page_num, 50).await
            };

            match page_result {
                Ok(page) => {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(&identity) {
                        state.issue_cache.merge_page(page);
                        if state.issue_cache.last_refreshed_at.is_none() {
                            state.issue_cache.mark_refreshed(now_iso8601());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(%page_num, err = %e, "failed to fetch issue page");
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(&identity) {
                        state.issue_cache.has_more = false;
                    }
                    break;
                }
            }
        }
    }

    /// Run a search query against the issue tracker and store the results.
    async fn search_issues(&self, repo: &Path, query: &str) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo).await else {
            return;
        };
        let registry = {
            let repos = self.repos.read().await;
            let Some(state) = repos.get(&identity) else {
                return;
            };
            state.registry()
        };

        let result = {
            let Some(tracker) = registry.issue_trackers.preferred() else {
                return;
            };
            tracker.search_issues(repo, query, 50).await
        };

        match result {
            Ok(issues) => {
                info!(count = issues.len(), "search returned issues for query");
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(&identity) {
                    state.search_results = Some(issues);
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "issue search failed");
            }
        }
    }

    /// Check all repos for linked issue IDs not yet in the cache, fetch and pin them.
    async fn fetch_missing_linked_issues(&self) {
        // Phase 1: read lock — find repos with missing linked issues
        let fetch_tasks: Vec<_> = {
            let repos = self.repos.read().await;
            repos
                .iter()
                .filter_map(|(identity, state)| {
                    let linked_ids = collect_linked_issue_ids(&state.providers());
                    let missing = state.issue_cache.missing_ids(&linked_ids);
                    if missing.is_empty() {
                        return None;
                    }
                    Some((identity.clone(), missing, state.registry(), state.issue_fetch_mutex()))
                })
                .collect()
        };

        if fetch_tasks.is_empty() {
            return;
        }

        // Phase 2: fetch outside locks, then update cache and re-broadcast.
        // Acquire the per-repo issue_fetch_mutex to avoid redundant API calls
        // if ensure_issues_cached is running concurrently.
        for (identity, missing, registry, fetch_mutex) in fetch_tasks {
            let _guard = fetch_mutex.lock().await;

            // Re-check missing after acquiring mutex — ensure_issues_cached may
            // have already fetched some of these while we waited.
            let (missing, path) = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(&identity) else {
                    continue;
                };
                (state.issue_cache.missing_ids(&missing), state.preferred_path().to_path_buf())
            };
            if missing.is_empty() {
                continue;
            }

            let Some(tracker) = registry.issue_trackers.preferred() else {
                continue;
            };
            match tracker.fetch_issues_by_id(&path, &missing).await {
                Ok(fetched) if !fetched.is_empty() => {
                    {
                        let mut repos = self.repos.write().await;
                        if let Some(state) = repos.get_mut(&identity) {
                            state.issue_cache.add_pinned(fetched);
                        }
                    }
                    self.broadcast_snapshot(&path).await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("failed to fetch linked issues for {}: {}", path.display(), e);
                }
            }
        }
    }

    /// Incremental issue refresh: fetch issues changed since last refresh,
    /// apply changeset to cache, and broadcast if anything changed.
    async fn refresh_issues_incremental(&self) {
        // Minimum interval between incremental refreshes (seconds).
        const MIN_INTERVAL_SECS: i64 = 30;

        let tasks: Vec<_> = {
            let repos = self.repos.read().await;
            repos
                .iter()
                .filter_map(|(identity, state)| {
                    let since = state.issue_cache.last_refreshed_at.as_ref()?;
                    if state.registry().issue_trackers.is_empty() {
                        return None;
                    }
                    // Skip if refreshed too recently
                    if let Ok(last) = chrono::DateTime::parse_from_rfc3339(since) {
                        let elapsed = chrono::Utc::now().signed_duration_since(last).num_seconds();
                        if elapsed < MIN_INTERVAL_SECS {
                            return None;
                        }
                    }
                    Some((
                        identity.clone(),
                        state.preferred_path().to_path_buf(),
                        since.clone(),
                        state.registry(),
                        state.issue_fetch_mutex(),
                        state.issue_cache.len(),
                    ))
                })
                .collect()
        };

        for (identity, path, since, registry, fetch_mutex, prev_count) in tasks {
            let _guard = fetch_mutex.lock().await;
            let Some(tracker) = registry.issue_trackers.preferred() else {
                continue;
            };

            // Record timestamp *before* the API call so the next `since`
            // window overlaps rather than gaps — avoids missing updates
            // that land on GitHub during the request.
            let refresh_ts = now_iso8601();

            debug!("issue incremental: repo={} since={} refresh_ts={} cache_len={}", path.display(), since, refresh_ts, prev_count,);

            match tracker.list_issues_changed_since(&path, &since, 50).await {
                Ok(changeset) => {
                    let n_updated = changeset.updated.len();
                    let n_closed = changeset.closed_ids.len();
                    let has_more = changeset.has_more;

                    if n_updated > 0 || n_closed > 0 || has_more {
                        let updated_ids: Vec<&str> = changeset.updated.iter().map(|(id, _)| id.as_str()).collect();
                        info!(
                            "issue incremental: repo={} updated={:?} closed={:?} has_more={}",
                            path.display(),
                            updated_ids,
                            changeset.closed_ids,
                            has_more,
                        );
                    }

                    if has_more {
                        // Too many changes — skip incremental, do a full re-fetch.
                        // Don't reset until we have data to replace it with,
                        // so transient API failures don't wipe the UI.
                        info!("issue incremental: escalating to full re-fetch for {}", path.display(),);
                        drop(_guard);
                        let first_page = {
                            let reg = {
                                let repos = self.repos.read().await;
                                repos.get(&identity).map(RepoState::registry)
                            };
                            if let Some(reg) = reg {
                                if let Some(t) = reg.issue_trackers.preferred() {
                                    t.list_issues_page(&path, 1, 50).await.ok()
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        };
                        if first_page.is_some() {
                            // First page succeeded — safe to reset and refill
                            {
                                let mut repos = self.repos.write().await;
                                if let Some(state) = repos.get_mut(&identity) {
                                    state.issue_cache.reset();
                                    if let Some(page) = first_page {
                                        state.issue_cache.merge_page(page);
                                    }
                                }
                            }
                            // Continue fetching remaining pages
                            self.ensure_issues_cached(&path, prev_count).await;
                            {
                                let mut repos = self.repos.write().await;
                                if let Some(state) = repos.get_mut(&identity) {
                                    state.issue_cache.mark_refreshed(refresh_ts.clone());
                                }
                            }
                            self.broadcast_snapshot(&path).await;
                        } else {
                            // Fetch failed — keep existing cache and do NOT advance
                            // the timestamp, so the next incremental call retries
                            // from the same `since` window.
                            warn!("issue incremental: escalation fetch failed for {}, keeping cache", path.display(),);
                        }
                    } else {
                        let has_changes = n_updated > 0 || n_closed > 0;
                        {
                            let mut repos = self.repos.write().await;
                            if let Some(state) = repos.get_mut(&identity) {
                                state.issue_cache.apply_changeset(changeset);
                                state.issue_cache.mark_refreshed(refresh_ts);
                            }
                        }
                        if has_changes {
                            self.broadcast_snapshot(&path).await;
                        }
                    }
                }
                Err(e) => {
                    warn!("incremental issue refresh failed for {}: {}", path.display(), e);
                }
            }
        }
    }

    /// Add a virtual repo (no local filesystem path) for a remote-only repo.
    ///
    /// Unlike `add_repo`, this skips provider discovery entirely — there is
    /// no local path to scan. Instead it creates a dormant `RepoState` with
    /// an empty provider registry and an idle refresh handle.
    ///
    /// The `synthetic_path` serves as a stable key for tab identity (e.g.
    /// `<remote>/desktop/home/dev/repo`).
    ///
    /// `peers` and `overlay_version` seed the peer overlay so the repo
    /// is immediately queryable — there is no window where the repo is
    /// visible but has empty data.
    ///
    /// Emits `DaemonEvent::RepoTracked` followed by a snapshot broadcast.
    pub async fn add_virtual_repo(
        &self,
        identity: flotilla_protocol::RepoIdentity,
        synthetic_path: PathBuf,
        peers: Vec<(HostName, ProviderData)>,
        overlay_version: u64,
    ) -> Result<(), String> {
        // Check if already tracked
        {
            let repos = self.repos.read().await;
            if repos.contains_key(&identity) {
                return Ok(());
            }
        }

        let mut model = RepoModel::new_virtual();
        model.data.loading = false;

        let repo_info = RepoInfo {
            identity: identity.clone(),
            path: synthetic_path.clone(),
            name: repo_name(&synthetic_path),
            labels: model.labels.clone(),
            provider_names: provider_names_from_registry(&model.registry),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.contains_key(&identity) {
                return Ok(());
            }
            repos.insert(
                identity.clone(),
                RepoState::new(identity.clone(), RepoRootState {
                    path: synthetic_path.clone(),
                    model,
                    slug: None,
                    repo_bag: EnvironmentBag::new(),
                    unmet: Vec::new(),
                    is_local: false,
                }),
            );
            order.push(identity.clone());
        }

        self.path_identities.write().await.insert(synthetic_path.clone(), identity);

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));

        // Set up the peer overlay and broadcast atomically — no window
        // where the repo is visible but has empty data.
        self.set_peer_providers(&synthetic_path, peers, overlay_version).await;

        Ok(())
    }

    /// Re-build and broadcast a snapshot for the given repo using current cache state.
    ///
    /// If peer provider data has been set for this repo via [`set_peer_providers`],
    /// it is merged into the snapshot before correlation and broadcasting.
    async fn broadcast_snapshot(&self, repo: &Path) {
        self.broadcast_snapshot_inner(repo, true).await;
    }

    async fn broadcast_snapshot_inner(&self, repo: &Path, is_local_change: bool) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo).await else {
            return;
        };
        // Read peer overlay (brief read lock)
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(&identity).cloned()
        };

        let mut repos = self.repos.write().await;
        let Some(state) = repos.get_mut(&identity) else {
            return;
        };

        let proto_snapshot =
            build_repo_snapshot_with_peers(state.snapshot_context(&self.host_name), state.seq() + 1, peer_overlay.as_deref());

        // Compute and log delta (also advances seq)
        let delta_entry = state.record_delta(
            &proto_snapshot.providers,
            &proto_snapshot.provider_health,
            &proto_snapshot.errors,
            proto_snapshot.work_items.clone(),
        );
        if is_local_change {
            state.mark_local_change();
        }
        state.set_cached_snapshot(proto_snapshot.clone());

        let event = choose_event(proto_snapshot, delta_entry);
        let _ = self.event_tx.send(event);
    }

    /// Send an arbitrary event to all subscribers.
    ///
    /// Mirrors host events into daemon-owned host state so replay/query paths
    /// can use a single authoritative source of truth.
    ///
    /// For peer status changes, prefer [`publish_peer_connection_status`](Self::publish_peer_connection_status)
    /// which emits both a `PeerStatusChanged` and a `HostSnapshot` for live subscribers.
    /// Calling `send_event(PeerStatusChanged)` directly only updates replay state.
    pub fn send_event(&self, event: DaemonEvent) {
        self.host_registry.apply_event(&event);
        let _ = self.event_tx.send(event);
    }
}

/// Non-trait methods that are called directly on the concrete `InProcessDaemon`
/// type by the daemon server peer-overlay code and by the `execute()` implementation.
impl InProcessDaemon {
    pub async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<(), String> {
        let repo = self.resolve_repo_selector(repo).await?;
        let (prev_count, registry, identity) = {
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let repos = self.repos.read().await;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            for root in &state.roots {
                if root.is_local {
                    root.model.refresh_handle.trigger_refresh();
                }
            }
            (state.issue_cache.len(), state.registry(), identity)
        };

        if prev_count > 0 {
            // Fetch page 1 before resetting, so failures don't wipe the UI.
            let first_page =
                if let Some(t) = registry.issue_trackers.preferred() { t.list_issues_page(&repo, 1, 50).await.ok() } else { None };

            if first_page.is_some() {
                {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(&identity) {
                        state.issue_cache.reset();
                        if let Some(page) = first_page {
                            state.issue_cache.merge_page(page);
                        }
                    }
                }
                self.ensure_issues_cached(&repo, prev_count).await;
                {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(&identity) {
                        state.issue_cache.mark_refreshed(now_iso8601());
                    }
                }
                self.broadcast_snapshot(&repo).await;
            }
        }

        Ok(())
    }

    /// Resolve a path that might be a git worktree to the main repo root.
    ///
    /// Returns `(resolved_path, Some(original_path))` if normalization changed
    /// the path, or `(original_path, None)` if no change was needed.
    async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
        use crate::providers::vcs::{git::GitVcs, Vcs};

        let vcs = GitVcs::new(self.discovery.runner.clone());
        match vcs.resolve_repo_root(path).await {
            Some(repo_root) => {
                // Canonicalize to handle symlinks (e.g. /var -> /private/var on macOS).
                let canonical_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
                let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if canonical_root != canonical_path {
                    debug!(
                        worktree = %path.display(),
                        repo_root = %canonical_root.display(),
                        "normalized worktree path to main repo root"
                    );
                    (canonical_root, Some(path.to_path_buf()))
                } else {
                    (canonical_root, None)
                }
            }
            None => (path.to_path_buf(), None),
        }
    }

    /// Add a repo to tracking, returning `(tracked_path, resolved_from)`.
    ///
    /// If `path` is a git worktree, the main repo root is resolved via
    /// `git rev-parse --path-format=absolute --git-common-dir` and tracked
    /// instead. `resolved_from` is `Some(original_path)` in that case.
    pub async fn add_repo(&self, path: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
        let (path, resolved_from) = self.normalize_repo_path(path).await;

        // Check if already tracked (under read lock for fast path)
        {
            let identities = self.path_identities.read().await;
            if identities.contains_key(&path) {
                return Ok((path, resolved_from));
            }
        }

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_providers(
            &self.host_bag,
            &path,
            &self.discovery.repo_detectors,
            &self.discovery.factories,
            &self.config,
            Arc::clone(&self.discovery.runner),
            self.discovery.shared_attachable_store(&self.config),
            &*self.discovery.env,
        )
        .await;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let identity = repo_identity_from_bag_or_path(&path, &host_repo_bag);
        let slug = repo_slug.clone();
        let mut model = RepoModel::new(
            path.clone(),
            registry,
            repo_slug,
            self.discovery.shared_attachable_store(&self.config),
            Arc::clone(&self.agent_state_store),
        );
        model.data.loading = true;
        let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

        let repo_info = RepoInfo {
            identity: identity.clone(),
            path: path.clone(),
            name: repo_name(&path),
            labels: root.model.labels.clone(),
            provider_names: provider_names_from_registry(&root.model.registry),
            provider_health: crate::convert::health_to_proto(&root.model.data.provider_health),
            loading: true,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        let mut added_new_identity = false;
        let mut preferred_changed = false;
        let already_tracked = self.path_identities.read().await.contains_key(&path);
        if already_tracked {
            return Ok((path, resolved_from));
        }
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if let Some(state) = repos.get_mut(&identity) {
                preferred_changed = state.add_root(root);
            } else {
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
                order.push(identity.clone());
                added_new_identity = true;
            }
            self.path_identities.write().await.insert(path.clone(), identity.clone());
        }

        // Persist to config
        self.config.save_repo(&path);
        let tab_order = {
            let repos = self.repos.read().await;
            let order = self.repo_order.read().await;
            order.iter().filter_map(|id| repos.get(id).map(|state| state.preferred_path().to_path_buf())).collect::<Vec<_>>()
        };
        self.config.save_tab_order(&tab_order);

        info!(repo = %path.display(), "added repo");
        if added_new_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));
        } else if preferred_changed {
            self.broadcast_snapshot_inner(&path, false).await;
        }

        Ok((path, resolved_from))
    }

    pub async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        let repo_identity = self.tracked_repo_identity_for_path(&path).await.unwrap_or_else(|| fallback_repo_identity(&path));

        let mut removed_identity = false;
        let mut new_preferred_path = None;
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            let Some(state) = repos.get_mut(&repo_identity) else {
                return Err(format!("repo not tracked: {}", path.display()));
            };
            let previous_preferred = state.preferred_path().to_path_buf();
            if !state.remove_root(&path) {
                return Err(format!("repo not tracked: {}", path.display()));
            }
            if state.roots.is_empty() {
                repos.remove(&repo_identity);
                order.retain(|repo| repo != &repo_identity);
                removed_identity = true;
            } else if previous_preferred == path {
                new_preferred_path = Some(state.preferred_path().to_path_buf());
            }
        }

        // Remove from identity map and peer overlay
        self.path_identities.write().await.remove(&path);
        if removed_identity {
            let mut pp = self.peer_providers.write().await;
            pp.remove(&repo_identity);
            drop(pp);
            self.peer_overlay_versions.write().await.remove(&repo_identity);
        }

        // Persist to config
        self.config.remove_repo(&path);
        let tab_order = {
            let repos = self.repos.read().await;
            let order = self.repo_order.read().await;
            order.iter().filter_map(|id| repos.get(id).map(|state| state.preferred_path().to_path_buf())).collect::<Vec<_>>()
        };
        self.config.save_tab_order(&tab_order);

        info!(repo = %path.display(), "removed repo");
        if removed_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoUntracked { repo_identity, path });
        } else if let Some(preferred_path) = new_preferred_path {
            self.broadcast_snapshot_inner(&preferred_path, false).await;
        }

        Ok(())
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoSnapshot, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        Ok(match state.cached_snapshot() {
            Some(s) => (**s).clone(),
            None => build_repo_snapshot_with_peers(state.snapshot_context(&self.host_name), state.seq(), peer_overlay.as_deref()),
        })
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut result = Vec::new();
        for identity in order.iter() {
            if let Some(state) = repos.get(identity) {
                result.push(RepoInfo {
                    identity: state.identity().clone(),
                    path: state.preferred_path().to_path_buf(),
                    name: repo_name(state.preferred_path()),
                    labels: state.labels().clone(),
                    provider_names: state.provider_names(),
                    provider_health: crate::convert::health_to_proto(state.provider_health()),
                    loading: state.loading(),
                });
            }
        }
        Ok(result)
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        let command_host = command.host.clone().unwrap_or_else(|| self.host_name.clone());
        if command_host != self.host_name {
            return Err(format!("remote command routing not implemented yet for host {command_host}"));
        }

        // Issue commands: execute inline, no lifecycle events.
        // These are synchronous cache operations that return immediately.
        match &command.action {
            flotilla_protocol::CommandAction::SetIssueViewport { repo, visible_count } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                self.ensure_issues_cached(&repo_path, *visible_count * 2).await;
                self.broadcast_snapshot(&repo_path).await;
                return Ok(INLINE_COMMAND_ID);
            }
            flotilla_protocol::CommandAction::FetchMoreIssues { repo, desired_count } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                self.ensure_issues_cached(&repo_path, *desired_count).await;
                self.broadcast_snapshot(&repo_path).await;
                return Ok(INLINE_COMMAND_ID);
            }
            flotilla_protocol::CommandAction::SearchIssues { repo, query } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                self.search_issues(&repo_path, query).await;
                self.broadcast_snapshot(&repo_path).await;
                return Ok(INLINE_COMMAND_ID);
            }
            flotilla_protocol::CommandAction::ClearIssueSearch { repo } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let identity = self.tracked_repo_identity_for_path(&repo_path).await;
                let mut repos = self.repos.write().await;
                if let Some(identity) = identity.as_ref() {
                    if let Some(state) = repos.get_mut(identity) {
                        state.search_results = None;
                    }
                }
                drop(repos);
                self.broadcast_snapshot(&repo_path).await;
                return Ok(INLINE_COMMAND_ID);
            }
            _ => {}
        }

        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        match &command.action {
            flotilla_protocol::CommandAction::TrackRepoPath { path } => {
                let repo_identity = self.detect_repo_identity(path).await;
                let description = command.description().to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: path.clone(),
                    description,
                });
                let result = match self.add_repo(path).await {
                    Ok((tracked_path, resolved_from)) => flotilla_protocol::CommandValue::RepoTracked { path: tracked_path, resolved_from },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: self.tracked_repo_identity_for_path(path).await.unwrap_or(repo_identity),
                    repo: path.clone(),
                    result,
                });
                return Ok(id);
            }
            flotilla_protocol::CommandAction::UntrackRepo { repo } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let repo_identity =
                    self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
                let description = command.description().to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: repo_path.clone(),
                    description,
                });
                let result = match self.remove_repo(&repo_path).await {
                    Ok(()) => flotilla_protocol::CommandValue::RepoUntracked { path: repo_path.clone() },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity,
                    repo: repo_path,
                    result,
                });
                return Ok(id);
            }
            flotilla_protocol::CommandAction::Refresh { repo: None } => {
                let repo_paths = {
                    let repos = self.repos.read().await;
                    let order = self.repo_order.read().await;
                    order
                        .iter()
                        .filter_map(|identity| repos.get(identity).map(|state| state.preferred_path().to_path_buf()))
                        .collect::<Vec<_>>()
                };
                let display_repo = repo_paths.first().cloned().unwrap_or_default();
                let display_repo_identity =
                    self.tracked_repo_identity_for_path(&display_repo).await.unwrap_or_else(|| fallback_repo_identity(&display_repo));
                let description = command.description().to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: display_repo_identity.clone(),
                    repo: display_repo.clone(),
                    description,
                });
                let mut refreshed = Vec::new();
                for repo in &repo_paths {
                    self.refresh(&flotilla_protocol::RepoSelector::Path(repo.clone())).await?;
                    refreshed.push(repo.clone());
                }
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: display_repo_identity,
                    repo: display_repo,
                    result: flotilla_protocol::CommandValue::Refreshed { repos: refreshed },
                });
                return Ok(id);
            }
            flotilla_protocol::CommandAction::Refresh { repo: Some(selector) } => {
                let repo_path = self.resolve_repo_selector(selector).await?;
                let repo_identity =
                    self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
                let description = command.description().to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity: repo_identity.clone(),
                    repo: repo_path.clone(),
                    description,
                });
                let result = match self.refresh(&flotilla_protocol::RepoSelector::Path(repo_path.clone())).await {
                    Ok(()) => flotilla_protocol::CommandValue::Refreshed { repos: vec![repo_path.clone()] },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    host: self.host_name.clone(),
                    repo_identity,
                    repo: repo_path,
                    result,
                });
                return Ok(id);
            }
            _ => {}
        }

        // Gather what the spawned task needs — validate repo before broadcasting
        let repo = self.resolve_repo_for_command(&command).await?;
        let runner = Arc::clone(&self.discovery.runner);
        let event_tx = self.event_tx.clone();
        let (repo_identity, registry, providers_data, refresh_trigger) = {
            let repos = self.repos.read().await;
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            (state.identity().clone(), state.registry(), state.providers(), state.refresh_trigger())
        };

        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let config_base = self.config.base_path().to_path_buf();

        // Register the cancellation token before broadcasting CommandStarted
        // so cancel(id) can find it the instant the TUI sees the event.
        let active_ref = Arc::clone(&self.active_commands);
        let token = CancellationToken::new();
        {
            let mut guard = active_ref.lock().await;
            guard.insert(id, token.clone());
        }

        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id: id,
            host: self.host_name.clone(),
            repo_identity: repo_identity.clone(),
            repo: repo_path.clone(),
            description,
        });

        // Spawn the entire build_plan + execution so execute() returns the
        // command_id immediately. This keeps the TUI event loop responsive.
        let local_host = self.host_name.clone();
        let attachable_store = self.discovery.shared_attachable_store(&self.config);
        let daemon_socket_path = self.daemon_socket_path.read().await.clone();
        tokio::spawn(async move {
            // Clone values the resolver needs before build_plan consumes them.
            let resolver_registry = Arc::clone(&registry);
            let resolver_providers_data = Arc::clone(&providers_data);
            let resolver_runner = Arc::clone(&runner);
            let resolver_config_base = config_base.clone();
            let resolver_attachable_store = attachable_store.clone();
            let resolver_local_host = local_host.clone();
            let resolver_repo = executor::RepoExecutionContext { identity: repo_identity.clone(), root: repo_path.clone() };

            let plan = executor::build_plan(
                command,
                executor::RepoExecutionContext { identity: repo_identity.clone(), root: repo_path.clone() },
                registry,
                providers_data,
                config_base,
                attachable_store,
                daemon_socket_path.clone(),
                local_host,
                None,
            )
            .await;

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
                    refresh_trigger.notify_one();
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        host: command_host.clone(),
                        repo_identity: repo_identity.clone(),
                        repo: repo_path,
                        result,
                    });
                }
                Ok(step_plan) => {
                    let resolver = executor::ExecutorStepResolver {
                        repo: resolver_repo,
                        registry: resolver_registry,
                        providers_data: resolver_providers_data,
                        runner: resolver_runner,
                        config_base: resolver_config_base,
                        attachable_store: resolver_attachable_store,
                        daemon_socket_path: daemon_socket_path.clone(),
                        local_host: resolver_local_host,
                    };
                    let result = run_step_plan(
                        step_plan,
                        id,
                        command_host.clone(),
                        repo_identity.clone(),
                        repo_path.clone(),
                        token,
                        event_tx.clone(),
                        &resolver,
                    )
                    .await;
                    refresh_trigger.notify_one();
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        host: command_host,
                        repo_identity,
                        repo: repo_path,
                        result,
                    });
                }
            }
        });

        Ok(id)
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_commands.lock().await;
        match guard.get(&command_id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err("no matching active command".into()),
        }
    }

    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut events = self.host_registry.replay_host_events(last_seen).await;

        // Emit repo events
        for identity in order.iter() {
            let Some(state) = repos.get(identity) else {
                continue;
            };
            let Some(snapshot) = state.cached_snapshot() else {
                continue;
            };

            let repo_stream_key = StreamKey::Repo { identity: state.identity().clone() };
            match last_seen.get(&repo_stream_key) {
                Some(&client_seq) => match state.deltas_since(client_seq) {
                    Some(deltas) => {
                        for entry in deltas {
                            events.push(DaemonEvent::RepoDelta(Box::new(RepoDelta {
                                seq: entry.seq,
                                prev_seq: entry.prev_seq,
                                repo_identity: state.identity().clone(),
                                repo: state.preferred_path().to_path_buf(),
                                changes: entry.changes.clone(),
                                work_items: entry.work_items.clone(),
                                issue_total: snapshot.issue_total,
                                issue_has_more: snapshot.issue_has_more,
                                issue_search_results: snapshot.issue_search_results.clone(),
                            })));
                        }
                    }
                    None => {
                        // Seq not in delta log — send full snapshot
                        events.push(DaemonEvent::RepoSnapshot(Box::new((**snapshot).clone())));
                    }
                },
                None => {
                    // Client has never seen this repo — send full snapshot
                    events.push(DaemonEvent::RepoSnapshot(Box::new((**snapshot).clone())));
                }
            }
        }

        Ok(events)
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        let peer_providers = self.peer_providers.read().await;
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut summaries = Vec::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
                Some(s) => std::borrow::Cow::Borrowed(s),
                None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                    state.snapshot_context(&self.host_name),
                    state.seq(),
                    peer_providers.get(identity).map(|v| v.as_slice()),
                )),
            };
            summaries.push(RepoSummary {
                path: state.preferred_path().to_path_buf(),
                slug: state.slug().map(str::to_string),
                provider_health: snapshot.provider_health.clone(),
                work_item_count: snapshot.work_items.len(),
                error_count: snapshot.errors.len(),
            });
        }
        Ok(StatusResponse { repos: summaries })
    }

    async fn get_repo_detail(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoDetailResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.host_name),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };
        Ok(RepoDetailResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            provider_health: snapshot.provider_health.clone(),
            work_items: snapshot.work_items.clone(),
            errors: snapshot.errors.clone(),
        })
    }

    async fn get_repo_providers(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.host_name),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };

        let host_discovery = self.host_bag.assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();
        let repo_discovery = state.repo_bag().assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();

        let provider_infos = state
            .preferred_root()
            .model
            .registry
            .provider_infos()
            .into_iter()
            .map(|(category, name)| {
                let healthy = snapshot.provider_health.get(&category).and_then(|providers| providers.get(&name)).copied().unwrap_or(true);
                ProviderInfo { category, name, healthy }
            })
            .collect();

        let unmet_requirements =
            state.unmet().iter().map(|(factory, req)| crate::convert::unmet_requirement_to_proto(factory, req)).collect();

        Ok(RepoProvidersResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            host_discovery,
            repo_discovery,
            providers: provider_infos,
            unmet_requirements,
        })
    }

    async fn get_repo_work(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoWorkResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.host_name),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };
        Ok(RepoWorkResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            work_items: snapshot.work_items.clone(),
        })
    }

    async fn list_hosts(&self) -> Result<HostListResponse, String> {
        let local_counts = self.local_host_counts().await;
        let remote_counts = self.remote_host_counts().await;
        Ok(self.host_registry.list_hosts(local_counts, &remote_counts).await)
    }

    async fn get_host_status(&self, host: &str) -> Result<HostStatusResponse, String> {
        let local_counts = self.local_host_counts().await;
        let remote_counts = self.remote_host_counts().await;
        self.host_registry.get_host_status(host, local_counts, &remote_counts).await
    }

    async fn get_host_providers(&self, host: &str) -> Result<HostProvidersResponse, String> {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry.get_host_providers(host, &remote_counts).await
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Ok(self.host_registry.get_topology().await)
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout};

    use super::*;

    fn checkout_with_issue(issue_id: &str) -> Checkout {
        Checkout {
            branch: "main".into(),
            is_main: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![AssociationKey::IssueRef("gh".into(), issue_id.into())],
        }
    }

    fn cr_with_issue(issue_id: &str) -> ChangeRequest {
        ChangeRequest {
            title: "Fix bug".into(),
            branch: "feature/fix".into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![AssociationKey::IssueRef("gh".into(), issue_id.into())],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    #[test]
    fn collect_linked_issue_ids_deduplicates_across_sources() {
        let mut providers = ProviderData::default();
        providers.checkouts.insert(
            flotilla_protocol::HostPath::new(flotilla_protocol::HostName::new("test-host"), PathBuf::from("/tmp/repo")),
            checkout_with_issue("123"),
        );
        providers.change_requests.insert("1".into(), cr_with_issue("123"));
        providers.change_requests.insert("2".into(), cr_with_issue("456"));

        let mut ids = collect_linked_issue_ids(&providers);
        ids.sort();
        assert_eq!(ids, vec!["123".to_string(), "456".to_string()]);
    }

    #[test]
    fn inject_issues_prefers_search_results_then_cache_then_empty() {
        let base = ProviderData::default();

        let mut cache = IssueCache::new();
        cache.add_pinned(vec![("1".into(), Issue {
            title: "cached".into(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        })]);

        let search_results = Some(vec![("2".into(), Issue {
            title: "search".into(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        })]);

        let from_search = inject_issues(&base, &cache, &search_results);
        assert_eq!(from_search.issues.len(), 1);
        assert!(from_search.issues.contains_key("2"));

        let from_cache = inject_issues(&base, &cache, &None);
        assert!(from_cache.issues.contains_key("1"));

        let empty_cache = IssueCache::new();
        let empty = inject_issues(&base, &empty_cache, &None);
        assert!(empty.issues.is_empty());
    }

    #[test]
    fn choose_event_uses_delta_for_non_initial_changes() {
        let repo = PathBuf::from("/tmp/repo");
        let snapshot = RepoSnapshot {
            seq: 2,
            repo_identity: fallback_repo_identity(&repo),
            repo: repo.clone(),
            host_name: HostName::local(),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };

        let initial = DeltaEntry { seq: 1, prev_seq: 0, changes: vec![], work_items: vec![] };
        assert!(matches!(choose_event(snapshot.clone(), initial), DaemonEvent::RepoSnapshot(_)));

        let non_empty = DeltaEntry {
            seq: 2,
            prev_seq: 1,
            changes: vec![flotilla_protocol::Change::Branch { key: "feature/x".into(), op: flotilla_protocol::EntryOp::Removed }],
            work_items: vec![],
        };
        assert!(matches!(choose_event(snapshot, non_empty), DaemonEvent::RepoDelta(_)));
    }

    #[test]
    fn choose_event_falls_back_to_full_when_delta_is_larger() {
        let snapshot = RepoSnapshot {
            seq: 3,
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            repo: PathBuf::from("/tmp/repo"),
            host_name: HostName::local(),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };

        let delta = DeltaEntry {
            seq: 3,
            prev_seq: 2,
            changes: vec![flotilla_protocol::Change::Branch { key: "feature/".repeat(128), op: flotilla_protocol::EntryOp::Removed }],
            work_items: vec![],
        };

        assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
    }

    #[test]
    fn build_repo_snapshot_sets_issue_metadata() {
        let mut cache = IssueCache::new();
        cache.total_count = Some(5);
        cache.has_more = true;
        cache.add_pinned(vec![("9".into(), Issue {
            title: "cached issue".into(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        })]);

        let default_snap = RefreshSnapshot::default();
        let snap = build_repo_snapshot_with_peers(
            SnapshotBuildContext {
                repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
                path: Path::new("/tmp/repo"),
                local_providers: &default_snap.providers,
                errors: &default_snap.errors,
                provider_health: &default_snap.provider_health,
                cache: &cache,
                search_results: &None,
                host_name: &HostName::local(),
            },
            7,
            None,
        );
        assert_eq!(snap.seq, 7);
        assert_eq!(snap.issue_total, Some(5));
        assert!(snap.issue_has_more);
        assert!(snap.providers.issues.contains_key("9"));
    }

    // --- now_iso8601 ---

    #[test]
    fn now_iso8601_returns_parseable_timestamp() {
        let ts = now_iso8601();
        assert!(ts.ends_with('Z'), "should be UTC: {ts}");
        assert!(ts.len() >= 20, "should be a full ISO 8601 timestamp: {ts}");
        // Verify it parses as a valid RFC 3339 timestamp
        chrono::DateTime::parse_from_rfc3339(&ts).expect("should parse as RFC 3339");
    }

    // --- choose_event edge case: empty changes with prev_seq > 0 ---

    #[test]
    fn choose_event_sends_full_when_delta_has_empty_changes() {
        let snapshot = RepoSnapshot {
            seq: 2,
            repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
            repo: PathBuf::from("/tmp/repo"),
            host_name: HostName::local(),
            work_items: vec![],
            providers: ProviderData::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };

        // prev_seq > 0 but changes is empty — should still send full
        let delta = DeltaEntry { seq: 2, prev_seq: 1, changes: vec![], work_items: vec![] };
        assert!(matches!(choose_event(snapshot, delta), DaemonEvent::RepoSnapshot(_)));
    }

    // --- build_repo_snapshot_with_peers ---

    #[test]
    fn build_repo_snapshot_with_peers_merges_peer_data() {
        let cache = IssueCache::new();
        let host_a = HostName::new("host-a");
        let host_b = HostName::new("host-b");

        // Create peer provider data with a checkout owned by host_b
        let mut peer_data = ProviderData::default();
        peer_data.checkouts.insert(flotilla_protocol::HostPath::new(host_b.clone(), PathBuf::from("/remote/repo")), Checkout {
            branch: "remote-feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        });

        let peers = vec![(host_b, peer_data)];
        let default_snap = RefreshSnapshot::default();
        let snap = build_repo_snapshot_with_peers(
            SnapshotBuildContext {
                repo_identity: fallback_repo_identity(Path::new("/tmp/repo")),
                path: Path::new("/tmp/repo"),
                local_providers: &default_snap.providers,
                errors: &default_snap.errors,
                provider_health: &default_snap.provider_health,
                cache: &cache,
                search_results: &None,
                host_name: &host_a,
            },
            1,
            Some(&peers),
        );

        // The snapshot should contain the merged peer checkout
        assert!(!snap.providers.checkouts.is_empty(), "peer checkout should be merged");
        assert_eq!(snap.providers.checkouts.len(), 1);
    }

    /// Regression test: when `base` already contains merged peer data (as happens
    /// after poll_snapshots stores `re_snapshot` in `last_snapshot`), calling
    /// `build_repo_snapshot_with_peers` again must not re-attribute peer checkouts
    /// to the local host via `normalize_local_provider_hosts`.
    #[test]
    fn build_repo_snapshot_with_peers_does_not_duplicate_from_merged_base() {
        let cache = IssueCache::new();
        let local_host = HostName::new("feta");
        let peer_host = HostName::new("kiwi");

        // Simulate local checkout
        let mut local_providers = ProviderData::default();
        local_providers.checkouts.insert(flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/home/dev/repo")), Checkout {
            branch: "main".into(),
            is_main: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        });

        // Create peer data
        let mut peer_data = ProviderData::default();
        peer_data.checkouts.insert(flotilla_protocol::HostPath::new(peer_host.clone(), PathBuf::from("/srv/kiwi/repo")), Checkout {
            branch: "peer-feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        });
        let peers = vec![(peer_host.clone(), peer_data.clone())];
        let default_snap = RefreshSnapshot::default();

        // First call — simulates the initial build (local-only base).
        // This produces a merged result containing both local + peer checkouts.
        let first_snap = build_repo_snapshot_with_peers(
            SnapshotBuildContext {
                repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
                path: Path::new("/home/dev/repo"),
                local_providers: &local_providers,
                errors: &default_snap.errors,
                provider_health: &default_snap.provider_health,
                cache: &cache,
                search_results: &None,
                host_name: &local_host,
            },
            1,
            Some(&peers),
        );
        assert_eq!(first_snap.providers.checkouts.len(), 2, "first build should have local + peer checkout");

        // Simulate poll_snapshots storing the merged result as last_snapshot
        // while last_local_providers retains only local data.
        // The bug was: passing merged providers as the base to a second call
        // would re-stamp peer checkouts as local via normalize_local_provider_hosts.
        // With the fix, callers always pass local_providers, never merged data.

        // Second call — uses local-only providers (the fix), not merged data.
        let second_snap = build_repo_snapshot_with_peers(
            SnapshotBuildContext {
                repo_identity: fallback_repo_identity(Path::new("/home/dev/repo")),
                path: Path::new("/home/dev/repo"),
                local_providers: &local_providers,
                errors: &default_snap.errors,
                provider_health: &default_snap.provider_health,
                cache: &cache,
                search_results: &None,
                host_name: &local_host,
            },
            2,
            Some(&peers),
        );

        // The peer checkout must appear exactly once under kiwi
        let kiwi_count = second_snap.providers.checkouts.keys().filter(|hp| hp.host == peer_host).count();
        assert_eq!(kiwi_count, 1, "peer checkout should appear once under kiwi, got {kiwi_count}");

        // No ghost checkout — kiwi's path must not appear under the local host
        let ghost = flotilla_protocol::HostPath::new(local_host.clone(), PathBuf::from("/srv/kiwi/repo"));
        assert!(
            !second_snap.providers.checkouts.contains_key(&ghost),
            "peer checkout at /srv/kiwi/repo must not be re-stamped as local host checkout"
        );

        // Total checkout count should remain 2 (1 local + 1 peer)
        assert_eq!(
            second_snap.providers.checkouts.len(),
            2,
            "should have exactly 2 checkouts (1 local + 1 peer), got {}",
            second_snap.providers.checkouts.len()
        );
    }

    #[test]
    fn build_repo_snapshot_with_peers_preserves_remote_attachable_set_for_local_workspace_binding() {
        let cache = IssueCache::new();
        let local_host = HostName::new("kiwi");
        let remote_host = HostName::new("feta");
        let remote_checkout = HostPath::new(remote_host.clone(), PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"));
        let set_id = flotilla_protocol::AttachableSetId::new("set-remote");

        let mut local_providers = ProviderData::default();
        local_providers.workspaces.insert("workspace:9".into(), flotilla_protocol::Workspace {
            name: "attachable-correlation@feta".into(),
            directories: vec![PathBuf::from("/Users/robert/dev/flotilla")],
            correlation_keys: vec![],
            attachable_set_id: Some(set_id.clone()),
        });
        local_providers.attachable_sets.insert(set_id.clone(), flotilla_protocol::AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(remote_host.clone()),
            checkout: Some(remote_checkout.clone()),
            template_identity: None,
            members: vec![],
        });

        let mut peer_data = ProviderData::default();
        peer_data.checkouts.insert(remote_checkout.clone(), Checkout {
            branch: "attachable-correlation".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![
                CorrelationKey::Branch("attachable-correlation".into()),
                CorrelationKey::CheckoutPath(remote_checkout.clone()),
            ],
            association_keys: vec![],
        });

        let peers = vec![(remote_host.clone(), peer_data)];
        let default_snap = RefreshSnapshot::default();
        let snapshot = build_repo_snapshot_with_peers(
            SnapshotBuildContext {
                repo_identity: fallback_repo_identity(Path::new("/Users/robert/dev/flotilla")),
                path: Path::new("/Users/robert/dev/flotilla"),
                local_providers: &local_providers,
                errors: &default_snap.errors,
                provider_health: &default_snap.provider_health,
                cache: &cache,
                search_results: &None,
                host_name: &local_host,
            },
            1,
            Some(&peers),
        );

        let set = snapshot.providers.attachable_sets.get(&set_id).expect("attachable set should remain projected");
        assert_eq!(set.host_affinity.as_ref(), Some(&remote_host), "remote attachable set host affinity should stay on feta");
        assert_eq!(set.checkout.as_ref(), Some(&remote_checkout), "remote attachable set checkout should stay on feta");

        let set_item =
            snapshot.work_items.iter().find(|item| item.attachable_set_id.as_ref() == Some(&set_id)).expect("work item for attachable set");
        assert_eq!(set_item.host, remote_host, "correlated work item should be anchored to feta");
        assert_eq!(
            set_item.checkout.as_ref().map(|checkout| &checkout.key),
            Some(&remote_checkout),
            "correlated work item should point at the remote checkout"
        );
        assert_eq!(set_item.workspace_refs, vec!["workspace:9".to_string()]);

        let ghost_checkout = HostPath::new(local_host, PathBuf::from("/home/robert/dev/flotilla.terminal-stuff"));
        assert!(
            !snapshot.providers.checkouts.contains_key(&ghost_checkout),
            "remote checkout path must not be duplicated under the local host"
        );
    }
}
