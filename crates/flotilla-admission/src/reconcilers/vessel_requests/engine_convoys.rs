//! [`ConvoyWorks`] read from the engine's convoy launches, this home's and those replicated here
//! (#3108, #4412): each launch is composed by its key, and the changes come from the launches'
//! records as they are written or replicated.

use std::sync::Arc;

use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{
    convoy_compose::compose, convoy_records::ConvoyChange, lifecycle_reads::LifecycleReads, Convoy, ReadResourceObject, ResourceObject,
    ResourceProvenance, WorkPhase,
};
use flotilla_store::load_metrics::carry;
use futures::{stream::BoxStream, StreamExt};
use tokio::sync::broadcast::error::RecvError;

use super::{ConvoyStanding, ConvoyTouched, ConvoyWorks, ConvoyWorksRead, WorkNeedingVessel};

/// The engine's convoy launches in one namespace, read through `reads`.
#[derive(Clone)]
pub struct EngineConvoyWorks {
    reads: Arc<dyn LifecycleReads>,
    namespace: String,
}

impl EngineConvoyWorks {
    pub fn new(reads: Arc<dyn LifecycleReads>, namespace: impl Into<String>) -> Self {
        Self { reads, namespace: namespace.into() }
    }

    /// `work` over the readings, on a blocking thread: the engine's reads are synchronous.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&dyn LifecycleReads, &str) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let (reads, namespace) = (Arc::clone(&self.reads), self.namespace.clone());
        tokio::task::spawn_blocking(carry(move || work(reads.as_ref(), &namespace))).await.map_err(|error| error.to_string())?
    }

    /// Launch `convoy` held at `convoy_home`, composed by its key.
    async fn read(&self, convoy: &str, convoy_home: &str) -> Result<Option<ResourceObject<Convoy>>, String> {
        let (convoy, convoy_home) = (convoy.to_owned(), convoy_home.to_owned());
        self.blocking(move |reads, namespace| Ok(compose(reads, namespace, &convoy, Some(&convoy_home))?.map(|view| view.object))).await
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

/// The home of a composed launch read at `here`.
fn home_of(view: &ReadResourceObject<Convoy>, here: &str) -> String {
    match &view.provenance {
        ResourceProvenance::Local => here.to_owned(),
        ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
    }
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
impl ConvoyWorks for EngineConvoyWorks {
    /// Every live launch at every home, each composed by its key: read at the first pass and after
    /// missed changes alone.
    async fn works_needing_vessel(&self) -> Result<Vec<WorkNeedingVessel>, String> {
        self.blocking(|reads, namespace| {
            let convoys = reads.convoys()?;
            let here = convoys.home().to_owned();
            let mut needing = Vec::new();
            for record in convoys.every()?.records.into_iter().filter(|record| record.ended.is_none()) {
                let Some(view) = compose(reads, namespace, &record.name, Some(&record.home))? else { continue };
                needing.extend(works(&view.object.metadata.name, &home_of(&view, &here), &view.object));
            }
            Ok(needing)
        })
        .await
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
        let changes = self.reads.convoys()?.changes();
        Ok(futures::stream::unfold(changes, |mut changes| async move {
            let touched = match changes.recv().await {
                Ok(ConvoyChange::Changed { name, home }) => ConvoyTouched::Convoy { convoy: name, convoy_home: home },
                Ok(ConvoyChange::Rescanned) | Err(RecvError::Lagged(_)) => ConvoyTouched::Every,
                Err(RecvError::Closed) => return None,
            };
            Some((touched, changes))
        })
        .boxed())
    }
}
