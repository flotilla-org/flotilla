//! A lifecycle kind's log whose launches are realised together with their facts.
//!
//! A launch's facts are written once, in the transaction of the launch's Realise edge, and never
//! updated (owner's look-overs, #3108): an Environment launch's [`EnvironmentLaunchFacts`], a
//! Clone launch's [`CloneFacts`]. The kind's controller stages a launch's facts here when its
//! provider work reports them, then steps the launch realised; this log writes the edge and
//! inserts the staged facts under the launch's name in one change. A Realise with nothing staged
//! is refused, so a realised launch always has its facts.

use std::{
    collections::BTreeMap,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use flotilla_engine_store::StoreLog;
use flotilla_schema::{ChangeItem, Kind, Timestamp};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleLabel},
    EdgeEventOf, InputRead, Log, LogError, Resource, StoredState, Version,
};

#[cfg(doc)]
use crate::stored::clone::CloneFacts;
use crate::stored::environment::EnvironmentKind;
use crate::stored::launch_facts::EnvironmentLaunchFacts;

/// Facts staged for launches about to be realised, by launch name: the provider work's
/// report, waiting for the Realise edge that writes it.
pub struct StagedFacts<F = EnvironmentLaunchFacts>(Arc<Mutex<BTreeMap<String, F>>>);

impl<F> Clone for StagedFacts<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<F> Default for StagedFacts<F> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<F> StagedFacts<F> {
    /// Stages `facts` for launch `name`'s Realise, replacing any staged before.
    pub fn stage(&self, name: &str, facts: F) {
        self.lock().insert(name.to_owned(), facts);
    }

    /// Takes what is staged for `name`, if anything.
    pub fn take(&self, name: &str) -> Option<F> {
        self.lock().remove(name)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, F>> {
        // A poisoned map is still a map: staging is a plain insert or remove.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Kind `K`'s log over the store: [`StoreLog`]'s writes, with each Realise also inserting the
/// launch's staged facts `F`.
pub struct LaunchLog<K = EnvironmentKind, F = EnvironmentLaunchFacts> {
    inner: StoreLog,
    staged: StagedFacts<F>,
    inputs: StagedFacts<Vec<ChangeItem>>,
    kind: PhantomData<fn() -> K>,
}

impl<K, F> Clone for LaunchLog<K, F> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), staged: self.staged.clone(), inputs: self.inputs.clone(), kind: PhantomData }
    }
}

impl<K, F> LaunchLog<K, F> {
    pub fn new(inner: StoreLog, staged: StagedFacts<F>) -> Self {
        Self { inner, staged, inputs: StagedFacts::default(), kind: PhantomData }
    }

    /// This log, creating each launch under a change whose items are those staged in `inputs`
    /// for it: the records its requester read to request it.
    pub fn with_inputs(mut self, inputs: StagedFacts<Vec<ChangeItem>>) -> Self {
        self.inputs = inputs;
        self
    }
}

impl<K, F> Log<Lifecycle<K>> for LaunchLog<K, F>
where
    K: LifecycleKind,
    StoreLog: Log<Lifecycle<K>>,
    K::Request: flotilla_schema::KindSpec<State = flotilla_state_machines::machines::lifecycle::LifecycleState>,
    F: Kind,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<Lifecycle<K>>, reads: &[InputRead]) -> Result<Version, LogError> {
        match self.inputs.take(resource.name()) {
            Some(items) => self.inner.create_with_items(writer, at, resource, items),
            None => self.inner.create(writer, at, resource, reads),
        }
    }

    fn edge(
        &mut self,
        writer: &str,
        previous: &StoredState<Lifecycle<K>>,
        next: &Resource<Lifecycle<K>>,
        event: &EdgeEventOf<Lifecycle<K>>,
    ) -> Result<Version, LogError> {
        self.edge_with_writes(writer, previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        writer: &str,
        previous: &StoredState<Lifecycle<K>>,
        next: &Resource<Lifecycle<K>>,
        event: &EdgeEventOf<Lifecycle<K>>,
        writes: &[<Lifecycle<K> as flotilla_state_machines::Machine>::Write],
    ) -> Result<Version, LogError> {
        let name = next.name();
        if event.to != LifecycleLabel::Realised {
            // A launch that ends unrealised drops what was staged for it.
            if matches!(event.to, LifecycleLabel::Gone | LifecycleLabel::GaveUp) {
                self.staged.take(name);
            }
            return self.inner.edge_with_writes(writer, previous, next, event, writes);
        }
        let facts = self.staged.take(name).ok_or_else(|| LogError::new(format!("launch {name} is realised without its facts")))?;
        let written =
            self.inner.edge_and(writer, previous, next, event, writes, |write, change| write.insert(name, &facts, change).map(|_| ()));
        if written.is_err() {
            // Not written: the facts wait for the next try.
            self.staged.stage(name, facts);
        }
        written
    }
}
