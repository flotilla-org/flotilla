//! The backend-free state-machine engine of ADR 0064.
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
//! recording them. Every write is an edge (R4): an attempt is an edge on the
//! resource that made it, and a receipt is a session's `Receive` edge, whose
//! master ([`EdgeDef::about`]) is the message it is for. Edge events are
//! append-only `flotilla_schema::Records`, keyed by owner and position. An
//! edge event is a `flotilla_schema::EdgeEvent` ([`EdgeEventOf`]) whose
//! document is an [`EdgePayload`]: the edge's arguments and its reads. Edge
//! events are history, for showing and auditing what happened; nothing that
//! decides reads its own resource's events. Retry counts, schedules and
//! exhaustion are judgements over the attempt tally an attempt edge leaves in
//! the state ([`attempt::retry`]).
//!
//! Time is `flotilla_schema::Timestamp` (microseconds), always supplied by the
//! caller. The engine never reads a clock, link age or any other liveness
//! signal.
//!
//! State labels and edge kinds are `flotilla_schema` code enums, state enums
//! are the schema crate's label-plus-data states, and records and events are
//! the schema crate's types, so the storage derive stores them as they are.
//! Every machine's stored kind is its [`Resource`] (#3392 ruling D5): the
//! storage derive targets the machine's spec, which names the kind, its code,
//! indexes and state, and its state; the resource supplies the edge log and
//! records, and the store its name and version. Message, AgentSession, Convoy,
//! Vessel and Ensure declare their specs here, listed under their permanent
//! codes by the production registry ([`kinds`]), and any other lifecycle
//! kind's request is its spec wherever it is declared. A reference to a
//! definitions-class kind or to anything outside the store is an address
//! (`flotilla_schema::Address`), whose existence admission reads through the
//! recording [`Admission`] handle; a reference to a record in the store is a
//! `flotilla_schema::Ref` (kind, name and home). Nothing here touches a
//! backend, replication or a wire encoding. The only workspace crate this one
//! reaches is `flotilla-schema` (and `flotilla-address` through it), and no
//! production crate depends on this one yet (`ci/build-graph/check.py`).

pub mod attempt;
mod code;
mod home;
mod machine;
pub mod machines;
pub mod placement;
mod read;
mod record;
mod stored;

pub use attempt::{AttemptPolicy, Cause, CauseCategory, Failure, Outcome, Retry, Schedule};
pub use code::{changed, Coded, Datum, Edge, EdgeArg, StateValue};
pub use home::{Created, Home, Resource, WriteError};
pub use machine::{
    Check, DeclaredEdge, DefError, Diagram, EdgeDef, EdgeKind, EndDef, Ended, Fault, Label, Machine, MachineDef, StepError, WrongState,
};
pub use read::{Admission, Handle, InputRead, Judged, Kind, Ref, Source, Table, Version};
pub use record::{EdgeEventOf, EdgePayload, Transport};
pub use stored::{kinds, StoredState};
