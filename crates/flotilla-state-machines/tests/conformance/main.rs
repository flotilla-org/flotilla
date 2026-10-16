//! Full-graph conformance: every reachable state of each machine's
//! conformance harness (`specs/primitives/conformance/*.qnt`), probed with
//! every event, against the Rust engine.
//!
//! TLC dumps the reachable graphs; `specs/primitives/conformance/export.sh DIR`
//! writes them as `DIR/<machine>.graph.json`. Run with
//! `QUINT_GRAPH_DIR=DIR cargo test -p flotilla-state-machines --test conformance -- --ignored`.
//!
//! For each machine: every reachable spec state is rebuilt in Rust by replaying
//! its shortest spec path through the real engine (the single writer is the
//! only way to reach a state, and gates that judge attempt records see the
//! records the path wrote), each step accepted and reaching the same state;
//! then every probe's event is applied to a fresh rebuild, and the spec's
//! `Moved(t)` must be a Rust acceptance reaching `t`, and `Refused(r)` the same
//! refusal with nothing recorded.
//!
//! States are compared as values. A machine's state is a value in both the
//! specs and the engine, so each adaptor encodes the Rust state value (from
//! `Resource::state()`, plus the immutable spec where the Quint state carries
//! request data, plus attempt records where its history is attempts) in the
//! spec's ITF shape, and the comparison is equality with the spec's state. No
//! adaptor reads edge events, and no spec state is projected before
//! comparison. Each adaptor's module doc lists the field mapping and every
//! remaining encoding choice.

#[path = "../common/itf.rs"]
mod itf;

mod graph;

mod agent_session;
mod convoy;
mod ensure;
mod lifecycle;
mod message;
mod vessel;
