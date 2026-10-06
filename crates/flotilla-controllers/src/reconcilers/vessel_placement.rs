use std::{
    collections::{btree_map::Entry, BTreeMap, HashMap},
    time::Duration,
};

use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{
    Convoy, InputMeta, LifecycleAuthority, OwnerReference, ReadResourceObject, ReadWatchEvent, Resource, ResourceBackend, ResourceError,
    ResourceObject, ResourceProvenance, Vessel, VesselSpec, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION,
    VESSEL_PLACEMENTS_ANNOTATION,
};
use futures::StreamExt;
use tracing::{debug, info, warn};

const PLACEMENT_COALESCE_INTERVAL: Duration = Duration::from_millis(25);
const PLACEMENT_RESYNC_INTERVAL: Duration = Duration::from_secs(60);

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
    local_host_ref: CanonicalHostId,
    additional_host_refs: std::collections::BTreeSet<CanonicalHostId>,
}

impl VesselPlacementProjector {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>, local_host_ref: CanonicalHostId) -> Self {
        Self { backend, namespace: namespace.into(), local_host_ref, additional_host_refs: Default::default() }
    }

    pub fn with_additional_host_refs(mut self, host_refs: impl IntoIterator<Item = CanonicalHostId>) -> Self {
        self.additional_host_refs = host_refs.into_iter().collect();
        self
    }

    pub async fn run(&self) -> Result<(), ResourceError> {
        let convoys = self.backend.including_replicas::<Convoy>(&self.namespace);
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace);
        let mut convoy_watch = convoys.watch().await?;
        let mut vessel_watch = vessels.watch().await?;
        // Watch before listing so changes during bootstrap remain queued.
        let mut convoy_inputs = ReplicaInputs::default();
        for source in convoys.list().await?.items {
            convoy_inputs.update(source, convoy_input);
        }
        let mut vessel_inputs = ReplicaInputs::default();
        for source in vessels.list().await?.items {
            vessel_inputs.update(source, vessel_input);
        }
        self.sync_once().await?;
        let mut resync = tokio::time::interval_at(tokio::time::Instant::now() + PLACEMENT_RESYNC_INTERVAL, PLACEMENT_RESYNC_INTERVAL);
        resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut deadline = None;
        loop {
            let flush = async move {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::pin!(flush);
            let changed = tokio::select! {
                event = convoy_watch.next() => convoy_inputs.event(event, Convoy::API_PATHS.kind, convoy_input)?,
                event = vessel_watch.next() => vessel_inputs.event(event, Vessel::API_PATHS.kind, vessel_input)?,
                _ = resync.tick() => {
                    self.sync_once().await?;
                    deadline = None;
                    false
                }
                () = &mut flush => {
                    self.sync_once().await?;
                    deadline = None;
                    false
                }
            };
            if changed {
                // A fixed deadline coalesces bursts without starving continuous changes.
                deadline.get_or_insert_with(|| tokio::time::Instant::now() + PLACEMENT_COALESCE_INTERVAL);
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
        let mut desired = BTreeMap::<String, (String, CanonicalHostId, ResourceObject<Vessel>)>::new();
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
            let target_host = flotilla_resources::vessel_placement_pin(convoy, &source.object.spec.vessel_name)
                .map(|pin| pin.decision.target_host.reference)
                .or_else(|| {
                    convoy
                        .status
                        .as_ref()
                        .and_then(|status| status.placement_decision.as_ref())
                        .map(|decision| decision.target_host.reference.clone())
                });
            let Some(target_host) = target_host.filter(|host| host == &self.local_host_ref || self.additional_host_refs.contains(host))
            else {
                continue;
            };

            match desired.entry(source.object.metadata.name.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert((origin_root, target_host, source.object));
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
                vessel.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_some_and(|host_ref| {
                    host_ref == self.local_host_ref.as_str() || self.additional_host_refs.contains(&CanonicalHostId::resolved(host_ref))
                })
            })
            .map(|vessel| (vessel.metadata.name.clone(), vessel))
            .collect::<BTreeMap<_, _>>();

        let mut result = VesselPlacementSync::default();
        for (name, (origin_root, target_host, source)) in desired {
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
            annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.to_string(), target_host.to_string());
            let source_root = origin_root.clone();
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
                info!(convoy = %source.spec.convoy_ref, vessel = %name, %source_root, authoring_path = "placed_vessel_projector", "created placed Vessel actuator");
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
            placement_sync_completed = true,
            host_ref = %self.local_host_ref,
            created = result.created,
            updated = result.updated,
            deleted = result.deleted,
            "synchronized placed Vessel actuators"
        );
        Ok(result)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ConvoyInput {
    placements: Option<String>,
    target_host: Option<CanonicalHostId>,
}

fn convoy_input(object: &ResourceObject<Convoy>) -> Option<ConvoyInput> {
    Some(ConvoyInput {
        placements: object.metadata.annotations.get(VESSEL_PLACEMENTS_ANNOTATION).cloned(),
        target_host: object
            .status
            .as_ref()
            .and_then(|status| status.placement_decision.as_ref())
            .map(|decision| decision.target_host.reference.clone()),
    })
}

#[derive(Debug, PartialEq, Eq, bon::Builder)]
struct VesselInput {
    spec: VesselSpec,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    owners: Vec<OwnerReference>,
}

fn vessel_input(object: &ResourceObject<Vessel>) -> Option<VesselInput> {
    if object.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION) || object.metadata.deletion_timestamp.is_some() {
        return None;
    }
    Some(
        VesselInput::builder()
            .spec(object.spec.clone())
            .labels(object.metadata.labels.clone())
            .annotations(object.metadata.annotations.clone())
            .owners(object.metadata.owner_references.clone())
            .build(),
    )
}

struct ReplicaInputs<I> {
    by_origin: HashMap<(String, String), I>,
}

impl<I> Default for ReplicaInputs<I> {
    fn default() -> Self {
        Self { by_origin: HashMap::new() }
    }
}

impl<I: PartialEq> ReplicaInputs<I> {
    // Inputs are recorded before reconciliation. Sync errors terminate run(); if
    // retries are added, failed inputs must remain dirty until reconciliation succeeds.
    fn update<T: Resource>(&mut self, source: ReadResourceObject<T>, input: impl Fn(&ResourceObject<T>) -> Option<I>) -> bool {
        let ResourceProvenance::Replica { origin_root, .. } = source.provenance else { return false };
        let key = (origin_root.to_string(), source.object.metadata.name.clone());
        match input(&source.object) {
            Some(value) if self.by_origin.get(&key) != Some(&value) => {
                self.by_origin.insert(key, value);
                true
            }
            Some(_) => false,
            None => self.by_origin.remove(&key).is_some(),
        }
    }

    fn event<T: Resource>(
        &mut self,
        event: Option<Result<ReadWatchEvent<T>, ResourceError>>,
        kind: &str,
        input: impl Fn(&ResourceObject<T>) -> Option<I>,
    ) -> Result<bool, ResourceError> {
        let event = event.ok_or_else(|| ResourceError::invalid(format!("{kind} replica watch ended")))??;
        match event {
            ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) => Ok(self.update(source, input)),
            ReadWatchEvent::Deleted(source) => Ok(match source.provenance {
                ResourceProvenance::Replica { origin_root, .. } => {
                    self.by_origin.remove(&(origin_root.to_string(), source.object.metadata.name)).is_some()
                }
                ResourceProvenance::Local => false,
            }),
            ReadWatchEvent::DeletedByName { tombstone, provenance } => Ok(match provenance {
                ResourceProvenance::Replica { origin_root, .. } => {
                    self.by_origin.remove(&(origin_root.to_string(), tombstone.name)).is_some()
                }
                ResourceProvenance::Local => false,
            }),
        }
    }
}
