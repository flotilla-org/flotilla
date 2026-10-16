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
//! Time is microseconds ([`Time`]), always supplied by the caller. The engine
//! never reads a clock, link age or any other liveness signal.
//!
//! Nothing here touches storage, replication or a wire encoding, and no
//! production crate depends on this one yet (`ci/build-graph/check.py`).

mod code;
mod machine;
mod read;
mod time;

pub use code::{changed, Coded, Datum, Edge, EdgeArg, StateValue};
pub use machine::{
    Check, DeclaredEdge, DefError, Diagram, EdgeDef, EdgeKind, EndDef, Ended, Fault, Label, Machine, MachineDef, StepError, WrongState,
};
pub use read::{Handle, InputRead, Judged, Kind, Ref, Source, Table, Version};
pub use time::Time;
