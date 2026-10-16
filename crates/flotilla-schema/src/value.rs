//! Logical values and rows: what the derive encodes, before any backend assigns local ids.

use std::{collections::BTreeMap, fmt};

use sha2::{Digest, Sha256};

/// The stable numeric code of a resource kind. Codes never change once a kind is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KindCode(pub u32);

impl fmt::Display for KindCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// SHA-256 of an object's canonical bytes; the key of the `objects` table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    pub kind: KindCode,
    pub name: String,
    pub home: String,
}

/// One logical column value. `Str`, `Ref` and `Object` are resolved to local ids only by a backend.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Blob(Vec<u8>),
    /// An interned string.
    Str(String),
    Ref(Identity),
    Object(Hash),
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row {
    pub key: Vec<Value>,
    pub values: Vec<Value>,
}

/// Every row of one resource, per table in pre-order, plus the frozen objects its rows name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rows {
    pub tables: Vec<Vec<Row>>,
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
