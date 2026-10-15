//! The Message-shaped fixture, transcribed (not imported) from
//! `flotilla_resources::message::MessageSpec`, `flotilla_protocol::{Leaf, MessageReference}` and
//! the #3108 prototype's Message machine (`MessageState`, `MessageEdge`, `DropReason`,
//! `Settlement`, `Transport`) and attempts (`attempt.rs`).
//!
//! Modelling decisions the storage list forces:
//! - The prototype's `ObjRef` becomes `Ref<K>` where the target kind is known (`Settlement::Answered`,
//!   `DropReason::Superseded`, the deliver receipt) and `Identity` where it is not (an authority, an
//!   observed condition, a reference's repository, a gate read).
//! - `receiver` on the spec is the prototype's `inbox: Ref<Inbox>`; the deliver edge keeps the
//!   receiving session's name, interned through the `AgentName` newtype.
//! - `references` is a child table of the seven-variant reference enum; `subject` keeps one
//!   reference as an optional column, as the spec does.
//! - Edge events are `history` (#3320 ruling 2), one row per event (ruling of 2026-10-15): the
//!   [`EdgeEvent`] header (edge, from- and to-label, writer, time) and one inline CBOR document of
//!   the edge's arguments (`Drop { reason: Undeliverable { cause } }` nests a data enum in a data
//!   enum) and the gate's reads. Attempts are `history` too.
//! - The sender is an `AgentName`: a newtype of `String`, interned.
//!
//! Serde attributes are the resource types' own; storage ignores them (codes are explicit).

use chrono::{DateTime, Utc};
use flotilla_schema::{Edge, EdgeEvent, Identity, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

pub use super::attempt::{Attempt, Cause};
use super::{
    appended, code_enum, event, floor, identity, list, name, payload, records, reference, seq, stamp, text, time, timestamp, Fixture,
    Payload, Pick, SHAPES,
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
    pub sender: AgentName,
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

/// One edge event: the header, and the edge's arguments and gate reads as one document.
pub type MessageEdgeEvent = EdgeEvent<MessageState, MessageEdgeName, Payload<MessageEdge>>;

/// The edge column of the message's edge log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum MessageEdgeName {
    #[store(code = 1)]
    Deliver,
    #[store(code = 2)]
    Settle,
    #[store(code = 3)]
    Drop,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct AgentName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    Turn,
    Notify,
    Muted,
}

/// The edges' arguments: serde only, stored inside each event's document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageEdge {
    /// `receipt` is the receipt row on the consumer's AgentSession; `transport` must match it.
    Deliver {
        receiver: AgentName,
        receipt: Ref<AgentSession>,
        transport: Transport,
    },
    Settle {
        by: Settlement,
    },
    Drop {
        reason: DropReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Settlement {
    Answered { reply: Ref<Message> },
    OutcomeObserved { observation: Identity },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DropReason {
    Undeliverable { cause: Cause },
    Expired,
    Withdrawn { by: String, authority: Identity },
    ConsumerUnreachable { by: String, authority: Identity },
    Superseded { by: Ref<Message> },
}

impl MessageEdge {
    pub fn name(&self) -> MessageEdgeName {
        match self {
            MessageEdge::Deliver { .. } => MessageEdgeName::Deliver,
            MessageEdge::Settle { .. } => MessageEdgeName::Settle,
            MessageEdge::Drop { .. } => MessageEdgeName::Drop,
        }
    }
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
            sender: AgentName(name(tc)),
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
            edges: records(tc, pick, 3, |pick| edge_event(tc, pick)),
            attempts: records(tc, pick, 2, |pick| Attempt::generate(tc, pick, text(tc))),
        }
    }

    /// A fixed message for example tests: no options, no edge, a pending state.
    pub fn sample() -> Message {
        Message {
            sender: AgentName("alice".into()),
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

    /// The spec drawn afresh (references and all); state, edge events and attempts kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Message {
        Message { state: self.state, edges: self.edges.clone(), attempts: self.attempts.clone(), ..Message::generate(tc, pick) }
    }

    /// The state moved and an edge event appended, and sometimes the subject set (a multi-column
    /// data enum, absent or present) and an attempt appended.
    fn edge(&self, tc: &TestCase, pick: Pick) -> (Edge<Message>, Message) {
        use MessageState::*;
        let args = MessageEdge::generate(tc, pick);
        let state = State::new(code_enum(tc, pick.nth(1), &[Pending, Awaiting, Settled, Dropped]), timestamp(tc));
        let event = event(tc, args.name(), self.state.value, state.value, payload(tc, pick, args));
        let mut edge = Edge::new(Message::STATE, &state, Message::EDGES, &event).expect("edge");
        let mut next = Message { edges: appended(tc, &self.edges, event), state, ..self.clone() };
        if pick.some(tc, 6) {
            let subject = pick.nth(2).some(tc, 8).then(|| MessageReference::generate(tc, pick.nth(3)));
            edge = edge.set(Message::SUBJECT, &subject).expect("subject");
            next.subject = subject;
        }
        if pick.nth(4).some(tc, 8) {
            let attempt = Attempt::generate(tc, pick, text(tc));
            edge = edge.append(Message::ATTEMPTS, &attempt).expect("attempt");
            next.attempts = appended(tc, &self.attempts, attempt);
        }
        (edge, next)
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

/// An event of any edge between any labels: history is never read to decide, so a stored log
/// need not be a path through the machine.
pub fn edge_event(tc: &TestCase, pick: Pick) -> MessageEdgeEvent {
    use MessageState::*;
    let args = MessageEdge::generate(tc, pick);
    let labels = [Pending, Awaiting, Settled, Dropped];
    event(tc, args.name(), code_enum(tc, pick, &labels), code_enum(tc, pick.nth(1), &labels), payload(tc, pick, args))
}
