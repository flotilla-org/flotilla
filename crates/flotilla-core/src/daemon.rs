use std::collections::HashMap;

use async_trait::async_trait;
use flotilla_protocol::{
    commands::CommandValue, Command, DaemonEvent, RepoInfo, RepoSelector, RepoSnapshot, StatusResponse, StreamKey, TopologyResponse,
};
use tokio::sync::broadcast;
use uuid::Uuid;

/// The boundary between daemon and client.
/// Both InProcessDaemon and SocketDaemon implement this.
#[async_trait]
pub trait DaemonHandle: Send + Sync {
    /// Subscribe to daemon events (snapshots, repo changes).
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent>;

    /// Get full current state for a repo.
    ///
    /// Note: the `SocketDaemon` implementation currently requires a
    /// `RepoSelector::Path` because the wire format sends a raw path.
    /// `Query` and `Identity` selectors work with `InProcessDaemon`.
    async fn get_state(&self, repo: &RepoSelector) -> Result<RepoSnapshot, String>;

    /// List all tracked repos.
    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String>;

    /// Execute a command. Returns a command ID; the result arrives via
    /// CommandStarted/CommandFinished events.
    async fn execute(&self, command: Command) -> Result<u64, String>;

    /// Cancel a running command. The command will finish with
    /// `CommandValue::Cancelled` once cancellation takes effect.
    async fn cancel(&self, command_id: u64) -> Result<(), String>;

    /// Get replay events for repos based on last-seen sequence numbers.
    ///
    /// For each repo in `last_seen`, checks the delta log:
    /// - If replayable: returns `RepoDelta` events for each missing entry
    /// - If not replayable (seq too old or unknown): returns `RepoSnapshot`
    ///
    /// Repos not in `last_seen` get a `RepoSnapshot`.
    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String>;

    /// Execute a query command synchronously. Returns the result directly
    /// without broadcasting. Only valid for commands where `action.is_query()`.
    /// The `session_id` ties cursor ownership to the calling client session.
    async fn execute_query(&self, command: Command, session_id: Uuid) -> Result<CommandValue, String>;

    /// High-level status: repos, health, counts.
    async fn get_status(&self) -> Result<StatusResponse, String>;

    /// Multi-host routing view.
    async fn get_topology(&self) -> Result<TopologyResponse, String>;
}
