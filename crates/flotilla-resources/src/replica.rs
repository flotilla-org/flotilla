use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};

use crate::{Resource, ResourceObject, ResourceTombstone, WatchEvent};

/// Compare whole observations without cloning them. A local copy wins only
/// after the resource's observation timestamp and authority tie exactly.
pub fn compare_replica_reads<T: Resource>(left: (&ResourceObject<T>, bool), right: (&ResourceObject<T>, bool)) -> std::cmp::Ordering {
    (T::replica_read_rank(left.0), left.1).cmp(&(T::replica_read_rank(right.0), right.1))
}

/// Select one whole copy while preserving its envelope or caller-owned payload.
/// The projection exposes the resource and whether its provenance is local.
/// On an exact tie, keep the last item (canonical source order is the caller's).
pub fn pick_replica_read<T: Resource, I>(
    items: impl IntoIterator<Item = I>,
    project: impl Fn(&I) -> (&ResourceObject<T>, bool),
) -> Option<I> {
    items.into_iter().max_by(|left, right| compare_replica_reads(project(left), project(right)))
}

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
    /// The event history a kind of this class keeps unless it declares its own.
    pub const fn event_history(self) -> crate::EventHistory {
        match self {
            Self::Observations => crate::EventHistory::LatestOnly,
            Self::None | Self::Definitions | Self::ConvergentFacts | Self::HomeBoundRuntime => crate::EventHistory::Configured,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The selector's documented full-tie contract keeps the last supplied
    // payload, and an empty inventory has no winner.
    #[hegel::test]
    fn pick_replica_read_full_tie_keeps_last(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Vary empty/single/multiple candidates and both provenance choices.
        // All candidates have identical timestamp, authority, and locality rank.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let local = tc.draw(gs::booleans());
        let object = ResourceObject::<crate::Issue>::from_k8s_object(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "flotilla.work/v1", "kind": "Issue",
                "metadata": {"namespace": "test", "name": "issue", "resourceVersion": "1",
                    "creationTimestamp": "2026-01-01T00:00:00Z"},
                "spec": {"service": "github.com", "scope": "test/repo", "number": 1, "observing_authority": "observer"}
            }))
            .unwrap(),
        )
        .unwrap();
        let selected = pick_replica_read((0..count).map(|index| (&object, local, index)), |(object, local, _)| (*object, *local));
        assert_eq!(selected.map(|(_, _, index)| index), count.checked_sub(1));
    }
}
