//! AgentSession (ADR 0064's table, `specs/primitives/machines/agent_session.qnt`).
//!
//! A session is Live with a channel that attaches and detaches as observed,
//! until its end is observed: then it is Ended, carrying the reason, so a
//! listing shows whether it completed or was lost. The session's name is its
//! identity. A receipt is the session's [`Receive`] edge (R4, G11): Live to
//! the same Live, refused once the session has ended. Its master is the
//! message it is for, so the message's gates read the receipts about it.
//! "Running" is a judgement, not a state.
//!
//! The stored kind is `Resource<AgentSession>` (code `as`), declared by
//! [`AgentSessionSpec`]: the state and the edge log as `history`, receipts
//! among its edges.

use flotilla_schema::{Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    edges,
    machines::consumer::{Consumer, ConsumerKind},
    states, Admission, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, Ref, Source, Table, Transport, Version, WrongState,
};

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

/// The latest hook observation; absent until a hook reports activity.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct ActivityObservation {
    pub activity: Activity,
    pub activity_since: Timestamp,
    pub activity_cause: String,
}

states! {
    /// Lifecycle labels stay Live and Ended; activity is observed data.
    pub enum AgentSessionState => AgentSessionLabel {
        Live { channel: Channel, activity: Option<Activity>, activity_since: Option<Timestamp>, activity_cause: Option<String> } = "li",
        Ended { reason: EndReason, activity: Option<Activity>, activity_since: Option<Timestamp>, activity_cause: Option<String> } = "en",
    }
}

impl AgentSessionState {
    pub fn observation(&self) -> Option<ActivityObservation> {
        let (activity, activity_since, activity_cause) = match self {
            Self::Live { activity, activity_since, activity_cause, .. } | Self::Ended { activity, activity_since, activity_cause, .. } => {
                (activity, activity_since, activity_cause)
            }
        };
        Some(ActivityObservation { activity: (*activity)?, activity_since: (*activity_since)?, activity_cause: activity_cause.clone()? })
    }

    pub fn with_observation(mut self, observation: Option<ActivityObservation>) -> Self {
        let (activity, activity_since, activity_cause) = match &mut self {
            Self::Live { activity, activity_since, activity_cause, .. } | Self::Ended { activity, activity_since, activity_cause, .. } => {
                (activity, activity_since, activity_cause)
            }
        };
        *activity = observation.as_ref().map(|o| o.activity);
        *activity_since = observation.as_ref().map(|o| o.activity_since);
        *activity_cause = observation.map(|o| o.activity_cause);
        self
    }

    pub fn live(channel: Channel) -> Self {
        Self::Live { channel, activity: None, activity_since: None, activity_cause: None }
    }

    pub fn ended(reason: EndReason) -> Self {
        Self::Ended { reason, activity: None, activity_since: None, activity_cause: None }
    }

    /// The channel, while Live.
    pub fn channel(&self) -> Option<Channel> {
        match self {
            AgentSessionState::Live { channel, .. } => Some(*channel),
            AgentSessionState::Ended { .. } => None,
        }
    }

    /// The reason, once Ended.
    pub fn end_reason(&self) -> Option<EndReason> {
        match self {
            AgentSessionState::Ended { reason, .. } => Some(*reason),
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

/// The consumer took a turn for `message`, reached by `transport` (spec
/// `Receive`). The message is the edge's master: the log entry is about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receive {
    /// The message's name at its home. An address: plain interned text until
    /// a later layer adopts `flotilla-address`.
    pub message: String,
    pub transport: Transport,
}

edges! {
    pub enum AgentSessionEdge => AgentSessionEdgeKind {
        Attach(Attach) = "at",
        Detach(Detach) = "de",
        End(End) = "en",
        Receive(Receive) = "rv",
        Observe(Observe) = "ob",
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
    UnchangedActivity,
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

/// Activity observed by a harness hook, independent of session machine state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Activity {
    #[store(code = "wo")]
    Working,
    #[store(code = "id")]
    Idle,
    #[store(code = "in")]
    Interrupted,
}

/// Records a hook's activity only when it changes. Cause is the native event
/// name, never tool arguments, call identifiers or transcript contents.
/// Since and cause identify the first event in the current activity run:
/// later hooks with that activity are refused and preserve both fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observe {
    pub activity: Activity,
    pub cause: String,
}

/// The session's name is its identity (kind code `as`).
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

    fn definition() -> MachineDef<Self> {
        use AgentSessionLabel::*;
        MachineDef::new(Live)
            .edge(
                EdgeDef::<Self, Attach>::new()
                    .from([Live])
                    .to([Live])
                    .changes(["channel"])
                    .gate("the channel is detached and is observed present", gate_attach)
                    .effect(|_, state, _| AgentSessionState::live(Channel::Attached).with_observation(state.observation())),
            )
            .edge(
                EdgeDef::<Self, Detach>::new()
                    .from([Live])
                    .to([Live])
                    .changes(["channel"])
                    .gate("the channel is attached and is observed absent", gate_detach)
                    .effect(|_, state, _| AgentSessionState::live(Channel::Detached).with_observation(state.observation())),
            )
            .edge(
                EdgeDef::<Self, Receive>::new()
                    .from([Live])
                    .to([Live])
                    .about(|receive| &receive.message)
                    .ungated("a receipt reports a turn the consumer took; the message's own gates judge it")
                    .effect(|_, state, _| state),
            )
            .edge(
                EdgeDef::<Self, Observe>::new()
                    .from([Live])
                    .to([Live])
                    .changes(["activity", "activity_since", "activity_cause"])
                    .gate("the observed activity differs from the current activity", |h, edge| {
                        if h.state().observation().is_some_and(|old| old.activity == edge.activity) {
                            Err(AgentSessionRefusal::UnchangedActivity)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|h, state, edge| {
                        AgentSessionState::live(state.channel().expect("observe is Live only")).with_observation(Some(
                            ActivityObservation { activity: edge.activity, activity_since: h.now(), activity_cause: edge.cause.clone() },
                        ))
                    }),
            )
            .edge(
                EdgeDef::<Self, End>::new()
                    .from([Live])
                    .to([Ended])
                    .changes(["reason"])
                    .gate("the session's end is observed with this reason", gate_end)
                    .effect(|_, state, end| AgentSessionState::ended(end.reason).with_observation(state.observation())),
            )
            .end(Ended, "the session has ended")
    }

    fn admit(_: &mut Admission<'_, AgentSession>, _: ()) -> Result<(AgentSessionSpec, AgentSessionState), AgentSessionRefusal> {
        Ok((AgentSessionSpec::default(), AgentSessionState::live(Channel::Detached)))
    }
}

impl Consumer for AgentSession {
    const CONSUMER: ConsumerKind = ConsumerKind::AgentSession;
    const RECEIVE: AgentSessionEdgeKind = AgentSessionEdgeKind::Receive;

    fn receipt(edge: &AgentSessionEdge) -> Option<(&str, Transport)> {
        match edge {
            AgentSessionEdge::Receive(receive) => Some((&receive.message, receive.transport)),
            _ => None,
        }
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
