//! Records: rows that belong to one resource, kept in `flotilla_schema::Records`.
//!
//! A record has no identity, watch or lifecycle of its own (ADR 0064). Only
//! the home appends, positions are dense and never reused, and nothing removes
//! or rewrites a row. Each row carries the owner's version at the write that
//! appended it ([`Version`](crate::Version)'s `u64`), which orders rows across
//! a resource's logs.

use flotilla_schema::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{EdgeKind, InputRead, Label, Machine};

/// The document of one edge event: the typed edge with its arguments, and
/// every read its gate and effect recorded. Stored as the event's one inline
/// CBOR document (#3320), so its shape can change without a schema change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgePayload<E> {
    pub edge: E,
    pub reads: Vec<InputRead>,
}

/// One applied edge of machine `M`: the edge kind, the labels it moved
/// between, the home's writer, the time and the [`EdgePayload`]. Events are
/// history for showing and auditing what happened; the current state, not a
/// replay of events, answers what is true now.
pub type EdgeEventOf<M> = flotilla_schema::EdgeEvent<Label<M>, EdgeKind<M>, EdgePayload<<M as Machine>::Edge>>;

/// How a turn reached its consumer. Filtering chooses how, never whether:
/// a muted delivery still counts as delivered (ADR 0064).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Transport {
    Turn,
    Notify,
    Mute,
}

/// A consumer's receipt for one message, appended after its turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The message's name at its home.
    pub message: String,
    pub transport: Transport,
    pub at: Timestamp,
}
