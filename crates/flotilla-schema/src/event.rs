//! Log entries: one event per write, as the per-home `log` holds it (#3392 G8).
//!
//! An entry is a header (the cause, writer, time, an optional explanation and, for an edge, its
//! codes and master) and an [`Effect`]: the authoritative change, in the kind's own named terms
//! (fields, collections, columns), never positional row operations. A store applies an effect
//! to its tables in one place, whether the event is its own write or another home's; replicas
//! apply effects without re-running machines, and reads never replay the log.
//!
//! An effect is ADR 0063 canonical CBOR (`flotilla_cbor::to_vec`), so equal effects are equal
//! bytes in every database that logs them. Field and variant names are one letter: an effect is
//! stored once per write and read on every catch-up. It is stored data under ADR 0047's N→N+1
//! rule once a production generation writes it: maps with named fields, so a later generation
//! adds a field with a serde `default`, and the one-letter names are stable and never reused.
//! Column, field and table names inside it are the descriptor's; a renamed field keeps its old
//! name as `#[store(alias = "old")]`, and a store resolves names through its aliases.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    inline::{Cbor, INLINE_LIMITS},
    time::Timestamp,
    value::{Hash, Identity, KindCode, Value},
};

/// Bounds of one encoded entry or effect.
///
/// A frozen document is at most 1 MiB of canonical CBOR (`Frozen::new` uses the ADR 0063
/// defaults), so one byte string may be 1 MiB, which also bounds a `Blob` column. A write may
/// name several objects and many rows, so an entry as a whole may be 16 MiB, and one array or map
/// (the changes, or one row's values) may hold 2^20 elements; the message bound already caps
/// memory. The depth an entry reaches is 8 (entry, effect, changes, change, row path, step, key,
/// value). A write whose entry exceeds these bounds is refused rather than logged partially.
pub const LOG_LIMITS: flotilla_cbor::Limits =
    flotilla_cbor::Limits { max_message: 16 * 1024 * 1024, max_depth: 16, max_elements: 1 << 20, max_byte_string: 1024 * 1024 };

/// Column values by column name. A null column is omitted.
pub type Named = BTreeMap<String, Value>;

/// Why an entry was written: its `cause` column. An edge's header carries the kind's edge code
/// and the codes of the labels it leaves and enters, which the writer and `apply` check against
/// the kind (one table cannot hold a per-kind CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cause {
    #[serde(rename = "c")]
    Create,
    /// A spec edit.
    #[serde(rename = "s")]
    Edit,
    #[serde(rename = "e")]
    Edge {
        #[serde(rename = "e")]
        edge: i64,
        #[serde(rename = "f")]
        from: i64,
        #[serde(rename = "t")]
        to: i64,
    },
    /// The resource's rows go; its history and identity stay (G9).
    #[serde(rename = "d")]
    Delete,
    /// The resource's history and identity go; this entry alone stays, naming the identity in its
    /// effect.
    #[serde(rename = "x")]
    Expunge,
}

impl Cause {
    /// The `cause` column's text.
    pub fn name(&self) -> &'static str {
        match self {
            Cause::Create => "create",
            Cause::Edit => "edit",
            Cause::Edge { .. } => "edge",
            Cause::Delete => "delete",
            Cause::Expunge => "expunge",
        }
    }
}

/// Who wrote a create, spec edit, delete or expunge, when, and why: an edge carries the same in
/// its event. `payload` is an inline CBOR document explaining the write, such as a create's
/// admission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub writer: String,
    pub at: Timestamp,
    pub payload: Option<Vec<u8>>,
}

impl Origin {
    pub fn new(writer: impl Into<String>, at: Timestamp) -> Origin {
        Origin { writer: writer.into(), at, payload: None }
    }

    /// `self` with `payload` as its explanation.
    pub fn with_payload<P: Cbor>(mut self, payload: &P) -> Result<Origin, EventError> {
        self.payload = Some(flotilla_cbor::to_vec(payload, &INLINE_LIMITS)?);
        Ok(self)
    }
}

/// One step of a row path: the table storing `field` of the row reached so far, and the row's own
/// key in it (`pos` for a `records` or `history` row, the map key or set value of a collection).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Step {
    #[serde(rename = "f")]
    pub field: String,
    #[serde(rename = "k")]
    pub key: Named,
}

/// One named, field-level change. Its `row` is the row it addresses: empty for the resource's own
/// row, one [`Step`] per nested table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// One column-shaped field of `row`: its columns, by name. Every root field of a create, the
    /// changed spec fields of an edit, and the fields an edge sets.
    #[serde(rename = "s")]
    Set {
        #[serde(rename = "r", default, skip_serializing_if = "Vec::is_empty")]
        row: Vec<Step>,
        #[serde(rename = "f")]
        field: String,
        #[serde(rename = "v")]
        values: Named,
    },
    /// A state as one unit (#3392 A3): the label field `field` of `row` and every column datum
    /// of its label, by column name. Each edge's entry therefore grows by the state's column data;
    /// collection data change row by row (`Put`, `Remove`).
    #[serde(rename = "t")]
    State {
        #[serde(rename = "r", default, skip_serializing_if = "Vec::is_empty")]
        row: Vec<Step>,
        #[serde(rename = "f")]
        field: String,
        #[serde(rename = "v")]
        values: Named,
    },
    /// A row of the collection `collection` (a `child` table or a state's collection datum) of
    /// `row`, inserted or replaced by its own key. The rows nested in it stay.
    #[serde(rename = "p")]
    Put {
        #[serde(rename = "r", default, skip_serializing_if = "Vec::is_empty")]
        row: Vec<Step>,
        #[serde(rename = "c")]
        collection: String,
        #[serde(rename = "k")]
        key: Named,
        #[serde(rename = "v")]
        values: Named,
    },
    /// A row of a collection of `row`, by its own key, with every row nested in it.
    #[serde(rename = "d")]
    Remove {
        #[serde(rename = "r", default, skip_serializing_if = "Vec::is_empty")]
        row: Vec<Step>,
        #[serde(rename = "c")]
        collection: String,
        #[serde(rename = "k")]
        key: Named,
    },
    /// A row appended at `position` of the `records` or `history` table `records` of `row`. Its
    /// version stamp is not a value here: it is always the entry's `home_seq`.
    #[serde(rename = "a")]
    Append {
        #[serde(rename = "r", default, skip_serializing_if = "Vec::is_empty")]
        row: Vec<Step>,
        #[serde(rename = "c")]
        records: String,
        #[serde(rename = "n")]
        position: u32,
        #[serde(rename = "v")]
        values: Named,
    },
}

/// What one event did, in execution order, with every object it sets.
///
/// A create sets every root field and adds every row, each parent row before its children. An
/// edit sets the spec fields that changed and puts and removes the collection rows that did. An
/// edge sets its state as one unit, then the other fields it sets, then edits state collection
/// rows and appends rows. A delete has no changes. An expunge has none either and names the
/// identity it removed, since its entry no longer references one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    #[serde(rename = "c", default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    /// An object named by any value the changes write: a receiver holding the resource's earlier
    /// state has every other object the rows name, so an effect applies on its own, and an edit
    /// that leaves a frozen field alone does not resend its document. The bytes are ADR 0063
    /// canonical CBOR (`Frozen` content), so an object carries no media code.
    #[serde(rename = "o", default, skip_serializing_if = "BTreeMap::is_empty", with = "objects")]
    pub objects: BTreeMap<Hash, Vec<u8>>,
    #[serde(rename = "x", default, skip_serializing_if = "Option::is_none")]
    pub expunged: Option<Identity>,
}

impl Effect {
    pub fn encode(&self) -> Result<Vec<u8>, EventError> {
        Ok(flotilla_cbor::to_vec(self, &LOG_LIMITS)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Effect, EventError> {
        Ok(flotilla_cbor::from_slice(bytes, &LOG_LIMITS)?)
    }
}

/// One log entry, as `changes`, `changes_since` and `about` read it and `apply` takes it from
/// another home: the resource it changed, the home sequence it took, the header and the effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "k")]
    pub kind: KindCode,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "h")]
    pub home: String,
    /// The home sequence the write took; the resource's version after it.
    #[serde(rename = "s")]
    pub home_seq: u64,
    /// The identity's sequence before this event: 0 for a resource never written before, the
    /// delete's sequence for one created again. A receiver whose sequence for the identity
    /// differs has missed an event (a gap).
    #[serde(rename = "p")]
    pub prev_seq: u64,
    #[serde(rename = "c")]
    pub cause: Cause,
    #[serde(rename = "w")]
    pub writer: String,
    #[serde(rename = "t")]
    pub at: Timestamp,
    /// An edge's master, the resource it is about (G11). `None` once that resource is expunged.
    #[serde(rename = "a", default, skip_serializing_if = "Option::is_none")]
    pub about: Option<Identity>,
    /// The explanation: an edge's arguments and recorded reads, or an origin's payload.
    #[serde(rename = "y", default, skip_serializing_if = "Option::is_none", with = "optional_bytes")]
    pub payload: Option<Vec<u8>>,
    #[serde(rename = "e")]
    pub effect: Effect,
}

impl Event {
    pub fn identity(&self) -> Identity {
        Identity { kind: self.kind, name: self.name.clone(), home: self.home.clone() }
    }

    /// The payload as its writer encoded it.
    pub fn payload<P: Cbor>(&self) -> Option<Result<P, EventError>> {
        self.payload.as_ref().map(|bytes| Ok(flotilla_cbor::from_slice(bytes, &INLINE_LIMITS)?))
    }

    pub fn encode(&self) -> Result<Vec<u8>, EventError> {
        Ok(flotilla_cbor::to_vec(self, &LOG_LIMITS)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Event, EventError> {
        Ok(flotilla_cbor::from_slice(bytes, &LOG_LIMITS)?)
    }
}

/// An entry or effect that could not be encoded within [`LOG_LIMITS`], or bytes that are not one.
#[derive(Debug, thiserror::Error)]
#[error("log entry: {0}")]
pub struct EventError(#[from] pub flotilla_cbor::Error);

impl EventError {
    pub fn kind(&self) -> flotilla_cbor::ErrorKind {
        self.0.kind()
    }
}

/// Objects as an array of `{h: hash, b: bytes}`, in hash order.
mod objects {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::value::{byte_string, Hash};

    #[derive(Serialize, Deserialize)]
    struct Object {
        #[serde(rename = "h")]
        hash: Hash,
        #[serde(rename = "b", with = "byte_string")]
        bytes: Vec<u8>,
    }

    pub fn serialize<S: Serializer>(objects: &BTreeMap<Hash, Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(objects.iter().map(|(hash, bytes)| Object { hash: *hash, bytes: bytes.clone() }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<Hash, Vec<u8>>, D::Error> {
        let objects = Vec::<Object>::deserialize(deserializer)?;
        Ok(objects.into_iter().map(|Object { hash, bytes }| (hash, bytes)).collect())
    }
}

mod optional_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => serializer.serialize_bytes(bytes),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<flotilla_cbor::Bytes>::deserialize(deserializer).map(|bytes| bytes.map(|bytes| bytes.0))
    }
}
