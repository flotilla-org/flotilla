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

    /// Delete prepared snapshots not referenced by any remaining convoy.
    ///
    /// `excluding_convoy` is used by the convoy finalizer: the deleting convoy
    /// still exists until its finalizer returns, but no longer retains a claim.
    pub async fn collect(&self, excluding_convoy: Option<&str>) -> Result<PreparedSnapshotGcResult, ResourceError> {
        let convoys = self.backend.including_replicas::<Convoy>(&self.namespace).list_sources().await?;
        let mut workflow_refs = BTreeSet::new();
        let mut placement_refs = BTreeSet::new();
        for source in convoys.items {
            if matches!(source.provenance, ResourceProvenance::Local)
                && excluding_convoy.is_some_and(|excluded| source.object.metadata.name == excluded)
            {
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
                    placement_refs.extend(pins.into_values().map(|pin| pin.policy_ref));
                }
            }
        }

        let workflows = self.backend.clone().using::<WorkflowTemplate>(&self.namespace);
        let mut result = PreparedSnapshotGcResult::default();
        for workflow in workflows.list().await?.items {
            if is_prepared_snapshot(&workflow.metadata.name, &workflow.metadata.labels, WORKFLOW_SNAPSHOT_KIND)
                && !workflow_refs.contains(&workflow.metadata.name)
            {
                match workflows.delete(&workflow.metadata.name).await {
                    Ok(()) => result.workflows_deleted += 1,
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }

        let placements = self.backend.clone().using::<PlacementPolicy>(&self.namespace);
        for placement in placements.list().await?.items {
            if is_prepared_snapshot(&placement.metadata.name, &placement.metadata.labels, PLACEMENT_SNAPSHOT_KIND)
                && !placement_refs.contains(&placement.metadata.name)
            {
                match placements.delete(&placement.metadata.name).await {
                    Ok(()) => result.placements_deleted += 1,
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(result)
    }
}
