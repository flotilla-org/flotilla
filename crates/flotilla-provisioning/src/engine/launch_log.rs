//! The Environment's log: a launch is realised together with its facts.
//!
//! An Environment launch's [`EnvironmentLaunchFacts`] are written once, in the transaction of the
//! launch's Realise edge, and never updated (owner's look-over, #3108). The Environment controller
//! stages a launch's facts here when its provider work reports them, then steps the launch
//! realised; this log writes the edge and inserts the staged facts under the launch's name in one
//! change. A Realise with nothing staged is refused, so a realised launch always has its facts.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use flotilla_engine_store::StoreLog;
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleLabel},
    EdgeEventOf, InputRead, Log, LogError, Resource, StoredState, Version,
};

use crate::stored::{environment::EnvironmentKind, launch_facts::EnvironmentLaunchFacts};

type Environment = Lifecycle<EnvironmentKind>;

/// Facts staged for launches about to be realised, by launch name: the provider work's
/// report, waiting for the Realise edge that writes it.
#[derive(Clone, Default)]
pub struct StagedFacts(Arc<Mutex<BTreeMap<String, EnvironmentLaunchFacts>>>);

impl StagedFacts {
    /// Stages `facts` for launch `name`'s Realise, replacing any staged before.
    pub fn stage(&self, name: &str, facts: EnvironmentLaunchFacts) {
        self.lock().insert(name.to_owned(), facts);
    }

    fn take(&self, name: &str) -> Option<EnvironmentLaunchFacts> {
        self.lock().remove(name)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, EnvironmentLaunchFacts>> {
        // A poisoned map is still a map: staging is a plain insert or remove.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The Environment kind's log over the store: [`StoreLog`]'s writes, with each Realise also
/// inserting the launch's staged facts.
#[derive(Clone)]
pub struct LaunchLog {
    inner: StoreLog,
    staged: StagedFacts,
}

impl LaunchLog {
    pub fn new(inner: StoreLog, staged: StagedFacts) -> Self {
        Self { inner, staged }
    }
}

impl Log<Environment> for LaunchLog {
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<Environment>, reads: &[InputRead]) -> Result<Version, LogError> {
        self.inner.create(writer, at, resource, reads)
    }

    fn edge(
        &mut self,
        writer: &str,
        previous: &StoredState<Environment>,
        next: &Resource<Environment>,
        event: &EdgeEventOf<Environment>,
    ) -> Result<Version, LogError> {
        self.edge_with_writes(writer, previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        writer: &str,
        previous: &StoredState<Environment>,
        next: &Resource<Environment>,
        event: &EdgeEventOf<Environment>,
        writes: &[<Environment as flotilla_state_machines::Machine>::Write],
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
