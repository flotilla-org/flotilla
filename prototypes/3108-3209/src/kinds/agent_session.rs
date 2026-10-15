//! Orthogonal conversation and channel dimensions; running is a judgement.
use crate::base::ObjRef;
use crate::home::Resource;
use crate::machine::{EdgeDef, GateFailure, Machine, MachineDef, Named, StateSet};
use crate::view::{Fact, Gate};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Conversation {
    Live,
    Ended,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    Detached,
    Attached,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionState {
    pub conversation: Conversation,
    pub channel: Channel,
}
const LD: SessionState = SessionState { conversation: Conversation::Live, channel: Channel::Detached };
const LA: SessionState = SessionState { conversation: Conversation::Live, channel: Channel::Attached };
const ED: SessionState = SessionState { conversation: Conversation::Ended, channel: Channel::Detached };
const EA: SessionState = SessionState { conversation: Conversation::Ended, channel: Channel::Attached };
impl Named for SessionState {
    fn name(&self) -> &'static str {
        match *self {
            LD => "LiveDetached",
            LA => "LiveAttached",
            ED => "EndedDetached",
            EA => "EndedAttached",
        }
    }
}
impl StateSet for SessionState {
    fn all() -> &'static [Self] {
        &[LD, LA, ED, EA]
    }
}
#[derive(Clone, Debug)]
pub enum EndReason {
    Completed,
    Lost,
}
impl EndReason {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Lost => "lost",
        }
    }
}
#[derive(Clone, Debug)]
pub enum SessionEdge {
    Attach { observed: ObjRef },
    Detach { observed: ObjRef },
    End { reason: EndReason, observed: ObjRef },
}
impl Named for SessionEdge {
    fn name(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::Detach { .. } => "detach",
            Self::End { .. } => "end",
        }
    }
}
#[derive(Clone, Debug)]
pub struct AgentSession;
fn gate_attach(g: &Gate<'_>, _: &Resource<AgentSession>, edge: &SessionEdge) -> Result<(), GateFailure> {
    let SessionEdge::Attach { observed } = edge else { unreachable!() };
    if matches!(g.observe(observed).map(|o| o.fact), Some(Fact::ChannelPresent(true))) {
        Ok(())
    } else {
        Err("channel not present".into())
    }
}
fn gate_detach(g: &Gate<'_>, _: &Resource<AgentSession>, edge: &SessionEdge) -> Result<(), GateFailure> {
    let SessionEdge::Detach { observed } = edge else { unreachable!() };
    if matches!(g.observe(observed).map(|o| o.fact), Some(Fact::ChannelPresent(false))) {
        Ok(())
    } else {
        Err("channel not absent".into())
    }
}
fn gate_end(g: &Gate<'_>, _: &Resource<AgentSession>, edge: &SessionEdge) -> Result<(), GateFailure> {
    let SessionEdge::End { reason, observed } = edge else { unreachable!() };
    if matches!(g.observe(observed).map(|o| o.fact), Some(Fact::ConversationEnded { reason: r }) if r == reason.name()) {
        Ok(())
    } else {
        Err("conversation end not observed".into())
    }
}
fn end_target(r: &Resource<AgentSession>, _: &SessionEdge) -> SessionState {
    SessionState { conversation: Conversation::Ended, ..r.state() }
}
impl Machine for AgentSession {
    const NAME: &'static str = "AgentSession";
    type Spec = ();
    type State = SessionState;
    type Edge = SessionEdge;
    fn initial() -> SessionState {
        LD
    }
    fn definition() -> MachineDef<Self> {
        MachineDef::new()
            .edge(EdgeDef::new("attach").from([LD]).to(LA).gate("channel present", gate_attach))
            .edge(EdgeDef::new("detach").from([LA]).to(LD).gate("channel absent", gate_detach))
            .edge(EdgeDef::new("end").from([LD, LA]).to_chosen(vec![ED, EA], end_target).gate("conversation ended", gate_end))
            .end(ED, "conversation ended")
            .end(EA, "conversation ended")
    }
}
