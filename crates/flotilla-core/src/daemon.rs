use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use flotilla_protocol::{
    Command, DaemonEvent, RepoDetailResponse, RepoInfo, RepoProvidersResponse, RepoWorkResponse, Snapshot, StatusResponse,
};
use tokio::sync::broadcast;

/// The boundary between daemon and client.
/// Both InProcessDaemon and SocketDaemon implement this.
#[async_trait]
pub trait DaemonHandle: Send + Sync {
    /// Subscribe to daemon events (snapshots, repo changes).
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent>;

    /// Get full current state for a repo.
    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String>;

    /// List all tracked repos.
    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String>;

    /// Execute a command. Returns a command ID; the result arrives via
    /// CommandStarted/CommandFinished events.
    async fn execute(&self, repo: &Path, command: Command) -> Result<u64, String>;

    /// Cancel a running command. The command will finish with
    /// `CommandResult::Cancelled` once cancellation takes effect.
    async fn cancel(&self, command_id: u64) -> Result<(), String>;

    /// Trigger an immediate refresh for a repo.
    async fn refresh(&self, repo: &Path) -> Result<(), String>;

    /// Add a repo to tracking.
    async fn add_repo(&self, path: &Path) -> Result<(), String>;

    /// Remove a repo from tracking.
    async fn remove_repo(&self, path: &Path) -> Result<(), String>;

    /// Get replay events for repos based on last-seen sequence numbers.
    ///
    /// For each repo in `last_seen`, checks the delta log:
    /// - If replayable: returns `SnapshotDelta` events for each missing entry
    /// - If not replayable (seq too old or unknown): returns `SnapshotFull`
    ///
    /// Repos not in `last_seen` get a `SnapshotFull`.
    async fn replay_since(&self, last_seen: &HashMap<PathBuf, u64>) -> Result<Vec<DaemonEvent>, String>;

    /// High-level status: repos, health, counts.
    async fn get_status(&self) -> Result<StatusResponse, String>;

    /// Repo detail: work items, provider health, errors.
    async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String>;

    /// Repo discovery: host/repo assertions, providers, unmet requirements.
    async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String>;

    /// Repo work items.
    async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String>;
}
