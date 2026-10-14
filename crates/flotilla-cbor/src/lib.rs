//! The stack CBOR profile of flotilla ADR 0063, as one thin wrapper around an
//! ordinary serde CBOR crate (`cbor2`).
//!
//! Protocols encode and decode through this crate, never by calling a CBOR
//! crate directly, so the profile holds everywhere and the crate underneath
//! stays an implementation detail that the conformance corpus pins down. No
//! `cbor2` type appears in the public API.
//!
//! - **Writers are deterministic** (RFC 8949 §4.2.1): shortest integer and
//!   float forms, definite lengths, and map keys sorted bytewise by their
//!   encoding. A float stays a float even when its value is whole: an `f64`
//!   holding `1.0` is written `f9 3c00`.
//! - **Readers are safe, not strict.** [`from_slice`] rejects what could make
//!   two readers see different messages in the same bytes, or what threatens
//!   the reader: duplicate map keys (compared as decoded text), any tag (the
//!   allowed-tag list is empty), non-text map keys, invalid UTF-8, trailing
//!   bytes, NaN, indefinite lengths, simple values other than `false`, `true`
//!   and `null`, and anything over its [`Limits`]. It accepts well-formed
//!   input that merely is not deterministic, such as a longer integer, float
//!   or length form, or unsorted keys. `-0.0` and the infinities pass through.
//! - **The canonical check is on demand.** [`check_canonical`] also requires
//!   deterministic form, for bytes that are hashed, signed or compared.
//!
//! Typed decoding ignores unknown map keys. A typed float field refuses an
//! integer, so a writer in any language must write float fields as floats.
//!
//! # Read path
//!
//! The message limit is checked first, then `cbor2::validate_slice` checks
//! well-formedness and UTF-8, then one scan of the raw bytes enforces the
//! profile's rejects and limits, and only then does `cbor2` deserialize, with
//! its recursion limit set from [`Limits::max_depth`]. The scan builds no
//! values and never allocates from a declared length or count, so a short
//! message claiming a huge array fails without allocating for it.
//!
//! # Why cbor2
//!
//! `cbor2` (MIT) has a deterministic writer with bytewise key order, keeps
//! floats as floats, is serde-native, and exposes the header decoder the scan
//! builds on. It is young, so it is pinned to an exact version and every
//! upgrade reruns the corpus.

mod frame;
mod scan;

use std::io;

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

pub use self::frame::{Framing, HEADER_LEN, MAGIC_LEN};
use self::scan::{scan, Mode};

/// The ADR 0063 defaults for a protocol that sets no limits of its own.
pub const DEFAULT_LIMITS: Limits = Limits { max_message: 1024 * 1024, max_depth: 64, max_elements: 65_536, max_byte_string: 1024 * 1024 };

/// Bounds a protocol places on one encoded message. Each limit is inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Encoded bytes in one message body, excluding any frame header.
    pub max_message: usize,
    /// Nested arrays and maps. The outermost container is at depth 1. Depths
    /// above 255 act as 255.
    pub max_depth: usize,
    /// Elements in one array, or entries in one map.
    pub max_elements: usize,
    /// Bytes in any one byte string.
    pub max_byte_string: usize,
}

impl Default for Limits {
    fn default() -> Self {
        DEFAULT_LIMITS
    }
}

/// A typed message: one CBOR map whose `type` text field names its kind.
///
/// A struct per type, or a `#[serde(tag = "type")]` enum, are the usual
/// shapes.
pub trait Message: Serialize + DeserializeOwned {
    /// Every `type` this message accepts. Any other type is rejected.
    const TYPES: &'static [&'static str];
}

/// Why a value could not be encoded or decoded under the profile.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("input ends inside a data item")]
    Truncated,
    #[error("bytes follow the data item")]
    TrailingBytes,
    #[error("not well-formed CBOR")]
    Malformed,
    #[error("text string is not UTF-8")]
    InvalidUtf8,
    #[error("indefinite length")]
    IndefiniteLength,
    #[error("NaN")]
    Nan,
    #[error("tag {0} is not allowed")]
    Tag(u64),
    #[error("simple value {0} is not allowed")]
    UnsupportedSimple(u8),
    #[error("map key is not a text string")]
    NonTextKey,
    #[error("duplicate map key{}", .0.as_ref().map(|key| format!(" {key:?}")).unwrap_or_default())]
    DuplicateKey(Option<String>),
    #[error("integer outside the CBOR range")]
    IntegerOutOfRange,
    #[error("integer not in its shortest form")]
    NonShortestInteger,
    #[error("length not in its shortest form")]
    NonShortestLength,
    #[error("float not in its shortest form")]
    NonShortestFloat,
    #[error("map keys out of order")]
    UnsortedKeys,
    #[error("nesting deeper than {limit}")]
    DepthLimit { limit: usize },
    #[error("{count} elements, over the limit of {limit}")]
    ElementLimit { count: u64, limit: usize },
    #[error("message of {len} bytes, over the limit of {limit}")]
    MessageLimit { len: u64, limit: usize },
    #[error("byte string of {len} bytes, over the limit of {limit}")]
    ByteStringLimit { len: u64, limit: usize },
    #[error("message is not a map")]
    NotAMap,
    #[error("message has no type field")]
    MissingType,
    #[error("message type is not a text string")]
    TypeNotText,
    #[error("unknown message type {0:?}")]
    UnknownType(String),
    #[error("frame magic does not match the protocol")]
    BadMagic,
    /// The value does not fit the type, or the type cannot be written.
    #[error("{0}")]
    Schema(String),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
}

impl Error {
    /// A stable name for the error, as used by the conformance corpus.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Truncated => "truncated",
            Error::TrailingBytes => "trailing-bytes",
            Error::Malformed => "malformed",
            Error::InvalidUtf8 => "invalid-utf8",
            Error::IndefiniteLength => "indefinite-length",
            Error::Nan => "nan",
            Error::Tag(_) => "tag",
            Error::UnsupportedSimple(_) => "unsupported-simple",
            Error::NonTextKey => "non-text-key",
            Error::DuplicateKey(_) => "duplicate-key",
            Error::IntegerOutOfRange => "integer-out-of-range",
            Error::NonShortestInteger => "non-shortest-integer",
            Error::NonShortestLength => "non-shortest-length",
            Error::NonShortestFloat => "non-shortest-float",
            Error::UnsortedKeys => "unsorted-keys",
            Error::DepthLimit { .. } => "depth-limit",
            Error::ElementLimit { .. } => "element-limit",
            Error::MessageLimit { .. } => "message-limit",
            Error::ByteStringLimit { .. } => "byte-string-limit",
            Error::NotAMap => "not-a-map",
            Error::MissingType => "missing-type",
            Error::TypeNotText => "type-not-text",
            Error::UnknownType(_) => "unknown-type",
            Error::BadMagic => "bad-magic",
            Error::Schema(_) => "schema",
            Error::Io(_) => "io",
        }
    }

    fn from_de(error: cbor2::de::Error, limits: &Limits) -> Self {
        match error {
            cbor2::de::Error::Io(_) => Error::Truncated,
            cbor2::de::Error::Syntax(_) => Error::Malformed,
            cbor2::de::Error::RecursionLimitExceeded => Error::DepthLimit { limit: limits.max_depth },
            other => Error::Schema(other.to_string()),
        }
    }
}

/// A byte string field. Serde maps a plain `Vec<u8>` to an array of integers;
/// wrap opaque bytes, such as descriptors, in this instead.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(v))
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}

/// Serialize and encode any value deterministically, within `limits`. Use
/// [`encode_message`] for messages.
///
/// The output is scanned like any input, so a writer never produces what a
/// reader would reject: NaN, a map key that is not text, an integer outside
/// `-2^64 ..= 2^64 - 1` (which `cbor2` would write as a bignum tag), or
/// anything over the limits.
pub fn to_vec<T: Serialize + ?Sized>(value: &T, limits: &Limits) -> Result<Vec<u8>, Error> {
    let bytes = cbor2::to_canonical_vec(value).map_err(|error| match error {
        // cbor2 reports a duplicate key only through this message text. The
        // corpus's duplicate-key encode vector fails if an upgrade changes it.
        cbor2::ser::Error::Value(message) if message == "duplicate map key" => Error::DuplicateKey(None),
        cbor2::ser::Error::Value(message) => Error::Schema(message),
        cbor2::ser::Error::Io(error) => Error::Io(error),
    })?;
    check_size(bytes.len(), limits)?;
    scan(&bytes, limits, Mode::Canonical).map_err(|error| match error {
        Error::Tag(2 | 3) => Error::IntegerOutOfRange,
        other => other,
    })?;
    Ok(bytes)
}

/// Check that `bytes` hold exactly one data item a safe reader accepts within
/// `limits`, without decoding it.
pub fn check(bytes: &[u8], limits: &Limits) -> Result<(), Error> {
    check_mode(bytes, limits, Mode::Safe)
}

/// Check that `bytes` pass [`check`] and are also in deterministic form, the
/// one byte form a writer produces for their value. Use this where bytes are
/// hashed, signed or compared.
pub fn check_canonical(bytes: &[u8], limits: &Limits) -> Result<(), Error> {
    check_mode(bytes, limits, Mode::Canonical)
}

/// Whether [`check_canonical`] accepts `bytes`.
pub fn is_canonical(bytes: &[u8], limits: &Limits) -> bool {
    check_canonical(bytes, limits).is_ok()
}

fn check_size(len: usize, limits: &Limits) -> Result<(), Error> {
    if len > limits.max_message {
        return Err(Error::MessageLimit { len: len as u64, limit: limits.max_message });
    }
    Ok(())
}

fn check_mode(bytes: &[u8], limits: &Limits, mode: Mode) -> Result<(), Error> {
    check_size(bytes.len(), limits)?;
    if let Err(error) = cbor2::validate_slice(bytes) {
        // The scan names the problem more precisely when it finds one.
        scan(bytes, limits, mode)?;
        return Err(Error::from_de(error, limits));
    }
    scan(bytes, limits, mode)
}

/// Check, then deserialize, any value. Use [`decode_message`] for messages.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8], limits: &Limits) -> Result<T, Error> {
    check(bytes, limits)?;
    deserialize(bytes, limits)
}

/// Deserialize bytes that have passed [`check`].
fn deserialize<'de, T: Deserialize<'de>>(bytes: &'de [u8], limits: &Limits) -> Result<T, Error> {
    let mut deserializer = cbor2::de::Deserializer::from_slice_with_recursion_limit(bytes, limits.max_depth.saturating_add(1));
    T::deserialize(&mut deserializer).map_err(|error| Error::from_de(error, limits))
}

/// Encode a message body, checking that it is a map of a known `type`.
pub fn encode_message<M: Message>(message: &M, limits: &Limits) -> Result<Vec<u8>, Error> {
    let bytes = to_vec(message, limits)?;
    checked_message_type(&bytes, limits, M::TYPES)?;
    Ok(bytes)
}

/// Decode a message body: the profile first, then its `type`, then its fields.
pub fn decode_message<M: Message>(bytes: &[u8], limits: &Limits) -> Result<M, Error> {
    check(bytes, limits)?;
    checked_message_type(bytes, limits, M::TYPES)?;
    deserialize(bytes, limits)
}

/// The `type` of a message body, if it passes [`check`], is a map, and its
/// `type` is a text string in `types`.
pub fn message_type(bytes: &[u8], limits: &Limits, types: &[&str]) -> Result<String, Error> {
    check(bytes, limits)?;
    checked_message_type(bytes, limits, types)
}

fn checked_message_type(bytes: &[u8], limits: &Limits, types: &[&str]) -> Result<String, Error> {
    /// Only the `type` field; every other key is skipped unread.
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "type", default, deserialize_with = "present")]
        kind: Option<cbor2::Value>,
    }
    fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<cbor2::Value>, D::Error> {
        cbor2::Value::deserialize(deserializer).map(Some)
    }

    // The bytes passed the scan, so the first head is the whole story: a map
    // is major type 5 with a definite length.
    if bytes.first().is_none_or(|head| head >> 5 != 5) {
        return Err(Error::NotAMap);
    }
    let probe: Probe = deserialize(bytes, limits)?;
    match probe.kind {
        None => Err(Error::MissingType),
        Some(cbor2::Value::Text(kind)) if types.contains(&kind.as_str()) => Ok(kind),
        Some(cbor2::Value::Text(kind)) => Err(Error::UnknownType(kind)),
        Some(_) => Err(Error::TypeNotText),
    }
}

#[cfg(test)]
mod tests;
