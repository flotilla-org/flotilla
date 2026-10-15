//! The rusqlite runtime for `flotilla-schema` (#3320): shared tables, per-kind statements rendered
//! once from each descriptor, interning and identity caches, and the write and read paths.
//!
//! Writes go through [`Write`], one immediate transaction holding many writes. Every write is
//! owner-checked: only resources homed at this store's home are written here. Versions are the
//! home's sequence, taken from `home_sequence` in the same transaction.

mod cache;
mod error;
mod shared;
mod sql;
mod store;

pub use error::StoreError;
pub use shared::{schema_ddl, MEDIA_CBOR};
pub use store::{Loaded, Read, ResourceId, Snapshot, Stamp, Store, StoreOptions, Write};
