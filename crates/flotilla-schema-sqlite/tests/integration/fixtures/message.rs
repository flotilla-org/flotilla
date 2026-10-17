//! The engine's Message (`flotilla_state_machines::machines::message::MessageRow`, kind code `ms`):
//! generators, and the worlds that let each edge pass its gate.

use flotilla_schema::{Edge, Records, State, Timestamp};
use flotilla_state_machines::{
    machines::message::{
        Deliver, DropMessage, DropReason, Expectation, MemoryMessageWorld, Message, MessageEdge, MessageState, MessageWorld, Receiver,
        Settle, Settlement, Supersedes,
    },
    Ref, Transport,
};
use hegel::{generators as gs, TestCase};

use super::{
    code_enum,
    engine::{self, after, Candidate, NAME},
    name, records, stamp_flat, timestamp, Fixture, MessageRow, Pick,
};

/// Covering indexes: the label is digit 0 (base 4), a drop's reason digit 1 (base 5) and whether
/// a settlement or a drop kept a receiver digit 2 (base 2), so every reason is reached with and
/// without one; the expectation cycles in base 3.
pub const COVERING: usize = 80;

pub fn receiver(tc: &TestCase) -> Receiver {
    Receiver { session: name(tc), host: name(tc) }
}

fn transport(tc: &TestCase, pick: Pick) -> Transport {
    code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute])
}

pub fn expectation(tc: &TestCase, pick: Pick) -> Expectation {
    match pick.digit(tc, 3, 1) {
        0 => Expectation::Nothing,
        1 => Expectation::Reply,
        _ => Expectation::Outcome { condition: Ref::new(name(tc)) },
    }
}

pub fn reason(tc: &TestCase, pick: Pick) -> DropReason {
    match pick.digit(tc, 5, 4) {
        0 => DropReason::Undeliverable,
        1 => DropReason::Expired,
        2 => DropReason::Superseded { by: Ref::new(name(tc)) },
        3 => DropReason::Withdrawn { by: name(tc), authority: Ref::new(name(tc)) },
        _ => DropReason::ConsumerUnreachable { by: name(tc), authority: Ref::new(name(tc)) },
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

/// One edge of every kind and argument variant, for generated history.
fn edges(tc: &TestCase, pick: Pick) -> Vec<MessageEdge> {
    let mut edges = vec![
        Deliver { receiver: receiver(tc), transport: transport(tc, pick) }.into(),
        Settle { by: Settlement::Answered }.into(),
        Settle { by: Settlement::OutcomeObserved }.into(),
    ];
    edges.extend((0..5).map(|n| MessageEdge::from(DropMessage { reason: reason(tc, Pick::Cover(n * 4)) })));
    edges
}

pub fn generate(tc: &TestCase, pick: Pick) -> MessageRow {
    MessageRow {
        inbox: name(tc),
        expectation: expectation(tc, pick),
        deadline: pick.some(tc, 8).then(|| timestamp(tc)),
        unbound_after: tc.draw(gs::integers::<i64>()),
        state: State::new(state_value(tc, pick), timestamp(tc)),
        edges: records(tc, pick, 3, |pick| engine::event::<Message>(tc, pick, &edges(tc, pick))),
    }
}

/// A fixed message for example tests: expecting a reply, no deadline, Pending, no history.
pub fn sample() -> MessageRow {
    MessageRow {
        inbox: "inbox".into(),
        expectation: Expectation::Reply,
        deadline: None,
        unbound_after: 0,
        state: State::new(MessageState::Pending, Timestamp(0)),
        edges: Records::new(),
    }
}

fn boxed(world: MemoryMessageWorld) -> Box<dyn MessageWorld> {
    Box::new(world)
}

/// Every edge the message's state may take, each with the evidence its gate reads: a delivery
/// with its receipt, a settlement by reply or by the outcome condition, and a drop for each
/// reason (an expiry just past the deadline, with the consumer stream applied past it).
pub fn candidates(tc: &TestCase, row: &MessageRow) -> Vec<Candidate<Message>> {
    let now = timestamp(tc);
    fn own<K: flotilla_state_machines::Kind>() -> Ref<K> {
        Ref::new(NAME)
    }
    let mut candidates = Vec::new();

    let (receiver, transport) = (receiver(tc), transport(tc, Pick::Draw));
    let mut world = MemoryMessageWorld::default();
    world.receipts.put(&own(), [(receiver.session.clone(), transport)].into());
    candidates.push((Deliver { receiver, transport }.into(), boxed(world), now));

    let mut world = MemoryMessageWorld::default();
    world.reply.put(&own(), ());
    candidates.push((Settle { by: Settlement::Answered }.into(), boxed(world), now));
    if let Expectation::Outcome { condition } = &row.expectation {
        let mut world = MemoryMessageWorld::default();
        world.condition.put(condition, true);
        candidates.push((Settle { by: Settlement::OutcomeObserved }.into(), boxed(world), now));
    }

    let (by, authority) = (name(tc), Ref::new(name(tc)));
    for reason in [
        DropReason::Withdrawn { by: by.clone(), authority: authority.clone() },
        DropReason::ConsumerUnreachable { by, authority: authority.clone() },
    ] {
        let mut world = MemoryMessageWorld::default();
        world.authority.put(&authority, true);
        candidates.push((DropMessage { reason }.into(), boxed(world), now));
    }
    let newer = Ref::new(name(tc));
    let mut world = MemoryMessageWorld::default();
    world.supersession.put(&newer, Supersedes { older: NAME.into() });
    candidates.push((DropMessage { reason: DropReason::Superseded { by: newer } }.into(), boxed(world), now));
    if let Some(deadline) = row.deadline {
        let past = after(deadline, tc.draw(gs::integers::<i64>().min_value(1).max_value(3)));
        let mut world = MemoryMessageWorld::default();
        world.applied_through.put(&Ref::new(row.inbox.clone()), after(past, 1));
        candidates.push((DropMessage { reason: DropReason::Expired }.into(), boxed(world), past));
    }
    let mut world = MemoryMessageWorld::default();
    world.delivery_failure.put(&own(), ());
    candidates.push((DropMessage { reason: DropReason::Undeliverable }.into(), boxed(world), now));
    candidates
}

/// The edge the engine applies to `row` first among its candidates, led by `pick`.
pub fn edge(tc: &TestCase, pick: Pick, row: &MessageRow) -> Option<(Edge<MessageRow>, MessageRow)> {
    engine::drive(tc, pick, row, candidates(tc, row))
}

impl Fixture for MessageRow {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> MessageRow {
        generate(tc, pick)
    }

    /// The envelope drawn afresh; the state and edge log kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> MessageRow {
        MessageRow { state: self.state.clone(), edges: self.edges.clone(), ..generate(tc, pick) }
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<MessageRow>, MessageRow)> {
        edge(tc, pick, self)
    }

    fn stamped(&self, before: Option<&MessageRow>, version: u64) -> MessageRow {
        MessageRow { edges: stamp_flat(&self.edges, before.map(|before| &before.edges), version), ..self.clone() }
    }
}
