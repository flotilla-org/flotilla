//! Change-log entries: the row operations one write executed, in strings and identities rather
//! than local ids, so another database can apply them.
//!
//! An entry is ADR 0063 canonical CBOR (`flotilla_cbor::to_vec`), so equal changes are equal
//! bytes in every database that logs them. Field and variant names are one letter: the entry is
//! stored once per write and read on every replay.

use serde::{Deserialize, Serialize};

use crate::value::{Hash, KindCode, Value};

/// Bounds of one encoded entry.
///
/// A frozen document is at most 1 MiB of canonical CBOR (`Frozen::new` uses the ADR 0063
/// defaults), so one byte string may be 1 MiB, which also bounds a `Blob` column. A write may
/// name several objects and many rows, so the entry as a whole may be 16 MiB, and one array
/// (the ops, or one row's values) may hold 2^20 elements; the message bound already caps memory.
/// The depth an entry reaches is 7 (entry, ops, op, op fields, values, value, reference).
/// A write whose entry exceeds these bounds is refused rather than logged partially.
pub const CHANGE_LIMITS: flotilla_cbor::Limits =
    flotilla_cbor::Limits { max_message: 16 * 1024 * 1024, max_depth: 16, max_elements: 1 << 20, max_byte_string: 1024 * 1024 };

/// One write to one resource, as its home executed it, at the home sequence it took.
///
/// This is the stored format of `change_log` entries, and ADR 0047's N→N+1 rule applies to it
/// once a production generation writes it. Entries are CBOR maps with named fields, so a later
/// generation adds a field with a serde `default` and still decodes this one; the one-letter
/// field and variant names are therefore stable and never reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowChange {
    #[serde(rename = "k")]
    pub kind: KindCode,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "h")]
    pub home: String,
    /// The home sequence the write took; the resource's version after it.
    #[serde(rename = "s")]
    pub home_seq: u64,
    /// The identity's sequence before this change: 0 for a resource never written before, the
    /// delete's sequence for one inserted again. A receiver whose sequence for the identity
    /// differs has missed a change (a gap).
    #[serde(rename = "p")]
    pub prev_seq: u64,
    /// In execution order: objects, then the root row, then child deletes deepest table first,
    /// child updates, and child inserts shallowest table first.
    #[serde(rename = "o")]
    pub ops: Vec<RowOp>,
}

/// One row operation. Tables are named, keys exclude the owner, and the root row's key is empty.
/// The root's `version` is not a column here: it is always the change's `home_seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowOp {
    /// An object this change sets: named by any column of a row it inserts, or by an object
    /// column of an updated row whose value changed; whether or not the write had to store it.
    /// A receiver holding the resource's earlier state (a snapshot, or the changes before) has
    /// every other object the rows name, so a change applies without other resources' changes,
    /// and an update that leaves a frozen column alone does not resend its document.
    /// `apply` verifies the hash and inserts it if absent. `media` is the `objects` media code;
    /// ADR 0063 CBOR (1) is the only media today, and `apply` refuses any other.
    #[serde(rename = "p")]
    PutObject {
        #[serde(rename = "h")]
        hash: Hash,
        #[serde(rename = "m")]
        media: u8,
        #[serde(rename = "b", with = "crate::value::byte_string")]
        bytes: Vec<u8>,
    },
    #[serde(rename = "i")]
    Insert {
        #[serde(rename = "t")]
        table: String,
        #[serde(rename = "k")]
        key: Vec<Value>,
        #[serde(rename = "v")]
        values: Vec<Value>,
    },
    #[serde(rename = "u")]
    Update {
        #[serde(rename = "t")]
        table: String,
        #[serde(rename = "k")]
        key: Vec<Value>,
        #[serde(rename = "v")]
        values: Vec<Value>,
    },
    #[serde(rename = "d")]
    Delete {
        #[serde(rename = "t")]
        table: String,
        #[serde(rename = "k")]
        key: Vec<Value>,
    },
    /// The root row, and with it every child row.
    #[serde(rename = "x")]
    DeleteResource,
}

/// A change-log entry that could not be encoded within [`CHANGE_LIMITS`], or bytes that are not one.
#[derive(Debug, thiserror::Error)]
#[error("change log entry: {0}")]
pub struct ChangeError(#[from] pub flotilla_cbor::Error);

impl ChangeError {
    pub fn kind(&self) -> flotilla_cbor::ErrorKind {
        self.0.kind()
    }
}

impl RowChange {
    pub fn encode(&self) -> Result<Vec<u8>, ChangeError> {
        Ok(flotilla_cbor::to_vec(self, &CHANGE_LIMITS)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<RowChange, ChangeError> {
        Ok(flotilla_cbor::from_slice(bytes, &CHANGE_LIMITS)?)
    }
}
