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
//! Declared ahead of its machine for the owner's schema look-over (#3587
//! layer 3); layer 4 implements the machine over [`AwaitingReplySpec`].

use flotilla_schema::{Address, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{edges, machines::message::MessageRef, states};

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

/// A reply to the message is present in the sender's own inbox.
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
}

edges! {
    pub enum AwaitingReplyEdge => AwaitingReplyEdgeKind {
        Answer(Answer) = "an",
        Expire(Expire) = "ex",
        Withdraw(Withdraw) = "wd",
    }
}
