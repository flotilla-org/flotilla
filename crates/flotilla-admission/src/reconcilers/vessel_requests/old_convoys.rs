//! [`ConvoyWorks`] read from the old store's Convoy objects, local and replicated: the bridge until
//! the engine Convoy implements the port from its records (#3108), when this is deleted.

use std::collections::BTreeMap;

use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{Convoy, ReadResourceObject, ReadWatchEvent, ResourceObject, ResourceProvenance, WorkPhase};
use flotilla_store::ResourceBackend;
use futures::{stream::BoxStream, StreamExt};

use super::{ConvoyStanding, ConvoyTouched, ConvoyWorks, ConvoyWorksRead, WorkNeedingVessel};

/// The old store's convoys in one namespace, at the home `backend` serves.
#[derive(Clone)]
pub struct OldConvoyWorks {
    backend: ResourceBackend,
    namespace: String,
}

impl OldConvoyWorks {
    pub fn new(backend: ResourceBackend, namespace: impl Into<String>) -> Self {
        Self { backend, namespace: namespace.into() }
    }

    fn here(&self) -> String {
        self.backend.local_root().map(|root| root.as_str().to_owned()).unwrap_or_default()
    }

    /// Every convoy read here, by name and home.
    async fn convoys(&self) -> Result<BTreeMap<(String, String), ResourceObject<Convoy>>, String> {
        let here = self.here();
        let sources = self.backend.including_replicas::<Convoy>(&self.namespace).list().await.map_err(|error| error.to_string())?.items;
        Ok(sources.into_iter().map(|source| homed(source, &here)).collect())
    }

    /// Convoy `convoy` homed at `convoy_home`, read by its name.
    async fn read(&self, convoy: &str, convoy_home: &str) -> Result<Option<ResourceObject<Convoy>>, String> {
        let here = self.here();
        let copies = self.backend.including_replicas::<Convoy>(&self.namespace).get_all(convoy).await.map_err(|error| error.to_string())?;
        Ok(copies.items.into_iter().map(|source| homed(source, &here)).find(|((_, home), _)| home == convoy_home).map(|(_, convoy)| convoy))
    }
}

fn standing(convoy: &ResourceObject<Convoy>) -> ConvoyStanding {
    if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
        ConvoyStanding::Ended
    } else {
        ConvoyStanding::Live
    }
}

/// The works of `convoy`, named `name` and homed at `convoy_home`, that run on a vessel now.
fn works(name: &str, convoy_home: &str, convoy: &ResourceObject<Convoy>) -> Vec<WorkNeedingVessel> {
    let Some(status) = convoy.status.as_ref().filter(|status| !status.phase.is_terminal()) else { return Vec::new() };
    let Some(snapshot) = status.workflow_snapshot.as_ref() else { return Vec::new() };
    snapshot
        .vessels
        .iter()
        .filter(|requirement| status.work.get(&requirement.name).is_some_and(|work| needs_vessel(work.phase)))
        .filter_map(|requirement| {
            let host = placed_host(convoy, &requirement.name)?;
            Some(WorkNeedingVessel { convoy: name.to_owned(), convoy_home: convoy_home.to_owned(), work: requirement.name.clone(), host })
        })
        .collect()
}

fn home(provenance: &ResourceProvenance, here: &str) -> String {
    match provenance {
        ResourceProvenance::Local => here.to_owned(),
        ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
    }
}

fn homed(source: ReadResourceObject<Convoy>, here: &str) -> ((String, String), ResourceObject<Convoy>) {
    ((source.object.metadata.name.clone(), home(&source.provenance, here)), source.object)
}

/// Whether a work in `phase` runs on a vessel.
fn needs_vessel(phase: WorkPhase) -> bool {
    matches!(phase, WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running | WorkPhase::Stalled | WorkPhase::Interrupted)
}

/// Where `work` of `convoy` is placed: `Some(Some(host))` on its vessel's pin, else on its
/// convoy's placement; `Some(None)` not yet placed; `None` when no fulfilment kind is pinned, so
/// it runs on no vessel.
fn placed_host(convoy: &ResourceObject<Convoy>, work: &str) -> Option<Option<CanonicalHostId>> {
    if let Some(pin) = flotilla_resources::vessel_placement_pin(convoy, work) {
        return Some(Some(pin.decision.target_host.reference));
    }
    flotilla_resources::pinned_placement_ref(convoy)?;
    Some(
        convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref()).map(|decision| decision.target_host.reference.clone()),
    )
}

#[async_trait::async_trait]
impl ConvoyWorks for OldConvoyWorks {
    async fn works_needing_vessel(&self) -> Result<Vec<WorkNeedingVessel>, String> {
        Ok(self.convoys().await?.iter().flat_map(|((name, convoy_home), convoy)| works(name, convoy_home, convoy)).collect())
    }

    async fn convoy(&self, convoy: &str, convoy_home: &str) -> Result<Option<ConvoyWorksRead>, String> {
        Ok(self.read(convoy, convoy_home).await?.map(|object| ConvoyWorksRead {
            standing: standing(&object),
            works: object
                .status
                .iter()
                .filter_map(|status| status.workflow_snapshot.as_ref())
                .flat_map(|snapshot| snapshot.vessels.iter().map(|requirement| requirement.name.clone()))
                .collect(),
            needing: works(convoy, convoy_home, &object),
        }))
    }

    async fn standing(&self, convoy: &str, convoy_home: &str) -> Result<ConvoyStanding, String> {
        Ok(self.read(convoy, convoy_home).await?.map_or(ConvoyStanding::Gone, |object| standing(&object)))
    }

    async fn changes(&self) -> Result<BoxStream<'static, ConvoyTouched>, String> {
        let here = self.here();
        let watch = self.backend.including_replicas::<Convoy>(&self.namespace).watch().await.map_err(|error| error.to_string())?;
        Ok(watch
            .map(move |event| match event {
                Ok(ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read)) => {
                    let ((convoy, convoy_home), _) = homed(read, &here);
                    ConvoyTouched::Convoy { convoy, convoy_home }
                }
                Ok(ReadWatchEvent::DeletedByName { tombstone, provenance }) => {
                    ConvoyTouched::Convoy { convoy: tombstone.name, convoy_home: home(&provenance, &here) }
                }
                Err(_) => ConvoyTouched::Every,
            })
            .boxed())
    }
}
