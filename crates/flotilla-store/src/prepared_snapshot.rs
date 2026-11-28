use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::*;

use crate::ResourceBackend;

#[derive(Debug, Clone)]
pub struct PreparedSnapshotGarbageCollector {
    backend: ResourceBackend,
    namespace: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreparedSnapshotGcResult {
    pub workflows_deleted: usize,
    pub placements_deleted: usize,
}

impl PreparedSnapshotGarbageCollector {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>) -> Self {
        Self { backend, namespace: namespace.into() }
    }

    /// Release `releasing`'s own prepared snapshots, workflow and placement, that no other convoy
    /// references. A snapshot is its convoy's, by its label, to release: admission writes it before
    /// it admits the convoy, so no other convoy's release may judge it unreferenced. One with no
    /// owner's label is never released here.
    ///
    /// The releasing convoy may still be listed: a refused admission's launch or an ended convoy
    /// retains no claim on its snapshots.
    pub async fn collect(&self, releasing: &str) -> Result<PreparedSnapshotGcResult, ResourceError> {
        let convoys = self.backend.including_replicas::<Convoy>(&self.namespace).list_sources().await?;
        let mut workflow_refs = BTreeSet::new();
        let mut placement_refs = BTreeSet::new();
        for source in convoys.items {
            if matches!(source.provenance, ResourceProvenance::Local) && source.object.metadata.name == releasing {
                continue;
            }
            let convoy = source.object;
            workflow_refs.insert(convoy.spec.workflow_ref);
            if let Some(snapshot) = convoy.metadata.annotations.get(WORKFLOW_SNAPSHOT_ANNOTATION) {
                workflow_refs.insert(snapshot.clone());
            }
            if let Some(policy) = convoy.spec.placement_policy {
                placement_refs.insert(policy);
            }
            if let Some(snapshot) = convoy.metadata.annotations.get(PLACEMENT_SNAPSHOT_ANNOTATION) {
                placement_refs.insert(snapshot.clone());
            }
            if let Some(encoded) = convoy.metadata.annotations.get(crate::VESSEL_PLACEMENTS_ANNOTATION) {
                if let Ok(pins) = serde_json::from_str::<BTreeMap<String, crate::VesselPlacementPin>>(encoded) {
                    placement_refs.extend(pins.into_values().map(|pin| pin.kind_ref));
                }
            }
        }
        Ok(PreparedSnapshotGcResult {
            workflows_deleted: self.release::<WorkflowTemplate>(WORKFLOW_SNAPSHOT_KIND, &workflow_refs, releasing).await?,
            placements_deleted: self.release::<FulfilmentKind>(PLACEMENT_SNAPSHOT_KIND, &placement_refs, releasing).await?,
        })
    }

    /// Deletes `releasing`'s prepared snapshots of `kind`, stored as `R`, that are not `referenced`.
    async fn release<R: Resource>(&self, kind: &str, referenced: &BTreeSet<String>, releasing: &str) -> Result<usize, ResourceError> {
        let snapshots = self.backend.clone().using::<R>(&self.namespace);
        let mut deleted = 0;
        for snapshot in snapshots.list().await?.items {
            if is_prepared_snapshot(&snapshot.metadata.labels, kind)
                && snapshot.metadata.labels.get(CONVOY_LABEL).is_some_and(|owner| owner == releasing)
                && !referenced.contains(&snapshot.metadata.name)
            {
                match snapshots.delete(&snapshot.metadata.name).await {
                    Ok(()) => deleted += 1,
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(deleted)
    }
}
