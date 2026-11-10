//! [`ConvoyWorks`] read from the old store's Convoy objects, local and replicated: the bridge until
//! the engine Convoy implements the port from its records (#3108), when this is deleted.

use std::collections::BTreeMap;

use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{Convoy, ReadResourceObject, ResourceObject, ResourceProvenance, WorkPhase};
use flotilla_store::ResourceBackend;
use futures::{stream::BoxStream, StreamExt};

use super::{ConvoyStanding, ConvoyWorks, WorkNeedingVessel};

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

    /// Every convoy read here, by name and home.
    async fn convoys(&self) -> Result<BTreeMap<(String, String), ResourceObject<Convoy>>, String> {
        let here = self.backend.local_root().map(|root| root.as_str().to_owned()).unwrap_or_default();
        let sources = self.backend.including_replicas::<Convoy>(&self.namespace).list().await.map_err(|error| error.to_string())?.items;
        Ok(sources.into_iter().map(|source| homed(source, &here)).collect())
    }
}

fn homed(source: ReadResourceObject<Convoy>, here: &str) -> ((String, String), ResourceObject<Convoy>) {
    let home = match &source.provenance {
        ResourceProvenance::Local => here.to_owned(),
        ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
    };
    ((source.object.metadata.name.clone(), home), source.object)
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
        let mut works = Vec::new();
        for ((name, convoy_home), convoy) in self.convoys().await? {
            let Some(status) = convoy.status.as_ref().filter(|status| !status.phase.is_terminal()) else { continue };
            let Some(snapshot) = status.workflow_snapshot.as_ref() else { continue };
            for requirement in &snapshot.vessels {
                if !status.work.get(&requirement.name).is_some_and(|work| needs_vessel(work.phase)) {
                    continue;
                }
                let Some(host) = placed_host(&convoy, &requirement.name) else { continue };
                works.push(WorkNeedingVessel {
                    convoy: name.clone(),
                    convoy_home: convoy_home.clone(),
                    work: requirement.name.clone(),
                    host,
                });
            }
        }
        Ok(works)
    }

    async fn standing(&self, convoy: &str, convoy_home: &str) -> Result<ConvoyStanding, String> {
        let convoys = self.convoys().await?;
        Ok(match convoys.get(&(convoy.to_owned(), convoy_home.to_owned())) {
            None => ConvoyStanding::Gone,
            Some(convoy) if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => ConvoyStanding::Ended,
            Some(_) => ConvoyStanding::Live,
        })
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let watch = self.backend.including_replicas::<Convoy>(&self.namespace).watch().await.map_err(|error| error.to_string())?;
        Ok(watch.map(|_| ()).boxed())
    }
}
