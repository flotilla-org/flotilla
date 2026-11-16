//! The in-memory port keeps the engine port contract, for each kind of port the real ones are
//! checked against: a plain kind, a kind with a status, and a kind the engine holds.

use std::sync::Arc;

use flotilla_kind_records_testkit::{
    contract::{self, Samples, Subject},
    memory_records,
    samples::{Artifacts, Environments, Repositories},
    MemoryRecords, Profile,
};
use flotilla_resources::{engine_records::KindRecords, Artifact, Environment, Repository, Resource};

struct InMemory<R: Resource, S> {
    records: Arc<MemoryRecords<R>>,
    samples: S,
}

impl<R: Resource, S> InMemory<R, S> {
    fn new(samples: S) -> Self {
        Self { records: memory_records("alpha"), samples }
    }
}

#[async_trait::async_trait]
impl<R: Resource, S: Samples<R>> Subject<R> for InMemory<R, S> {
    fn port(&self) -> Arc<dyn KindRecords<R>> {
        self.records.clone()
    }
    fn profile(&self) -> Profile {
        flotilla_kind_records_testkit::profile_of(R::API_PATHS.kind)
    }
    fn samples(&self) -> &dyn Samples<R> {
        &self.samples
    }
    async fn hold(&self, index: usize) {
        let (name, spec) = self.samples.sample(index);
        if self.profile().writes {
            self.records.apply(&name, &Default::default(), &Default::default(), &spec).await.expect("the port takes its sample");
        } else {
            self.records.hold("alpha", &name, spec, None);
        }
    }
    async fn change(&self, index: usize) -> Option<()> {
        let (name, spec) = self.samples.sample(index);
        self.records.hold("alpha", &name, spec, self.samples.status());
        Some(())
    }
    async fn hold_elsewhere(&self, home: &str, index: usize) -> Option<()> {
        let (name, _) = self.samples.sample(index);
        self.records.hold(home, &name, self.samples.edited(index), None);
        Some(())
    }
}

#[tokio::test]
async fn a_plain_kind_in_memory_keeps_the_contract() {
    contract::check::<Artifact, _>(&InMemory::new(Artifacts)).await;
}

#[tokio::test]
async fn a_kind_with_a_status_in_memory_keeps_the_contract() {
    contract::check::<Repository, _>(&InMemory::new(Repositories)).await;
}

#[tokio::test]
async fn a_kind_the_engine_holds_in_memory_keeps_the_contract() {
    contract::check::<Environment, _>(&InMemory::new(Environments)).await;
}
