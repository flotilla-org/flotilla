//! Stored codes (#3392 ruling G7): short explicit text, written by hand and never generated from a
//! name. Kind codes, state labels, data-enum variants and edge kinds are all [`Code`]s, stored as
//! `TEXT` restricted by a CHECK to the enum's codes.
//!
//! A code is one to [`Code::MAX`] bytes: a lowercase ASCII letter, then lowercase letters, digits
//! or `_`. [`Code::new`] is a `const fn` that panics on anything else, so a code the derive
//! generates (`const { Code::new("dr") }`) is checked when the crate compiles. A rename changes a
//! name, never a code, so anything keyed by code survives it.

use std::{cmp::Ordering, fmt};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    codec::{DecodeError, Decoder},
    value::Value,
};

/// A stored code: short text, held inline so it is `Copy` and decodes without allocating. Unused
/// bytes are zero, so the derived equality and hash are the text's, and a code constant can be a
/// match pattern.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code {
    len: u8,
    bytes: [u8; Code::MAX],
}

/// Why a text is not a code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{text:?} is not a code: {problem}")]
pub struct CodeError {
    pub text: String,
    pub problem: &'static str,
}

impl Code {
    /// The longest code, in bytes.
    pub const MAX: usize = 8;

    /// The code `text`, panicking when it is not one; in a `const` context the panic is a
    /// compile error naming the problem.
    pub const fn new(text: &str) -> Code {
        match Code::check(text) {
            Ok(code) => code,
            Err(problem) => panic!("{}", problem),
        }
    }

    /// The code `text`, or why it is not one.
    pub fn parse(text: &str) -> Result<Code, CodeError> {
        Code::check(text).map_err(|problem| CodeError { text: text.to_owned(), problem })
    }

    const fn check(text: &str) -> Result<Code, &'static str> {
        let source = text.as_bytes();
        if source.is_empty() {
            return Err("a code is not empty");
        }
        if source.len() > Code::MAX {
            return Err("a code is at most 8 bytes");
        }
        if !source[0].is_ascii_lowercase() {
            return Err("a code starts with a lowercase ASCII letter");
        }
        let mut bytes = [0u8; Code::MAX];
        let mut index = 0;
        while index < source.len() {
            let byte = source[index];
            if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') {
                return Err("a code holds only lowercase ASCII letters, digits and `_`");
            }
            bytes[index] = byte;
            index += 1;
        }
        Ok(Code { len: source.len() as u8, bytes })
    }

    pub fn as_str(&self) -> &str {
        match std::str::from_utf8(&self.bytes[..usize::from(self.len)]) {
            Ok(text) => text,
            Err(_) => unreachable!("every byte of a code was checked to be ASCII on construction"),
        }
    }
}

impl PartialOrd for Code {
    fn partial_cmp(&self, other: &Code) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Codes order as their text.
impl Ord for Code {
    fn cmp(&self, other: &Code) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for Code {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Code {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl Serialize for Code {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Code, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Code::parse(&text).map_err(de::Error::custom)
    }
}

/// Reads one code column: `None` when null, refusing a code outside `codes`. Generated code and
/// `State` read every code column through this.
#[doc(hidden)]
pub fn decode_code(d: &mut Decoder, codes: &[Code]) -> Result<Option<Code>, DecodeError> {
    match d.take()? {
        Value::Null => Ok(None),
        Value::Text(text) => match codes.iter().find(|code| code.as_str() == text) {
            Some(code) => Ok(Some(*code)),
            None => Err(DecodeError::Code { column: String::new(), code: text }),
        },
        other => Err(DecodeError::unexpected("code", &other)),
    }
}

/// A code as the value of its column.
#[doc(hidden)]
pub fn code_value(code: Code) -> Value {
    Value::Text(code.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::Code;

    // Behaviour (G7): a code is short lowercase text starting with a letter; anything else is
    // refused with the problem named, and codes compare and order as their text.
    #[test]
    fn codes_are_short_lowercase_text() {
        for good in ["a", "dr", "ms", "x_1", "abcdefgh"] {
            assert_eq!(Code::parse(good).expect("a code").as_str(), good);
        }
        for (bad, problem) in [
            ("", "not empty"),
            ("abcdefghi", "at most 8"),
            ("1a", "starts with"),
            ("_a", "starts with"),
            ("Ab", "starts with"),
            ("aB", "only lowercase"),
            ("a-b", "only lowercase"),
            ("é", "starts with"),
        ] {
            let error = Code::parse(bad).expect_err(bad);
            assert!(error.problem.contains(problem), "{bad:?}: {error}");
        }
        assert!(Code::new("ab") < Code::new("b"));
        assert_eq!(Code::new("dr"), "dr");
        const PINNED: Code = Code::new("pe");
        assert_eq!(PINNED.to_string(), "pe");
    }

    #[test]
    fn codes_serialize_as_text() {
        let json = serde_json::to_string(&Code::new("dr")).expect("serializes");
        assert_eq!(json, "\"dr\"");
        assert_eq!(serde_json::from_str::<Code>(&json).expect("deserializes"), Code::new("dr"));
        assert!(serde_json::from_str::<Code>("\"Dr\"").is_err());
    }
}
