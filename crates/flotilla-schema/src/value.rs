//! Logical values and rows: what the derive encodes, before any backend assigns local ids.

use std::{collections::BTreeMap, fmt};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::code::Code;

/// The stable code of a resource kind (#3392 ruling G7): short text written by hand. Codes never
/// change once a kind is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KindCode(pub Code);

impl KindCode {
    /// The kind code `text`; panics (a compile error in a `const` context) when it is not a code.
    pub const fn new(text: &str) -> KindCode {
        KindCode(Code::new(text))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for KindCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// SHA-256 of an object's canonical bytes; the key of the `objects` table. Serde writes it as
/// a 32-byte byte string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Hash, D::Error> {
        let bytes = byte_string::deserialize(deserializer)?;
        let len = bytes.len();
        bytes.try_into().map(Hash).map_err(|_| de::Error::invalid_length(len, &"32 bytes"))
    }
}

impl Hash {
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(Sha256::digest(bytes).into())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}

/// An untyped resource reference. Logical rows carry identities, never a database's local ids,
/// so rows compare equal across databases.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub kind: KindCode,
    pub name: String,
    pub home: String,
}

/// One logical column value. `Str`, `Ref` and `Object` are resolved to local ids only by a backend.
///
/// Serde (used by log entries only) names variants with one letter, writes blobs as byte
/// strings and references as `[kind, name, home]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Value {
    #[serde(rename = "n")]
    Null,
    #[serde(rename = "i")]
    Int(i64),
    #[serde(rename = "t")]
    Text(String),
    #[serde(rename = "b", with = "byte_string")]
    Blob(Vec<u8>),
    /// An interned string.
    #[serde(rename = "s")]
    Str(String),
    #[serde(rename = "r", with = "identity_tuple")]
    Ref(Identity),
    #[serde(rename = "o")]
    Object(Hash),
}

/// Bytes as a CBOR byte string rather than serde's default array of integers.
pub(crate) mod byte_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        flotilla_cbor::Bytes::deserialize(deserializer).map(|bytes| bytes.0)
    }
}

/// A reference as a three-element array; the field names would outweigh the values.
mod identity_tuple {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Identity, KindCode};

    pub fn serialize<S: Serializer>(identity: &Identity, serializer: S) -> Result<S::Ok, S::Error> {
        (identity.kind, &identity.name, &identity.home).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Identity, D::Error> {
        let (kind, name, home) = <(KindCode, String, String)>::deserialize(deserializer)?;
        Ok(Identity { kind, name, home })
    }
}

impl Value {
    /// A short name of the variant, for decode errors.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Int(_) => "integer",
            Value::Text(_) => "text",
            Value::Blob(_) => "blob",
            Value::Str(_) => "interned string",
            Value::Ref(_) => "reference",
            Value::Object(_) => "object",
        }
    }
}

/// One row of one table. The key excludes the owner id: the root row's key is empty, and a child
/// row's key is its parent's key followed by its own key values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Row {
    pub key: Vec<Value>,
    pub values: Vec<Value>,
}

/// Every row of one resource, per table in pre-order, plus the frozen objects its rows name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rows {
    pub tables: Vec<Vec<Row>>,
    #[serde(with = "crate::event::objects")]
    pub objects: BTreeMap<Hash, Vec<u8>>,
}

impl Rows {
    pub fn with_tables(count: usize) -> Rows {
        Rows { tables: vec![Vec::new(); count], objects: BTreeMap::new() }
    }

    /// Orders every table by key, the canonical order in which rows compare and decode.
    pub fn sort(&mut self) {
        for table in &mut self.tables {
            table.sort_by(|left, right| left.key.cmp(&right.key));
        }
    }
}
