//! One contract suite for every backend, including backends outside this repo.
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};

use flotilla_inbox::{ConsumerPort, Cursor, Error, InboxBackend, Observations, Rule, Rules, Supervision};
use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{
    machines::{
        agent_session::{AgentSession, MemoryAgentSessionWorld, Receive},
        consumer::ConsumerKind,
        message::*,
    },
    Home, Log, Ref, Resource, Transport, WriteError,
};
use futures::{StreamExt, TryStreamExt};

pub fn address(value: &str) -> Address {
    value.parse().expect("test address")
}
pub fn inbox() -> Address {
    address("project:test/role:inbox")
}
pub fn consumer(id: &str) -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: id.into(), host: "test".into() }
}
pub fn spec() -> MessageSpec {
    MessageSpec::new(
        address("project:test/role:sender"),
        inbox(),
        Relation::Peer,
        MessageBody::text("contract message").expect("small body"),
    )
}

pub struct Evidence {
    pub clock: AtomicI64,
    pub world: Mutex<MemoryMessageWorld>,
}
impl Default for Evidence {
    fn default() -> Self {
        Self { clock: AtomicI64::new(10), world: Mutex::new(MemoryMessageWorld::default()) }
    }
}
impl Observations for Evidence {
    fn now(&self) -> Timestamp {
        Timestamp(self.clock.load(Ordering::SeqCst))
    }
    fn snapshot(&self) -> MemoryMessageWorld {
        self.world.lock().expect("evidence").clone()
    }
}

pub struct Supervisors;
impl Supervision for Supervisors {
    fn supervisor(&self, sender: &Address) -> Option<Address> {
        if sender == &address("project:test/role:alone") {
            None
        } else {
            Some(address("project:test/role:supervisor"))
        }
    }
    fn system_sender(&self) -> Address {
        address("project:test/role:system")
    }
}

/// A real AgentSession single writer. Its Receive log is the boundary's
/// collaborator, including on SQLite; it is not a receipt-shaped mock.
pub struct SessionPort<L: Log<AgentSession>> {
    home: Home<AgentSession, L>,
    name: String,
}
impl<L: Log<AgentSession>> SessionPort<L> {
    pub fn new(name: &str, log: L) -> Self {
        let mut home = Home::with_log("test", log).expect("session declaration");
        home.create(name, (), &MemoryAgentSessionWorld::default(), Timestamp(0)).expect("create consumer");
        Self { home, name: name.into() }
    }
    pub fn resource(&self) -> &Resource<AgentSession> {
        self.home.get(&self.name).expect("session")
    }
}
impl<L: Log<AgentSession> + Send + flotilla_inbox::ConsumerLog> ConsumerPort for SessionPort<L> {
    fn receipt_scope(&self) -> Option<flotilla_inbox::ReceiptScope> {
        self.home.log().receipt_scope()
    }
    fn receive(&mut self, message: &str, transport: Transport, now: Timestamp) -> Result<(), Error> {
        self.home
            .apply(&self.name, Receive { message: message.into(), transport }, &MemoryAgentSessionWorld::default(), now)
            .map_err(|e| Error::Consumer(format!("{e:?}")))?;
        Ok(())
    }
}

pub struct Harness<B> {
    pub backend: B,
    pub evidence: Arc<Evidence>,
}
pub trait Factory {
    type Backend: InboxBackend;
    fn create(&self) -> Harness<Self::Backend>;
    fn inspect(&self, backend: &Self::Backend, message: &str) -> Resource<Message>;
}

fn refused(result: Result<(), Error>, expected: MessageRefusal) {
    assert!(matches!(result, Err(Error::Message(WriteError::Refused(refusal))) if refusal == expected), "expected {expected:?}");
}

/// Behaviour (ADR 0064): the first accepted deliver claims a shared message;
/// receipts from a departed consumer remain valid; later receipts are refused.
pub async fn claim<F: Factory>(factory: &F, first: bool) {
    let Harness { backend, .. } = factory.create();
    let a = consumer("a");
    let b = consumer("b");
    backend.bind(inbox(), a.clone()).await.expect("bind a");
    backend.bind(inbox(), b.clone()).await.expect("bind b");
    let message = backend.enqueue(spec()).await.expect("admit");
    backend.receive(&a, &message, Transport::Turn).await.expect("a receives");
    backend.receive(&b, &message, Transport::Turn).await.expect("b receives");
    let winner = if first { a } else { b };
    backend.unbind(&inbox(), &winner).await.expect("unbind winner");
    backend.deliver(&message, Deliver { receiver: winner.clone(), transport: Transport::Turn }).await.expect("departed receipt accepted");
    refused(backend.deliver(&message, Deliver { receiver: winner, transport: Transport::Turn }).await, MessageRefusal::Ended);
    assert!(backend.receive(&consumer("b"), &message, Transport::Turn).await.is_err());
}

/// Behaviour: a new binding cannot race past a receipt still awaiting deliver.
pub async fn binding_barrier<F: Factory>(factory: &F) {
    let Harness { backend, .. } = factory.create();
    let a = consumer("a");
    backend.bind(inbox(), a.clone()).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    backend.receive(&a, &message, Transport::Turn).await.expect("receive");
    let binding = backend.bind(inbox(), consumer("b"));
    tokio::pin!(binding);
    assert!(tokio::time::timeout(std::time::Duration::from_millis(100), binding.as_mut()).await.is_err(), "binding waits");
    backend.deliver(&message, Deliver { receiver: a, transport: Transport::Turn }).await.expect("deliver");
    binding.await.expect("binding resumes");
}

/// Behaviour: mode selection uses all three selectors, first matching wins;
/// Notify and Mute have a receipt as soon as pending reads the inbox.
pub async fn modes<F: Factory>(factory: &F) {
    for mode in [Transport::Turn, Transport::Notify, Transport::Mute] {
        let Harness { backend, .. } = factory.create();
        let a = consumer("a");
        backend.bind(inbox(), a.clone()).await.expect("bind");
        let message = backend.enqueue(spec()).await.expect("enqueue");
        let rules = Rules(vec![Rule {
            sender: Some(spec().sender),
            relation: Some(Relation::Peer),
            expectation: Some(Expectation::Nothing),
            mode,
        }]);
        let pending = backend.pending(&inbox(), &a, &rules).await.expect("read").try_collect::<Vec<_>>().await.expect("pending items");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mode, mode);
        if mode == Transport::Turn {
            refused(backend.deliver(&message, Deliver { receiver: a.clone(), transport: mode }).await, MessageRefusal::IncorrectReceipt);
            backend.receive(&a, &message, mode).await.expect("turn receipt");
        }
        backend.deliver(&message, Deliver { receiver: a.clone(), transport: mode }).await.expect("matching receipt");
        assert!(backend
            .pending(&inbox(), &a, &rules)
            .await
            .expect("read again")
            .try_collect::<Vec<_>>()
            .await
            .expect("pending items")
            .is_empty());
    }
}

/// Behaviour: ordinary system messages with a subject still use consumer
/// rules; only backend reactions carry the forced Notify mode.
pub async fn ordinary_system_mode<F: Factory>(factory: &F) {
    let Harness { backend, .. } = factory.create();
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut envelope = spec();
    envelope.sender = address("project:test/role:system");
    envelope.relation = Relation::System;
    envelope.references.push(MessageReference { address: inbox(), revision: "1".into(), subject: true });
    backend.enqueue(envelope).await.expect("enqueue");
    let rules = Rules(vec![Rule { sender: None, relation: None, expectation: None, mode: Transport::Mute }]);
    let pending = backend.pending(&inbox(), &consumer("a"), &rules).await.expect("pending").try_collect::<Vec<_>>().await.expect("items");
    assert_eq!(pending[0].mode, Transport::Mute, "a system envelope is not itself dead-letter provenance");
}

/// Behaviour: every drop reason needs exactly its engine evidence. Deadline,
/// applied prefix, unbound threshold and expectation gates straddle boundaries.
pub async fn gates<F: Factory>(factory: &F) {
    let Harness { backend, evidence } = factory.create();
    let a = consumer("a");
    backend.bind(inbox(), a.clone()).await.expect("bind");
    let mut envelope = spec();
    envelope.deadline = Some(Timestamp(10));
    envelope.expectation = Expectation::Reply;
    let message = backend.enqueue(envelope).await.expect("enqueue");
    refused(backend.drop(&message, DropReason::Expired).await, MessageRefusal::MissingEvidence);
    evidence.clock.store(11, Ordering::SeqCst);
    refused(backend.drop(&message, DropReason::Expired).await, MessageRefusal::PrefixBeforeDeadline);
    evidence.world.lock().expect("world").applied_through.put(&Ref::new(inbox().as_str()), Timestamp(10));
    refused(backend.drop(&message, DropReason::Expired).await, MessageRefusal::PrefixBeforeDeadline);
    evidence.world.lock().expect("world").applied_through.put(&Ref::new(inbox().as_str()), Timestamp(11));
    evidence.world.lock().expect("world").reply.put(&Ref::new(&message), ());
    refused(backend.drop(&message, DropReason::Expired).await, MessageRefusal::ExpectationMet);

    let other = backend
        .enqueue({
            let mut s = spec();
            s.deadline = Some(Timestamp(10));
            s
        })
        .await
        .expect("other");
    backend.drop(&other, DropReason::Expired).await.expect("past deadline and watermark");
    let missing = backend.enqueue(spec()).await.expect("message");
    refused(backend.drop(&missing, DropReason::Undeliverable).await, MessageRefusal::MissingEvidence);
    backend.unbind(&inbox(), &a).await.expect("last consumer leaves");
    refused(backend.drop(&missing, DropReason::Undeliverable).await, MessageRefusal::MissingEvidence);
    evidence.clock.store(12, Ordering::SeqCst);
    backend.drop(&missing, DropReason::Undeliverable).await.expect("unbound past threshold");

    for reason in [
        DropReason::Withdrawn { by: spec().sender, authority: address("project:test/role:authority") },
        DropReason::ConsumerUnreachable { by: spec().sender, authority: address("project:test/role:authority") },
    ] {
        let message = backend.enqueue(spec()).await.expect("enqueue");
        evidence.world.lock().expect("world").authority.put(&Ref::new("project:test/role:authority"), false);
        refused(backend.drop(&message, reason.clone()).await, MessageRefusal::Unauthorised);
        evidence.world.lock().expect("world").authority.put(&Ref::new("project:test/role:authority"), true);
        backend.drop(&message, reason).await.expect("authority");
    }
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let newer = address("project:test/role:newer");
    refused(backend.drop(&message, DropReason::Superseded { by: newer.clone() }).await, MessageRefusal::MissingEvidence);
    evidence.world.lock().expect("world").supersession.put(&Ref::new(newer.as_str()), Supersedes { older: "another-message".into() });
    refused(backend.drop(&message, DropReason::Superseded { by: newer.clone() }).await, MessageRefusal::MissingEvidence);
    evidence.world.lock().expect("world").supersession.put(&Ref::new(newer.as_str()), Supersedes { older: message.clone() });
    backend.drop(&message, DropReason::Superseded { by: newer }).await.expect("supersession");
}

/// Behaviour: receipts exclude Undeliverable, failure evidence permits it only
/// without receipts; withdrawal may instead cite an observed failed condition.
pub async fn receipt_and_condition_gates<F: Factory>(factory: &F) {
    let Harness { backend, evidence } = factory.create();
    let a = consumer("a");
    backend.bind(inbox(), a.clone()).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    backend.receive(&a, &message, Transport::Turn).await.expect("receive");
    refused(
        backend.deliver(&message, Deliver { receiver: a.clone(), transport: Transport::Notify }).await,
        MessageRefusal::IncorrectReceipt,
    );
    backend.unbind(&inbox(), &a).await.expect("unbind");
    evidence.clock.store(20, Ordering::SeqCst);
    refused(backend.drop(&message, DropReason::Undeliverable).await, MessageRefusal::ReceiptConfirmed);
    let mut envelope = spec();
    envelope.delivery_condition = Some(address("project:test/role:condition"));
    let message = backend.enqueue(envelope).await.expect("condition message");
    let reason = DropReason::Withdrawn { by: spec().sender, authority: address("project:test/role:authority") };
    refused(backend.drop(&message, reason.clone()).await, MessageRefusal::Unauthorised);
    evidence.world.lock().expect("world").condition.put(&Ref::new("project:test/role:condition"), true);
    refused(backend.drop(&message, reason.clone()).await, MessageRefusal::Unauthorised);
    evidence.world.lock().expect("world").condition.put(&Ref::new("project:test/role:condition"), false);
    backend.drop(&message, reason).await.expect("observed failed condition");
    let Harness { backend, evidence } = factory.create();
    backend.bind(inbox(), a).await.expect("bound");
    let message = backend.enqueue(spec()).await.expect("failure message");
    evidence.world.lock().expect("world").delivery_failure.put(&Ref::new(&message), ());
    backend.drop(&message, DropReason::Undeliverable).await.expect("published failure");
}

/// Behaviour: unbound inboxes have no pending turns. Admission rejects unknown
/// replies and invalid subjects/revisions; refusals create no watch event.
pub async fn admission<F: Factory>(factory: &F) {
    let Harness { backend, .. } = factory.create();
    backend.enqueue(spec()).await.expect("valid message");
    assert!(backend
        .pending(&inbox(), &consumer("a"), &Rules::default())
        .await
        .expect("read")
        .try_collect::<Vec<_>>()
        .await
        .expect("items")
        .is_empty());
    let reference = MessageReference { address: inbox(), revision: "1".into(), subject: true };
    let mut envelope = spec();
    envelope.references = vec![reference.clone(), reference.clone()];
    assert!(matches!(backend.enqueue(envelope).await, Err(Error::Message(WriteError::Refused(MessageRefusal::SubjectsConflict)))));
    let mut envelope = spec();
    envelope.references = vec![MessageReference { revision: String::new(), ..reference }];
    assert!(matches!(backend.enqueue(envelope).await, Err(Error::Message(WriteError::Refused(MessageRefusal::EmptyRevision)))));
    let mut envelope = spec();
    envelope.in_reply_to = Some(flotilla_schema::Ref::new("missing", "test"));
    assert!(matches!(backend.enqueue(envelope).await, Err(Error::Message(WriteError::Refused(MessageRefusal::UnknownMessage)))));
}

/// Behaviour: an observed outcome settles directly from Pending; unsupported
/// settlement kinds and unobserved conditions refuse without a transition.
pub async fn outcome<F: Factory>(factory: &F) {
    let Harness { backend, evidence } = factory.create();
    let mut envelope = spec();
    envelope.expectation = Expectation::Outcome { condition: address("project:test/role:outcome") };
    let message = backend.enqueue(envelope).await.expect("enqueue");
    refused(backend.settle(&message, Settle { by: Settlement::OutcomeObserved }).await, MessageRefusal::MissingEvidence);
    refused(backend.settle(&message, Settle { by: Settlement::Answered }).await, MessageRefusal::WrongState);
    evidence.world.lock().expect("world").condition.put(&Ref::new("project:test/role:outcome"), true);
    backend.settle(&message, Settle { by: Settlement::OutcomeObserved }).await.expect("outcome from Pending");
}

/// Behaviour: a Reply message awaits after its one delivery, refuses later
/// receipts and deliveries, and settles only after its own reply is observed.
pub async fn awaiting_reply<F: Factory>(factory: &F) {
    let Harness { backend, evidence } = factory.create();
    let a = consumer("a");
    backend.bind(inbox(), a.clone()).await.expect("bind");
    let mut envelope = spec();
    envelope.expectation = Expectation::Reply;
    let message = backend.enqueue(envelope).await.expect("enqueue");
    backend.receive(&a, &message, Transport::Turn).await.expect("receipt");
    backend.deliver(&message, Deliver { receiver: a.clone(), transport: Transport::Turn }).await.expect("awaiting reply");
    refused(backend.receive(&consumer("b"), &message, Transport::Turn).await, MessageRefusal::WrongState);
    refused(backend.deliver(&message, Deliver { receiver: consumer("b"), transport: Transport::Turn }).await, MessageRefusal::WrongState);
    refused(backend.drop(&message, DropReason::Undeliverable).await, MessageRefusal::WrongState);
    refused(backend.settle(&message, Settle { by: Settlement::OutcomeObserved }).await, MessageRefusal::UnsupportedSettlement);
    refused(backend.settle(&message, Settle { by: Settlement::Answered }).await, MessageRefusal::MissingEvidence);
    evidence.world.lock().expect("world").reply.put(&Ref::new(&message), ());
    backend.settle(&message, Settle { by: Settlement::Answered }).await.expect("own reply observed");
}

/// Behaviour: dead letters target the sender's supervisor with the original as
/// subject, by Notify; a self-supervisor causes no recursive notification.
pub async fn dead_letters<F: Factory>(factory: &F, self_supervisor: bool) {
    let Harness { backend, evidence } = factory.create();
    let mut envelope = spec();
    if self_supervisor {
        envelope.sender = address("project:test/role:supervisor");
    }
    let message = backend.enqueue(envelope).await.expect("enqueue");
    evidence.clock.store(11, Ordering::SeqCst);
    backend.drop(&message, DropReason::Undeliverable).await.expect("drop");
    let dropped_version = factory.inspect(&backend, &message).version();
    refused(backend.drop(&message, DropReason::Undeliverable).await, MessageRefusal::Ended);
    assert_eq!(factory.inspect(&backend, &message).version(), dropped_version, "a repeated drop records no new Message edge");
    let supervisor = address("project:test/role:supervisor");
    backend.bind(supervisor.clone(), consumer("a")).await.expect("bind supervisor");
    let mute_all = Rules(vec![Rule { sender: None, relation: None, expectation: None, mode: Transport::Mute }]);
    let pending = backend
        .pending(&supervisor, &consumer("a"), &mute_all)
        .await
        .expect("supervisor reads")
        .try_collect::<Vec<_>>()
        .await
        .expect("pending items");
    assert_eq!(pending.len(), usize::from(!self_supervisor));
    if !self_supervisor {
        assert_eq!(pending[0].mode, Transport::Notify);
        assert_eq!(pending[0].spec.subject().expect("subject").address.as_str(), message);
    }
}

/// Behaviour: resuming a watch excludes its cursor, includes receipts and
/// message changes, filters other inboxes and stays attached for future writes.
pub async fn watch<F: Factory>(factory: &F) {
    let Harness { backend, .. } = factory.create();
    let a = consumer("a");
    backend.bind(inbox(), a.clone()).await.expect("bind");
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let mut initial = backend.watch(&inbox(), Some(Cursor(0))).await.expect("watch");
    let first = initial.next().await.expect("create").expect("change");
    backend.receive(&a, &message, Transport::Turn).await.expect("receipt");
    backend.deliver(&message, Deliver { receiver: a, transport: Transport::Turn }).await.expect("deliver");
    let mut other = spec();
    other.inbox = address("project:test/role:other");
    backend.enqueue(other).await.expect("unrelated inbox change");
    let mut resumed = backend.watch(&inbox(), Some(first.cursor)).await.expect("resume");
    let receipt = resumed.next().await.expect("receipt").expect("change");
    let deliver = resumed.next().await.expect("deliver").expect("change");
    assert!(receipt.cursor > first.cursor && deliver.cursor > receipt.cursor);
    assert!(matches!(receipt.kind, flotilla_inbox::ChangeKind::Receipt { .. }));
    assert!(matches!(deliver.kind, flotilla_inbox::ChangeKind::Message(MessageState::Settled { .. })));
    assert!(tokio::time::timeout(std::time::Duration::from_millis(100), resumed.next()).await.is_err());
    let mut live = backend.watch(&inbox(), None).await.expect("fresh live tail");
    let next = backend.enqueue(spec()).await.expect("next message");
    assert_eq!(resumed.next().await.expect("live").expect("change").message, next);
    assert_eq!(live.next().await.expect("new live tail").expect("change").message, next);
}

pub async fn suite<F: Factory>(factory: &F) {
    claim(factory, true).await;
    claim(factory, false).await;
    binding_barrier(factory).await;
    pending_arrival_order(factory).await;
    modes(factory).await;
    ordinary_system_mode(factory).await;
    gates(factory).await;
    receipt_and_condition_gates(factory).await;
    admission(factory).await;
    outcome(factory).await;
    awaiting_reply(factory).await;
    dead_letters(factory, false).await;
    dead_letters(factory, true).await;
    watch(factory).await;
}

/// Behaviour: after every generated Receive/Deliver/unbind interleaving, the
/// first accepted delivery owns the message and pending exactly excludes each
/// consumer's receipts. Empty, duplicate, unbound and losing claims are drawn.
pub async fn generated_claim<F: Factory>(factory: &F, operations: &[usize]) {
    let Harness { backend, .. } = factory.create();
    let consumers = [consumer("a"), consumer("b")];
    for consumer in &consumers {
        backend.bind(inbox(), consumer.clone()).await.expect("bind");
    }
    let message = backend.enqueue(spec()).await.expect("enqueue");
    let mut receipts = [false; 2];
    let mut bound = [true; 2];
    let mut ended = false;
    for &operation in operations {
        let who = operation % 2;
        match operation / 2 {
            0 => {
                let result = backend.receive(&consumers[who], &message, Transport::Turn).await;
                if ended {
                    assert!(result.is_err());
                } else {
                    result.expect("receive");
                    receipts[who] = true;
                }
            }
            1 => {
                let result = backend.deliver(&message, Deliver { receiver: consumers[who].clone(), transport: Transport::Turn }).await;
                if ended {
                    refused(result, MessageRefusal::Ended);
                } else if !receipts[who] {
                    refused(result, MessageRefusal::IncorrectReceipt);
                } else {
                    result.expect("first accepted claim");
                    ended = true;
                }
            }
            _ => {
                backend.unbind(&inbox(), &consumers[who]).await.expect("unbind");
                bound[who] = false;
            }
        }
        for index in 0..2 {
            let pending = backend
                .pending(&inbox(), &consumers[index], &Rules::default())
                .await
                .expect("pending")
                .try_collect::<Vec<_>>()
                .await
                .expect("items");
            assert_eq!(pending.len(), usize::from(!ended && bound[index] && !receipts[index]));
        }
    }
}

pub mod quint;

/// Behaviour: pending messages arrive oldest first, including when generated
/// names cross a decimal digit boundary. Reading Turn items does not reorder them.
pub async fn pending_arrival_order<F: Factory>(factory: &F) {
    let Harness { backend, .. } = factory.create();
    backend.bind(inbox(), consumer("a")).await.expect("bind");
    let mut expected = Vec::new();
    for _ in 0..12 {
        expected.push(backend.enqueue(spec()).await.expect("enqueue"));
    }
    let actual = backend
        .pending(&inbox(), &consumer("a"), &Rules::default())
        .await
        .expect("pending")
        .map_ok(|item| item.message)
        .try_collect::<Vec<_>>()
        .await
        .expect("items");
    assert_eq!(actual, expected, "pending preserves admission order across message-9/message-10");
}
