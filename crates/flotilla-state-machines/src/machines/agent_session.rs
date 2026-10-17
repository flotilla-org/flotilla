//! AgentSession (ADR 0064's table, `specs/primitives/machines/agent_session.qnt`).
//!
//! A session is Live with a channel that attaches and detaches as observed,
//! until its end is observed: then it is Ended, carrying the reason, so a
//! listing shows whether it completed or was lost. The session's name is its
//! identity. Receipts are recorded on the session and refused once it has
//! ended. "Running" is a judgement, not a state.
//!
//! The stored kind is `Resource<AgentSession>` (code `as`), declared by
//! [`AgentSessionSpec`]: the state, the edge log, and the receipts, which
//! never change once appended, as `history`.

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{edges, states, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, Ref, Source, Table, Version, WrongState};

/// Whether a channel to a Live session is attached (spec `Channel`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Stored)]
pub enum Channel {
    #[store(code = "de")]
    Detached,
    #[store(code = "at")]
    Attached,
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum EndReason {
    #[store(code = "co")]
    Completed,
    #[store(code = "lo")]
    Lost,
}

states! {
    /// The spec's `State`, with Ended carrying its reason.
    pub enum AgentSessionState => AgentSessionLabel {
        Live { channel: Channel } = "li",
        Ended { reason: EndReason } = "en",
    }
}

impl AgentSessionState {
    /// The channel, while Live.
    pub fn channel(&self) -> Option<Channel> {
        match self {
            AgentSessionState::Live { channel } => Some(*channel),
            AgentSessionState::Ended { .. } => None,
        }
    }

    /// The reason, once Ended.
    pub fn end_reason(&self) -> Option<EndReason> {
        match self {
            AgentSessionState::Ended { reason } => Some(*reason),
            AgentSessionState::Live { .. } => None,
        }
    }
}

/// A channel to the session was observed present (spec `Attach`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub observed: Ref<ChannelPresence>,
}

/// The channel was observed absent (spec `Detach`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detach {
    pub observed: Ref<ChannelPresence>,
}

/// The session ended for `reason`, as observed (spec `End`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct End {
    pub reason: EndReason,
    pub observed: Ref<SessionEnd>,
}

edges! {
    pub enum AgentSessionEdge => AgentSessionEdgeKind {
        Attach(Attach) = "at",
        Detach(Detach) = "de",
        End(End) = "en",
    }
}

/// The observed end of a session and its reason; absent while it runs.
pub struct SessionEnd;
impl Kind for SessionEnd {
    const NAME: &'static str = "session_end";
    type Value = EndReason;
}

/// Whether a channel to the session is observed present.
pub struct ChannelPresence;
impl Kind for ChannelPresence {
    const NAME: &'static str = "channel_presence";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSessionRefusal {
    AlreadyEnded,
    WrongChannel,
    MissingEvidence,
}

impl From<Ended<AgentSessionLabel>> for AgentSessionRefusal {
    fn from(_: Ended<AgentSessionLabel>) -> Self {
        AgentSessionRefusal::AlreadyEnded
    }
}

/// Unreachable: every edge leaves Live, and Ended, the only other label, is
/// an end and refuses first.
impl From<WrongState<AgentSessionLabel, AgentSessionEdgeKind>> for AgentSessionRefusal {
    fn from(_: WrongState<AgentSessionLabel, AgentSessionEdgeKind>) -> Self {
        AgentSessionRefusal::AlreadyEnded
    }
}

/// What session gates read.
pub trait AgentSessionWorld: Source<SessionEnd> + Source<ChannelPresence> {}
impl<T: Source<SessionEnd> + Source<ChannelPresence> + ?Sized> AgentSessionWorld for T {}

/// An in-memory session world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryAgentSessionWorld {
    pub ends: Table<SessionEnd>,
    pub channels: Table<ChannelPresence>,
}

impl Source<SessionEnd> for MemoryAgentSessionWorld {
    fn lookup(&self, reference: &Ref<SessionEnd>) -> Option<(Version, &EndReason)> {
        self.ends.lookup(reference)
    }
}

impl Source<ChannelPresence> for MemoryAgentSessionWorld {
    fn lookup(&self, reference: &Ref<ChannelPresence>) -> Option<(Version, &bool)> {
        self.channels.lookup(reference)
    }
}

pub struct AgentSession;

/// A session's spec: nothing, since the session's name is its identity. It
/// declares the stored AgentSession (kind code `as`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Stored)]
#[store(kind = "AgentSession", code = "as", state = AgentSessionState)]
pub struct AgentSessionSpec {}

impl Machine for AgentSession {
    const NAME: &'static str = "AgentSession";
    type Request = ();
    type Spec = AgentSessionSpec;
    type State = AgentSessionState;
    type Edge = AgentSessionEdge;
    type Refusal = AgentSessionRefusal;
    type World = dyn AgentSessionWorld;
    const RECORDS_RECEIPTS: bool = true;

    fn definition() -> MachineDef<Self> {
        use AgentSessionLabel::*;
        MachineDef::new(Live)
            .edge(
                EdgeDef::<Self, Attach>::new()
                    .from([Live])
                    .to([Live])
                    .changes(["channel"])
                    .gate("the channel is detached and is observed present", gate_attach)
                    .effect(|_, _, _| AgentSessionState::Live { channel: Channel::Attached }),
            )
            .edge(
                EdgeDef::<Self, Detach>::new()
                    .from([Live])
                    .to([Live])
                    .changes(["channel"])
                    .gate("the channel is attached and is observed absent", gate_detach)
                    .effect(|_, _, _| AgentSessionState::Live { channel: Channel::Detached }),
            )
            .edge(
                EdgeDef::<Self, End>::new()
                    .from([Live])
                    .to([Ended])
                    .changes(["reason"])
                    .gate("the session's end is observed with this reason", gate_end)
                    .effect(|_, _, end| AgentSessionState::Ended { reason: end.reason }),
            )
            .end(Ended, "the session has ended")
    }

    fn admit(_: ()) -> Result<(AgentSessionSpec, AgentSessionState), AgentSessionRefusal> {
        Ok((AgentSessionSpec {}, AgentSessionState::Live { channel: Channel::Detached }))
    }
}

// Spec: `channel != Detached` refuses WrongChannel, then `o.channelPresent`.
fn gate_attach(h: &mut Handle<'_, AgentSession>, edge: &Attach) -> Result<(), AgentSessionRefusal> {
    if h.state().channel() != Some(Channel::Detached) {
        return Err(AgentSessionRefusal::WrongChannel);
    }
    if h.read(&edge.observed) == Some(&true) {
        Ok(())
    } else {
        Err(AgentSessionRefusal::MissingEvidence)
    }
}

// Spec: `channel != Attached` refuses WrongChannel, then `o.channelAbsent`.
// No observation is not an observed absence.
fn gate_detach(h: &mut Handle<'_, AgentSession>, edge: &Detach) -> Result<(), AgentSessionRefusal> {
    if h.state().channel() != Some(Channel::Attached) {
        return Err(AgentSessionRefusal::WrongChannel);
    }
    if h.read(&edge.observed) == Some(&false) {
        Ok(())
    } else {
        Err(AgentSessionRefusal::MissingEvidence)
    }
}

// Spec: `o.observed`, an end observation carrying the edge's reason.
fn gate_end(h: &mut Handle<'_, AgentSession>, edge: &End) -> Result<(), AgentSessionRefusal> {
    if h.read(&edge.observed) == Some(&edge.reason) {
        Ok(())
    } else {
        Err(AgentSessionRefusal::MissingEvidence)
    }
}
