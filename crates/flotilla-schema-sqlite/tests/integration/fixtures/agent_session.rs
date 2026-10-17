//! The engine's AgentSession (`Resource<AgentSession>`, declared by `AgentSessionSpec`, kind code
//! `as`): generators, and the worlds that let each edge pass its gate.

use flotilla_schema::{Edge, State, Timestamp};
use flotilla_state_machines::{
    machines::agent_session::{
        AgentSession, AgentSessionEdge, AgentSessionSpec, AgentSessionState, AgentSessionWorld, Attach, Channel, Detach, End, EndReason,
        MemoryAgentSessionWorld, Receive,
    },
    Ref, Resource, Transport,
};
use hegel::TestCase;

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, records, stamp_flat, timestamp, Fixture, Pick,
};

/// Covering indexes: the label is digit 0 (base 2), and the channel or end reason digit 1.
pub const COVERING: usize = 16;

pub fn state_value(tc: &TestCase, pick: Pick) -> AgentSessionState {
    match (pick.digit(tc, 2, 1), pick.digit(tc, 2, 2)) {
        (0, 0) => AgentSessionState::Live { channel: Channel::Detached },
        (0, _) => AgentSessionState::Live { channel: Channel::Attached },
        (_, 0) => AgentSessionState::Ended { reason: EndReason::Completed },
        _ => AgentSessionState::Ended { reason: EndReason::Lost },
    }
}

/// A receipt for a drawn message, by each transport in turn.
pub fn receive(tc: &TestCase, pick: Pick) -> Receive {
    Receive { message: name(tc), transport: code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute]) }
}

fn edges(tc: &TestCase, pick: Pick) -> Vec<AgentSessionEdge> {
    let observed = || Ref::new(name(tc));
    vec![
        Attach { observed: observed() }.into(),
        Detach { observed: observed() }.into(),
        End { reason: EndReason::Completed, observed: Ref::new(name(tc)) }.into(),
        End { reason: EndReason::Lost, observed: Ref::new(name(tc)) }.into(),
        receive(tc, pick).into(),
    ]
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<AgentSession> {
    resource(AgentSessionSpec {}, State::new(state_value(tc, pick), timestamp(tc)))
        .with_events(records(tc, pick, 2, |pick| engine::event::<AgentSession>(tc, pick, &edges(tc, pick))))
}

/// A live, detached session with no history.
pub fn sample() -> Resource<AgentSession> {
    resource(AgentSessionSpec {}, State::new(AgentSessionState::Live { channel: Channel::Detached }, Timestamp(0)))
}

fn boxed(world: MemoryAgentSessionWorld) -> Box<dyn AgentSessionWorld> {
    Box::new(world)
}

/// Every edge a live session may take: attaching or detaching on an observed channel, a receipt,
/// and ending for each reason as observed.
pub fn candidates(tc: &TestCase, pick: Pick) -> Vec<Candidate<AgentSession>> {
    let now = timestamp(tc);
    let mut candidates = Vec::new();
    for present in [true, false] {
        let observed = Ref::new(name(tc));
        let mut world = MemoryAgentSessionWorld::default();
        world.channels.put(&observed, present);
        let edge = if present { Attach { observed }.into() } else { Detach { observed }.into() };
        candidates.push((edge, boxed(world), now));
    }
    for reason in [EndReason::Completed, EndReason::Lost] {
        let observed = Ref::new(name(tc));
        let mut world = MemoryAgentSessionWorld::default();
        world.ends.put(&observed, reason);
        candidates.push((End { reason, observed }.into(), boxed(world), now));
    }
    candidates.push((receive(tc, pick).into(), boxed(MemoryAgentSessionWorld::default()), now));
    candidates
}

impl Fixture for Resource<AgentSession> {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Resource<AgentSession> {
        generate(tc, pick)
    }

    /// A session has no spec: a spec edit writes the same value.
    fn evolve(&self, _tc: &TestCase, _pick: Pick) -> Resource<AgentSession> {
        self.clone()
    }

    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Resource<AgentSession>>, Resource<AgentSession>)> {
        engine::drive(tc, pick, self, candidates(tc, pick))
    }

    fn stamped(&self, before: Option<&Resource<AgentSession>>, version: u64) -> Resource<AgentSession> {
        self.clone().with_events(stamp_flat(self.events(), before.map(|before| before.events()), version))
    }
}
