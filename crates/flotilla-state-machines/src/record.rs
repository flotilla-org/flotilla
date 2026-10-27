//! Edge events: what each of a resource's edges recorded, as its entry in the
//! home's per-home log (#3392 G8).
//!
//! An event has no identity, watch or lifecycle of its own (ADR 0064). Only
//! the home writes one, through its [`Log`](crate::Log), which gives it the
//! write's sequence; nothing removes or rewrites one short of an expunge.

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{EdgeKind, InputRead, Label, Machine};

/// The document of one edge event: the typed edge with its arguments, and
/// every read its gate and effect recorded. Its log entry stores the edge as
/// its arguments, one inline CBOR document (#3320); the reads are its change's
/// inputs (#4024).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgePayload<E> {
    pub edge: E,
    pub reads: Vec<InputRead>,
}

/// One applied edge of machine `M`: the edge kind, the labels it moved
/// between, the time and the [`EdgePayload`]. Events are
/// history for showing and auditing what happened; the current state, not a
/// replay of events, answers what is true now.
pub type EdgeEventOf<M> = flotilla_schema::EdgeEvent<Label<M>, EdgeKind<M>, EdgePayload<<M as Machine>::Edge>>;

/// How a turn reached its consumer: an argument of a session's `Receive`
/// edge and of a message's `Deliver`. Filtering chooses how, never whether:
/// a muted delivery still counts as delivered (ADR 0064).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
pub enum Transport {
    #[store(code = "tu")]
    Turn,
    #[store(code = "no")]
    Notify,
    #[store(code = "mu")]
    Mute,
}
