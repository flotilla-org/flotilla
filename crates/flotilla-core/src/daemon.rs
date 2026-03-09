use std::path::Path;

use async_trait::async_trait;
use tokio::sync::broadcast;

use flotilla_protocol::{Command, DaemonEvent, RepoInfo, Snapshot};

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

    /// Trigger an immediate refresh for a repo.
    async fn refresh(&self, repo: &Path) -> Result<(), String>;

    /// Add a repo to tracking.
    async fn add_repo(&self, path: &Path) -> Result<(), String>;

    /// Remove a repo from tracking.
    async fn remove_repo(&self, path: &Path) -> Result<(), String>;
}
