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
