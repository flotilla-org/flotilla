//! One pass over the raw bytes of a message that enforces what the profile
//! adds to well-formed CBOR: the safe-reader rejects and the limits, and on
//! request the deterministic form.
//!
//! The scan builds no values and never allocates from a declared length or
//! count. Every declared size is compared with its limit and with the bytes
//! left before it is used; the only allocation is one set of key slices per
//! map, which grows with the keys actually present. Headers are decoded with
//! `cbor2::core`, so the scan and the deserializer agree on what each head
//! means.
use std::collections::HashSet;

use cbor2::core::{self as cbor_core, Decoder, Header};

use crate::{Error, Limits};

/// The deepest nesting the scan follows, whatever the limits say. It matches
/// `cbor2::validate_slice`, which refuses anything deeper, and bounds the
/// scan's recursion.
pub(crate) const MAX_DEPTH: usize = 255;

/// What the scan requires beyond well-formed CBOR.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The safe-reader rejects and the limits.
    Safe,
    /// As `Safe`, plus deterministic form (RFC 8949 §4.2.1): shortest heads
    /// and floats, and map keys in ascending bytewise order of their
    /// encodings.
    Canonical,
}

pub(crate) fn scan(bytes: &[u8], limits: &Limits, mode: Mode) -> Result<(), Error> {
    let mut scanner = Scanner { bytes, pos: 0, limits, max_depth: limits.max_depth.min(MAX_DEPTH), canonical: mode == Mode::Canonical };
    scanner.item(0)?;
    if scanner.pos != bytes.len() {
        return Err(Error::TrailingBytes);
    }
    Ok(())
}

struct Scanner<'a, 'l> {
    bytes: &'a [u8],
    pos: usize,
    limits: &'l Limits,
    max_depth: usize,
    canonical: bool,
}

impl<'a> Scanner<'a, '_> {
    /// The next head and its raw bytes.
    fn head(&mut self) -> Result<(Header, &'a [u8]), Error> {
        let rest = &self.bytes[self.pos..];
        let mut decoder = Decoder::from(rest);
        let header = decoder.pull().map_err(|error| match error {
            // A slice reader only fails by running out of input.
            cbor_core::Error::Io(_) => Error::Truncated,
            cbor_core::Error::Syntax(_) => Error::Malformed,
        })?;
        let raw = &rest[..decoder.offset()];
        self.pos += raw.len();
        Ok((header, raw))
    }

    /// `len` body bytes, without trusting `len` for anything but the bounds.
    fn body(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(len).filter(|&end| end <= self.bytes.len()).ok_or(Error::Truncated)?;
        let body = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(body)
    }

    /// The head of a non-float item must use the shortest argument width.
    fn shortest(&self, raw: &[u8], arg: u64, error: Error) -> Result<(), Error> {
        let width = match arg {
            0..=23 => 1,
            24..=0xff => 2,
            0x100..=0xffff => 3,
            0x1_0000..=0xffff_ffff => 5,
            _ => 9,
        };
        if self.canonical && raw.len() != width {
            return Err(error);
        }
        Ok(())
    }

    /// A definite length or count, which the profile requires.
    fn definite(len: Option<usize>) -> Result<usize, Error> {
        len.ok_or(Error::IndefiniteLength)
    }

    /// A container's own depth, checked before anything inside it is read.
    fn enter(&self, depth: usize) -> Result<usize, Error> {
        let depth = depth + 1;
        if depth > self.max_depth {
            return Err(Error::DepthLimit { limit: self.limits.max_depth });
        }
        Ok(depth)
    }

    fn elements(&self, count: usize) -> Result<(), Error> {
        if count > self.limits.max_elements {
            return Err(Error::ElementLimit { count: count as u64, limit: self.limits.max_elements });
        }
        Ok(())
    }

    /// One data item, nested inside `depth` arrays and maps.
    fn item(&mut self, depth: usize) -> Result<(), Error> {
        let (header, raw) = self.head()?;
        match header {
            Header::Positive(n) | Header::Negative(n) => self.shortest(raw, n, Error::NonShortestInteger),
            Header::Float(f) => {
                if f.is_nan() {
                    return Err(Error::Nan);
                }
                let width = if cbor_core::f64_to_f16(f).is_some() {
                    3
                } else if cbor_core::f64_to_f32(f).is_some() {
                    5
                } else {
                    9
                };
                if self.canonical && raw.len() != width {
                    return Err(Error::NonShortestFloat);
                }
                Ok(())
            }
            Header::Simple(cbor_core::simple::FALSE | cbor_core::simple::TRUE | cbor_core::simple::NULL) => Ok(()),
            Header::Simple(value) => Err(Error::UnsupportedSimple(value)),
            Header::Tag(tag) => Err(Error::Tag(tag)),
            Header::Break => Err(Error::Malformed),
            Header::Bytes(len) => {
                let len = Self::definite(len)?;
                if len > self.limits.max_byte_string {
                    return Err(Error::ByteStringLimit { len: len as u64, limit: self.limits.max_byte_string });
                }
                self.shortest(raw, len as u64, Error::NonShortestLength)?;
                self.body(len).map(drop)
            }
            Header::Text(len) => {
                let len = Self::definite(len)?;
                self.shortest(raw, len as u64, Error::NonShortestLength)?;
                self.text(len).map(drop)
            }
            Header::Array(len) => {
                let len = Self::definite(len)?;
                let depth = self.enter(depth)?;
                self.elements(len)?;
                self.shortest(raw, len as u64, Error::NonShortestLength)?;
                for _ in 0..len {
                    self.item(depth)?;
                }
                Ok(())
            }
            Header::Map(len) => {
                let len = Self::definite(len)?;
                let depth = self.enter(depth)?;
                self.elements(len)?;
                self.shortest(raw, len as u64, Error::NonShortestLength)?;
                self.map_entries(len, depth)
            }
        }
    }

    /// A text string's UTF-8 content, after its head.
    fn text(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let body = self.body(len)?;
        std::str::from_utf8(body).map_err(|_| Error::InvalidUtf8)?;
        Ok(body)
    }

    fn map_entries(&mut self, len: usize, depth: usize) -> Result<(), Error> {
        // Keys compare as decoded text, whatever their encoded form. The set
        // grows with the keys actually read, never with the declared count.
        let mut seen = HashSet::new();
        let mut previous: Option<&[u8]> = None;
        for _ in 0..len {
            let start = self.pos;
            let (header, raw) = self.head()?;
            let Header::Text(key_len) = header else {
                return Err(Error::NonTextKey);
            };
            let key_len = Self::definite(key_len)?;
            let key = self.text(key_len)?;
            // A duplicate is reported as one whatever the form of either copy,
            // so the canonical check names the same problem as a safe reader.
            if !seen.insert(key) {
                return Err(Error::DuplicateKey(Some(String::from_utf8_lossy(key).into_owned())));
            }
            self.shortest(raw, key_len as u64, Error::NonShortestLength)?;
            if self.canonical {
                // Every head is already shortest, so the raw key is its
                // deterministic encoding.
                let encoded = &self.bytes[start..self.pos];
                if previous.is_some_and(|previous| previous >= encoded) {
                    return Err(Error::UnsortedKeys);
                }
                previous = Some(encoded);
            }
            self.item(depth)?;
        }
        Ok(())
    }
}
