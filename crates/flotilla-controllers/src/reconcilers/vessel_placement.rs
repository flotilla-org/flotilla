use std::collections::{btree_map::Entry, BTreeMap, HashMap};

use flotilla_resources::{
    Convoy, InputMeta, LifecycleAuthority, ReadWatchEvent, Resource, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance,
    Vessel, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
};
use futures::StreamExt;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VesselPlacementSync {
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Projects remotely-authored Vessels placed on this host into this host's
/// local resource log. The ordinary Vessel controller then owns all local
/// actuation and records its status on the projected object; replication
/// carries that actuator-authored fact back to the admitting store.
#[derive(Clone)]
pub struct VesselPlacementProjector {
    backend: ResourceBackend,
    namespace: String,
    local_host_ref: String,
}

impl VesselPlacementProjector {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>, local_host_ref: impl Into<String>) -> Self {
        Self { backend, namespace: namespace.into(), local_host_ref: local_host_ref.into() }
    }

    pub async fn run(&self) -> Result<(), ResourceError> {
        let mut convoy_watch = self.backend.including_replicas::<Convoy>(&self.namespace).watch().await?;
        let mut vessel_watch = self.backend.including_replicas::<Vessel>(&self.namespace).watch().await?;
        self.sync_once().await?;

        loop {
            let replica_changed = tokio::select! {
                event = convoy_watch.next() => replica_changed(event, Convoy::API_PATHS.kind)?,
                event = vessel_watch.next() => replica_changed(event, Vessel::API_PATHS.kind)?,
            };
            if replica_changed {
                self.sync_once().await?;
            }
        }
    }

    pub async fn sync_once(&self) -> Result<VesselPlacementSync, ResourceError> {
        let convoy_sources = self.backend.including_replicas::<Convoy>(&self.namespace).list().await?;
        let convoys_by_origin = convoy_sources
            .items
            .into_iter()
            .filter_map(|source| match source.provenance {
                ResourceProvenance::Replica { origin_root, .. } => {
                    Some(((origin_root.to_string(), source.object.metadata.name.clone()), source.object))
                }
                ResourceProvenance::Local => None,
            })
            .collect::<HashMap<_, _>>();

        let vessel_sources = self.backend.including_replicas::<Vessel>(&self.namespace).list().await?;
        let mut desired = BTreeMap::<String, (String, ResourceObject<Vessel>)>::new();
        for source in vessel_sources.items {
            let ResourceProvenance::Replica { origin_root, .. } = source.provenance else {
                continue;
            };
            if source.object.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION)
                || source.object.metadata.deletion_timestamp.is_some()
            {
                continue;
            }
            let origin_root = origin_root.to_string();
            let Some(convoy) = convoys_by_origin.get(&(origin_root.clone(), source.object.spec.convoy_ref.clone())) else {
                continue;
            };
            let placed_here = convoy
                .status
                .as_ref()
                .and_then(|status| status.placement_decision.as_ref())
                .is_some_and(|decision| decision.target_host.reference == self.local_host_ref);
            if !placed_here {
                continue;
            }

            match desired.entry(source.object.metadata.name.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert((origin_root, source.object));
                }
                Entry::Occupied(entry) => {
                    warn!(
                        vessel = %entry.key(),
                        first_origin = %entry.get().0,
                        second_origin = %origin_root,
                        host_ref = %self.local_host_ref,
                        "multiple admitting stores projected the same Vessel name; leaving the existing local actuator untouched"
                    );
                }
            }
        }

        let vessels = self.backend.using::<Vessel>(&self.namespace);
        let existing = vessels.list().await?;
        let mut local_actuators = existing
            .items
            .into_iter()
            .filter(|vessel| {
                vessel.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_some_and(|host_ref| host_ref == &self.local_host_ref)
            })
            .map(|vessel| (vessel.metadata.name.clone(), vessel))
            .collect::<BTreeMap<_, _>>();

        let mut result = VesselPlacementSync::default();
        for (name, (origin_root, source)) in desired {
            let current = match vessels.get(&name).await {
                Ok(current) => Some(current),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            };
            if let Some(current) = current.as_ref() {
                let projected_origin = current.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION);
                if projected_origin.is_none_or(|projected_origin| projected_origin != &origin_root) {
                    warn!(
                        vessel = %name,
                        source_origin = %origin_root,
                        host_ref = %self.local_host_ref,
                        "cannot project remotely placed Vessel because the local store already owns that name"
                    );
                    continue;
                }
            }

            let mut labels = source.metadata.labels.clone();
            labels.insert(flotilla_resources::AUTHORITY_LABEL.to_string(), LifecycleAuthority::Managed.as_label_value().to_string());
            let mut annotations = source.metadata.annotations.clone();
            annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.to_string(), self.local_host_ref.clone());
            annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), origin_root);
            let mut meta = InputMeta::builder()
                .name(name.clone())
                .labels(labels)
                .annotations(annotations)
                .owner_references(source.metadata.owner_references.clone())
                .build();
            if let Some(current) = current {
                local_actuators.remove(&name);
                if current.spec == source.spec
                    && current.metadata.labels == meta.labels
                    && current.metadata.annotations == meta.annotations
                    && current.metadata.owner_references == meta.owner_references
                {
                    continue;
                }
                meta.finalizers = current.metadata.finalizers.clone();
                meta.deletion_timestamp = current.metadata.deletion_timestamp;
                vessels.update(&meta, &current.metadata.resource_version, &source.spec).await?;
                result.updated += 1;
            } else {
                vessels.create(&meta, &source.spec).await?;
                result.created += 1;
            }
        }

        for (name, actuator) in local_actuators {
            if actuator.metadata.deletion_timestamp.is_some() {
                continue;
            }
            vessels.delete(&name).await?;
            result.deleted += 1;
        }

        debug!(
            host_ref = %self.local_host_ref,
            created = result.created,
            updated = result.updated,
            deleted = result.deleted,
            "synchronized placed Vessel actuators"
        );
        Ok(result)
    }
}

fn replica_changed<T: Resource>(event: Option<Result<ReadWatchEvent<T>, ResourceError>>, kind: &str) -> Result<bool, ResourceError> {
    let event = event.ok_or_else(|| ResourceError::invalid(format!("{kind} replica watch ended")))?;
    let provenance = match event? {
        ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) | ReadWatchEvent::Deleted(source) => source.provenance,
        ReadWatchEvent::DeletedByName { provenance, .. } => provenance,
    };
    Ok(matches!(provenance, ResourceProvenance::Replica { .. }))
}
