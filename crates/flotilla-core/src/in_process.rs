//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{debug, info};

use flotilla_protocol::{
    AssociationKey, Command, DaemonEvent, DeltaEntry, Issue, ProviderError, RepoInfo, Snapshot,
};

use flotilla_protocol::ProviderData;

use crate::config::ConfigStore;
use crate::convert::snapshot_to_proto;
use crate::daemon::DaemonHandle;
use crate::delta;
use crate::executor;
use crate::issue_cache::IssueCache;
use crate::model::{provider_names_from_registry, repo_name, RepoModel};
use crate::providers::CommandRunner;
use crate::refresh::RefreshSnapshot;

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
fn inject_issues(
    base_providers: &ProviderData,
    cache: &IssueCache,
    search_results: &Option<Vec<(String, Issue)>>,
) -> ProviderData {
    let mut providers = base_providers.clone();
    if let Some(ref results) = search_results {
        providers.issues = results
            .iter()
            .map(|(id, i)| (id.clone(), i.clone()))
            .collect();
    } else {
        providers.issues = (*cache.to_index_map()).clone();
    }
    providers
}

/// Build a proto Snapshot by injecting issues, re-correlating, and patching issue metadata.
fn build_repo_snapshot(
    path: &Path,
    seq: u64,
    base: &RefreshSnapshot,
    health: &HashMap<&'static str, bool>,
    cache: &IssueCache,
    search_results: &Option<Vec<(String, Issue)>>,
) -> Snapshot {
    let providers = Arc::new(inject_issues(&base.providers, cache, search_results));
    let (work_items, correlation_groups) = crate::data::correlate(&providers);
    let re_snapshot = RefreshSnapshot {
        providers,
        work_items,
        correlation_groups,
        errors: base.errors.clone(),
        provider_health: base.provider_health.clone(),
    };
    let mut snapshot = snapshot_to_proto(path, seq, &re_snapshot);
    snapshot.provider_health = health.iter().map(|(k, v)| (k.to_string(), *v)).collect();
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
        issue_total: snapshot.issue_total,
        issue_has_more: snapshot.issue_has_more,
        issue_search_results: snapshot.issue_search_results.clone(),
    };

    // Compare serialized sizes — if delta is larger, send full
    let delta_size = serde_json::to_string(&snapshot_delta).map(|s| s.len());
    let full_size = serde_json::to_string(&snapshot).map(|s| s.len());

    match (delta_size, full_size) {
        (Ok(d), Ok(f)) if d < f => {
            debug!("delta ({d} bytes) smaller than full ({f} bytes), sending delta");
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
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
    issue_cache: IssueCache,
    search_results: Option<Vec<(String, Issue)>>,
    /// Serializes issue fetch operations for this repo to prevent concurrent page skips.
    issue_fetch_mutex: Arc<Mutex<()>>,
    /// Last broadcast provider data (with injected issues), used for delta computation.
    last_broadcast_providers: ProviderData,
    /// Last broadcast provider health, used for delta computation.
    last_broadcast_health: HashMap<String, bool>,
    /// Last broadcast errors, used for delta computation.
    last_broadcast_errors: Vec<ProviderError>,
    /// Bounded delta log for replay on client reconnect.
    delta_log: VecDeque<DeltaEntry>,
}

impl RepoState {
    /// Compute a delta from the last broadcast state to the new state,
    /// append to the delta log, and update tracking fields.
    fn record_delta(
        &mut self,
        new_providers: &ProviderData,
        new_health: &HashMap<String, bool>,
        new_errors: &[ProviderError],
    ) -> DeltaEntry {
        let mut changes = delta::diff_provider_data(&self.last_broadcast_providers, new_providers);

        // Diff provider health
        for (key, &val) in new_health {
            match self.last_broadcast_health.get(key) {
                Some(&prev) if prev == val => {}
                Some(_) => changes.push(flotilla_protocol::Change::ProviderHealth {
                    provider: key.clone(),
                    op: flotilla_protocol::EntryOp::Updated(val),
                }),
                None => changes.push(flotilla_protocol::Change::ProviderHealth {
                    provider: key.clone(),
                    op: flotilla_protocol::EntryOp::Added(val),
                }),
            }
        }
        for key in self.last_broadcast_health.keys() {
            if !new_health.contains_key(key) {
                changes.push(flotilla_protocol::Change::ProviderHealth {
                    provider: key.clone(),
                    op: flotilla_protocol::EntryOp::Removed,
                });
            }
        }

        // Diff errors
        if let Some(error_change) = delta::diff_errors(&self.last_broadcast_errors, new_errors) {
            changes.push(error_change);
        }

        let prev_seq = self.seq;
        let entry = DeltaEntry {
            seq: self.seq + 1,
            prev_seq,
            changes,
        };

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

pub struct InProcessDaemon {
    repos: RwLock<HashMap<PathBuf, RepoState>>,
    repo_order: RwLock<Vec<PathBuf>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    runner: Arc<dyn CommandRunner>,
    next_command_id: AtomicU64,
}

impl InProcessDaemon {
    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because a background poll task is spawned that
    /// holds a reference. The poll loop checks every 100ms for new refresh
    /// snapshots and broadcasts delta or full events for each change.
    pub async fn new(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        let runner: Arc<dyn CommandRunner> = Arc::new(crate::providers::ProcessCommandRunner);
        let mut repos = HashMap::new();
        let mut order = Vec::new();

        for path in repo_paths {
            if repos.contains_key(&path) {
                continue;
            }
            let (registry, repo_slug) =
                crate::providers::discovery::detect_providers(&path, &config, Arc::clone(&runner))
                    .await;
            let mut model = RepoModel::new(path.clone(), registry, repo_slug);
            model.data.loading = true;
            repos.insert(
                path.clone(),
                RepoState {
                    model,
                    seq: 0,
                    last_snapshot: Arc::new(RefreshSnapshot::default()),
                    issue_cache: IssueCache::new(),
                    search_results: None,
                    issue_fetch_mutex: Arc::new(Mutex::new(())),
                    last_broadcast_providers: ProviderData::default(),
                    last_broadcast_health: HashMap::new(),
                    last_broadcast_errors: Vec::new(),
                    delta_log: VecDeque::new(),
                },
            );
            order.push(path);
        }

        let daemon = Arc::new(Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
            config,
            runner,
            next_command_id: AtomicU64::new(1),
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
                    let providers = inject_issues(
                        &snapshot.providers,
                        &state.issue_cache,
                        &state.search_results,
                    );

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

        // Correlate and build proto snapshots outside any lock
        let mut updates = Vec::new();
        for (path, snapshot, providers, issue_total, issue_has_more, search_results) in changed {
            let providers = Arc::new(providers);
            let (work_items, correlation_groups) = crate::data::correlate(&providers);

            let re_snapshot = RefreshSnapshot {
                providers,
                work_items,
                correlation_groups,
                errors: snapshot.errors.clone(),
                provider_health: snapshot.provider_health.clone(),
            };
            updates.push((
                path,
                snapshot,
                re_snapshot,
                issue_total,
                issue_has_more,
                search_results,
            ));
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

            let mut proto_snapshot = snapshot_to_proto(&path, state.seq + 1, &re_snapshot);
            proto_snapshot.provider_health = state
                .model
                .data
                .provider_health
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect();
            proto_snapshot.issue_total = issue_total;
            proto_snapshot.issue_has_more = issue_has_more;
            proto_snapshot.issue_search_results = search_results;

            // Compute and log delta before updating seq
            let delta_entry = state.record_delta(
                &proto_snapshot.providers,
                &proto_snapshot.provider_health,
                &proto_snapshot.errors,
            );
            debug!(
                "repo {}: delta seq {} → {} with {} changes",
                path.display(),
                delta_entry.prev_seq,
                delta_entry.seq,
                delta_entry.changes.len()
            );

            state.seq += 1;
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
                (
                    state.issue_cache.next_page,
                    Arc::clone(&state.model.registry),
                )
            };

            // Fetch the next page outside any lock
            let page_result = {
                let tracker = registry.issue_trackers.values().next().unwrap();
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
                    tracing::warn!("failed to fetch issue page {}: {}", page_num, e);
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
            let Some(tracker) = registry.issue_trackers.values().next() else {
                return;
            };
            tracker.search_issues(repo, query, 50).await
        };

        match result {
            Ok(issues) => {
                info!("search returned {} issues for query", issues.len());
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(repo) {
                    state.search_results = Some(issues);
                }
            }
            Err(e) => {
                tracing::warn!("issue search failed: {}", e);
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
                    Some((
                        path.clone(),
                        missing,
                        Arc::clone(&state.model.registry),
                        Arc::clone(&state.issue_fetch_mutex),
                    ))
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

            let Some(tracker) = registry.issue_trackers.values().next() else {
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
                    tracing::warn!(
                        "failed to fetch linked issues for {}: {}",
                        path.display(),
                        e
                    );
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
                Some(t) => t,
                None => continue,
            };

            // Record timestamp *before* the API call so the next `since`
            // window overlaps rather than gaps — avoids missing updates
            // that land on GitHub during the request.
            let refresh_ts = now_iso8601();

            match tracker.list_issues_changed_since(&path, &since, 50).await {
                Ok(changeset) => {
                    if changeset.has_more {
                        // Too many changes — skip incremental, do a full re-fetch.
                        // Don't reset until we have data to replace it with,
                        // so transient API failures don't wipe the UI.
                        drop(_guard);
                        let first_page = {
                            let reg = {
                                let repos = self.repos.read().await;
                                repos.get(&path).map(|s| Arc::clone(&s.model.registry))
                            };
                            if let Some(reg) = reg {
                                if let Some(t) = reg.issue_trackers.values().next() {
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
                        }
                    } else {
                        let has_changes =
                            !changeset.updated.is_empty() || !changeset.closed_ids.is_empty();
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
                    tracing::warn!(
                        "incremental issue refresh failed for {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    /// Re-build and broadcast a snapshot for the given repo using current cache state.
    async fn broadcast_snapshot(&self, repo: &Path) {
        let mut repos = self.repos.write().await;
        let Some(state) = repos.get_mut(repo) else {
            return;
        };

        let proto_snapshot = build_repo_snapshot(
            repo,
            state.seq + 1,
            &state.last_snapshot,
            &state.model.data.provider_health,
            &state.issue_cache,
            &state.search_results,
        );

        // Compute and log delta
        let delta_entry = state.record_delta(
            &proto_snapshot.providers,
            &proto_snapshot.provider_health,
            &proto_snapshot.errors,
        );
        state.seq += 1;

        let event = choose_event(proto_snapshot, delta_entry);
        let _ = self.event_tx.send(event);
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        let repos = self.repos.read().await;
        let state = repos
            .get(repo)
            .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;

        Ok(build_repo_snapshot(
            repo,
            state.seq,
            &state.last_snapshot,
            &state.model.data.provider_health,
            &state.issue_cache,
            &state.search_results,
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
                    provider_health: state
                        .model
                        .data
                        .provider_health
                        .iter()
                        .map(|(k, v)| (k.to_string(), *v))
                        .collect(),
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
            let state = repos
                .get(repo)
                .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
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
        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id: id,
            repo: repo_path.clone(),
            description,
        });

        tokio::spawn(async move {
            let result = executor::execute(
                command,
                &repo_path,
                &registry,
                &providers_data,
                &*runner,
                &config_base,
            )
            .await;

            // Trigger a refresh after command execution
            refresh_trigger.notify_one();

            let _ = event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                repo: repo_path,
                result,
            });
        });

        Ok(id)
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        let (prev_count, registry) = {
            let repos = self.repos.read().await;
            let state = repos
                .get(repo)
                .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            state.model.refresh_handle.trigger_refresh();
            (state.issue_cache.len(), Arc::clone(&state.model.registry))
        };

        if prev_count > 0 {
            // Fetch page 1 before resetting, so failures don't wipe the UI.
            let first_page = if let Some(t) = registry.issue_trackers.values().next() {
                t.list_issues_page(repo, 1, 50).await.ok()
            } else {
                None
            };

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
        let (registry, repo_slug) = crate::providers::discovery::detect_providers(
            &path,
            &self.config,
            Arc::clone(&self.runner),
        )
        .await;
        let mut model = RepoModel::new(path.clone(), registry, repo_slug);
        model.data.loading = true;

        let repo_info = RepoInfo {
            path: path.clone(),
            name: repo_name(&path),
            labels: model.labels.clone(),
            provider_names: provider_names_from_registry(&model.registry),
            provider_health: model
                .data
                .provider_health
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            loading: true,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.contains_key(&path) {
                return Ok(());
            }
            repos.insert(
                path.clone(),
                RepoState {
                    model,
                    seq: 0,
                    last_snapshot: Arc::new(RefreshSnapshot::default()),
                    issue_cache: IssueCache::new(),
                    search_results: None,
                    issue_fetch_mutex: Arc::new(Mutex::new(())),
                    last_broadcast_providers: ProviderData::default(),
                    last_broadcast_health: HashMap::new(),
                    last_broadcast_errors: Vec::new(),
                    delta_log: VecDeque::new(),
                },
            );
            order.push(path.clone());
        }

        // Persist to config
        self.config.save_repo(&path);
        let order = self.repo_order.read().await;
        self.config.save_tab_order(&order);

        info!("added repo {}", path.display());
        let _ = self
            .event_tx
            .send(DaemonEvent::RepoAdded(Box::new(repo_info)));

        Ok(())
    }

    async fn replay_since(
        &self,
        last_seen: &HashMap<PathBuf, u64>,
    ) -> Result<Vec<DaemonEvent>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut events = Vec::new();

        for path in order.iter() {
            let Some(state) = repos.get(path) else {
                continue;
            };
            let snapshot = || {
                build_repo_snapshot(
                    path,
                    state.seq,
                    &state.last_snapshot,
                    &state.model.data.provider_health,
                    &state.issue_cache,
                    &state.search_results,
                )
            };

            match last_seen.get(path) {
                Some(&client_seq) => {
                    // Try to find the client's seq in the delta log and replay from there
                    let replay_start = state
                        .delta_log
                        .iter()
                        .position(|entry| entry.prev_seq == client_seq);

                    if let Some(start_idx) = replay_start {
                        // Capture issue metadata once — it doesn't change per-entry
                        let issue_snapshot = snapshot();
                        // Replay delta entries
                        for entry in state.delta_log.iter().skip(start_idx) {
                            events.push(DaemonEvent::SnapshotDelta(Box::new(
                                flotilla_protocol::SnapshotDelta {
                                    seq: entry.seq,
                                    prev_seq: entry.prev_seq,
                                    repo: path.clone(),
                                    changes: entry.changes.clone(),
                                    issue_total: issue_snapshot.issue_total,
                                    issue_has_more: issue_snapshot.issue_has_more,
                                    issue_search_results: issue_snapshot
                                        .issue_search_results
                                        .clone(),
                                },
                            )));
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

        // Persist to config
        self.config.remove_repo(&path);
        let order = self.repo_order.read().await;
        self.config.save_tab_order(&order);

        info!("removed repo {}", path.display());
        let _ = self.event_tx.send(DaemonEvent::RepoRemoved { path });

        Ok(())
    }
}
