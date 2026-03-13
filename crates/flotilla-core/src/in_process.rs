//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::{
    AssociationKey, Command, DaemonEvent, DeltaEntry, HostName, Issue, PeerConnectionState, ProviderData, ProviderError, ProviderInfo,
    RepoDetailResponse, RepoInfo, RepoProvidersResponse, RepoSummary, RepoWorkResponse, Snapshot, StatusResponse, UnmetRequirementInfo,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    config::ConfigStore,
    convert::snapshot_to_proto,
    daemon::DaemonHandle,
    delta,
    executor::{self, ExecutionPlan},
    issue_cache::IssueCache,
    model::{provider_names_from_registry, repo_name, RepoModel},
    providers::{
        discovery::{discover_providers, DiscoveryResult, EnvironmentBag, FactoryRegistry, ProcessEnvVars, RepoDetector, UnmetRequirement},
        CommandRunner,
    },
    refresh::RefreshSnapshot,
    step::run_step_plan,
};

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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

/// Build a proto Snapshot by injecting issues, re-correlating, and patching issue metadata.
fn build_repo_snapshot(
    path: &Path,
    seq: u64,
    base: &RefreshSnapshot,
    cache: &IssueCache,
    search_results: &Option<Vec<(String, Issue)>>,
    host_name: &HostName,
) -> Snapshot {
    build_repo_snapshot_with_peers(path, seq, base, cache, search_results, host_name, None)
}

/// Build a proto Snapshot, optionally merging peer provider data before correlation.
fn build_repo_snapshot_with_peers(
    path: &Path,
    seq: u64,
    base: &RefreshSnapshot,
    cache: &IssueCache,
    search_results: &Option<Vec<(String, Issue)>>,
    host_name: &HostName,
    peer_overlay: Option<&[(HostName, ProviderData)]>,
) -> Snapshot {
    let local_providers = inject_issues(&base.providers, cache, search_results);

    // Merge peer provider data if any
    let providers = if let Some(peers) = peer_overlay {
        let peer_refs: Vec<(HostName, &ProviderData)> = peers.iter().map(|(h, d)| (h.clone(), d)).collect();
        Arc::new(crate::merge::merge_provider_data(&local_providers, host_name, &peer_refs))
    } else {
        Arc::new(local_providers)
    };

    let (work_items, correlation_groups) = crate::data::correlate(&providers);
    let re_snapshot = RefreshSnapshot {
        providers,
        work_items,
        correlation_groups,
        errors: base.errors.clone(),
        provider_health: base.provider_health.clone(),
    };
    let mut snapshot = snapshot_to_proto(path, seq, &re_snapshot, host_name);
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
fn choose_event(snapshot: Snapshot, delta: DeltaEntry) -> DaemonEvent {
    // First broadcast or empty delta → always send full
    if delta.prev_seq == 0 || delta.changes.is_empty() {
        return DaemonEvent::SnapshotFull(Box::new(snapshot));
    }

    let snapshot_delta = flotilla_protocol::SnapshotDelta {
        seq: delta.seq,
        prev_seq: delta.prev_seq,
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
            DaemonEvent::SnapshotDelta(Box::new(snapshot_delta))
        }
        _ => {
            debug!("sending full snapshot (delta not smaller)");
            DaemonEvent::SnapshotFull(Box::new(snapshot))
        }
    }
}

/// Maximum number of delta entries retained per repo.
const DELTA_LOG_CAPACITY: usize = 16;

struct RepoState {
    model: RepoModel,
    slug: Option<String>,
    repo_bag: EnvironmentBag,
    unmet: Vec<(String, UnmetRequirement)>,
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
    issue_cache: IssueCache,
    search_results: Option<Vec<(String, Issue)>>,
    /// Serializes issue fetch operations for this repo to prevent concurrent page skips.
    issue_fetch_mutex: Arc<Mutex<()>>,
    /// Last broadcast provider data (with injected issues), used for delta computation.
    last_broadcast_providers: ProviderData,
    /// Last broadcast provider health, used for delta computation.
    last_broadcast_health: HashMap<String, HashMap<String, bool>>,
    /// Last broadcast errors, used for delta computation.
    last_broadcast_errors: Vec<ProviderError>,
    /// Bounded delta log for replay on client reconnect.
    delta_log: VecDeque<DeltaEntry>,
    /// Incremented only when local provider data changes (not peer data merges).
    /// Used by the outbound peer task to avoid re-sending unchanged local data.
    local_data_version: u64,
}

impl RepoState {
    /// Compute a delta from the last broadcast state to the new state,
    /// append to the delta log, and update tracking fields.
    fn record_delta(
        &mut self,
        new_providers: &ProviderData,
        new_health: &HashMap<String, HashMap<String, bool>>,
        new_errors: &[ProviderError],
        work_items: Vec<flotilla_protocol::snapshot::WorkItem>,
    ) -> DeltaEntry {
        let mut changes = delta::diff_provider_data(&self.last_broadcast_providers, new_providers);

        // Diff provider health (nested: category → provider → bool)
        for (category, providers) in new_health {
            let old_providers = self.last_broadcast_health.get(category);
            for (provider, &val) in providers {
                let old_val = old_providers.and_then(|p| p.get(provider));
                match old_val {
                    Some(&prev) if prev == val => {}
                    Some(_) => changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Updated(val),
                    }),
                    None => changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Added(val),
                    }),
                }
            }
        }
        // Check for removed entries
        for (category, old_providers) in &self.last_broadcast_health {
            let new_providers = new_health.get(category);
            for provider in old_providers.keys() {
                if new_providers.and_then(|p| p.get(provider)).is_none() {
                    changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Removed,
                    });
                }
            }
        }

        // Diff errors
        if let Some(error_change) = delta::diff_errors(&self.last_broadcast_errors, new_errors) {
            changes.push(error_change);
        }

        let prev_seq = self.seq;
        let entry = DeltaEntry { seq: self.seq + 1, prev_seq, changes, work_items };

        // Append to bounded log
        self.delta_log.push_back(entry.clone());
        if self.delta_log.len() > DELTA_LOG_CAPACITY {
            self.delta_log.pop_front();
        }

        // Update tracking state
        self.last_broadcast_providers = new_providers.clone();
        self.last_broadcast_health = new_health.clone();
        self.last_broadcast_errors = new_errors.to_vec();

        entry
    }
}

/// Tracks a currently executing step-based command for cancellation.
struct ActiveCommand {
    command_id: u64,
    token: CancellationToken,
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<PathBuf, RepoState>>,
    repo_order: RwLock<Vec<PathBuf>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    runner: Arc<dyn CommandRunner>,
    next_command_id: AtomicU64,
    host_name: HostName,
    /// When true, only local providers (VCS, checkout manager, workspace
    /// manager, terminal pool) are registered. External providers (code
    /// review, issue tracker, cloud agents, AI utilities) are skipped
    /// because the follower receives that data from the leader via PeerData.
    follower: bool,
    /// Peer provider data overlay, keyed by local repo path.
    /// Set by the DaemonServer when peer snapshots arrive. Merged into
    /// the local snapshot during broadcast.
    peer_providers: RwLock<HashMap<PathBuf, Vec<(HostName, ProviderData)>>>,
    /// Maps RepoIdentity → local repo path, built during repo setup.
    /// Used to route inbound peer data to the correct local repo.
    repo_identities: RwLock<HashMap<flotilla_protocol::RepoIdentity, PathBuf>>,
    /// Current peer connection status, updated via `set_peer_status()` and
    /// replayed to late-subscribing clients via `replay_since()`.
    peer_status: RwLock<HashMap<HostName, PeerConnectionState>>,
    /// Host-level environment assertions, computed once at startup and
    /// reused for each repo discovery.
    host_bag: EnvironmentBag,
    /// Repo-level detectors, computed once at startup.
    repo_detectors: Vec<Box<dyn RepoDetector>>,
    /// Provider factories, computed once at startup (follower vs full).
    factories: FactoryRegistry,
    /// The currently active step-based command, if any — for cancellation.
    active_command: Arc<Mutex<Option<ActiveCommand>>>,
}

impl InProcessDaemon {
    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because a background poll task is spawned that
    /// holds a reference. The poll loop checks every 100ms for new refresh
    /// snapshots and broadcasts delta or full events for each change.
    pub async fn new(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<Self> {
        Self::new_with_options(repo_paths, config, false, HostName::local()).await
    }

    /// Create a new in-process daemon with explicit follower mode.
    ///
    /// When `follower` is true, external providers (code review, issue
    /// tracker, cloud agents, AI utilities) are skipped during provider
    /// discovery. The follower daemon only reports local state (VCS,
    /// checkouts, workspace manager, terminal pool). Service-level data
    /// arrives from the leader via PeerData messages.
    pub async fn new_with_options(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>, follower: bool, host_name: HostName) -> Arc<Self> {
        use crate::providers::discovery::{self, detectors, DiscoveryResult, FactoryRegistry, ProcessEnvVars};

        let (event_tx, _) = broadcast::channel(256);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let mut repos = HashMap::new();
        let mut order = Vec::new();
        let mut identities = HashMap::new();

        // Run host detection once before the repo loop
        let host_detectors = detectors::default_host_detectors();
        let repo_detectors = detectors::default_repo_detectors();
        let host_bag = discovery::run_host_detectors(&host_detectors, &*runner, &ProcessEnvVars).await;
        let factories = if follower { FactoryRegistry::for_follower() } else { FactoryRegistry::default_all() };

        for path in repo_paths {
            if repos.contains_key(&path) {
                continue;
            }
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } =
                discovery::discover_providers(&host_bag, &path, &repo_detectors, &factories, &config, Arc::clone(&runner), &ProcessEnvVars)
                    .await;
            if !unmet.is_empty() {
                debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
            }

            // RepoIdentity from the merged bag
            if let Some(identity) = host_repo_bag.repo_identity() {
                identities.insert(identity, path.clone());
            }

            let slug = repo_slug.clone();
            let mut model = RepoModel::new(path.clone(), registry, repo_slug);
            model.data.loading = true;
            repos.insert(path.clone(), RepoState {
                model,
                slug,
                repo_bag,
                unmet,
                seq: 0,
                last_snapshot: Arc::new(RefreshSnapshot::default()),
                issue_cache: IssueCache::new(),
                search_results: None,
                issue_fetch_mutex: Arc::new(Mutex::new(())),
                last_broadcast_providers: ProviderData::default(),
                last_broadcast_health: HashMap::new(),
                last_broadcast_errors: Vec::new(),
                delta_log: VecDeque::new(),
                local_data_version: 0,
            });
            order.push(path);
        }

        let daemon = Arc::new(Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
            config,
            runner,
            next_command_id: AtomicU64::new(1),
            host_name,
            follower,
            peer_providers: RwLock::new(HashMap::new()),
            repo_identities: RwLock::new(identities),
            peer_status: RwLock::new(HashMap::new()),
            host_bag,
            repo_detectors,
            factories,
            active_command: Arc::new(Mutex::new(None)),
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

    /// Returns whether this daemon is running in follower mode.
    pub fn is_follower(&self) -> bool {
        self.follower
    }

    /// Find the local repo path that matches a given RepoIdentity, if any.
    pub async fn find_repo_by_identity(&self, identity: &flotilla_protocol::RepoIdentity) -> Option<PathBuf> {
        self.repo_identities.read().await.get(identity).cloned()
    }

    /// Reverse lookup: find the RepoIdentity for a given local repo path.
    pub async fn find_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        let identities = self.repo_identities.read().await;
        identities.iter().find(|(_, path)| path.as_path() == repo_path).map(|(id, _)| id.clone())
    }

    /// Get the local-only provider data for a repo (without peer overlay).
    ///
    /// Used by the outbound replication task to send only this host's
    /// authoritative data to peers, avoiding echo-back of merged peer data.
    pub async fn get_local_providers(&self, repo: &Path) -> Option<(ProviderData, u64)> {
        let repos = self.repos.read().await;
        let state = repos.get(repo)?;
        let providers = inject_issues(&state.last_snapshot.providers, &state.issue_cache, &state.search_results);
        Some((providers, state.local_data_version))
    }

    /// Update the peer provider data overlay for a repo and trigger re-broadcast.
    ///
    /// Called by the DaemonServer when PeerManager receives updated peer data.
    /// The peer data is merged into the local snapshot during the next broadcast.
    pub async fn set_peer_providers(&self, repo_path: &Path, peers: Vec<(HostName, ProviderData)>) {
        {
            let mut pp = self.peer_providers.write().await;
            pp.insert(repo_path.to_path_buf(), peers);
        }
        self.broadcast_snapshot_inner(repo_path, false).await;
    }

    /// Poll all repos for new refresh snapshots.
    ///
    /// For each repo whose background refresh has produced a new snapshot,
    /// update internal state, increment the sequence number, and broadcast
    /// a `DaemonEvent::SnapshotFull` or `DaemonEvent::SnapshotDelta`.
    ///
    /// Called automatically by the background poll loop spawned in `new()`.
    async fn poll_snapshots(&self) {
        // Collect changed snapshots under a brief write lock (need &mut for borrow_and_update),
        // then do correlation work outside the lock to avoid blocking other operations.
        let changed: Vec<_> = {
            let mut repos = self.repos.write().await;
            repos
                .iter_mut()
                .filter_map(|(path, state)| {
                    let handle = &mut state.model.refresh_handle;
                    if !handle.snapshot_rx.has_changed().unwrap_or(false) {
                        return None;
                    }
                    let snapshot = handle.snapshot_rx.borrow_and_update().clone();
                    let providers = inject_issues(&snapshot.providers, &state.issue_cache, &state.search_results);

                    Some((
                        path.clone(),
                        snapshot,
                        providers,
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
        for (path, snapshot, providers, issue_total, issue_has_more, search_results) in changed {
            // Merge peer provider data if any
            let providers = if let Some(peers) = peer_overlay.get(&path) {
                let peer_refs: Vec<(HostName, &ProviderData)> = peers.iter().map(|(h, d)| (h.clone(), d)).collect();
                Arc::new(crate::merge::merge_provider_data(&providers, &self.host_name, &peer_refs))
            } else {
                Arc::new(providers)
            };
            let (work_items, correlation_groups) = crate::data::correlate(&providers);

            let re_snapshot = RefreshSnapshot {
                providers,
                work_items,
                correlation_groups,
                errors: snapshot.errors.clone(),
                provider_health: snapshot.provider_health.clone(),
            };
            updates.push((path, snapshot, re_snapshot, issue_total, issue_has_more, search_results));
        }

        // Apply updates under write lock and broadcast
        let mut repos = self.repos.write().await;
        for (path, snapshot, re_snapshot, issue_total, issue_has_more, search_results) in updates {
            let Some(state) = repos.get_mut(&path) else {
                continue;
            };

            state.model.data.providers = Arc::clone(&re_snapshot.providers);
            state.model.data.correlation_groups = re_snapshot.correlation_groups.clone();
            state.model.data.provider_health = snapshot.provider_health.clone();
            state.model.data.loading = false;

            let mut proto_snapshot = snapshot_to_proto(&path, state.seq + 1, &re_snapshot, &self.host_name);
            proto_snapshot.provider_health = crate::convert::health_to_proto(&state.model.data.provider_health);
            proto_snapshot.issue_total = issue_total;
            proto_snapshot.issue_has_more = issue_has_more;
            proto_snapshot.issue_search_results = search_results;

            // Compute and log delta before updating seq
            let delta_entry = state.record_delta(
                &proto_snapshot.providers,
                &proto_snapshot.provider_health,
                &proto_snapshot.errors,
                proto_snapshot.work_items.clone(),
            );
            debug!(
                "repo {}: delta seq {} → {} with {} changes",
                path.display(),
                delta_entry.prev_seq,
                delta_entry.seq,
                delta_entry.changes.len()
            );

            state.seq += 1;
            state.local_data_version += 1;
            state.last_snapshot = snapshot;

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
        // Serialize fetches per-repo to prevent concurrent calls from reading the same
        // next_page and skipping pages.
        let mutex = {
            let repos = self.repos.read().await;
            match repos.get(repo) {
                Some(state) => Arc::clone(&state.issue_fetch_mutex),
                None => return,
            }
        };
        let _guard = mutex.lock().await;
        loop {
            // Check cache state and grab registry Arc (single read lock)
            let (page_num, registry) = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(repo) else {
                    return;
                };
                let need = state.issue_cache.len() < desired_count && state.issue_cache.has_more;
                if !need {
                    break;
                }
                if state.model.registry.issue_trackers.is_empty() {
                    // No tracker — stop claiming more pages are available
                    drop(repos);
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(repo) {
                        state.issue_cache.has_more = false;
                    }
                    break;
                }
                (state.issue_cache.next_page, Arc::clone(&state.model.registry))
            };

            // Fetch the next page outside any lock
            let page_result = {
                let (_, tracker) = registry.issue_trackers.values().next().unwrap();
                tracker.list_issues_page(repo, page_num, 50).await
            };

            match page_result {
                Ok(page) => {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(repo) {
                        state.issue_cache.merge_page(page);
                        if state.issue_cache.last_refreshed_at.is_none() {
                            state.issue_cache.mark_refreshed(now_iso8601());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(%page_num, err = %e, "failed to fetch issue page");
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(repo) {
                        state.issue_cache.has_more = false;
                    }
                    break;
                }
            }
        }
    }

    /// Run a search query against the issue tracker and store the results.
    async fn search_issues(&self, repo: &Path, query: &str) {
        let registry = {
            let repos = self.repos.read().await;
            let Some(state) = repos.get(repo) else {
                return;
            };
            Arc::clone(&state.model.registry)
        };

        let result = {
            let Some((_, tracker)) = registry.issue_trackers.values().next() else {
                return;
            };
            tracker.search_issues(repo, query, 50).await
        };

        match result {
            Ok(issues) => {
                info!(count = issues.len(), "search returned issues for query");
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(repo) {
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
                .filter_map(|(path, state)| {
                    let linked_ids = collect_linked_issue_ids(&state.last_snapshot.providers);
                    let missing = state.issue_cache.missing_ids(&linked_ids);
                    if missing.is_empty() {
                        return None;
                    }
                    Some((path.clone(), missing, Arc::clone(&state.model.registry), Arc::clone(&state.issue_fetch_mutex)))
                })
                .collect()
        };

        if fetch_tasks.is_empty() {
            return;
        }

        // Phase 2: fetch outside locks, then update cache and re-broadcast.
        // Acquire the per-repo issue_fetch_mutex to avoid redundant API calls
        // if ensure_issues_cached is running concurrently.
        for (path, missing, registry, fetch_mutex) in fetch_tasks {
            let _guard = fetch_mutex.lock().await;

            // Re-check missing after acquiring mutex — ensure_issues_cached may
            // have already fetched some of these while we waited.
            let missing = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(&path) else {
                    continue;
                };
                state.issue_cache.missing_ids(&missing)
            };
            if missing.is_empty() {
                continue;
            }

            let Some((_, tracker)) = registry.issue_trackers.values().next() else {
                continue;
            };
            match tracker.fetch_issues_by_id(&path, &missing).await {
                Ok(fetched) if !fetched.is_empty() => {
                    {
                        let mut repos = self.repos.write().await;
                        if let Some(state) = repos.get_mut(&path) {
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
                .filter_map(|(path, state)| {
                    let since = state.issue_cache.last_refreshed_at.as_ref()?;
                    if state.model.registry.issue_trackers.is_empty() {
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
                        path.clone(),
                        since.clone(),
                        Arc::clone(&state.model.registry),
                        Arc::clone(&state.issue_fetch_mutex),
                        state.issue_cache.len(),
                    ))
                })
                .collect()
        };

        for (path, since, registry, fetch_mutex, prev_count) in tasks {
            let _guard = fetch_mutex.lock().await;
            let tracker = match registry.issue_trackers.values().next() {
                Some((_, t)) => t,
                None => continue,
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
                                repos.get(&path).map(|s| Arc::clone(&s.model.registry))
                            };
                            if let Some(reg) = reg {
                                if let Some((_, t)) = reg.issue_trackers.values().next() {
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
                                if let Some(state) = repos.get_mut(&path) {
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
                                if let Some(state) = repos.get_mut(&path) {
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
                            if let Some(state) = repos.get_mut(&path) {
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
    /// `<remote>/desktop/home/dev/repo`). The `provider_data` is the
    /// initial merged data from peer snapshots.
    ///
    /// Emits `DaemonEvent::RepoAdded` so the TUI creates a tab.
    pub async fn add_virtual_repo(&self, synthetic_path: PathBuf, provider_data: ProviderData) -> Result<(), String> {
        // Check if already tracked
        {
            let repos = self.repos.read().await;
            if repos.contains_key(&synthetic_path) {
                return Ok(());
            }
        }

        let mut model = RepoModel::new_virtual();
        model.data.providers = Arc::new(provider_data);
        model.data.loading = false;

        let repo_info = RepoInfo {
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
            if repos.contains_key(&synthetic_path) {
                return Ok(());
            }
            repos.insert(synthetic_path.clone(), RepoState {
                model,
                slug: None,
                repo_bag: EnvironmentBag::new(),
                unmet: Vec::new(),
                seq: 0,
                last_snapshot: Arc::new(RefreshSnapshot::default()),
                issue_cache: IssueCache::new(),
                search_results: None,
                issue_fetch_mutex: Arc::new(Mutex::new(())),
                last_broadcast_providers: ProviderData::default(),
                last_broadcast_health: HashMap::new(),
                last_broadcast_errors: Vec::new(),
                delta_log: VecDeque::new(),
                local_data_version: 0,
            });
            order.push(synthetic_path.clone());
        }

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        let _ = self.event_tx.send(DaemonEvent::RepoAdded(Box::new(repo_info)));

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
        // Read peer overlay (brief read lock)
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(repo).cloned()
        };

        let mut repos = self.repos.write().await;
        let Some(state) = repos.get_mut(repo) else {
            return;
        };

        let proto_snapshot = build_repo_snapshot_with_peers(
            repo,
            state.seq + 1,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        );

        // Compute and log delta
        let delta_entry = state.record_delta(
            &proto_snapshot.providers,
            &proto_snapshot.provider_health,
            &proto_snapshot.errors,
            proto_snapshot.work_items.clone(),
        );
        state.seq += 1;
        if is_local_change {
            state.local_data_version += 1;
        }

        let event = choose_event(proto_snapshot, delta_entry);
        let _ = self.event_tx.send(event);
    }

    /// Send an arbitrary event to all subscribers.
    ///
    /// If the event is a `PeerStatusChanged`, also records the status so
    /// `replay_since` can include it for late-subscribing clients.
    pub fn send_event(&self, event: DaemonEvent) {
        if let DaemonEvent::PeerStatusChanged { ref host, ref status } = event {
            // Use try_write to avoid blocking; if contended, the replay
            // will use a slightly stale value — acceptable for display.
            if let Ok(mut map) = self.peer_status.try_write() {
                map.insert(host.clone(), *status);
            }
        }
        let _ = self.event_tx.send(event);
    }

    /// Resolve a slug (directory name, explicit slug, or full path) to a repo PathBuf.
    async fn resolve_slug(&self, slug: &str) -> Result<PathBuf, String> {
        let repos = self.repos.read().await;
        let entries: Vec<_> = repos.iter().map(|(path, state)| (path.as_path(), state.slug.as_deref())).collect();
        crate::resolve::resolve_repo(slug, entries.into_iter()).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(repo).cloned()
        };
        let repos = self.repos.read().await;
        let state = repos.get(repo).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;

        Ok(build_repo_snapshot_with_peers(
            repo,
            state.seq,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        ))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut result = Vec::new();
        for path in order.iter() {
            if let Some(state) = repos.get(path) {
                result.push(RepoInfo {
                    path: path.clone(),
                    name: repo_name(path),
                    labels: state.model.labels.clone(),
                    provider_names: provider_names_from_registry(&state.model.registry),
                    provider_health: crate::convert::health_to_proto(&state.model.data.provider_health),
                    loading: state.model.data.loading,
                });
            }
        }
        Ok(result)
    }

    async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String> {
        // Issue commands: execute inline, no lifecycle events.
        // These are synchronous cache operations that return immediately.
        match &command {
            Command::SetIssueViewport { visible_count, .. } => {
                self.ensure_issues_cached(repo, *visible_count * 2).await;
                self.broadcast_snapshot(repo).await;
                return Ok(INLINE_COMMAND_ID);
            }
            Command::FetchMoreIssues { desired_count, .. } => {
                self.ensure_issues_cached(repo, *desired_count).await;
                self.broadcast_snapshot(repo).await;
                return Ok(INLINE_COMMAND_ID);
            }
            Command::SearchIssues { query, .. } => {
                self.search_issues(repo, query).await;
                self.broadcast_snapshot(repo).await;
                return Ok(INLINE_COMMAND_ID);
            }
            Command::ClearIssueSearch { .. } => {
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(repo) {
                    state.search_results = None;
                }
                drop(repos);
                self.broadcast_snapshot(repo).await;
                return Ok(INLINE_COMMAND_ID);
            }
            _ => {}
        }

        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        // Gather what the spawned task needs — validate repo before broadcasting
        let runner = Arc::clone(&self.runner);
        let event_tx = self.event_tx.clone();
        let (registry, providers_data, refresh_trigger) = {
            let repos = self.repos.read().await;
            let state = repos.get(repo).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            (
                Arc::clone(&state.model.registry),
                Arc::clone(&state.model.data.providers),
                Arc::clone(&state.model.refresh_handle.refresh_trigger),
            )
        };

        // Broadcast started after repo validation (ensures no orphaned CommandStarted)
        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let config_base = self.config.base_path().to_path_buf();
        let _ = self.event_tx.send(DaemonEvent::CommandStarted { command_id: id, repo: repo_path.clone(), description });

        // Spawn the entire build_plan + execution so execute() returns the
        // command_id immediately. This keeps the TUI event loop responsive —
        // build_plan runs execute() inline for Immediate commands, which may
        // do network I/O (e.g. GenerateBranchName, ArchiveSession).
        let active_ref = Arc::clone(&self.active_command);
        tokio::spawn(async move {
            let plan = executor::build_plan(command, repo_path.clone(), registry, providers_data, runner, config_base).await;

            match plan {
                ExecutionPlan::Immediate(result) => {
                    refresh_trigger.notify_one();
                    let _ = event_tx.send(DaemonEvent::CommandFinished { command_id: id, repo: repo_path, result });
                }
                ExecutionPlan::Steps(step_plan) => {
                    // Reject if another step command is already running.
                    // Single-slot design: one step command at a time (global).
                    // Hold the lock across check-and-set to avoid TOCTOU races.
                    let token = CancellationToken::new();
                    {
                        let mut guard = active_ref.lock().await;
                        if let Some(active) = &*guard {
                            let _ = event_tx.send(DaemonEvent::CommandFinished {
                                command_id: id,
                                repo: repo_path,
                                result: flotilla_protocol::CommandResult::Error {
                                    message: format!("another command is already running (id {})", active.command_id),
                                },
                            });
                            return;
                        }
                        *guard = Some(ActiveCommand { command_id: id, token: token.clone() });
                    }

                    let result = run_step_plan(step_plan, id, repo_path.clone(), token, event_tx.clone()).await;
                    refresh_trigger.notify_one();
                    let mut guard = active_ref.lock().await;
                    if guard.as_ref().map(|a| a.command_id) == Some(id) {
                        *guard = None;
                    }
                    let _ = event_tx.send(DaemonEvent::CommandFinished { command_id: id, repo: repo_path, result });
                }
            }
        });

        Ok(id)
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_command.lock().await;
        match &*guard {
            Some(active) if active.command_id == command_id => {
                active.token.cancel();
                Ok(())
            }
            _ => Err("no matching active command".into()),
        }
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        let (prev_count, registry) = {
            let repos = self.repos.read().await;
            let state = repos.get(repo).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            state.model.refresh_handle.trigger_refresh();
            (state.issue_cache.len(), Arc::clone(&state.model.registry))
        };

        if prev_count > 0 {
            // Fetch page 1 before resetting, so failures don't wipe the UI.
            let first_page =
                if let Some((_, t)) = registry.issue_trackers.values().next() { t.list_issues_page(repo, 1, 50).await.ok() } else { None };

            if first_page.is_some() {
                {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(repo) {
                        state.issue_cache.reset();
                        if let Some(page) = first_page {
                            state.issue_cache.merge_page(page);
                        }
                    }
                }
                self.ensure_issues_cached(repo, prev_count).await;
                {
                    let mut repos = self.repos.write().await;
                    if let Some(state) = repos.get_mut(repo) {
                        state.issue_cache.mark_refreshed(now_iso8601());
                    }
                }
                self.broadcast_snapshot(repo).await;
            }
        }

        Ok(())
    }

    async fn add_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();

        // Check if already tracked (under read lock for fast path)
        {
            let repos = self.repos.read().await;
            if repos.contains_key(&path) {
                return Ok(());
            }
        }

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_providers(
            &self.host_bag,
            &path,
            &self.repo_detectors,
            &self.factories,
            &self.config,
            Arc::clone(&self.runner),
            &ProcessEnvVars,
        )
        .await;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let slug = repo_slug.clone();
        let mut model = RepoModel::new(path.clone(), registry, repo_slug);
        model.data.loading = true;

        let repo_info = RepoInfo {
            path: path.clone(),
            name: repo_name(&path),
            labels: model.labels.clone(),
            provider_names: provider_names_from_registry(&model.registry),
            provider_health: crate::convert::health_to_proto(&model.data.provider_health),
            loading: true,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.contains_key(&path) {
                return Ok(());
            }
            repos.insert(path.clone(), RepoState {
                model,
                slug,
                repo_bag,
                unmet,
                seq: 0,
                last_snapshot: Arc::new(RefreshSnapshot::default()),
                issue_cache: IssueCache::new(),
                search_results: None,
                issue_fetch_mutex: Arc::new(Mutex::new(())),
                last_broadcast_providers: ProviderData::default(),
                last_broadcast_health: HashMap::new(),
                last_broadcast_errors: Vec::new(),
                delta_log: VecDeque::new(),
                local_data_version: 0,
            });
            order.push(path.clone());
        }

        // Register RepoIdentity for peer routing
        if let Some(identity) = host_repo_bag.repo_identity() {
            self.repo_identities.write().await.insert(identity, path.clone());
        }

        // Persist to config
        self.config.save_repo(&path);
        let order = self.repo_order.read().await;
        self.config.save_tab_order(&order);

        info!(repo = %path.display(), "added repo");
        let _ = self.event_tx.send(DaemonEvent::RepoAdded(Box::new(repo_info)));

        Ok(())
    }

    async fn replay_since(&self, last_seen: &HashMap<PathBuf, u64>) -> Result<Vec<DaemonEvent>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut events = Vec::new();

        for path in order.iter() {
            let Some(state) = repos.get(path) else {
                continue;
            };
            // Skip repos that haven't completed their first refresh yet —
            // broadcasting empty placeholder state would clear the loading indicator.
            if state.seq == 0 {
                continue;
            }
            let snapshot =
                || build_repo_snapshot(path, state.seq, &state.last_snapshot, &state.issue_cache, &state.search_results, &self.host_name);

            match last_seen.get(path) {
                Some(&client_seq) => {
                    // Try to find the client's seq in the delta log and replay from there
                    let replay_start = state.delta_log.iter().position(|entry| entry.prev_seq == client_seq);

                    if let Some(start_idx) = replay_start {
                        // Capture issue metadata once — it doesn't change per-entry
                        let issue_snapshot = snapshot();
                        // Replay delta entries (each carries pre-correlated work_items)
                        for entry in state.delta_log.iter().skip(start_idx) {
                            events.push(DaemonEvent::SnapshotDelta(Box::new(flotilla_protocol::SnapshotDelta {
                                seq: entry.seq,
                                prev_seq: entry.prev_seq,
                                repo: path.clone(),
                                changes: entry.changes.clone(),
                                work_items: entry.work_items.clone(),
                                issue_total: issue_snapshot.issue_total,
                                issue_has_more: issue_snapshot.issue_has_more,
                                issue_search_results: issue_snapshot.issue_search_results.clone(),
                            })));
                        }
                    } else if client_seq == state.seq {
                        // Client is up to date — no replay needed
                    } else {
                        // Seq not in delta log — send full snapshot
                        events.push(DaemonEvent::SnapshotFull(Box::new(snapshot())));
                    }
                }
                None => {
                    // Client has never seen this repo — send full snapshot
                    events.push(DaemonEvent::SnapshotFull(Box::new(snapshot())));
                }
            }
        }

        // Include current peer status so late-subscribing clients see it
        let peer_status = self.peer_status.read().await;
        for (host, status) in peer_status.iter() {
            events.push(DaemonEvent::PeerStatusChanged { host: host.clone(), status: *status });
        }

        Ok(events)
    }

    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();

        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.remove(&path).is_none() {
                return Err(format!("repo not tracked: {}", path.display()));
            }
            order.retain(|p| p != &path);
        }

        // Remove from identity map and peer overlay
        {
            let mut ids = self.repo_identities.write().await;
            ids.retain(|_, p| p != &path);
        }
        {
            let mut pp = self.peer_providers.write().await;
            pp.remove(&path);
        }

        // Persist to config
        self.config.remove_repo(&path);
        let order = self.repo_order.read().await;
        self.config.save_tab_order(&order);

        info!(repo = %path.display(), "removed repo");
        let _ = self.event_tx.send(DaemonEvent::RepoRemoved { path });

        Ok(())
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        let peer_providers = self.peer_providers.read().await;
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut summaries = Vec::new();

        for path in repo_order.iter() {
            let Some(state) = repos.get(path) else { continue };
            let peer_overlay = peer_providers.get(path).cloned();
            let snapshot = build_repo_snapshot_with_peers(
                path,
                state.seq,
                &state.last_snapshot,
                &state.issue_cache,
                &state.search_results,
                &self.host_name,
                peer_overlay.as_deref(),
            );
            summaries.push(RepoSummary {
                path: path.clone(),
                slug: state.slug.clone(),
                provider_health: snapshot.provider_health,
                work_item_count: snapshot.work_items.len(),
                error_count: snapshot.errors.len(),
            });
        }
        Ok(StatusResponse { repos: summaries })
    }

    async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String> {
        let path = self.resolve_slug(slug).await?;
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(&path).cloned()
        };
        let repos = self.repos.read().await;
        let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
        let snapshot = build_repo_snapshot_with_peers(
            &path,
            state.seq,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        );
        Ok(RepoDetailResponse {
            path,
            slug: state.slug.clone(),
            provider_health: snapshot.provider_health,
            work_items: snapshot.work_items,
            errors: snapshot.errors,
        })
    }

    async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String> {
        let path = self.resolve_slug(slug).await?;
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(&path).cloned()
        };
        let repos = self.repos.read().await;
        let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
        let snapshot = build_repo_snapshot_with_peers(
            &path,
            state.seq,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        );

        let host_discovery = self.host_bag.assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();
        let repo_discovery = state.repo_bag.assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();

        let provider_infos = state
            .model
            .registry
            .provider_infos()
            .into_iter()
            .map(|(category, name)| {
                let healthy = snapshot.provider_health.get(&category).and_then(|providers| providers.get(&name)).copied().unwrap_or(true);
                ProviderInfo { category, name, healthy }
            })
            .collect();

        let unmet_requirements = state
            .unmet
            .iter()
            .map(|(factory, req)| UnmetRequirementInfo { factory: factory.clone(), requirement: format!("{req:?}") })
            .collect();

        Ok(RepoProvidersResponse {
            path,
            slug: state.slug.clone(),
            host_discovery,
            repo_discovery,
            providers: provider_infos,
            unmet_requirements,
        })
    }

    async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String> {
        let path = self.resolve_slug(slug).await?;
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(&path).cloned()
        };
        let repos = self.repos.read().await;
        let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
        let snapshot = build_repo_snapshot_with_peers(
            &path,
            state.seq,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        );
        Ok(RepoWorkResponse { path, slug: state.slug.clone(), work_items: snapshot.work_items })
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
        let snapshot = Snapshot {
            seq: 2,
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
        assert!(matches!(choose_event(snapshot.clone(), initial), DaemonEvent::SnapshotFull(_)));

        let non_empty = DeltaEntry {
            seq: 2,
            prev_seq: 1,
            changes: vec![flotilla_protocol::Change::Branch { key: "feature/x".into(), op: flotilla_protocol::EntryOp::Removed }],
            work_items: vec![],
        };
        assert!(matches!(choose_event(snapshot, non_empty), DaemonEvent::SnapshotDelta(_)));
    }

    #[test]
    fn choose_event_falls_back_to_full_when_delta_is_larger() {
        let snapshot = Snapshot {
            seq: 3,
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

        assert!(matches!(choose_event(snapshot, delta), DaemonEvent::SnapshotFull(_)));
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

        let snap = build_repo_snapshot(Path::new("/tmp/repo"), 7, &RefreshSnapshot::default(), &cache, &None, &HostName::local());
        assert_eq!(snap.seq, 7);
        assert_eq!(snap.issue_total, Some(5));
        assert!(snap.issue_has_more);
        assert!(snap.providers.issues.contains_key("9"));
    }
}
