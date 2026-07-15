use std::collections::HashMap;

use async_trait::async_trait;
use flotilla_protocol::{
    commands::CommandValue, Command, DaemonEvent, QueryCursor, RepoInfo, RepoSelector, RepoSnapshot, StatusResponse, StreamKey,
    TopologyResponse,
};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Lifetime token for an in-process query subscriber. Dropping it tears the
/// subscriber out of the registry; socket implementations return a no-op
/// token because connection teardown owns that lifecycle.
pub struct QuerySubscription {
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl QuerySubscription {
    pub fn noop() -> Self {
        Self { cleanup: None }
    }

    pub(crate) fn new(cleanup: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self { cleanup: Some(Box::new(cleanup)) }
    }
}

impl Drop for QuerySubscription {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

/// The boundary between daemon and client.
/// Both InProcessDaemon and SocketDaemon implement this.
#[async_trait]
pub trait DaemonHandle: Send + Sync {
    /// Subscribe to daemon events (snapshots, repo changes).
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent>;

    fn query_subscription(&self, _subscriber_id: Uuid) -> QuerySubscription {
        QuerySubscription::noop()
    }

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

    /// Subscribe to named query result sets, replacing any previous
    /// subscription. Returns a full `ResultSet` event for each query whose
    /// cursor is absent or stale; subsequent updates arrive as `ResultSet`/
    /// `ResultDelta` events on the event stream.
    ///
    /// Delivery restriction is a transport concern: `SocketDaemon`
    /// connections only receive events for subscribed queries, while
    /// `InProcessDaemon`'s shared broadcast may over-deliver.
    async fn subscribe_queries(&self, subscriber_id: Uuid, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String>;

    /// Remove a subscriber and tear down demand-backed materializations that
    /// no longer have an owner. Socket transports call this on disconnect;
    /// in-process consumers call it when their explicit session ends.
    async fn unsubscribe_queries(&self, _subscriber_id: Uuid) {}

    /// Request the next page for a live demand-backed query. Consumers keep
    /// one result set and observe the extension on the event stream.
    async fn fetch_more(&self, _query: &flotilla_protocol::QueryId) -> Result<(), String> {
        Err("fetch-more is unsupported by this daemon".to_string())
    }

    /// Execute a query command synchronously. Returns the result directly
    /// without broadcasting. Only valid for commands where `action.is_query()`.
    /// The `session_id` ties cursor ownership to the calling client session.
    async fn execute_query(&self, command: Command, session_id: Uuid) -> Result<CommandValue, String>;

    /// High-level status: repos, health, counts.
    async fn get_status(&self) -> Result<StatusResponse, String>;

    /// Multi-host routing view.
    async fn get_topology(&self) -> Result<TopologyResponse, String>;
}
