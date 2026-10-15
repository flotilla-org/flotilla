//! The rusqlite runtime for `flotilla-schema` (#3320): shared tables, per-kind statements rendered
//! once from each descriptor, interning and identity caches, and the write and read paths.
//!
//! Writes go through [`Write`], one immediate transaction holding many writes. Every write is
//! owner-checked: only resources homed at this store's home are written here. Versions are the
//! home's sequence, taken from `home_sequence` in the same transaction.
//!
//! Every write, including a delete, takes a sequence number and appends one `change_log` row
//! holding its [`RowChange`](flotilla_schema::RowChange) in the same transaction; an update that
//! changes no row takes neither. [`Write::apply`] writes another home's changes through the same
//! statements and logs them under that home, so a store's log replays into another store.
//!
//! Reads are by key or declared index only. [`Read::page`] runs a kind's generated
//! [`IndexQuery`](flotilla_schema::IndexQuery) keyset-paged through its partial index, and
//! [`Read::scan`] is the one pass over a whole kind (startup), keyset-paged by id through
//! `resource_identity_by_home`. Both take a required limit. `Store::statements` lists every
//! statement the runtime prepares, and the query-plan tests check each one searches by key.
//!
//! `strings` and `resource_identity` rows are never removed: a deleted resource keeps its
//! identity row so cached `ResourceId`s and references to it stay valid. Collecting unused
//! string and identity rows is separate work (#3320 out of scope).
//!
//! `Store::open` creates missing tables and indexes and does not yet compare an existing database with the
//! rendered DDL; opening the previous generation's schema belongs to the post-cut-over form of
//! ADR 0047.

mod cache;
mod error;
mod query;
mod shared;
mod sql;
mod store;

pub use error::{Malformed, StoreError};
pub use query::{Cursor, Page, MAX_PAGE};
pub use shared::{schema_ddl, MEDIA_CBOR};
pub use store::{Applied, Loaded, Read, ResourceId, Snapshot, Stamp, Store, StoreOptions, Write, MAX_CHANGES_PAGE_BYTES};
