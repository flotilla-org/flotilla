//! Canonical percent-encoding of one segment value (ADR 0066 §4).
//!
//! Only RFC 3986 unreserved bytes (`A–Z a–z 0–9 - . _ ~`) stay bare. Every
//! other byte of the UTF-8 value is written `%XX` with uppercase hex, so one
//! value has exactly one spelling and an encoded value never contains `/`,
//! `:`, `@` or a space.

use std::fmt::Write as _;

use crate::Error;

pub(crate) fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Appends the canonical encoding of `value` to `out`.
pub(crate) fn encode_into(value: &str, out: &mut String) {
    for &byte in value.as_bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
}

/// The canonical encoding of `value`.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    encode_into(value, &mut out);
    out
}

/// Decodes a value that must already be in canonical form.
///
/// Refuses lowercase hex, an escape of an unreserved byte, a bare reserved
/// byte, a truncated escape and an escape sequence that is not UTF-8.
pub fn decode_canonical(encoded: &str) -> Result<String, Error> {
    decode(encoded, true)
}

/// Decodes a value typed by a person: escapes may use either hex case and may
/// escape unreserved bytes. Bare reserved bytes other than `/` are kept as
/// written, because the caller has already split on `/`.
pub(crate) fn decode_lenient(encoded: &str) -> Result<String, Error> {
    decode(encoded, false)
}

fn decode(encoded: &str, canonical: bool) -> Result<String, Error> {
    let non_canonical = || Error::NonCanonicalEncoding(encoded.to_string());
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let digits = bytes.get(index + 1..index + 3).ok_or_else(|| Error::InvalidEscape(encoded.to_string()))?;
            let value = hex_pair(digits, canonical).ok_or_else(|| {
                if hex_pair(digits, false).is_some() {
                    non_canonical()
                } else {
                    Error::InvalidEscape(encoded.to_string())
                }
            })?;
            if canonical && is_unreserved(value) {
                return Err(non_canonical());
            }
            out.push(value);
            index += 3;
        } else {
            if canonical && !is_unreserved(byte) {
                return Err(non_canonical());
            }
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::InvalidEscape(encoded.to_string()))
}

fn hex_pair(digits: &[u8], uppercase_only: bool) -> Option<u8> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' if !uppercase_only => Some(byte - b'a' + 10),
        _ => None,
    };
    Some(digit(digits[0])? << 4 | digit(digits[1])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR 0066 §4: only unreserved bytes stay bare; everything else, including
    // `/`, `:`, `%` and space, is encoded with uppercase hex.
    #[test]
    fn encodes_every_reserved_byte_with_uppercase_hex() {
        assert_eq!(encode("flotilla-org/flotilla"), "flotilla-org%2Fflotilla");
        assert_eq!(encode("a b:c%d~e._-"), "a%20b%3Ac%25d~e._-");
        assert_eq!(encode("é"), "%C3%A9");
    }

    // ADR 0066 §4: the parser refuses non-canonical input in stored data.
    #[test]
    fn canonical_decode_refuses_every_other_spelling() {
        for spelling in ["a%2fb", "a%41", "a b", "a/b", "a:b", "%", "%2", "%G0", "%FF"] {
            assert!(decode_canonical(spelling).is_err(), "{spelling:?} must be refused");
        }
        assert_eq!(decode_canonical("a%2Fb").as_deref(), Ok("a/b"));
    }

    #[test]
    fn lenient_decode_accepts_lowercase_hex_and_redundant_escapes() {
        assert_eq!(decode_lenient("a%2fb%41").as_deref(), Ok("a/bA"));
    }
}
