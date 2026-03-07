//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use flotilla_protocol::{Command, CommandResult, DaemonEvent, Issue, RepoInfo, Snapshot};

use crate::config::ConfigStore;
use crate::convert::snapshot_to_proto;
use crate::daemon::DaemonHandle;
use crate::executor;
use crate::issue_cache::IssueCache;
use crate::model::{provider_names_from_registry, repo_name, RepoModel};
use crate::providers::CommandRunner;
use crate::refresh::RefreshSnapshot;

struct RepoState {
    model: RepoModel,
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
    issue_cache: IssueCache,
    search_results: Option<Vec<Issue>>,
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<PathBuf, RepoState>>,
    repo_order: RwLock<Vec<PathBuf>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    runner: Arc<dyn CommandRunner>,
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
        let mut repos = self.repos.write().await;

        for (path, state) in repos.iter_mut() {
            let handle = &mut state.model.refresh_handle;
            if !handle.snapshot_rx.has_changed().unwrap_or(false) {
                continue;
            }

            let snapshot = handle.snapshot_rx.borrow_and_update().clone();

            // Merge cached issues into providers and re-correlate
            let mut providers = (*snapshot.providers).clone();
            providers.issues = state.issue_cache.to_index_map();
            let providers = Arc::new(providers);

            let (work_items, correlation_groups) = crate::data::correlate(&providers);

            // Update the model's DataStore with the merged provider data
            state.model.data.providers = Arc::clone(&providers);
            state.model.data.correlation_groups = correlation_groups.clone();
            state.model.data.provider_health = snapshot.provider_health.clone();
            state.model.data.loading = false;

            // Increment sequence and store snapshot
            state.seq += 1;
            state.last_snapshot = snapshot.clone();

            // Build proto snapshot from re-correlated data
            let re_snapshot = RefreshSnapshot {
                providers,
                work_items,
                correlation_groups,
                errors: snapshot.errors.clone(),
                provider_health: snapshot.provider_health.clone(),
            };
            let mut proto_snapshot = snapshot_to_proto(path, state.seq, &re_snapshot);
            // Use the model's (suppressed) health map, not the raw refresh snapshot's.
            proto_snapshot.provider_health = state
                .model
                .data
                .provider_health
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect();
            proto_snapshot.issue_total = state.issue_cache.total_count;
            proto_snapshot.issue_has_more = state.issue_cache.has_more;
            proto_snapshot.issue_search_results = state.search_results.clone();
            // Ignore send errors (no receivers is fine)
            let _ = self
                .event_tx
                .send(DaemonEvent::Snapshot(Box::new(proto_snapshot)));
        }
    }

    /// Fetch issue pages until the cache has at least `desired_count` entries
    /// (or no more pages are available).
    async fn ensure_issues_cached(&self, repo: &Path, desired_count: usize) {
        loop {
            // Check if we already have enough (under read lock)
            let (need_more, page_num, tracker_exists) = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(repo) else {
                    return;
                };
                let need =
                    state.issue_cache.entries.len() < desired_count && state.issue_cache.has_more;
                let page = state.issue_cache.next_page;
                let has_tracker = state
                    .model
                    .registry
                    .issue_trackers
                    .values()
                    .next()
                    .is_some();
                (need, page, has_tracker)
            };

            if !need_more || !tracker_exists {
                break;
            }

            // Fetch the next page outside the lock
            let page_result = {
                let repos = self.repos.read().await;
                let Some(state) = repos.get(repo) else {
                    return;
                };
                let tracker = state.model.registry.issue_trackers.values().next().unwrap();
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
                    break;
                }
            }
        }
    }

    /// Run a search query against the issue tracker and store the results.
    async fn search_issues(&self, repo: &Path, query: &str) {
        let result = {
            let repos = self.repos.read().await;
            let Some(state) = repos.get(repo) else {
                return;
            };
            let Some(tracker) = state.model.registry.issue_trackers.values().next() else {
                return;
            };
            tracker.search_issues(repo, query, 50).await
        };

        match result {
            Ok(issues) => {
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

    /// Re-build and broadcast a snapshot for the given repo using current cache state.
    async fn broadcast_snapshot(&self, repo: &Path) {
        let repos = self.repos.read().await;
        let Some(state) = repos.get(repo) else {
            return;
        };

        // Rebuild snapshot with current cache state
        let mut providers = (*state.last_snapshot.providers).clone();
        providers.issues = state.issue_cache.to_index_map();
        let providers = Arc::new(providers);
        let (work_items, correlation_groups) = crate::data::correlate(&providers);

        let re_snapshot = RefreshSnapshot {
            providers,
            work_items,
            correlation_groups,
            errors: state.last_snapshot.errors.clone(),
            provider_health: state.last_snapshot.provider_health.clone(),
        };

        let mut proto_snapshot = snapshot_to_proto(repo, state.seq, &re_snapshot);
        proto_snapshot.provider_health = state
            .model
            .data
            .provider_health
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        proto_snapshot.issue_total = state.issue_cache.total_count;
        proto_snapshot.issue_has_more = state.issue_cache.has_more;
        proto_snapshot.issue_search_results = state.search_results.clone();

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

        // Merge cached issues into providers and re-correlate
        let mut providers = (*state.last_snapshot.providers).clone();
        providers.issues = state.issue_cache.to_index_map();
        let providers = Arc::new(providers);
        let (work_items, correlation_groups) = crate::data::correlate(&providers);

        let re_snapshot = RefreshSnapshot {
            providers,
            work_items,
            correlation_groups,
            errors: state.last_snapshot.errors.clone(),
            provider_health: state.last_snapshot.provider_health.clone(),
        };

        let mut snapshot = snapshot_to_proto(repo, state.seq, &re_snapshot);
        // Use the model's suppressed health map (consistent with broadcast path)
        snapshot.provider_health = state
            .model
            .data
            .provider_health
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        snapshot.issue_total = state.issue_cache.total_count;
        snapshot.issue_has_more = state.issue_cache.has_more;
        snapshot.issue_search_results = state.search_results.clone();
        Ok(snapshot)
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

    async fn execute(&self, repo: &Path, command: Command) -> Result<CommandResult, String> {
        // Handle daemon-level issue commands directly
        match &command {
            Command::SetIssueViewport { visible_count, .. } => {
                // Fetch at least 200 so small repos get all issues upfront
                self.ensure_issues_cached(repo, (*visible_count * 2).max(200)).await;
                self.broadcast_snapshot(repo).await;
                return Ok(CommandResult::Ok);
            }
            Command::FetchMoreIssues { desired_count, .. } => {
                self.ensure_issues_cached(repo, *desired_count).await;
                self.broadcast_snapshot(repo).await;
                return Ok(CommandResult::Ok);
            }
            Command::SearchIssues { query, .. } => {
                self.search_issues(repo, query).await;
                self.broadcast_snapshot(repo).await;
                return Ok(CommandResult::Ok);
            }
            Command::ClearIssueSearch { .. } => {
                let mut repos = self.repos.write().await;
                if let Some(state) = repos.get_mut(repo) {
                    state.search_results = None;
                }
                drop(repos);
                self.broadcast_snapshot(repo).await;
                return Ok(CommandResult::Ok);
            }
            _ => {} // fall through to executor
        }

        // Extract the data we need under a read lock, then drop it before the async work
        let runner = Arc::clone(&self.runner);
        let (registry, providers_data, repo_root) = {
            let repos = self.repos.read().await;
            let state = repos
                .get(repo)
                .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            (
                Arc::clone(&state.model.registry),
                Arc::clone(&state.model.data.providers),
                repo.to_path_buf(),
            )
        };

        let result =
            executor::execute(command, &repo_root, &registry, &providers_data, &*runner).await;

        // Trigger a refresh after command execution
        {
            let repos = self.repos.read().await;
            if let Some(state) = repos.get(repo) {
                state.model.refresh_handle.trigger_refresh();
            }
        }

        Ok(result)
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
