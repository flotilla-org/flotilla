//! Thin InboxBackend adapter for the shared Message race mapping.
use crate::{consumer, Factory, Harness};
use flotilla_inbox::{InboxBackend, Rules};
use flotilla_quint_testkit::message::MessageSubject;
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::message::{MemoryMessageWorld, Message, MessageEdge, MessageSpec, MessageState, Receiver},
    Ref, Resource,
};
use futures::TryStreamExt;
use std::{collections::BTreeSet, sync::atomic::Ordering};

struct Subject<F: Factory> {
    factory: F,
    harness: Harness<F::Backend>,
    runtime: tokio::runtime::Runtime,
    message: String,
    observed: BTreeSet<Receiver>,
}
impl<F: Factory + Default> MessageSubject for Subject<F> {
    fn create(spec: MessageSpec) -> Self {
        let factory = F::default();
        let harness = factory.create();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let message = runtime.block_on(harness.backend.enqueue(spec)).expect("enqueue");
        Self { factory, harness, runtime, message, observed: BTreeSet::new() }
    }
    fn name(&self) -> &str {
        &self.message
    }
    fn receiver(&self) -> Receiver {
        consumer("a")
    }
    fn canonical_receiver(&self, receiver: &Receiver) -> Receiver {
        Receiver { id: "1".into(), host: "consumer".into(), ..receiver.clone() }
    }
    fn bind(&mut self) {
        self.runtime.block_on(self.harness.backend.bind(self.resource().spec().inbox.clone(), self.receiver())).expect("bind");
    }
    fn observe(&mut self, mut world: MemoryMessageWorld, now: Timestamp) {
        self.harness.evidence.clock.store(now.micros(), Ordering::SeqCst);
        let receipts = world.receipts.remove(&Ref::new(&self.message)).unwrap_or_default();
        *self.harness.evidence.world.lock().expect("evidence") = world;
        for (consumer, transport) in receipts {
            if self.observed.insert(consumer.clone()) {
                let result = self.runtime.block_on(self.harness.backend.receive(&consumer, &self.message, transport));
                if *self.resource().state() == MessageState::Pending {
                    result.expect("observed consumer Receive");
                } else {
                    assert!(result.is_err(), "late receipt refused");
                }
            }
        }
    }
    fn pending(&mut self) -> bool {
        let resource = self.resource();
        self.runtime.block_on(async {
            self.harness
                .backend
                .pending(&resource.spec().inbox, &self.receiver(), &Rules::default())
                .await
                .expect("pending")
                .map_ok(|pending| pending.message)
                .try_collect::<Vec<_>>()
                .await
                .expect("items")
                .contains(&self.message)
        })
    }
    fn apply(&mut self, edge: MessageEdge) {
        self.runtime
            .block_on(async {
                match edge {
                    MessageEdge::Deliver(edge) => self.harness.backend.deliver(&self.message, edge).await,
                    MessageEdge::Settle(edge) => self.harness.backend.settle(&self.message, edge).await,
                    MessageEdge::Drop(edge) => self.harness.backend.drop(&self.message, edge.reason).await,
                }
            })
            .expect("model edge accepted");
    }
    fn resource(&self) -> Resource<Message> {
        self.factory.inspect(&self.harness.backend, &self.message)
    }
}

/// Behaviour: the same Message race drives each backend's real binding,
/// receipt and pending operations, as well as its gated Message edges.
pub fn replay_message_race<F: Factory + Default>() {
    flotilla_quint_testkit::message::replay_message_race::<Subject<F>>();
}
