use std::collections::BTreeSet;

use crate::{
    Convoy, InputMeta, PlacementPolicy, ResourceBackend, ResourceError, WorkflowTemplate, PLACEMENT_SNAPSHOT_ANNOTATION,
    PREPARED_SNAPSHOT_PENDING_ANNOTATION, WORKFLOW_SNAPSHOT_ANNOTATION,
};

pub const PREPARED_SNAPSHOT_LABEL: &str = "flotilla.work/prepared-snapshot";
pub const WORKFLOW_SNAPSHOT_KIND: &str = "workflow";
pub const PLACEMENT_SNAPSHOT_KIND: &str = "placement";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreparedSnapshotRecoveryResult {
    pub completed: usize,
    pub discarded: usize,
}

impl PreparedSnapshotGarbageCollector {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>) -> Self {
        Self { backend, namespace: namespace.into() }
    }

    /// Recover claims left pending by a daemon crash during prepared admission.
    ///
    /// Complete claims whose snapshots were fully materialized; discard claims
    /// that cannot be completed because their shipped specs died with the
    /// interrupted command.
    pub async fn recover_pending_claims(&self) -> Result<PreparedSnapshotRecoveryResult, ResourceError> {
        let convoys = self.backend.clone().using::<Convoy>(&self.namespace);
        let workflows = self.backend.clone().using::<WorkflowTemplate>(&self.namespace);
        let placements = self.backend.clone().using::<PlacementPolicy>(&self.namespace);
        let mut result = PreparedSnapshotRecoveryResult::default();
        for convoy in convoys.list().await?.items {
            if !convoy.metadata.annotations.contains_key(PREPARED_SNAPSHOT_PENDING_ANNOTATION) {
                continue;
            }
            let workflow_ready = match convoy.metadata.annotations.get(WORKFLOW_SNAPSHOT_ANNOTATION) {
                Some(name) => resource_exists(&workflows, name).await?,
                None => false,
            };
            let placement_ready = match convoy.metadata.annotations.get(PLACEMENT_SNAPSHOT_ANNOTATION) {
                Some(name) => resource_exists(&placements, name).await?,
                None => convoy.spec.placement_policy.is_none(),
            };
            if workflow_ready && placement_ready {
                loop {
                    let current = convoys.get(&convoy.metadata.name).await?;
                    let mut meta = InputMeta::from(&current.metadata);
                    meta.annotations.remove(PREPARED_SNAPSHOT_PENDING_ANNOTATION);
                    match convoys.update(&meta, &current.metadata.resource_version, &current.spec).await {
                        Ok(_) => {
                            result.completed += 1;
                            break;
                        }
                        Err(ResourceError::Conflict { .. }) => continue,
                        Err(error) => return Err(error),
                    }
                }
            } else {
                convoys.delete(&convoy.metadata.name).await?;
                result.discarded += 1;
            }
        }
        self.collect(None).await?;
        Ok(result)
    }

    /// Delete prepared snapshots not referenced by any remaining convoy.
    ///
    /// `excluding_convoy` is used by the convoy finalizer: the deleting convoy
    /// still exists until its finalizer returns, but no longer retains a claim.
    pub async fn collect(&self, excluding_convoy: Option<&str>) -> Result<PreparedSnapshotGcResult, ResourceError> {
        let convoys = self.backend.clone().using::<Convoy>(&self.namespace).list().await?;
        let mut workflow_refs = BTreeSet::new();
        let mut placement_refs = BTreeSet::new();
        for convoy in convoys.items {
            if excluding_convoy.is_some_and(|excluded| convoy.metadata.name == excluded) {
                continue;
            }
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

async fn resource_exists<T: crate::Resource>(resolver: &crate::TypedResolver<T>, name: &str) -> Result<bool, ResourceError> {
    match resolver.get(name).await {
        Ok(_) => Ok(true),
        Err(ResourceError::NotFound { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_prepared_snapshot(name: &str, labels: &std::collections::BTreeMap<String, String>, kind: &str) -> bool {
    labels.get(PREPARED_SNAPSHOT_LABEL).is_some_and(|value| value == kind)
        || has_content_hash_suffix(name, &format!("{kind}-snapshot-"))
        || has_content_hash_suffix(name, &format!("-remote-{kind}-"))
}

fn has_content_hash_suffix(name: &str, marker: &str) -> bool {
    name.rsplit_once(marker).is_some_and(|(_, suffix)| suffix.len() == 12 && suffix.chars().all(|character| character.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_current_and_legacy_snapshot_names_without_matching_human_templates() {
        let labels = Default::default();
        assert!(is_prepared_snapshot("workflow-snapshot-012345abcdef", &labels, WORKFLOW_SNAPSHOT_KIND));
        assert!(is_prepared_snapshot("old-convoy-remote-workflow-012345abcdef", &labels, WORKFLOW_SNAPSHOT_KIND));
        assert!(!is_prepared_snapshot("workflow-snapshot-custom", &labels, WORKFLOW_SNAPSHOT_KIND));
        assert!(!is_prepared_snapshot("my-workflow", &labels, WORKFLOW_SNAPSHOT_KIND));
    }
}
