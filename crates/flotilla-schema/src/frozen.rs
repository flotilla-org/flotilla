//! Frozen documents: immutable serde values stored once, by content hash, in `objects`.

use std::{cmp::Ordering, fmt, hash, ops::Deref, sync::Arc};

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

use crate::{codec::DecodeError, value::Hash};

/// The only place serde reaches storage: a `Frozen<T>` field (`#[store(frozen)]`) stores the
/// SHA-256 of `T`'s canonical CBOR (ADR 0063, `flotilla_cbor::to_vec`) and the bytes go to
/// `objects` once, shared by every row naming the same content.
///
/// Equality, ordering and hashing are by content hash. The canonical bytes are kept beside the
/// value, so writing the owner never serializes `T` again and the stored hash always names the
/// bytes it was computed from, even for a `T` whose serde form does not round-trip exactly.
pub struct Frozen<T> {
    hash: Hash,
    value: Arc<T>,
    bytes: Arc<[u8]>,
}

/// A type usable as `Frozen<T>`'s content.
pub trait Freezable: Serialize + DeserializeOwned {}

impl<T: Serialize + DeserializeOwned> Freezable for T {}

impl<T: Freezable> Frozen<T> {
    /// Freezes a value. Fails only when `T` has no canonical CBOR form within the ADR 0063 default
    /// limits (1 MiB, depth 64), such as a map with non-text keys.
    pub fn new(value: T) -> Result<Frozen<T>, flotilla_cbor::Error> {
        let bytes = flotilla_cbor::to_vec(&value, &flotilla_cbor::DEFAULT_LIMITS)?;
        Ok(Frozen { hash: Hash::of(&bytes), value: Arc::new(value), bytes: bytes.into() })
    }

    /// Rebuilds a frozen value from stored bytes, refusing bytes that do not hash to `hash`.
    pub fn from_stored(hash: Hash, bytes: &[u8]) -> Result<Frozen<T>, DecodeError> {
        let found = Hash::of(bytes);
        if found != hash {
            return Err(DecodeError::ObjectHash { expected: hash, found });
        }
        let value = flotilla_cbor::from_slice(bytes, &flotilla_cbor::DEFAULT_LIMITS)
            .map_err(|error| DecodeError::Object { hash, kind: error.kind() })?;
        Ok(Frozen { hash, value: Arc::new(value), bytes: bytes.into() })
    }
}

impl<T> Frozen<T> {
    pub fn hash(&self) -> Hash {
        self.hash
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    /// The shared value, for holders that outlive this handle.
    pub fn shared(&self) -> &Arc<T> {
        &self.value
    }

    /// The canonical CBOR bytes, as stored in `objects`.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl<T> Deref for Frozen<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

// Manual impls: they depend on the hash only, not on `T`.
impl<T> Clone for Frozen<T> {
    fn clone(&self) -> Frozen<T> {
        Frozen { hash: self.hash, value: Arc::clone(&self.value), bytes: Arc::clone(&self.bytes) }
    }
}

impl<T> PartialEq for Frozen<T> {
    fn eq(&self, other: &Frozen<T>) -> bool {
        self.hash == other.hash
    }
}

impl<T> Eq for Frozen<T> {}

impl<T> PartialOrd for Frozen<T> {
    fn partial_cmp(&self, other: &Frozen<T>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Frozen<T> {
    fn cmp(&self, other: &Frozen<T>) -> Ordering {
        self.hash.cmp(&other.hash)
    }
}

impl<T> hash::Hash for Frozen<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

impl<T: fmt::Debug> fmt::Debug for Frozen<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frozen").field("hash", &self.hash).field("value", &self.value).finish()
    }
}

/// Serde sees the value itself; deserializing freezes it again.
impl<T: Serialize> Serialize for Frozen<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, T: Freezable> Deserialize<'de> for Frozen<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Frozen<T>, D::Error> {
        Frozen::new(T::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::Frozen;
    use crate::{codec::DecodeError, value::Hash};

    // Behaviour (DESIGN Frozen): the hash is SHA-256 of the canonical CBOR, so equal content gives
    // equal frozen values whatever the construction order; stored bytes are verified against
    // their hash before they are decoded.
    #[test]
    fn hash_is_of_canonical_bytes_and_verified_on_load() {
        let one: BTreeMap<String, u32> = [("b".to_owned(), 2), ("a".to_owned(), 1)].into();
        let frozen = Frozen::new(one.clone()).expect("freeze");
        assert_eq!(frozen.hash(), Hash::of(frozen.bytes()));
        assert_eq!(frozen, Frozen::new(one.into_iter().rev().collect::<BTreeMap<_, _>>()).expect("freeze"));
        assert_ne!(frozen, Frozen::new(BTreeMap::<String, u32>::new()).expect("freeze"));

        let loaded = Frozen::<BTreeMap<String, u32>>::from_stored(frozen.hash(), frozen.bytes()).expect("load");
        assert_eq!(loaded.value(), frozen.value());
        let other = Hash::of(b"other");
        assert_eq!(
            Frozen::<BTreeMap<String, u32>>::from_stored(other, frozen.bytes()),
            Err(DecodeError::ObjectHash { expected: other, found: frozen.hash() })
        );
        let garbage = [0xff_u8];
        assert!(matches!(Frozen::<BTreeMap<String, u32>>::from_stored(Hash::of(&garbage), &garbage), Err(DecodeError::Object { .. })));
    }
}
