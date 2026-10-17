//! AgentSession against `conformance/agent_session.qnt`.
//!
//! Evidence mapping onto a fresh [`MemoryAgentSessionWorld`]:
//! - `Attach{channelPresent}`: the channel presence reads `channelPresent`.
//! - `Detach{channelAbsent}`: the channel presence reads `not channelAbsent`
//!   (an observed absence is a `false` observation).
//! - `End{reason, observed}`: the session's end is observed with `reason` iff `observed`.
//! - `Receive{message, transport}`: the receive edge for message `m<message>`
//!   by that transport (`turn`, `notify` or `mute`); it reads nothing.
//!
//! State mapping, one to one: `Live { channel }` is `Live(channel)` and
//! `Ended { reason }` is `Ended(reason)`, with the same constructor names.

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::agent_session::{
        AgentSession, AgentSessionRefusal, AgentSessionState, Attach, Channel, Detach, End, EndReason, MemoryAgentSessionWorld, Receive,
    },
    Home, Ref, Transport, WriteError,
};
use serde_json::Value;

use crate::{
    graph::{self, bare, refusal, tick, Conformance, Report, Step},
    itf::{boolean, number, payload, tag, text, variant},
};

const NAME: &str = "s";

fn outcome<T>(result: Result<T, WriteError<AgentSession>>) -> Step {
    match result {
        Ok(_) => Step::Accepted,
        Err(WriteError::Refused(AgentSessionRefusal::AlreadyEnded)) => refusal("AlreadyEnded"),
        Err(WriteError::Refused(AgentSessionRefusal::WrongChannel)) => refusal("WrongChannel"),
        Err(WriteError::Refused(AgentSessionRefusal::MissingEvidence)) => refusal("MissingEvidence"),
        Err(other) => refusal(&format!("{other:?}")),
    }
}

struct SessionConformance;

impl Conformance for SessionConformance {
    const MACHINE: &'static str = "agent_session";
    type Subject = Home<AgentSession>;

    fn create(_: &Value) -> Home<AgentSession> {
        let mut home = Home::<AgentSession>::new("home").expect("valid declaration");
        home.create(NAME, (), &MemoryAgentSessionWorld::default(), Timestamp(0)).expect("created");
        home
    }

    fn apply(home: &mut Home<AgentSession>, event: &Value) -> Step {
        let o = payload(event);
        let mut world = MemoryAgentSessionWorld::default();
        let at = tick(1);
        match tag(event) {
            "Attach" => {
                world.channels.put(&Ref::new(NAME), boolean(&o["channelPresent"]));
                outcome(home.apply(NAME, Attach { observed: Ref::new(NAME) }, &world, at))
            }
            "Detach" => {
                world.channels.put(&Ref::new(NAME), !boolean(&o["channelAbsent"]));
                outcome(home.apply(NAME, Detach { observed: Ref::new(NAME) }, &world, at))
            }
            "End" => {
                let reason = match tag(&o["reason"]) {
                    "Completed" => EndReason::Completed,
                    "Lost" => EndReason::Lost,
                    other => panic!("end reason {other}"),
                };
                if boolean(&o["observed"]) {
                    world.ends.put(&Ref::new(NAME), reason);
                }
                outcome(home.apply(NAME, End { reason, observed: Ref::new(NAME) }, &world, at))
            }
            "Receive" => {
                let transport = match text(&o["transport"]) {
                    "turn" => Transport::Turn,
                    "notify" => Transport::Notify,
                    "mute" => Transport::Mute,
                    other => panic!("transport {other}"),
                };
                let message = format!("m{}", number(&o["message"]));
                outcome(home.apply(NAME, Receive { message, transport }, &world, at))
            }
            other => panic!("agent session event {other}"),
        }
    }

    fn encode(home: &Home<AgentSession>) -> Value {
        match home.get(NAME).expect("exists").state() {
            AgentSessionState::Live { channel } => variant(
                "Live",
                bare(match channel {
                    Channel::Detached => "Detached",
                    Channel::Attached => "Attached",
                }),
            ),
            AgentSessionState::Ended { reason } => variant(
                "Ended",
                bare(match reason {
                    EndReason::Completed => "Completed",
                    EndReason::Lost => "Lost",
                }),
            ),
        }
    }

    fn version(home: &Home<AgentSession>) -> u64 {
        home.get(NAME).expect("exists").version().0
    }
}

// Every reachable session state is reached in Rust by the spec's path, and
// every probe agrees: the same move to the same state value (an ended session
// carries the reason it ended for), or the same refusal with nothing recorded.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn agent_session_follows_the_full_graph() {
    let mut report = Report::new("agent_session");
    graph::check::<SessionConformance>(&graph::load(SessionConformance::MACHINE), &mut report);
    report.finish();
}
