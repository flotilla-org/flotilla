//! Consumers of an inbox (ADR 0064 "Roles, inboxes and delivery", #3424
//! rulings 1 to 3): an agent session or a deterministic controller. A
//! person's kind comes with the principals model (owner's schema look-over,
//! 2026-10-18): their receipt is an acknowledgement or a response, never a
//! surface loading the message.
//!
//! A consumer is named by a typed [`Receiver`]: its kind, its resource's
//! name and the host the resource is homed at. A receiver is compared only
//! for equality. It is not the Role Binding: a binding changes, while a
//! receipt keeps naming who took the message. The host stays because expiry
//! waits on the consumer host's stream.
//!
//! Every consumer kind takes a message the same way: a `Receive` edge on its
//! own resource, whose master is the message, so the message's gates read
//! the receipts about it whatever kind wrote them ([`Consumer`]).

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{EdgeKind, Machine, Transport};

/// What kind of consumer took a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum ConsumerKind {
    /// An agent conversation (`AgentSession`).
    #[store(code = "as")]
    AgentSession,
    /// A deterministic controller, through its own resource.
    #[store(code = "co")]
    Controller,
}

/// The consumer a receipt came from: its kind, its resource's name (`id`)
/// and its host, the resource's home. Stored flattened: the kind as a code,
/// the id as interned resource-name text (`rn`) and the host as interned
/// home text (`hm`). Compared only for equality: the ordering exists so
/// receivers can key maps and sets.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub struct Receiver {
    pub kind: ConsumerKind,
    #[store(intern = "rn")]
    pub id: String,
    #[store(intern = "hm")]
    pub host: String,
}

/// A machine whose resource consumes inboxes: it declares a `Receive` edge,
/// from a live state to the same state, whose master is the message it
/// took. The receipt reader accepts a receipt from any kind implementing
/// this (#3424 ruling 2), naming the consumer by this kind, the entry's
/// resource and its home.
pub trait Consumer: Machine {
    const CONSUMER: ConsumerKind;
    /// The `Receive` edge's kind: the receipt reader decodes only entries of
    /// this edge, so another edge's payload is never read as a receipt.
    const RECEIVE: EdgeKind<Self>;

    /// The message's name and the transport, when `edge` is a receipt.
    fn receipt(edge: &Self::Edge) -> Option<(&str, Transport)>;
}
