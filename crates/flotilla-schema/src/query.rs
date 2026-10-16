//! Backend-neutral index queries: what a generated query constructor returns.
//!
//! `#[store(index(name = "open_by_inbox", eq(inbox), ...))]` generates
//! `Message::open_by_inbox(inbox: &Ref<Inbox>) -> IndexQuery<Message>`. The query names the index
//! and carries its `eq` arguments as logical values (strings and identities, not local ids), so
//! any backend can run it; the sqlite runtime pages it by keyset.

use std::{fmt, marker::PhantomData};

use crate::{
    codec::{EncodeError, Encoder},
    traits::{Element, Intern, KeyColumn, Kind},
    value::Value,
};

/// A query of one declared index of kind `K`: rows matching its predicate whose `eq` columns
/// equal the arguments, in `order..., id` order.
pub struct IndexQuery<K> {
    index: &'static str,
    /// An argument that cannot be stored (a `u64` above `i64::MAX`) surfaces when the query runs,
    /// so constructors stay infallible.
    eq: Result<Vec<Value>, EncodeError>,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> IndexQuery<K> {
    /// For generated constructors: `index` is the declared name, `eq` the encoded arguments in
    /// declaration order.
    #[doc(hidden)]
    pub fn new(index: &'static str, eq: Result<Vec<Value>, EncodeError>) -> IndexQuery<K> {
        IndexQuery { index, eq, kind: PhantomData }
    }

    /// The declared index name, unique within `K`.
    pub fn index(&self) -> &'static str {
        self.index
    }

    /// The `eq` arguments as logical values.
    pub fn eq(&self) -> Result<&[Value], EncodeError> {
        self.eq.as_deref().map_err(Clone::clone)
    }
}

impl<K> Clone for IndexQuery<K> {
    fn clone(&self) -> IndexQuery<K> {
        IndexQuery { index: self.index, eq: self.eq.clone(), kind: PhantomData }
    }
}

impl<K> fmt::Debug for IndexQuery<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexQuery").field("index", &self.index).field("eq", &self.eq).finish()
    }
}

/// Encodes one `eq` argument of a single-column type (the `KeyColumn` bound guarantees one value).
#[doc(hidden)]
pub fn key_value<T: KeyColumn + Element>(value: &T) -> Result<Value, EncodeError> {
    let mut rows = Encoder::encode(value)?;
    let mut values = rows.tables.swap_remove(0).pop().map(|row| row.values).unwrap_or_default();
    debug_assert_eq!(values.len(), 1, "a key column is one value");
    Ok(values.pop().unwrap_or(Value::Null))
}

/// Encodes one `eq` argument of an `#[store(intern)]` field.
#[doc(hidden)]
pub fn interned_value<T: Intern>(value: &T) -> Value {
    Value::Str(value.as_str().to_owned())
}
