//! Typed references to resources.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use serde::{Deserialize, Serialize};

use crate::{
    codec::{DecodeError, Decoder, EncodeError, Encoder},
    descriptor::ColumnType,
    layout::Layout,
    traits::{Column, Columns, Element, KeyColumn, Kind},
    value::{Identity, Value},
};

/// A reference to a resource of kind `K`, stored as a `resource_identity` id. The target need not
/// exist yet: writing a reference creates its identity row.
#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Ref<K> {
    pub name: String,
    pub home: String,
    #[serde(skip)]
    kind: PhantomData<fn() -> K>,
}

impl<K> Ref<K> {
    pub fn new(name: impl Into<String>, home: impl Into<String>) -> Ref<K> {
        Ref { name: name.into(), home: home.into(), kind: PhantomData }
    }
}

impl<K: Kind> Ref<K> {
    pub fn identity(&self) -> Identity {
        Identity { kind: K::CODE, name: self.name.clone(), home: self.home.clone() }
    }
}

// Manual impls: the kind is a marker and needs none of these traits itself.
impl<K> Clone for Ref<K> {
    fn clone(&self) -> Ref<K> {
        Ref::new(self.name.clone(), self.home.clone())
    }
}

impl<K> fmt::Debug for Ref<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ref").field("name", &self.name).field("home", &self.home).finish()
    }
}

impl<K> PartialEq for Ref<K> {
    fn eq(&self, other: &Ref<K>) -> bool {
        (&self.name, &self.home) == (&other.name, &other.home)
    }
}

impl<K> Eq for Ref<K> {}

impl<K> PartialOrd for Ref<K> {
    fn partial_cmp(&self, other: &Ref<K>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Ref<K> {
    fn cmp(&self, other: &Ref<K>) -> Ordering {
        (&self.name, &self.home).cmp(&(&other.name, &other.home))
    }
}

impl<K> Hash for Ref<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&self.name, &self.home).hash(state);
    }
}

// `TABLES` does not mention `K`, so a kind may reference itself (`in_reply_to: Ref<Message>`)
// without a cycle through its own constants.
impl<K: Kind> Columns for Ref<K> {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Ref, required);
        layout.reference(&crate::snake_case(K::KIND));
    }

    fn encode(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Ref(self.identity()));
        Ok(())
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        Ok(())
    }

    fn decode(d: &mut Decoder) -> Result<Option<Self>, DecodeError> {
        match d.take()? {
            Value::Null => Ok(None),
            Value::Ref(identity) if identity.kind == K::CODE => Ok(Some(Ref::new(identity.name, identity.home))),
            Value::Ref(identity) => Err(DecodeError::RefKind { column: String::new(), expected: K::CODE, found: identity.kind }),
            other => Err(DecodeError::unexpected("reference", &other)),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        match d.take()? {
            Value::Null => Ok(()),
            other => Err(DecodeError::unexpected("null", &other)),
        }
    }
}

impl<K: Kind> Column for Ref<K> {}

impl<K: Kind> KeyColumn for Ref<K> {}

impl<K: Kind> Element for Ref<K> {
    const TABLES: usize = 0;

    fn layout_row(layout: &mut Layout) {
        <Self as Columns>::layout(layout, "", true);
    }

    fn encode_row(&self, e: &mut Encoder) -> Result<(), EncodeError> {
        <Self as Columns>::encode(self, e)
    }

    fn decode_row(d: &mut Decoder) -> Result<Self, DecodeError> {
        <Self as Columns>::decode(d)?.ok_or_else(|| DecodeError::missing(""))
    }
}
