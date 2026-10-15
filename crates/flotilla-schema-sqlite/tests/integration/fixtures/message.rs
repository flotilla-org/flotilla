//! The Message-shaped fixture, transcribed (not imported) from
//! `flotilla_resources::message::MessageSpec`, `flotilla_protocol::{Leaf, MessageReference}` and
//! the #3108 prototype's Message machine (`MessageState`, `MessageEdge`, `DropReason`,
//! `Settlement`, `Transport`) and attempts (`attempt.rs`).
//!
//! Modelling decisions the storage list forces:
//! - The prototype's `ObjRef` becomes `Ref<K>` where the target kind is known (`Settlement::Answered`,
//!   `DropReason::Superseded`, the deliver receipt) and `Identity` where it is not (an authority, an
//!   observed condition, a reference's repository, a gate input).
//! - `receiver` on the spec is the prototype's `inbox: Ref<Inbox>`; the deliver edge keeps the
//!   receiving session's name, interned through the `AgentName` newtype.
//! - `references` is a child table of the seven-variant reference enum; `subject` keeps one
//!   reference as an optional column, as the spec does.
//! - Edge events are `history` (#3320 ruling 2): each row is the edge (`Drop { reason: Undeliverable
//!   { cause } }` nests a data enum in a data enum), when and by whom it was written, and a child
//!   table of the gate's inputs. Attempts are `history` too.
//!
//! Serde attributes are the resource types' own; storage ignores them (codes are explicit).

use chrono::{DateTime, Utc};
use flotilla_schema::{Identity, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

pub use super::attempt::{Attempt, Cause};
use super::{
    code_enum, evolve_records, floor, identity, list, name, records, reference, seq, stamp, text, time, timestamp, Fixture, Pick, SHAPES,
};

/// A minimal target kind for `Message::inbox`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Inbox", code = 2)]
pub struct Inbox {
    pub role: String,
}

/// A minimal target kind for the deliver edge's receipt.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "AgentSession", code = 3)]
pub struct AgentSession {
    pub harness: String,
}

/// Indexes from #3208 §4.2: an inbox's open messages in acceptance order, a message's replies,
/// and open messages with a deadline in deadline order.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "Message",
    code = 1,
    index(name = "open_by_inbox", eq(inbox), order(accepted_seq), when(state in [Pending, Awaiting])),
    index(name = "by_reply", eq(in_reply_to), when(in_reply_to is_some)),
    index(name = "deadline", order(deadline), when(state in [Pending, Awaiting], deadline is_some))
)]
pub struct Message {
    #[store(intern)]
    pub sender: String,
    pub inbox: Ref<Inbox>,
    pub relation: MessageRelation,
    pub body: String,
    pub body_digest: Option<String>,
    #[store(child)]
    pub references: Vec<MessageReference>,
    pub subject: Option<MessageReference>,
    pub expectation: MessageExpectation,
    pub in_reply_to: Option<Ref<Message>>,
    pub deadline: Option<DateTime<Utc>>,
    pub supersedes: Option<Ref<Message>>,
    pub interrupting: bool,
    #[store(flatten)]
    pub delivery_condition: Option<Leaf>,
    pub accepted_seq: u64,
    pub ordinal: u16,
    #[store(state)]
    pub state: State<MessageState>,
    #[store(history)]
    pub edges: Records<MessageEdgeEvent>,
    #[store(history)]
    pub attempts: Records<Attempt<String>>,
}

/// One edge event: the edge, when and by whom it was written, and what its gate relied on.
#[derive(Debug, Clone, PartialEq, Stored)]
pub struct MessageEdgeEvent {
    pub edge: MessageEdge,
    pub at: Timestamp,
    #[store(intern)]
    pub writer: String,
    #[store(child)]
    pub inputs: Vec<GateInput>,
}

/// A resource the gate observed, at the version it observed.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct GateInput {
    pub observed: Identity,
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(rename_all = "snake_case")]
pub enum MessageRelation {
    #[store(code = 1)]
    Supervisor,
    #[store(code = 2)]
    Peer,
    #[store(code = 3)]
    Dependency,
    #[store(code = 4)]
    Dependee,
    #[store(code = 5)]
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageExpectation {
    #[store(code = 1)]
    None,
    #[store(code = 2)]
    Reply,
    #[store(code = 3)]
    Outcome {
        #[store(flatten)]
        condition: Leaf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Leaf {
    pub address: LeafAddress,
    pub field_path: String,
    pub operator: LeafOperator,
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum LeafAddress {
    #[store(code = 1)]
    Convoy { name: String },
    #[store(code = 2)]
    Vessel { name: String },
    #[store(code = 3)]
    Work { convoy: String, work: String },
    #[store(code = 4)]
    ChangeRequest {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
    },
    #[store(code = 5)]
    Issue {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
    },
    #[store(code = 6)]
    Usage { provider: String, account: String },
    #[store(code = 7)]
    Artifact { convoy: String, producer: String, kind: String, subject: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum LeafOperator {
    #[serde(rename = "==")]
    #[store(code = 1)]
    Equal,
    #[serde(rename = "!=")]
    #[store(code = 2)]
    NotEqual,
    #[serde(rename = "<")]
    #[store(code = 3)]
    LessThan,
    #[serde(rename = "<=")]
    #[store(code = 4)]
    LessThanOrEqual,
    #[serde(rename = ">")]
    #[store(code = 5)]
    GreaterThan,
    #[serde(rename = ">=")]
    #[store(code = 6)]
    GreaterThanOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageReference {
    #[store(code = 1)]
    ChangeRequest {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
        revision: String,
    },
    #[store(code = 2)]
    Commit { repository: Identity, revision: String },
    #[store(code = 3)]
    Ref { repository: Identity, name: String, revision: String },
    #[store(code = 4)]
    Artifact { resource: Identity, revision: String },
    #[store(code = 5)]
    Issue {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
        revision: String,
    },
    #[store(code = 6)]
    Comment {
        #[store(intern)]
        service: String,
        scope: String,
        id: String,
        revision: String,
    },
    #[store(code = 7)]
    ControlRecord { resource: Identity, revision: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum MessageState {
    #[store(code = 1)]
    Pending,
    #[store(code = 2)]
    Awaiting,
    #[store(code = 3)]
    Settled,
    #[store(code = 4)]
    Dropped,
}

/// A session name: a newtype of `String`, interned where it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct AgentName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum Transport {
    #[store(code = 1)]
    Turn,
    #[store(code = 2)]
    Notify,
    #[store(code = 3)]
    Muted,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum MessageEdge {
    /// `receipt` is the receipt row on the consumer's AgentSession; `transport` must match it.
    #[store(code = 1)]
    Deliver {
        #[store(intern)]
        receiver: AgentName,
        receipt: Ref<AgentSession>,
        transport: Transport,
    },
    #[store(code = 2)]
    Settle { by: Settlement },
    #[store(code = 3)]
    Drop { reason: DropReason },
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum Settlement {
    #[store(code = 1)]
    Answered { reply: Ref<Message> },
    #[store(code = 2)]
    OutcomeObserved { observation: Identity },
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum DropReason {
    #[store(code = 1)]
    Undeliverable {
        #[store(flatten)]
        cause: Cause,
    },
    #[store(code = 2)]
    Expired,
    #[store(code = 3)]
    Withdrawn {
        #[store(intern)]
        by: String,
        authority: Identity,
    },
    #[store(code = 4)]
    ConsumerUnreachable {
        #[store(intern)]
        by: String,
        authority: Identity,
    },
    #[store(code = 5)]
    Superseded { by: Ref<Message> },
}

/// Structural shapes: indexes `0..SHAPES` reach, while their enclosing variants are active, every
/// `LeafAddress` under `Outcome` and under a present `delivery_condition` (0..7), every
/// `MessageReference` (0..7), every edge (`shape % 3`) with every `Settlement` and `DropReason`
/// (`shape / 3`), and every unit variant and absent option (7 and above). Collections take every
/// size up to their maximum in turn, and their elements shifted shapes, so the reference and edge
/// tables also hold every variant. Five rounds let code enums take every code at every position,
/// including `CauseCategory`, which only an `Undeliverable` drop or a failed attempt holds.
pub const COVERING: usize = SHAPES * 5;

impl Leaf {
    pub fn generate(tc: &TestCase, pick: Pick) -> Leaf {
        use LeafOperator::*;
        Leaf {
            address: LeafAddress::generate(tc, pick),
            field_path: text(tc),
            operator: code_enum(tc, pick, &[Equal, NotEqual, LessThan, LessThanOrEqual, GreaterThan, GreaterThanOrEqual]),
            literal: text(tc),
        }
    }
}

impl LeafAddress {
    fn generate(tc: &TestCase, pick: Pick) -> LeafAddress {
        match pick.variant(tc, 7, |index| index) {
            0 => LeafAddress::Convoy { name: text(tc) },
            1 => LeafAddress::Vessel { name: text(tc) },
            2 => LeafAddress::Work { convoy: text(tc), work: text(tc) },
            3 => LeafAddress::ChangeRequest { service: name(tc), scope: text(tc), number: seq(tc) },
            4 => LeafAddress::Issue { service: name(tc), scope: text(tc), number: seq(tc) },
            5 => LeafAddress::Usage { provider: text(tc), account: text(tc) },
            _ => LeafAddress::Artifact { convoy: text(tc), producer: text(tc), kind: text(tc), subject: text(tc) },
        }
    }
}

impl MessageReference {
    fn generate(tc: &TestCase, pick: Pick) -> MessageReference {
        match pick.variant(tc, 7, |index| index) {
            0 => MessageReference::ChangeRequest { service: name(tc), scope: text(tc), number: seq(tc), revision: text(tc) },
            1 => MessageReference::Commit { repository: identity(tc), revision: text(tc) },
            2 => MessageReference::Ref { repository: identity(tc), name: text(tc), revision: text(tc) },
            3 => MessageReference::Artifact { resource: identity(tc), revision: text(tc) },
            4 => MessageReference::Issue { service: name(tc), scope: text(tc), number: seq(tc), revision: text(tc) },
            5 => MessageReference::Comment { service: name(tc), scope: text(tc), id: text(tc), revision: text(tc) },
            _ => MessageReference::ControlRecord { resource: identity(tc), revision: text(tc) },
        }
    }
}

impl MessageEdge {
    pub fn generate(tc: &TestCase, pick: Pick) -> MessageEdge {
        match pick.variant(tc, 3, |index| index) {
            0 => MessageEdge::Deliver {
                receiver: AgentName(name(tc)),
                receipt: reference(tc),
                transport: code_enum(tc, pick, &[Transport::Turn, Transport::Notify, Transport::Muted]),
            },
            1 => MessageEdge::Settle {
                by: match pick.variant(tc, 2, |index| index / 3) {
                    0 => Settlement::Answered { reply: reference(tc) },
                    _ => Settlement::OutcomeObserved { observation: identity(tc) },
                },
            },
            _ => MessageEdge::Drop {
                reason: match pick.variant(tc, 5, |index| index / 3) {
                    0 => DropReason::Undeliverable { cause: Cause::generate(tc, pick) },
                    1 => DropReason::Expired,
                    2 => DropReason::Withdrawn { by: name(tc), authority: identity(tc) },
                    3 => DropReason::ConsumerUnreachable { by: name(tc), authority: identity(tc) },
                    _ => DropReason::Superseded { by: reference(tc) },
                },
            },
        }
    }
}

impl Message {
    /// Spans every field's boundaries: empty, quoting and colliding text, absent and present
    /// options, the whole storable `u64` range, times with sub-microsecond digits either side of
    /// the epoch, the whole `u16` and `i64` ranges, references to missing targets at two homes, and
    /// every enum variant (drawn, or reached by the covering indexes; see [`COVERING`]).
    pub fn generate(tc: &TestCase, pick: Pick) -> Message {
        use MessageRelation::*;
        use MessageState::*;
        Message {
            sender: name(tc),
            inbox: reference(tc),
            relation: code_enum(tc, pick, &[Supervisor, Peer, Dependency, Dependee, System]),
            body: text(tc),
            body_digest: pick.some(tc, 8).then(|| text(tc)),
            references: list(tc, pick, 3, |pick| MessageReference::generate(tc, pick)),
            subject: pick.some(tc, 7).then(|| MessageReference::generate(tc, pick)),
            expectation: match pick.variant(tc, 3, |index| if index < 7 { 2 } else { index }) {
                0 => MessageExpectation::None,
                1 => MessageExpectation::Reply,
                _ => MessageExpectation::Outcome { condition: Leaf::generate(tc, pick) },
            },
            in_reply_to: pick.some(tc, 8).then(|| reference(tc)),
            deadline: pick.some(tc, 8).then(|| time(tc)),
            supersedes: pick.some(tc, 9).then(|| reference(tc)),
            interrupting: tc.draw(gs::booleans()),
            delivery_condition: pick.some(tc, 7).then(|| Leaf::generate(tc, pick)),
            accepted_seq: seq(tc),
            ordinal: tc.draw(gs::integers::<u16>()),
            state: State::new(code_enum(tc, pick, &[Pending, Awaiting, Settled, Dropped]), Timestamp(tc.draw(gs::integers::<i64>()))),
            edges: records(tc, pick, 3, |pick| MessageEdgeEvent::generate(tc, pick)),
            attempts: records(tc, pick, 2, |pick| Attempt::generate(tc, pick, text(tc))),
        }
    }

    /// A fixed message for example tests: no options, no edge, a pending state.
    pub fn sample() -> Message {
        Message {
            sender: "alice".into(),
            inbox: Ref::new("inbox", super::LOCAL),
            relation: MessageRelation::Peer,
            body: "b".into(),
            body_digest: None,
            references: Vec::new(),
            subject: None,
            expectation: MessageExpectation::None,
            in_reply_to: None,
            deadline: None,
            supersedes: None,
            interrupting: false,
            delivery_condition: None,
            accepted_seq: 0,
            ordinal: 0,
            state: State::new(MessageState::Pending, Timestamp(0)),
            edges: Records::new(),
            attempts: Records::new(),
        }
    }
}

impl Fixture for Message {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Message {
        Message::generate(tc, pick)
    }

    /// The spec drawn afresh (references and all), edge events and attempts appended.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Message {
        let spec = Message::generate(tc, pick);
        Message {
            edges: evolve_records(tc, pick, &self.edges, |edge, _| edge.clone(), |pick| MessageEdgeEvent::generate(tc, pick)),
            attempts: evolve_records(tc, pick, &self.attempts, |attempt, _| attempt.clone(), |pick| Attempt::generate(tc, pick, text(tc))),
            ..spec
        }
    }

    fn stored(&self) -> Message {
        Message { deadline: self.deadline.map(floor), ..self.clone() }
    }

    fn stamped(&self, before: Option<&Message>, version: u64) -> Message {
        Message {
            edges: stamp(&self.edges, before.map(|before| &before.edges), version, |edge, _| edge.clone()),
            attempts: stamp(&self.attempts, before.map(|before| &before.attempts), version, |attempt, _| attempt.clone()),
            ..self.clone()
        }
    }
}

impl MessageEdgeEvent {
    pub fn generate(tc: &TestCase, pick: Pick) -> MessageEdgeEvent {
        MessageEdgeEvent {
            edge: MessageEdge::generate(tc, pick),
            at: timestamp(tc),
            writer: name(tc),
            inputs: list(tc, pick, 2, |_| GateInput { observed: identity(tc), version: seq(tc) }),
        }
    }
}
