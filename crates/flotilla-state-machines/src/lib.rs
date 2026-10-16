//! The storage-free state-machine engine of ADR 0064.
//!
//! A machine is declared once with ordinary Rust: a state enum whose variants
//! carry their named data, an enum of edges whose variants each wrap their own
//! argument struct, and a [`MachineDef`] built from [`EdgeDef`]s. A state is a
//! value, as in the Quint specs: its label is derived for listing, indexing
//! and drawing, and its data is what gates read and edges change. Each edge
//! declares the labels it leaves and reaches and the data it changes. The
//! declaration is data, so [`MachineDef::validate`] checks the rules and
//! [`MachineDef::mermaid`] draws the diagram from the same value the engine
//! enforces.
//!
//! Gates and effects are typed on their edge's argument struct and read only
//! through the recording [`Handle`]: the current state, and typed reads of
//! the world, each recording its reference and version, or its absence. A
//! gate allows or refuses; an effect produces the next state.
//!
//! [`Home`] is the single writer. It applies an edge in a fixed order (the end
//! check, the from-label, the gate, the effect), stores the next state,
//! appends the edge event with its reads, and returns refusals without
//! recording them. Records (edge events, attempts and receipts) are
//! append-only logs keyed by owner and position, and an ended resource
//! refuses them. Edge events are history, for showing and auditing what
//! happened; nothing that decides reads them. Retry counts, schedules and
//! giving up are judgements over attempts ([`attempt::retry`]).
//!
//! Time is microseconds ([`Time`]), always supplied by the caller. The engine
//! never reads a clock, link age or any other liveness signal.
//!
//! Nothing here touches storage, replication or a wire encoding, and no
//! production crate depends on this one yet (`ci/build-graph/check.py`).

pub mod attempt;
mod code;
mod home;
mod machine;
mod read;
mod record;
mod time;

pub use attempt::{Attempt, AttemptPolicy, Cause, CauseCategory, Outcome, Retry, Schedule};
pub use code::{changed, Coded, Datum, Edge, EdgeArg, StateValue};
pub use home::{Home, Resource, WriteError};
pub use machine::{
    Check, DeclaredEdge, DefError, Diagram, EdgeDef, EdgeKind, EndDef, Ended, Fault, Label, Machine, MachineDef, StepError, WrongState,
};
pub use read::{Handle, InputRead, Judged, Kind, Ref, Source, Table, Version, OWN_ATTEMPTS, OWN_RECEIPTS};
pub use record::{EdgeEvent, Position, Receipt, RecordLog, Row, Transport, Writer};
pub use time::Time;
