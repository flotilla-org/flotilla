use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};

use crate::{Resource, ResourceObject, WatchEvent};

pub(crate) const ORIGIN_ROOT_ANNOTATION: &str = "flotilla.work/origin-root";
pub(crate) const LAST_SYNCED_AT_ANNOTATION: &str = "flotilla.work/last-synced-at";

/// Cross-root behavior for a resource kind.
///
/// Replication is deliberately opt-in at the kind declaration. Additional
/// classes will grow their own read semantics in later overlay slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationClass {
    None,
    HomeBoundRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ResourceProvenance {
    Local,
    Replica { origin_root: NodeId, last_synced_at: DateTime<Utc> },
}

#[derive(Debug, Clone)]
pub struct ReadResourceObject<T: Resource> {
    pub object: ResourceObject<T>,
    pub provenance: ResourceProvenance,
}

#[derive(Debug, Clone)]
pub struct ReadResourceList<T: Resource> {
    pub items: Vec<ReadResourceObject<T>>,
}

#[derive(Debug, Clone)]
pub enum ReadWatchEvent<T: Resource> {
    Added(ReadResourceObject<T>),
    Modified(ReadResourceObject<T>),
    Deleted(ReadResourceObject<T>),
}

impl<T: Resource> ReadWatchEvent<T> {
    pub(crate) fn local(event: WatchEvent<T>) -> Self {
        match event {
            WatchEvent::Added(object) => Self::Added(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            WatchEvent::Modified(object) => Self::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            WatchEvent::Deleted(object) => Self::Deleted(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaCursor {
    pub resource_version: String,
    pub generation: Option<String>,
}

#[derive(Debug, Clone, bon::Builder)]
pub(crate) struct StoredReplicaEvent {
    pub origin_root: NodeId,
    pub synced_at: DateTime<Utc>,
    pub kind: StoredReplicaEventKind,
    pub object: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum StoredReplicaEventKind {
    Added,
    Modified,
    Deleted,
}
