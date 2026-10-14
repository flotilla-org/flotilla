//! One pass over the raw bytes of a message that enforces what the profile
//! adds to well-formed CBOR: the safe-reader rejects and the limits.
//!
//! The scan builds no values and never allocates from a declared length or
//! count. Every declared size is compared with its limit and with the bytes
//! left before it is used; the only allocation is one set of key slices per
//! map, which grows with the keys actually present. Headers are decoded with
//! `cbor2::core`, so the scan and the deserializer agree on each head.
use std::collections::HashSet;

use cbor2::core::{self as cbor_core, simple, Decoder, Header};

use crate::{Error, ErrorKind, Limits};

/// The deepest nesting the scan follows, whatever the limits say. It matches
/// `cbor2::validate_slice`, which refuses anything deeper, and bounds the
/// scan's recursion.
const MAX_DEPTH: usize = 255;

pub(crate) fn scan(bytes: &[u8], limits: &Limits) -> Result<(), Error> {
    let mut scanner = Scanner { bytes, pos: 0, limits, max_depth: limits.max_depth.min(MAX_DEPTH) };
    scanner.item(0)?;
    if scanner.pos != bytes.len() {
        return Err(ErrorKind::TrailingBytes.into());
    }
    Ok(())
}

fn over(kind: ErrorKind, count: usize, limit: usize) -> Error {
    Error::detail(kind, format!("{count}, limit {limit}"))
}

struct Scanner<'a, 'l> {
    bytes: &'a [u8],
    pos: usize,
    limits: &'l Limits,
    max_depth: usize,
}

impl<'a> Scanner<'a, '_> {
    fn head(&mut self) -> Result<Header, Error> {
        let mut decoder = Decoder::from(&self.bytes[self.pos..]);
        let header = decoder.pull().map_err(|error| match error {
            // A slice reader only fails by running out of input.
            cbor_core::Error::Io(_) => ErrorKind::Truncated,
            cbor_core::Error::Syntax(_) => ErrorKind::Malformed,
        })?;
        self.pos += decoder.offset();
        Ok(header)
    }

    /// A definite length or count, which the profile requires.
    fn definite(len: Option<usize>) -> Result<usize, Error> {
        len.ok_or_else(|| ErrorKind::IndefiniteLength.into())
    }

    /// `len` body bytes, trusting `len` for nothing but the bounds check.
    fn body(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(len).filter(|&end| end <= self.bytes.len()).ok_or(ErrorKind::Truncated)?;
        let body = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(body)
    }

    fn text(&mut self, len: Option<usize>) -> Result<&'a [u8], Error> {
        let body = self.body(Self::definite(len)?)?;
        std::str::from_utf8(body).map_err(|_| ErrorKind::InvalidUtf8)?;
        Ok(body)
    }

    /// A container's depth and count, both checked before anything inside it
    /// is read.
    fn enter(&self, depth: usize, len: Option<usize>) -> Result<(usize, usize), Error> {
        let len = Self::definite(len)?;
        if depth + 1 > self.max_depth {
            return Err(Error::detail(ErrorKind::DepthLimit, self.limits.max_depth));
        }
        if len > self.limits.max_elements {
            return Err(over(ErrorKind::ElementLimit, len, self.limits.max_elements));
        }
        Ok((depth + 1, len))
    }

    /// One data item, nested inside `depth` arrays and maps.
    fn item(&mut self, depth: usize) -> Result<(), Error> {
        match self.head()? {
            Header::Positive(_) | Header::Negative(_) => Ok(()),
            Header::Float(f) if f.is_nan() => Err(ErrorKind::Nan.into()),
            Header::Float(_) | Header::Simple(simple::FALSE | simple::TRUE | simple::NULL) => Ok(()),
            Header::Simple(value) => Err(Error::detail(ErrorKind::UnsupportedSimple, value)),
            Header::Tag(tag) => Err(Error::detail(ErrorKind::Tag, tag)),
            Header::Break => Err(ErrorKind::Malformed.into()),
            Header::Bytes(len) => {
                let len = Self::definite(len)?;
                if len > self.limits.max_byte_string {
                    return Err(over(ErrorKind::ByteStringLimit, len, self.limits.max_byte_string));
                }
                self.body(len).map(drop)
            }
            Header::Text(len) => self.text(len).map(drop),
            Header::Array(len) => {
                let (depth, len) = self.enter(depth, len)?;
                (0..len).try_for_each(|_| self.item(depth))
            }
            Header::Map(len) => {
                let (depth, len) = self.enter(depth, len)?;
                // Keys compare as decoded text, whatever their encoded form.
                // The set grows with the keys actually read.
                let mut seen = HashSet::new();
                for _ in 0..len {
                    let Header::Text(key_len) = self.head()? else {
                        return Err(ErrorKind::NonTextKey.into());
                    };
                    let key = self.text(key_len)?;
                    if !seen.insert(key) {
                        return Err(Error::detail(ErrorKind::DuplicateKey, format!("{:?}", String::from_utf8_lossy(key))));
                    }
                    self.item(depth)?;
                }
                Ok(())
            }
        }
    }
}
