//! The storage-free state-machine engine of ADR 0064.
//!
//! A machine is declared once with ordinary Rust: a unit enum of states, an
//! enum of edges whose variants each wrap their own argument struct, and a
//! [`MachineDef`] built from [`EdgeDef`]s. The declaration is data, so
//! [`MachineDef::validate`] checks the rules and [`MachineDef::mermaid`] draws
//! the diagram from the same value the engine enforces.
//!
//! Gates are typed on their edge's argument struct and read only through the
//! recording [`Handle`]. Each typed read records its reference and version, or
//! its absence, so an edge event's inputs are recorded by construction.
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

pub use code::{Coded, Edge, EdgeArg};
pub use machine::{DeclaredEdge, DefError, EdgeDef, EdgeKind, EndDef, Ended, Machine, MachineDef, StepError, Target, WrongState};
pub use read::{Handle, InputRead, Judged, Kind, Ref, Source, Table, Version};
pub use time::Time;
