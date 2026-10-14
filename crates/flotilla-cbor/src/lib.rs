//! The stack CBOR profile of flotilla ADR 0063, as one thin wrapper around
//! `cbor2`. Protocols encode and decode through this crate, never through a
//! CBOR crate directly, and no `cbor2` type appears in the public API.
//!
//! - **Writers are deterministic** (RFC 8949 §4.2.1): shortest integer and
//!   float forms, definite lengths, map keys sorted bytewise. A whole-number
//!   float stays a float: an `f64` holding `1.0` is written `f9 3c00`.
//! - **Readers are safe, not strict.** They reject duplicate map keys
//!   (compared as decoded text), any tag, non-text map keys, invalid UTF-8,
//!   trailing bytes, NaN, indefinite lengths, simple values other than
//!   `false`, `true` and `null`, and anything over its [`Limits`]. They accept
//!   well-formed input that merely is not deterministic.
//!
//! Typed decoding ignores unknown map keys. A typed float field refuses an
//! integer, so writers in every language must write float fields as floats.
//! The ADR's on-demand canonical check is not implemented yet; it arrives with
//! its first user.
//!
//! # Read path
//!
//! The message limit is checked first, then `cbor2::validate_slice` checks
//! well-formedness and UTF-8, then one scan of the raw bytes enforces the
//! profile's rejects and limits, and only then does `cbor2` deserialize, with
//! its recursion limit set from [`Limits::max_depth`]. The scan never
//! allocates from a declared length or count, so a short message claiming a
//! huge array fails without allocating for it.

mod frame;
mod scan;

use std::{fmt, io};

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

pub use self::frame::{Framing, HEADER_LEN, MAGIC_LEN};

/// The ADR 0063 defaults for a protocol that sets no limits of its own.
pub const DEFAULT_LIMITS: Limits = Limits { max_message: 1024 * 1024, max_depth: 64, max_elements: 65_536, max_byte_string: 1024 * 1024 };

/// Bounds a protocol places on one encoded message. Each limit is inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Encoded bytes in one message body, excluding any frame header.
    pub max_message: usize,
    /// Nested arrays and maps; the outermost container is at depth 1. Depths
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

/// A typed message: one CBOR map whose `type` text field names its kind,
/// usually a struct per type or a `#[serde(tag = "type")]` enum.
pub trait Message: Serialize + DeserializeOwned {
    /// Every `type` this message accepts. Any other type is rejected.
    const TYPES: &'static [&'static str];
}

macro_rules! error_kinds {
    ($($(#[$doc:meta])* $kind:ident => $name:literal,)*) => {
        /// What kind of problem an [`Error`] reports.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum ErrorKind {
            $($(#[$doc])* $kind,)*
        }

        impl ErrorKind {
            /// The stable name the conformance corpus uses.
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$kind => $name,)*
                }
            }
        }
    };
}

error_kinds! {
    Truncated => "truncated",
    TrailingBytes => "trailing-bytes",
    Malformed => "malformed",
    InvalidUtf8 => "invalid-utf8",
    IndefiniteLength => "indefinite-length",
    Nan => "nan",
    Tag => "tag",
    UnsupportedSimple => "unsupported-simple",
    NonTextKey => "non-text-key",
    DuplicateKey => "duplicate-key",
    DepthLimit => "depth-limit",
    ElementLimit => "element-limit",
    MessageLimit => "message-limit",
    ByteStringLimit => "byte-string-limit",
    NotAMap => "not-a-map",
    MissingType => "missing-type",
    /// A `type` outside the message's list, or not a text string.
    UnknownType => "unknown-type",
    BadMagic => "bad-magic",
    /// The value does not fit the type, or the type cannot be written.
    Schema => "schema",
    Io => "io",
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a value could not be encoded or decoded under the profile.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{kind}{}", detail.as_ref().map(|detail| format!(": {detail}")).unwrap_or_default())]
    Profile { kind: ErrorKind, detail: Option<String> },
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl Error {
    /// What kind of problem this is; [`ErrorKind::Io`] for [`Error::Io`].
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Profile { kind, .. } => *kind,
            Error::Io(_) => ErrorKind::Io,
        }
    }

    pub(crate) fn detail(kind: ErrorKind, detail: impl fmt::Display) -> Self {
        Error::Profile { kind, detail: Some(detail.to_string()) }
    }

    fn from_de(error: cbor2::de::Error, limits: &Limits) -> Self {
        match error {
            cbor2::de::Error::Io(_) => ErrorKind::Truncated.into(),
            cbor2::de::Error::Syntax(_) => ErrorKind::Malformed.into(),
            cbor2::de::Error::RecursionLimitExceeded => Error::detail(ErrorKind::DepthLimit, limits.max_depth),
            other => Error::detail(ErrorKind::Schema, other),
        }
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::Profile { kind, detail: None }
    }
}

/// A byte string field. Serde writes a plain `Vec<u8>` as an array of
/// integers; wrap opaque bytes, such as descriptors, in this instead.
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
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
        }
        deserializer.deserialize_byte_buf(Visitor)
    }
}

/// Serialize any value deterministically, within `limits`. Use
/// [`encode_message`] for messages.
///
/// The output is scanned like any input, so a writer never produces what a
/// reader would reject: NaN, a non-text map key, an integer outside
/// `-2^64 ..= 2^64 - 1` (which `cbor2` would write as a bignum tag), or
/// anything over the limits.
pub fn to_vec<T: Serialize + ?Sized>(value: &T, limits: &Limits) -> Result<Vec<u8>, Error> {
    let bytes = cbor2::to_canonical_vec(value).map_err(|error| match error {
        // cbor2 reports a duplicate key only through this message text; a
        // unit test fails if an upgrade rewords it.
        cbor2::ser::Error::Value(message) if message == "duplicate map key" => ErrorKind::DuplicateKey.into(),
        cbor2::ser::Error::Value(message) => Error::detail(ErrorKind::Schema, message),
        cbor2::ser::Error::Io(error) => Error::Io(error),
    })?;
    check(&bytes, limits).map_err(|error| match error.kind() {
        ErrorKind::Tag => Error::detail(ErrorKind::Schema, "integer outside the CBOR range"),
        _ => error,
    })?;
    Ok(bytes)
}

/// Check that `bytes` hold exactly one data item a safe reader accepts within
/// `limits`, without decoding it.
pub fn check(bytes: &[u8], limits: &Limits) -> Result<(), Error> {
    if bytes.len() > limits.max_message {
        return Err(Error::detail(ErrorKind::MessageLimit, format!("{} bytes, limit {}", bytes.len(), limits.max_message)));
    }
    if let Err(error) = cbor2::validate_slice(bytes) {
        // The scan names the problem more precisely when it finds one.
        scan::scan(bytes, limits)?;
        return Err(Error::from_de(error, limits));
    }
    scan::scan(bytes, limits)
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
    message_type(bytes, limits, M::TYPES)?;
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

    // The bytes passed the scan, so a map is major type 5 in the first head.
    if bytes.first().is_none_or(|head| head >> 5 != 5) {
        return Err(ErrorKind::NotAMap.into());
    }
    match deserialize::<Probe>(bytes, limits)?.kind {
        None => Err(ErrorKind::MissingType.into()),
        Some(cbor2::Value::Text(kind)) if types.contains(&kind.as_str()) => Ok(kind),
        Some(cbor2::Value::Text(kind)) => Err(Error::detail(ErrorKind::UnknownType, format!("{kind:?}"))),
        Some(_) => Err(Error::detail(ErrorKind::UnknownType, "type is not a text string")),
    }
}

#[cfg(test)]
mod tests;
