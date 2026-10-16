//! The rusqlite runtime for `flotilla-schema` (#3320): shared tables, per-kind statements rendered
//! once from each descriptor, interning and identity caches, and the write and read paths.
//!
//! Writes go through [`Write`], one immediate transaction holding many writes. Every write is
//! owner-checked: only resources homed at this store's home are written here. Versions are the
//! home's sequence, taken from `home_sequence` in the same transaction.
//!
//! Every write, including a delete, takes a sequence number. `change_log` is created here but
//! not yet written: until the change log lands (#3320, layer 4), `home_sequence` runs ahead of
//! it by design.
//!
//! `strings` and `resource_identity` rows are never removed: a deleted resource keeps its
//! identity row so cached `ResourceId`s and references to it stay valid. Collecting unused
//! string and identity rows is separate work (#3320 out of scope).
//!
//! `Store::open` creates missing tables and does not yet compare an existing database with the
//! rendered DDL; opening the previous generation's schema belongs to the post-cut-over form of
//! ADR 0047.

mod cache;
mod error;
mod shared;
mod sql;
mod store;

pub use error::StoreError;
pub use shared::{schema_ddl, MEDIA_CBOR};
pub use store::{Loaded, Read, ResourceId, Snapshot, Stamp, Store, StoreOptions, Write};
