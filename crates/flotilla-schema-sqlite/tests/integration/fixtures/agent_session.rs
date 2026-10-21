//! The engine's AgentSession (`Resource<AgentSession>`, declared by `AgentSessionSpec`, kind code
//! `as`): generators, and the worlds that let each edge pass its gate.

use flotilla_schema::{Edge, State, Timestamp};
use flotilla_state_machines::{
    machines::agent_session::{
        Activity, ActivityObservation, AgentSession, AgentSessionSpec, AgentSessionState, AgentSessionWorld, Attach, Channel, Detach, End,
        EndReason, MemoryAgentSessionWorld, Observe, Receive,
    },
    Ref, Resource, Transport,
};
use hegel::TestCase;

use super::{
    code_enum,
    engine::{self, resource, Candidate},
    name, timestamp, Fixture, Pick,
};

/// Covering indexes span lifecycle, channel/end reason and absent/all three activities.
pub const COVERING: usize = 64;

pub fn state_value(tc: &TestCase, pick: Pick) -> AgentSessionState {
    let state = match (pick.digit(tc, 2, 1), pick.digit(tc, 2, 2)) {
        (0, 0) => AgentSessionState::live(Channel::Detached),
        (0, _) => AgentSessionState::live(Channel::Attached),
        (_, 0) => AgentSessionState::ended(EndReason::Completed),
        _ => AgentSessionState::ended(EndReason::Lost),
    };
    let observation = match pick.digit(tc, 4, 4) {
        0 => None,
        digit => Some(ActivityObservation {
            activity: [Activity::Working, Activity::Idle, Activity::Interrupted][digit - 1],
            activity_since: timestamp(tc),
            activity_cause: name(tc),
        }),
    };
    state.with_observation(observation)
}

/// A receipt for a drawn message, by each transport in turn.
pub fn receive(tc: &TestCase, pick: Pick) -> Receive {
    Receive { message: name(tc), transport: code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Mute]) }
}

pub fn generate(tc: &TestCase, pick: Pick) -> Resource<AgentSession> {
    resource(AgentSessionSpec::default(), State::new(state_value(tc, pick), timestamp(tc)))
}

/// A live, detached session with no history.
pub fn sample() -> Resource<AgentSession> {
    resource(AgentSessionSpec::default(), State::new(AgentSessionState::live(Channel::Detached), Timestamp(0)))
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
    for activity in [Activity::Working, Activity::Idle, Activity::Interrupted] {
        candidates.push((Observe { activity, cause: name(tc) }.into(), boxed(MemoryAgentSessionWorld::default()), now));
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

    fn stamped(&self, _before: Option<&Resource<AgentSession>>, _version: u64) -> Resource<AgentSession> {
        self.clone()
    }
}
