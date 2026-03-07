//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

use flotilla_protocol::{Command, CommandResult, DaemonEvent, RepoInfo, Snapshot};

use crate::config;
use crate::convert::snapshot_to_proto;
use crate::daemon::DaemonHandle;
use crate::executor;
use crate::model::{provider_names_from_registry, repo_name, RepoModel};
use crate::refresh::RefreshSnapshot;

struct RepoState {
    model: RepoModel,
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<PathBuf, RepoState>>,
    repo_order: RwLock<Vec<PathBuf>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    poll_task: tokio::task::AbortHandle,
}

impl Drop for InProcessDaemon {
    fn drop(&mut self) {
        self.poll_task.abort();
    }
}

impl InProcessDaemon {
    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because a background poll task is spawned that
    /// holds a reference. The poll loop checks every 100ms for new refresh
    /// snapshots and broadcasts `DaemonEvent::Snapshot` for each change.
    pub async fn new(repo_paths: Vec<PathBuf>) -> Arc<Self> {
        let (event_tx, _) = broadcast::channel(256);
        let mut repos = HashMap::new();
        let mut order = Vec::new();

        for path in repo_paths {
            if repos.contains_key(&path) {
                continue;
            }
            let (registry, repo_slug) = crate::providers::discovery::detect_providers(&path).await;
            let mut model = RepoModel::new(path.clone(), registry, repo_slug);
            model.data.loading = true;
            repos.insert(
                path.clone(),
                RepoState {
                    model,
                    seq: 0,
                    last_snapshot: Arc::new(RefreshSnapshot::default()),
                },
            );
            order.push(path);
        }

        // Spawn poll loop first to get the AbortHandle, using a
        // placeholder Arc that we'll replace via the channel.
        let (arc_tx, arc_rx) = tokio::sync::oneshot::channel::<Arc<InProcessDaemon>>();
        let poll_task = tokio::spawn(async move {
            let d = arc_rx.await.expect("daemon Arc sender dropped");
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                d.poll_snapshots().await;
            }
        })
        .abort_handle();

        let daemon = Arc::new(Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
            poll_task,
        });

        let _ = arc_tx.send(daemon.clone());

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

            // Update the model's DataStore with the new provider data
            state.model.data.providers = Arc::clone(&snapshot.providers);
            state.model.data.correlation_groups = snapshot.correlation_groups.clone();
            state.model.data.provider_health = snapshot.provider_health.clone();
            state.model.data.loading = false;

            // Handle issues_disabled — tell the background task to stop querying,
            // and suppress from provider health display
            let issues_disabled = snapshot
                .errors
                .iter()
                .any(|e| e.category == "issues" && e.message.contains("has disabled issues"));
            if issues_disabled {
                state.model.data.provider_health.remove("issue_tracker");
                handle.skip_issues.store(true, Ordering::Relaxed);
            }

            // Increment sequence and store snapshot
            state.seq += 1;
            state.last_snapshot = snapshot.clone();

            // Build and broadcast proto snapshot
            let proto_snapshot = snapshot_to_proto(path, state.seq, &snapshot);
            // Ignore send errors (no receivers is fine)
            let _ = self
                .event_tx
                .send(DaemonEvent::Snapshot(Box::new(proto_snapshot)));
        }
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
        let snapshot = snapshot_to_proto(repo, state.seq, &state.last_snapshot);
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
        // Extract the data we need under a read lock, then drop it before the async work
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

        let result = executor::execute(command, &repo_root, &registry, &providers_data).await;

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
        let (registry, repo_slug) = crate::providers::discovery::detect_providers(&path).await;
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
                },
            );
            order.push(path.clone());
        }

        // Persist to config
        config::save_repo(&path);
        let order = self.repo_order.read().await;
        config::save_tab_order(&order);

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
        config::remove_repo(&path);
        let order = self.repo_order.read().await;
        config::save_tab_order(&order);

        info!("removed repo {}", path.display());
        let _ = self.event_tx.send(DaemonEvent::RepoRemoved { path });

        Ok(())
    }
}
