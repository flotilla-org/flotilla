//! Inline CBOR documents: serde values stored in one column of their row (`#[store(cbor)]`).
//!
//! The second place serde reaches storage, beside [`crate::Frozen`]. A frozen document is shared
//! by content hash in `objects`; an inline document belongs to its row alone, such as an edge
//! event's arguments and gate reads (#3320 ruling of 2026-10-15). Its shape can change without a
//! schema change, so it follows the stored-data rule that each generation decodes the previous
//! one's documents (ADR 0047): add a serde `default` or `alias`, never a column.

use flotilla_cbor::Limits;
use serde::{de::DeserializeOwned, Serialize};

/// A type usable as a `#[store(cbor)]` field.
pub trait Cbor: Serialize + DeserializeOwned {}

impl<T: Serialize + DeserializeOwned> Cbor for T {}

/// Bounds on one inline document: ADR 0063's defaults with a 64 KiB ceiling, so a document stays
/// a small part of its row. Larger documents belong in `frozen`, stored once in `objects`.
pub const INLINE_LIMITS: Limits = Limits { max_message: 64 * 1024, max_byte_string: 64 * 1024, ..flotilla_cbor::DEFAULT_LIMITS };
