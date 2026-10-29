/// Cap replica transactions so catch-up yields the writer between batches.
pub const REPLICA_BATCH_SIZE: usize = 64;

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;

pub use flotilla_resources::ORIGIN_ROOT_ANNOTATION;
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

/// A replica body's origin resource version, when it is numeric.
pub fn replica_version(body: &serde_json::Value) -> Option<u64> {
    body.get("metadata")?.get("resourceVersion")?.as_str()?.parse().ok()
}

/// Whether a held replica body is newer than an arriving one by its origin's
/// resource version. An origin's versions only grow within a generation, so
/// the newer body wins whatever the arrival order or the receiver's clock
/// (#4137). Bodies without a numeric version fall back to sync-time ordering.
pub fn replica_body_is_newer(held: &serde_json::Value, arriving: &serde_json::Value) -> bool {
    replica_version_is_newer(replica_version(held), arriving)
}

/// [`replica_body_is_newer`] for a held version already read from storage.
pub fn replica_version_is_newer(held: Option<u64>, arriving: &serde_json::Value) -> bool {
    match (held, replica_version(arriving)) {
        (Some(held), Some(arriving)) => held > arriving,
        (None, None) => false,
        (held, arriving) => {
            tracing::warn!(?held, ?arriving, "replica resource version is not numeric on one side; ordering by sync time");
            false
        }
    }
}
