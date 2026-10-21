//! The engine's AwaitingReply (`Resource<AwaitingReply>`, declared by `AwaitingReplySpec`, kind
//! code `ar`): generators, and the worlds that let each edge pass its gate.

use flotilla_schema::{Edge, State};
use flotilla_state_machines::{
    machines::awaiting_reply::{
        Answer, Awaited, AwaitingReply, AwaitingReplySpec, AwaitingReplyState, AwaitingReplyWorld, Expire, MemoryAwaitingReplyWorld,
        Withdraw,
    },
    Ref, Resource,
};
use hegel::{generators as gs, TestCase};

use super::{
    address, code_enum,
    engine::{self, after, resource, Candidate},
    name, reference, text, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 4) and the awaited reply digit 1 (base 2); the
/// subject is present for shapes below 4 and the deadline for shapes below 8, so each is reached
/// both present and absent.
pub const COVERING: usize = 16;

pub fn state_value(tc: &TestCase, pick: Pick) -> AwaitingReplyState {
    match pick.digit(tc, 4, 1) {
        0 => AwaitingReplyState::Open,
        1 => AwaitingReplyState::Answered,
        2 => AwaitingReplyState::Expired,
        _ => AwaitingReplyState::Withdrawn { reason: text(tc) },
    }
}

/// The send's record, drawn or covered.
pub fn spec(tc: &TestCase, pick: Pick) -> AwaitingReplySpec {
    AwaitingReplySpec {
        message: reference(tc),
        sender: address(tc),
        recipient: address(tc),
        subject: pick.some(tc, 4).then(|| address(tc)),
        kind: name(tc),
        awaited: match pick.digit(tc, 2, 4) {
            0 => Awaited::Reply,
            _ => Awaited::Outcome,
        },
        deadline: pick.some(tc, 8).then(|| timestamp(tc)),
    }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<AwaitingReply> {
    resource(spec(tc, pick), State::new(state_value(tc, pick), timestamp(tc)))
}

fn boxed(world: MemoryAwaitingReplyWorld) -> Box<dyn AwaitingReplyWorld> {
    Box::new(world)
}

/// Every edge an open record may take, each with the evidence its gate reads: an answer by a
/// reply landed in the sender's inbox, an expiry just past the deadline with no reply there, and
/// an authorised withdrawal with a reason.
pub fn candidates(tc: &TestCase, record: &Resource<AwaitingReply>) -> Vec<Candidate<AwaitingReply>> {
    let spec = record.spec();
    let now = timestamp(tc);
    let mut candidates = Vec::new();

    let reply = reference(tc);
    let mut world = MemoryAwaitingReplyWorld::default();
    world.land(&reply, &spec.sender, Some(&spec.message));
    candidates.push((Answer { reply }.into(), boxed(world), now));

    if let Some(deadline) = spec.deadline {
        let past = after(deadline, tc.draw(gs::integers::<i64>().min_value(1).max_value(3)));
        candidates.push((Expire.into(), boxed(MemoryAwaitingReplyWorld::default()), past));
    }

    let authority = Ref::new(name(tc));
    let mut world = MemoryAwaitingReplyWorld::default();
    world.authority.put(&authority, true);
    let reason = code_enum(tc, Pick::Draw, &["superseded", "withdrawn", "ünïcødé ✓"]).to_owned();
    candidates.push((Withdraw { by: address(tc), reason, authority }.into(), boxed(world), now));
    candidates
}

impl Fixture for Resource<AwaitingReply> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<AwaitingReply> {
        generate(tc, pick)
    }

    /// The record drawn afresh; the state kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Resource<AwaitingReply> {
        self.clone().with_spec(spec(tc, pick))
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<AwaitingReply>>, Resource<AwaitingReply>)> {
        engine::drive(tc, pick, self, candidates(tc, self))
    }

    fn stamped(&self, _before: Option<&Resource<AwaitingReply>>, _version: u64) -> Resource<AwaitingReply> {
        self.clone()
    }
}
