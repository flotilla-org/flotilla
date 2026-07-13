//! The manifest wire layer: flotilla's producer-side binding to Presentation
//! Manager metadata planes (zellij + andamento today, wheelhouse next).
//!
//! Three flotilla producers import this crate, with disjoint patch targets
//! (design on flotilla-org/flotilla#667, build ticket #708):
//!
//! - `flotilla attach` stamps `Pane(id)` with the session identity at the
//!   binding moment (no TTL),
//! - the workspace actuator stamps `Tab(id)` on tabs it creates (no TTL),
//! - `flotilla pm connect` publishes the catalog against `Group`/`Identity`
//!   targets (TTL'd, re-asserted).
//!
//! v0 deliberately binds to andamento's *current* pipe names and key
//! spellings (decision recorded on #667). Every spelling and serde shape a
//! PM sees lives in this crate, so the Leg-1 contract rename (the
//! `flotilla-org/manifest` extraction) is a change to this crate only. At
//! that point the mirrored types in [`wire`] are deleted in favour of a
//! dependency on the shared manifest crate — flotilla becomes its third
//! consumer — and this crate keeps only the flotilla-specific spellings
//! ([`keys`]), recipe minting ([`recipe`]), and send plumbing ([`sink`]).

pub mod keys;
pub mod pm;
pub mod projection;
pub mod recipe;
pub mod sink;
pub mod stamp;
pub mod wire;
