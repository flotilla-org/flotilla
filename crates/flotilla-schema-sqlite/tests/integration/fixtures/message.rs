//! The engine's Message (`Resource<Message>`, declared by `MessageSpec`, kind code `ms`):
//! generators, and the worlds that let each edge pass its gate.

use flotilla_schema::{Address, Edge, Frozen, State, Timestamp};
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        convoy::{ConvoyRequest, Priority, Promise, PromiseId, VesselClass},
        message::{
            Deliver, DropMessage, DropReason, Expectation, MemoryMessageWorld, Message, MessageBody, MessageReference, MessageSpec,
            MessageState, MessageWorld, Receiver, Relation, RequestBody, Settle, Settlement, Supersedes,
        },
    },
    Ref, Resource, Transport,
};
use hegel::{generators as gs, TestCase};

use super::{
    addr, address, code_enum,
    engine::{self, after, resource, Candidate, NAME},
    list, name, reference, text, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4), a drop's reason digit 1 (base 5) and whether
/// a settlement or a drop kept a receiver digit 2 (base 2), so every reason is reached with and
/// without one; the expectation cycles in base 3.
pub const COVERING: usize = 80;

pub fn receiver(tc: &TestCase) -> Receiver {
    let kind = code_enum(tc, Pick::Draw, &[ConsumerKind::AgentSession, ConsumerKind::Controller]);
    Receiver { kind, id: name(tc), host: name(tc) }
}

/// Agent session `id` on `host`, as a receiver.
pub fn agent(id: &str, host: &str) -> Receiver {
    Receiver { kind: ConsumerKind::AgentSession, id: id.into(), host: host.into() }
}

/// Prose or a convoy request, each in turn when covering.
pub fn body(tc: &TestCase, pick: Pick) -> Frozen<MessageBody> {
    let body = match pick.variant(tc, 2, |index| index) {
        0 => MessageBody::Text(text(tc)),
        _ => MessageBody::Request(RequestBody::Convoy(ConvoyRequest {
            project: address(tc),
            promises: [(PromiseId(0), Promise { kind: name(tc), target: address(tc) })].into(),
            quota: [(VesselClass(1), 2)].into(),
            priority: Priority(3),
        })),
    };
    Frozen::new(body).expect("a fixture body freezes")
}

/// Up to three references, the first of them the subject when drawn so: at most one subject.
pub fn references(tc: &TestCase, pick: Pick) -> Vec<MessageReference> {
    let mut first = true;
    list(tc, pick, 3, |_| {
        let subject = std::mem::take(&mut first) && tc.draw(gs::booleans());
        MessageReference { address: address(tc), revision: text(tc), subject }
    })
}

fn transport(tc: &TestCase, pick: Pick) -> Transport {
    code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute])
}

pub fn expectation(tc: &TestCase, pick: Pick) -> Expectation {
    match pick.digit(tc, 3, 1) {
        0 => Expectation::Nothing,
        1 => Expectation::Reply,
        _ => Expectation::Outcome { condition: super::condition(tc) },
    }
}

pub fn reason(tc: &TestCase, pick: Pick) -> DropReason {
    match pick.digit(tc, 5, 4) {
        0 => DropReason::Undeliverable,
        1 => DropReason::Expired,
        2 => DropReason::Superseded { by: address(tc) },
        3 => DropReason::Withdrawn { by: address(tc), authority: address(tc) },
        _ => DropReason::ConsumerUnreachable { by: address(tc), authority: address(tc) },
    }
}

/// A state of every label in turn when covering, its data drawn.
pub fn state_value(tc: &TestCase, pick: Pick) -> MessageState {
    match pick.digit(tc, 4, 1) {
        0 => MessageState::Pending,
        1 => MessageState::Awaiting { receiver: receiver(tc) },
        2 => MessageState::Settled { receiver: (pick.digit(tc, 2, 20) == 0).then(|| receiver(tc)) },
        _ => MessageState::Dropped {
            reason: reason(tc, pick),
            receiver: (pick.digit(tc, 2, 20) == 0).then(|| receiver(tc)),
            watermark: tc.draw(gs::booleans()).then(|| timestamp(tc)),
        },
    }
}

/// The envelope, drawn or covered.
pub fn spec(tc: &TestCase, pick: Pick) -> MessageSpec {
    MessageSpec {
        sender: address(tc),
        inbox: address(tc),
        admitted_seq: tc.draw(gs::integers::<u64>().max_value(i64::MAX as u64)),
        relation: code_enum(tc, pick, &[Relation::Supervisor, Relation::Peer, Relation::Dependency, Relation::Dependee, Relation::System]),
        body: body(tc, pick),
        references: references(tc, pick),
        in_reply_to: pick.some(tc, 8).then(|| reference(tc)),
        supersedes: pick.some(tc, 4).then(|| reference(tc)),
        expectation: expectation(tc, pick),
        deadline: pick.some(tc, 8).then(|| timestamp(tc)),
        unbound_after: tc.draw(gs::integers::<i64>()),
        delivery_condition: pick.some(tc, 12).then(|| super::condition(tc)),
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<Message> {
    resource(spec(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

/// A fixed envelope for example tests: prose from `sender`, expecting a reply in `inbox`, no
/// references, deadline or delivery condition.
pub fn sample_spec() -> MessageSpec {
    let body = MessageBody::text("hello").expect("small body");
    MessageSpec { expectation: Expectation::Reply, ..MessageSpec::new(addr("sender"), addr("inbox"), Relation::Peer, body) }
}

/// A fixed message for example tests: [`sample_spec`], Pending, no history.
pub fn sample() -> Resource<Message> {
    resource(sample_spec(), State::new(MessageState::Pending, Timestamp(0)))
}

/// [`sample`] in another inbox.
pub fn sample_in(inbox: Address) -> Resource<Message> {
    sample().with_spec(MessageSpec { inbox, ..sample_spec() })
}

fn boxed(world: MemoryMessageWorld) -> Box<dyn MessageWorld> {
    Box::new(world)
}

/// Every edge the message's state may take, each with the evidence its gate reads: a delivery
/// with its receipt, a settlement by reply or by the outcome condition, and a drop for each
/// reason (an expiry just past the deadline, with the consumer stream applied past it).
pub fn candidates(tc: &TestCase, message: &Resource<Message>) -> Vec<Candidate<Message>> {
    let spec = message.spec();
    let now = timestamp(tc);
    fn own<K: flotilla_state_machines::Kind>() -> Ref<K> {
        Ref::new(NAME)
    }
    let mut candidates = Vec::new();

    let (receiver, transport) = (receiver(tc), transport(tc, Pick::Draw));
    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&own(), [(receiver.clone(), transport)].into());
    candidates.push((Deliver { receiver, transport }.into(), boxed(world), now));

    let mut world = MemoryMessageWorld::default();
    world.reply.put(&own(), ());
    candidates.push((Settle { by: Settlement::Answered }.into(), boxed(world), now));
    if let Expectation::Outcome { condition } = &spec.expectation {
        let mut world = MemoryMessageWorld::default();
        world.condition.put(&Ref::new(condition.to_string()), true);
        candidates.push((Settle { by: Settlement::OutcomeObserved }.into(), boxed(world), now));
    }

    let (by, authority) = (address(tc), address(tc));
    for reason in [
        DropReason::Withdrawn { by: by.clone(), authority: authority.clone() },
        DropReason::ConsumerUnreachable { by, authority: authority.clone() },
    ] {
        let mut world = MemoryMessageWorld::default();
        world.authority.put(&Ref::new(authority.as_str()), true);
        candidates.push((DropMessage { reason }.into(), boxed(world), now));
    }
    let newer = address(tc);
    let mut world = MemoryMessageWorld::default();
    world.supersession.put(&Ref::new(newer.as_str()), Supersedes { older: NAME.into() });
    candidates.push((DropMessage { reason: DropReason::Superseded { by: newer } }.into(), boxed(world), now));
    if let Some(deadline) = spec.deadline {
        let past = after(deadline, tc.draw(gs::integers::<i64>().min_value(1).max_value(3)));
        let mut world = MemoryMessageWorld::default();
        world.applied_through.put(&Ref::new(spec.inbox.as_str()), after(past, 1));
        candidates.push((DropMessage { reason: DropReason::Expired }.into(), boxed(world), past));
    }
    let mut world = MemoryMessageWorld::default();
    world.delivery_failure.put(&own(), ());
    candidates.push((DropMessage { reason: DropReason::Undeliverable }.into(), boxed(world), now));
    candidates
}

/// The edge the engine applies to `message` first among its candidates, led by `pick`.
pub fn edge(tc: &TestCase, pick: Pick, message: &Resource<Message>) -> Option<(Edge<Resource<Message>>, Resource<Message>)> {
    engine::drive(tc, pick, message, candidates(tc, message))
}

impl Fixture for Resource<Message> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<Message> {
        generate(tc, pick)
    }

    /// The envelope drawn afresh; the state and edge log kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Resource<Message> {
        self.clone().with_spec(spec(tc, pick))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<Message>>, Resource<Message>)> {
        edge(tc, pick, self)
    }

    fn stamped(&self, _before: Option<&Resource<Message>>, _version: u64) -> Resource<Message> {
        self.clone()
    }
}
