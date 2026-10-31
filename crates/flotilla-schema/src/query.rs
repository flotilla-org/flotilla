//! Backend-neutral index queries: what a generated query constructor returns.
//!
//! `#[store(index(name = "open_by_inbox", eq(inbox), ...))]` generates
//! `Message::open_by_inbox(inbox: &Ref<Inbox>) -> IndexQuery<Message>`. The query names its kind
//! and index and carries its `eq` arguments as logical values (strings and identities, not local
//! ids), so any backend can run it; the sqlite runtime pages it by keyset. A kind's spec declares
//! the indexes of the kind its holder stores (`MessageSpec::open_by_inbox`), so the query's type is
//! the declaring type and the kind is named by its code. An index on a `child` list
//! (`index(name = "by_closed", eq(closes))`) takes one element and reads the resources holding an
//! equal row.

use std::{fmt, marker::PhantomData};

use crate::{
    codec::{EncodeError, Encoder},
    traits::{Element, Intern, KeyColumn},
    value::{KindCode, Value},
};

/// A query of one index that `K` declares (a kind, or a kind's spec): rows of the kind matching
/// its predicate whose `eq` columns equal the arguments, in `order..., id` order; for an index on
/// a `child` list, the kind's resources holding a row equal to the argument, in id order.
pub struct IndexQuery<K> {
    kind: KindCode,
    index: &'static str,
    /// An argument that cannot be stored (a `u64` above `i64::MAX`) surfaces when the query runs,
    /// so constructors stay infallible.
    eq: Result<Vec<Value>, EncodeError>,
    declared_by: PhantomData<fn() -> K>,
}

impl<K> IndexQuery<K> {
    /// For generated constructors: `kind` is the code of the kind the index is on, `index` the
    /// declared name, `eq` the encoded arguments in declaration order.
    #[doc(hidden)]
    pub fn new(kind: KindCode, index: &'static str, eq: Result<Vec<Value>, EncodeError>) -> IndexQuery<K> {
        IndexQuery { kind, index, eq, declared_by: PhantomData }
    }

    /// The code of the kind whose rows the query reads.
    pub fn kind(&self) -> KindCode {
        self.kind
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
        IndexQuery { kind: self.kind, index: self.index, eq: self.eq.clone(), declared_by: PhantomData }
    }
}

impl<K> fmt::Debug for IndexQuery<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexQuery").field("kind", &self.kind).field("index", &self.index).field("eq", &self.eq).finish()
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

/// Encodes the argument of an index on a `child` list: one element's row values, which are the
/// `eq` values in column order.
#[doc(hidden)]
pub fn element_values<T: Element>(value: &T) -> Result<Vec<Value>, EncodeError> {
    let mut rows = Encoder::encode(value)?;
    Ok(rows.tables.swap_remove(0).pop().map(|row| row.values).unwrap_or_default())
}

/// Encodes one `eq` argument of an `#[store(intern = "tx")]` field.
#[doc(hidden)]
pub fn interned_value<T: Intern>(value: &T) -> Value {
    Value::Str(value.as_str().to_owned())
}
