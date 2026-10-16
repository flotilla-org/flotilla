//! The Message-shaped fixture, transcribed (not imported) from
//! `flotilla_resources::message::MessageSpec`, `flotilla_protocol::{Leaf, MessageReference}` and
//! the #3108 prototype's Message machine (`MessageState`, `MessageEdge`, `DropReason`,
//! `Settlement`, `Cause`, `Transport`).
//!
//! Modelling decisions the storage list forces:
//! - The prototype's `ObjRef` becomes `Ref<K>` where the target kind is known (`Settlement::Answered`,
//!   `DropReason::Superseded`, the deliver receipt) and `Identity` where it is not (an authority, an
//!   observed condition, a reference's repository).
//! - `receiver` on the spec is the prototype's `inbox: Ref<Inbox>`; the deliver edge keeps the
//!   receiving session's name, interned through the `AgentName` newtype.
//! - `references: Vec<MessageReference>` waits for `child` (layer 3); `subject` exercises the
//!   seven-variant reference enum as an optional column now.
//! - Edges arrive as `records` in layer 3; until then `last_edge: Option<MessageEdge>` carries the
//!   edge shapes, including `Drop { reason: DropReason::Undeliverable { cause } }`, a data enum
//!   nested in a data enum.
//!
//! Serde attributes are the resource types' own; storage ignores them (codes are explicit).

use chrono::{DateTime, Utc};
use flotilla_schema::{Identity, KindCode, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{floor, text, time};

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

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Message", code = 1)]
pub struct Message {
    #[store(intern)]
    pub sender: String,
    pub inbox: Ref<Inbox>,
    pub relation: MessageRelation,
    pub body: String,
    pub body_digest: Option<String>,
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
    pub last_edge: Option<MessageEdge>,
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

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Cause {
    pub category: CauseCategory,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum CauseCategory {
    #[store(code = 1)]
    Transient,
    #[store(code = 2)]
    Refused,
    #[store(code = 3)]
    Misconfigured,
    #[store(code = 4)]
    MissingCapability,
    #[store(code = 5)]
    External,
}

/// How a generator chooses each enum variant and option: drawn by Hegel, or fixed by a covering
/// index so that the indexes `0..COVERING` reach every variant of every enum, nested ones included.
#[derive(Debug, Clone, Copy)]
pub enum Pick {
    Draw,
    Cover(usize),
}

/// Structural shapes: indexes `0..SHAPES` reach, while their enclosing variants are active, every
/// `LeafAddress` under `Outcome` and under a present `delivery_condition` (0..7), every
/// `MessageReference` (0..7), every edge (`shape % 3`) with every `Settlement` and `DropReason`
/// (`shape / 3`), and every unit variant and absent option (7 and above).
const SHAPES: usize = 16;

/// Five rounds of the shapes. Code enums take the covering index itself, so across the rounds
/// every code reaches every position, including `CauseCategory`, which only an `Undeliverable`
/// drop (one shape) holds.
pub const COVERING: usize = SHAPES * 5;

impl Pick {
    /// The variant index in `0..count`; `cover` maps a structural shape to it.
    fn variant(self, tc: &TestCase, count: usize, cover: impl FnOnce(usize) -> usize) -> usize {
        match self {
            Pick::Draw => tc.draw(gs::integers::<usize>().min_value(0).max_value(count - 1)),
            Pick::Cover(index) => cover(index % SHAPES) % count,
        }
    }

    /// Whether an option is present; shapes below `until` are present.
    fn some(self, tc: &TestCase, until: usize) -> bool {
        match self {
            Pick::Draw => tc.draw(gs::booleans()),
            Pick::Cover(index) => index % SHAPES < until,
        }
    }
}

fn seq(tc: &TestCase) -> u64 {
    tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64))
}

/// Interned values collide often, so equal strings share `strings` rows.
fn name(tc: &TestCase) -> String {
    tc.draw(gs::sampled_from(vec!["alice".to_owned(), "bob".to_owned(), String::new(), "ünïcødé".to_owned()]))
}

fn home(tc: &TestCase) -> String {
    tc.draw(gs::sampled_from(vec![super::LOCAL.to_owned(), "elsewhere".to_owned()]))
}

/// References to targets that mostly do not exist: writing one creates its identity row.
fn reference<K>(tc: &TestCase) -> Ref<K> {
    Ref::new(name(tc), home(tc))
}

fn identity(tc: &TestCase) -> Identity {
    Identity { kind: KindCode(tc.draw(gs::integers::<u32>())), name: name(tc), home: home(tc) }
}

fn code_enum<E: Copy>(tc: &TestCase, pick: Pick, all: &[E]) -> E {
    match pick {
        Pick::Draw => all[tc.draw(gs::integers::<usize>().min_value(0).max_value(all.len() - 1))],
        Pick::Cover(index) => all[index % all.len()],
    }
}

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
    fn generate(tc: &TestCase, pick: Pick) -> MessageEdge {
        use CauseCategory::*;
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
                    0 => DropReason::Undeliverable {
                        cause: Cause {
                            category: code_enum(tc, pick, &[Transient, Refused, Misconfigured, MissingCapability, External]),
                            detail: text(tc),
                        },
                    },
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
            last_edge: pick.some(tc, SHAPES - 1).then(|| MessageEdge::generate(tc, pick)),
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
            last_edge: None,
        }
    }

    /// The value a load returns: times floored to microseconds.
    pub fn stored(&self) -> Message {
        Message { deadline: self.deadline.map(floor), ..self.clone() }
    }
}
