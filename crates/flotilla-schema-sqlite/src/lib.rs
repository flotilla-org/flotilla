//! The rusqlite runtime for `flotilla-schema` (#3320): shared tables, per-kind statements rendered
//! once from each descriptor, interning and identity caches, and the write and read paths.
//!
//! Writes go through [`Write`], one immediate transaction holding many writes. Every write is
//! owner-checked: only resources homed at this store's home are written here. Versions are the
//! home's sequence, taken from `home_sequence` in the same transaction. A whole value is written
//! only to create a resource (`insert`) and for spec edits (`update`); states and `records` and
//! `history` rows move only through [`Write::apply_edge`], which appends the edge's rows and sets
//! only the fields it declares, each by its own statement.
//!
//! Every write, including a delete, takes a sequence number and appends one entry to the per-home
//! `log` in the same transaction (#3392 G8): an [`Event`](flotilla_schema::Event) recording its
//! cause, writer, time, explanation and its effect, named field-level changes. A local write
//! builds its effect, encodes it, decodes the bytes and lowers the decoded effect to rows through
//! the same code [`Write::apply`] runs for another home's events, which it logs under that home, so
//! a store's log replays into another store. An update that changes no row takes neither a
//! sequence nor an entry. Nothing trims the log: a delete keeps the resource's entries and
//! identity, and only [`Write::expunge`] removes them, leaving one entry naming the identity.
//! [`Read::about`] reads the entries about a master (G11), and [`Write::restore`] seeds a replica
//! from a snapshot.

//! Reads are by key or declared index only. [`Read::page`] runs a kind's generated
//! [`IndexQuery`](flotilla_schema::IndexQuery) keyset-paged through its partial index, and
//! [`Read::scan`] is the one pass over a whole kind (startup), keyset-paged by id through
//! `resource_identity_by_home`. Both take a required limit. `Store::statements` lists every
//! statement the runtime prepares, and the query-plan tests check each one searches by key.
//!
//! `strings` rows are never removed, and `resource_identity` rows only by an expunge: a deleted
//! resource keeps its identity row so cached `ResourceId`s and references to it stay valid.
//! Collecting unused string rows is separate work (#3320 out of scope).
//!
//! `Store::open` creates missing tables and indexes and does not yet compare an existing database with the
//! rendered DDL; opening the previous generation's schema belongs to the post-cut-over form of
//! ADR 0047.

mod audit;
mod cache;
mod effect;
mod error;
mod query;
mod shared;
mod sql;
mod store;

pub mod stream;

pub use audit::audit_domains;
pub use error::{Malformed, StoreError};
pub use query::{Cursor, Page, MAX_PAGE};
pub use shared::schema_ddl;
pub use store::{
    AboutCursor, AboutPage, Applied, Loaded, Read, ResourceId, Snapshot, Stamp, Store, StoreOptions, Write, MAX_CHANGES_PAGE_BYTES,
};
