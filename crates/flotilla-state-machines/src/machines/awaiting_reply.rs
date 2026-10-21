//! AwaitingReply: the sender's half of a Reply or Outcome exchange (spec
//! `machines/awaiting_reply.qnt`; #3587 awaiting-reply ruling).
//!
//! The record is homed at the sender's home and written in the same
//! transaction as the send. The Message it waits on is homed at its receiver
//! (ADR 0055), so the two are separate kinds. It is `Open → Answered |
//! Expired | Withdrawn`: answered when a reply to the message is present in
//! the sender's own inbox, a local input; expired once the sender home's clock
//! passes the deadline with no reply present; withdrawn by the sender with a
//! reason. A stall is an open record whose kind is `stall`. Which reply
//! answered is the reply message's `in_reply_to`, and an end's time is the
//! state's `since`, so no label carries data but Withdrawn's reason.
//!
//! Each gate reads its evidence through the world, keyed by the messages the
//! record and the edge name: [`Landed`], the inbox a message landed in at
//! this home and the message it replies to, for an answer; [`Replies`], the
//! inboxes holding a reply to the awaited message, for an expiry; and an
//! [`Authority`] for a withdrawal. The sender's inbox is the record's
//! `sender`, so a reply landed in another inbox is no evidence. The clock is
//! the caller's time, the sender home's.
//!
//! The request is the spec: the send writes the record in its own
//! transaction, and admission validates nothing beyond the type, which
//! already excludes a send that expects nothing.
//!
//! The stored kind is `Resource<AwaitingReply>` (code `ar`), declared by
//! [`AwaitingReplySpec`]: the send's record as columns and the state; its
//! edges are its log entries.

use std::collections::BTreeSet;

use flotilla_schema::{Address, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{
    edges, machines::message::MessageRef, states, Admission, EdgeDef, Ended, Handle, Kind, Machine, MachineDef, NoRecords, Ref, Source,
    Table, Version, WrongState,
};

/// What the sender expects back. A send that expects nothing writes no record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum Awaited {
    #[store(code = "re")]
    Reply,
    #[store(code = "ou")]
    Outcome,
}

/// What the send recorded; it never changes. It declares the stored
/// AwaitingReply (kind code `ar`), indexed by sender and by subject for the
/// open records (the agent board's and "stalled" reads), by the message
/// awaited (a reply's `in_reply_to` finds its record, and an expunge of the
/// message checks no table), and by deadline for the open records (the
/// expiry sweep).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "AwaitingReply",
    code = "ar",
    state = AwaitingReplyState,
    index(name = "open_by_sender", eq(sender), when(state in [Open])),
    index(name = "open_by_subject", eq(subject), when(state in [Open], subject is_some)),
    index(name = "by_message", eq(message)),
    index(name = "deadline", order(deadline), when(state in [Open], deadline is_some))
)]
pub struct AwaitingReplySpec {
    /// The message awaited, homed at its receiver.
    pub message: MessageRef,
    /// Who sent it: a seat, or a declared role.
    pub sender: Address,
    /// The inbox it was sent to.
    pub recipient: Address,
    /// What it is about, if anything: the message's subject reference.
    pub subject: Option<Address>,
    /// The record's kind: opaque text, such as `stall`.
    #[store(intern = "ak")]
    pub kind: String,
    pub awaited: Awaited,
    pub deadline: Option<Timestamp>,
}

states! {
    /// The spec's `State`. Answered and Expired carry nothing: the reply names
    /// the message it answers, and the time is the state's `since`.
    pub enum AwaitingReplyState => AwaitingReplyLabel {
        Open = "op",
        Answered = "an",
        Expired = "ex",
        Withdrawn { reason: String } = "wd",
    }
}

/// A reply to the message is present in the sender's own inbox. The gate
/// reads where `reply` landed and what it replies to ([`Landed`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub reply: MessageRef,
}

/// The deadline has passed on the sender home's clock, with no reply present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expire;

/// The sender withdraws the record, with a reason: its message was withdrawn
/// or superseded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withdraw {
    pub by: Address,
    pub reason: String,
    pub authority: Ref<Authority>,
}

edges! {
    pub enum AwaitingReplyEdge => AwaitingReplyEdgeKind {
        Answer(Answer) = "an",
        Expire(Expire) = "ex",
        Withdraw(Withdraw) = "wd",
    }
}

/// The world's key for a message by identity, `name@home`, as [`Landed`] and
/// [`Replies`] are keyed.
fn key(message: &MessageRef) -> String {
    format!("{}@{}", message.name, message.home)
}

/// Where `message` landed, by its identity.
pub fn landed(message: &MessageRef) -> Ref<Landed> {
    Ref::new(key(message))
}

/// The inboxes holding a reply to `message`, by its identity.
pub fn replies(message: &MessageRef) -> Ref<Replies> {
    Ref::new(key(message))
}

/// A message as it landed at this home: the inbox holding it and the message
/// it replies to, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    pub inbox: Address,
    pub in_reply_to: Option<MessageRef>,
}

/// A message that landed in an inbox at this home, by [`landed`]'s key;
/// absent until it lands.
pub struct Landed;
impl Kind for Landed {
    const NAME: &'static str = "landed";
    type Value = Landing;
}

/// The inboxes at this home holding a reply to a message, by [`replies`]'s
/// key; absent while none does.
pub struct Replies;
impl Kind for Replies {
    const NAME: &'static str = "replies";
    type Value = BTreeSet<Address>;
}

/// A decision by an authorised party; reads `true` when authorised.
pub struct Authority;
impl Kind for Authority {
    const NAME: &'static str = "authority";
    type Value = bool;
}

/// The spec's `Refusal`, variant for variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwaitingReplyRefusal {
    /// The record has ended (Answered, Expired or Withdrawn).
    NotOpen,
    ReplyAbsent,
    NotAReply,
    NoDeadline,
    NotDue,
    ReplyPresent,
    Unauthorised,
}

impl From<Ended<AwaitingReplyLabel>> for AwaitingReplyRefusal {
    fn from(_: Ended<AwaitingReplyLabel>) -> Self {
        AwaitingReplyRefusal::NotOpen
    }
}

/// Unreachable: every edge leaves Open, and every other label is an end and
/// refuses first.
impl From<WrongState<AwaitingReplyLabel, AwaitingReplyEdgeKind>> for AwaitingReplyRefusal {
    fn from(_: WrongState<AwaitingReplyLabel, AwaitingReplyEdgeKind>) -> Self {
        AwaitingReplyRefusal::NotOpen
    }
}

/// What awaiting-reply gates read.
pub trait AwaitingReplyWorld: Source<Landed> + Source<Replies> + Source<Authority> {}
impl<T: Source<Landed> + Source<Replies> + Source<Authority> + ?Sized> AwaitingReplyWorld for T {}

/// An in-memory awaiting-reply world, for tests, replay and conformance.
#[derive(Default)]
pub struct MemoryAwaitingReplyWorld {
    pub landed: Table<Landed>,
    pub replies: Table<Replies>,
    pub authority: Table<Authority>,
}

impl MemoryAwaitingReplyWorld {
    /// Lands `message` in `inbox`, replying to `in_reply_to` if any: where it
    /// landed, and the inboxes holding a reply to the message it answers.
    pub fn land(&mut self, message: &MessageRef, inbox: &Address, in_reply_to: Option<&MessageRef>) {
        self.landed.put(&landed(message), Landing { inbox: inbox.clone(), in_reply_to: in_reply_to.cloned() });
        if let Some(answered) = in_reply_to {
            let reference = replies(answered);
            let mut inboxes = self.replies.lookup(&reference).map(|(_, inboxes)| inboxes.clone()).unwrap_or_default();
            inboxes.insert(inbox.clone());
            self.replies.put(&reference, inboxes);
        }
    }
}

impl Source<Landed> for MemoryAwaitingReplyWorld {
    fn lookup(&self, reference: &Ref<Landed>) -> Option<(Version, &Landing)> {
        self.landed.lookup(reference)
    }
}

impl Source<Replies> for MemoryAwaitingReplyWorld {
    fn lookup(&self, reference: &Ref<Replies>) -> Option<(Version, &BTreeSet<Address>)> {
        self.replies.lookup(reference)
    }
}

impl Source<Authority> for MemoryAwaitingReplyWorld {
    fn lookup(&self, reference: &Ref<Authority>) -> Option<(Version, &bool)> {
        self.authority.lookup(reference)
    }
}

/// The AwaitingReply machine.
pub struct AwaitingReply;

impl Machine for AwaitingReply {
    const NAME: &'static str = "AwaitingReply";
    type Request = AwaitingReplySpec;
    type Spec = AwaitingReplySpec;
    type State = AwaitingReplyState;
    type Edge = AwaitingReplyEdge;
    type Write = NoRecords;
    type Refusal = AwaitingReplyRefusal;
    type World = dyn AwaitingReplyWorld;

    fn definition() -> MachineDef<Self> {
        use AwaitingReplyLabel::*;
        MachineDef::new(Open)
            .edge(
                EdgeDef::<Self, Answer>::new()
                    .from([Open])
                    .to([Answered])
                    .gate("the reply landed in the sender's inbox and replies to the awaited message", gate_answer)
                    .effect(|_, _, _| AwaitingReplyState::Answered),
            )
            .edge(
                EdgeDef::<Self, Expire>::new()
                    .from([Open])
                    .to([Expired])
                    .gate("the clock is strictly past the deadline and no reply is in the sender's inbox", gate_expire)
                    .effect(|_, _, _| AwaitingReplyState::Expired),
            )
            .edge(
                EdgeDef::<Self, Withdraw>::new()
                    .from([Open])
                    .to([Withdrawn])
                    .changes(["reason"])
                    .gate("an authorised withdrawal with a reason", gate_withdraw)
                    .effect(|_, _, withdraw| AwaitingReplyState::Withdrawn { reason: withdraw.reason.clone() }),
            )
            .end(Answered, "a reply answered the record")
            .end(Expired, "the record expired unanswered")
            .end(Withdrawn, "the sender withdrew the record")
    }

    /// The send writes the record as sent; there is nothing to refuse.
    fn admit(
        _: &mut Admission<'_, AwaitingReply>,
        spec: AwaitingReplySpec,
    ) -> Result<(AwaitingReplySpec, AwaitingReplyState), AwaitingReplyRefusal> {
        Ok((spec, AwaitingReplyState::Open))
    }
}

/// Whether the sender's inbox holds a reply to the awaited message.
fn reply_present(h: &mut Handle<'_, AwaitingReply>) -> bool {
    let spec = h.spec();
    h.read(&replies(&spec.message)).is_some_and(|inboxes| inboxes.contains(&spec.sender))
}

// Spec: `not(present)` refuses ReplyAbsent, then `inReplyTo != message`
// NotAReply. A reply landed in another inbox is not present.
fn gate_answer(h: &mut Handle<'_, AwaitingReply>, answer: &Answer) -> Result<(), AwaitingReplyRefusal> {
    let spec = h.spec();
    match h.read(&landed(&answer.reply)) {
        Some(landing) if landing.inbox == spec.sender => {
            if landing.in_reply_to.as_ref() == Some(&spec.message) {
                Ok(())
            } else {
                Err(AwaitingReplyRefusal::NotAReply)
            }
        }
        _ => Err(AwaitingReplyRefusal::ReplyAbsent),
    }
}

// Spec: no deadline refuses NoDeadline, `now <= deadline` NotDue, then a
// reply present ReplyPresent: a reply in the inbox wins over a passed deadline.
fn gate_expire(h: &mut Handle<'_, AwaitingReply>, _: &Expire) -> Result<(), AwaitingReplyRefusal> {
    let Some(deadline) = h.spec().deadline else { return Err(AwaitingReplyRefusal::NoDeadline) };
    if h.now() <= deadline {
        return Err(AwaitingReplyRefusal::NotDue);
    }
    if reply_present(h) {
        Err(AwaitingReplyRefusal::ReplyPresent)
    } else {
        Ok(())
    }
}

// Spec: an unauthorised withdrawal and an empty reason both refuse `Unauthorised`.
fn gate_withdraw(h: &mut Handle<'_, AwaitingReply>, withdraw: &Withdraw) -> Result<(), AwaitingReplyRefusal> {
    if !h.read(&withdraw.authority).copied().unwrap_or(false) || withdraw.reason.is_empty() {
        Err(AwaitingReplyRefusal::Unauthorised)
    } else {
        Ok(())
    }
}
