/// Cap replica transactions so catch-up yields the writer between batches.
pub const REPLICA_BATCH_SIZE: usize = 64;

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;

pub const ORIGIN_ROOT_ANNOTATION: &str = "flotilla.work/origin-root";
pub const LAST_SYNCED_AT_ANNOTATION: &str = "flotilla.work/last-synced-at";
#[derive(Debug, Clone, bon::Builder)]
pub struct StoredReplicaEvent {
    pub origin_root: NodeId,
    pub synced_at: DateTime<Utc>,
    pub kind: StoredReplicaEventKind,
    pub object: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
pub enum StoredReplicaEventKind {
    Added,
    Modified,
    Deleted,
}

/// A retained replica deletion, ordered by its origin's sync timestamp rather
/// than an authoritative log position. Relay snapshots must include these
/// fences so late subscribers cannot keep or resurrect an older live record.
#[derive(Debug, Clone)]
pub struct ReplicaDeletion {
    pub origin_root: NodeId,
    pub name: String,
    pub last_synced_at: DateTime<Utc>,
}
