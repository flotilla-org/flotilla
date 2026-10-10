use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};

use crate::{Resource, ResourceObject, ResourceTombstone, WatchEvent};

/// Cross-root behavior for a resource kind.
///
/// Replication is deliberately opt-in at the kind declaration. Additional
/// classes will grow their own read semantics in later overlay slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationClass {
    None,
    Definitions,
    /// Durable facts materialized independently at each root and read as the
    /// natural-key union of local and replica sources.
    ConvergentFacts,
    HomeBoundRuntime,
    /// Demand-scoped observed state. Retain only the latest watch handoff
    /// event so lagging peers relist current state instead of replaying history.
    Observations,
}

impl ReplicationClass {
    pub fn event_retention(self, configured: usize) -> usize {
        if self == Self::Observations {
            1
        } else {
            configured
        }
    }
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
    DeletedByName { tombstone: ResourceTombstone, provenance: ResourceProvenance },
}

impl<T: Resource> ReadWatchEvent<T> {
    pub fn local(event: WatchEvent<T>) -> Self {
        match event {
            WatchEvent::Added(object) => Self::Added(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            WatchEvent::Modified(object) => Self::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            WatchEvent::Deleted(object) => Self::Deleted(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            WatchEvent::DeletedByName(tombstone) => Self::DeletedByName { tombstone, provenance: ResourceProvenance::Local },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaCursor {
    pub resource_version: String,
    pub generation: Option<String>,
}
