//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::info;

use flotilla_protocol::{AssociationKey, Command, DaemonEvent, Issue, RepoInfo, Snapshot};

use flotilla_protocol::ProviderData;

use crate::config::ConfigStore;
use crate::convert::snapshot_to_proto;
use crate::daemon::DaemonHandle;
use crate::executor;
use crate::issue_cache::IssueCache;
use crate::model::{provider_names_from_registry, repo_name, RepoModel};
use crate::providers::CommandRunner;
use crate::refresh::RefreshSnapshot;

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
    search_results: &Option<Vec<Issue>>,
) -> ProviderData {
    let mut providers = base_providers.clone();
    if let Some(ref results) = search_results {
        providers.issues = results.iter().map(|i| (i.id.clone(), i.clone())).collect();
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
    search_results: &Option<Vec<Issue>>,
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

struct RepoState {
    model: RepoModel,
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
    issue_cache: IssueCache,
    search_results: Option<Vec<Issue>>,
    /// Serializes issue fetch operations for this repo to prevent concurrent page skips.
    issue_fetch_mutex: Arc<Mutex<()>>,
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
    /// snapshots and broadcasts `DaemonEvent::Snapshot` for each change.
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
    /// a `DaemonEvent::Snapshot`.
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
            state.seq += 1;
            state.last_snapshot = snapshot;

            let mut proto_snapshot = snapshot_to_proto(&path, state.seq, &re_snapshot);
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
            let _ = self
                .event_tx
                .send(DaemonEvent::Snapshot(Box::new(proto_snapshot)));
        }

        // After broadcasting, check for linked issues that aren't cached yet
        // and fetch/pin them. This is a separate step so it doesn't block the
        // main snapshot broadcast path.
        drop(repos);
        self.fetch_missing_linked_issues().await;
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

    /// Re-build and broadcast a snapshot for the given repo using current cache state.
    async fn broadcast_snapshot(&self, repo: &Path) {
        let repos = self.repos.read().await;
        let Some(state) = repos.get(repo) else {
            return;
        };

        let proto_snapshot = build_repo_snapshot(
            repo,
            state.seq,
            &state.last_snapshot,
            &state.model.data.provider_health,
            &state.issue_cache,
            &state.search_results,
        );

        let _ = self
            .event_tx
            .send(DaemonEvent::Snapshot(Box::new(proto_snapshot)));
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
        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        // Issue commands: execute inline (already backgrounded by TUI), return ID
        match &command {
            Command::SetIssueViewport { visible_count, .. } => {
                self.ensure_issues_cached(repo, *visible_count * 2).await;
                self.broadcast_snapshot(repo).await;
                return Ok(id);
            }
            Command::FetchMoreIssues { desired_count, .. } => {
                self.ensure_issues_cached(repo, *desired_count).await;
                self.broadcast_snapshot(repo).await;
                return Ok(id);
            }
            Command::SearchIssues { query, .. } => {
                self.search_issues(repo, query).await;
                self.broadcast_snapshot(repo).await;
                return Ok(id);
            }
            Command::ClearIssueSearch { .. } => {
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(repo) {
                    state.search_results = None;
                }
                drop(repos);
                self.broadcast_snapshot(repo).await;
                return Ok(id);
            }
            _ => {}
        }

        // Broadcast started
        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id: id,
            repo: repo_path.clone(),
            description,
        });

        // Gather what the spawned task needs
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

        tokio::spawn(async move {
            let result =
                executor::execute(command, &repo_path, &registry, &providers_data, &*runner).await;

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
        let repos = self.repos.read().await;
        let state = repos
            .get(repo)
            .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
        state.model.refresh_handle.trigger_refresh();
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
