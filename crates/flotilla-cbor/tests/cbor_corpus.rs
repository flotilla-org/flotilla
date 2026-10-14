//! Runs the language-neutral CBOR profile corpus in tests/cbor_corpus/.
//!
//! The corpus values are modelled by this file's own [`Value`], through serde
//! alone, so the tests depend on no type of the CBOR crate underneath.
use std::{fs, path::PathBuf};

use flotilla_cbor::{Error, Framing, Limits, Message, DEFAULT_LIMITS, SETUP_LIMITS};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use self::value::Value;

fn read(name: &str) -> Json {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cbor_corpus").join(name);
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn load(name: &str) -> Vec<Json> {
    let vectors = read(name)["vectors"].as_array().unwrap().clone();
    assert!(!vectors.is_empty(), "{name} has no vectors");
    vectors
}

/// A file whose vectors come in named groups.
fn load_groups(name: &str) -> Vec<(String, Vec<Json>)> {
    read(name)["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            let vectors = group["vectors"].as_array().unwrap().clone();
            assert!(!vectors.is_empty(), "{name}: group {} has no vectors", group["name"]);
            (group["name"].as_str().unwrap().to_owned(), vectors)
        })
        .collect()
}

fn name(vector: &Json) -> &str {
    vector["name"].as_str().unwrap()
}

/// A hex string, or a list of hex strings and `{"repeat": hex, "count": n}`.
fn bytes(spec: &Json) -> Vec<u8> {
    match spec {
        Json::String(hex) => unhex(hex),
        Json::Array(parts) => parts
            .iter()
            .flat_map(|part| match part {
                Json::String(hex) => unhex(hex),
                Json::Object(repeat) => unhex(repeat["repeat"].as_str().unwrap()).repeat(repeat["count"].as_u64().unwrap() as usize),
                _ => panic!("bad byte part {part}"),
            })
            .collect(),
        _ => panic!("bad byte spec {spec}"),
    }
}

fn unhex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd hex {hex}");
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

fn limits(vector: &Json) -> Limits {
    match vector["limits"].as_str().unwrap() {
        "default" => DEFAULT_LIMITS,
        "setup" => SETUP_LIMITS,
        other => panic!("unknown limits {other}"),
    }
}

fn check(vector: &Json, reader: &str, expect: &str, result: Result<impl std::fmt::Debug, Error>) {
    match result {
        Ok(_) if expect == "accept" => {}
        Ok(value) => panic!("{}: {reader} accepted {value:?}, expected {expect}", name(vector)),
        Err(error) => assert_eq!(error.kind(), expect, "{}: {reader}: {error}", name(vector)),
    }
}

#[test]
fn canonical_vectors_round_trip() {
    for vector in load("canonical.json") {
        let value = diag::parse(vector["diag"].as_str().unwrap());
        let hex = bytes(&vector["hex"]);
        let decoded: Value = flotilla_cbor::from_slice(&hex, &DEFAULT_LIMITS).unwrap_or_else(|e| panic!("{}: {e}", name(&vector)));
        assert_eq!(decoded, value, "{}: decode", name(&vector));
        assert_eq!(flotilla_cbor::to_vec(&value, &DEFAULT_LIMITS).unwrap(), hex, "{}: encode", name(&vector));
        flotilla_cbor::check_canonical(&hex, &DEFAULT_LIMITS).unwrap_or_else(|e| panic!("{}: canonical check: {e}", name(&vector)));
    }
}

#[test]
fn encode_vectors() {
    for vector in load("encode.json") {
        let result = flotilla_cbor::to_vec(&diag::parse(vector["diag"].as_str().unwrap()), &DEFAULT_LIMITS);
        match vector.get("error") {
            Some(error) => assert_eq!(result.unwrap_err().kind(), error.as_str().unwrap(), "{}", name(&vector)),
            None => assert_eq!(result.unwrap(), bytes(&vector["hex"]), "{}", name(&vector)),
        }
    }
}

#[test]
fn decode_vectors() {
    let groups = load_groups("decode.json");
    let names: Vec<_> = groups.iter().map(|(group, _)| group.as_str()).collect();
    assert_eq!(names, ["accepted-not-deterministic", "rejected", "limits"]);
    for (group, vectors) in &groups {
        for vector in vectors {
            let input = bytes(&vector["hex"]);
            let limits = limits(vector);
            let expect = vector["expect"].as_str().unwrap();
            let canonical = vector.get("canonical").and_then(Json::as_str).unwrap_or(expect);
            match group.as_str() {
                "accepted-not-deterministic" => {
                    assert_eq!(expect, "accept", "{}", name(vector));
                    assert_ne!(canonical, "accept", "{}", name(vector));
                    let decoded: Value = flotilla_cbor::from_slice(&input, &limits).unwrap_or_else(|e| panic!("{}: {e}", name(vector)));
                    assert_eq!(decoded, diag::parse(vector["diag"].as_str().unwrap()), "{}", name(vector));
                }
                "rejected" => assert_ne!(expect, "accept", "{}", name(vector)),
                _ => {}
            }
            check(vector, "reader", expect, flotilla_cbor::check(&input, &limits));
            check(vector, "canonical check", canonical, flotilla_cbor::check_canonical(&input, &limits));
        }
    }
}

/// The canonical check agrees with the writer: an input a safe reader
/// accepts passes it exactly when re-encoding the decoded value gives the
/// same bytes.
#[test]
fn canonical_check_agrees_with_re_encoding() {
    let mut inputs: Vec<(Vec<u8>, Limits)> = load("canonical.json").iter().map(|vector| (bytes(&vector["hex"]), DEFAULT_LIMITS)).collect();
    for (_, vectors) in load_groups("decode.json") {
        inputs.extend(vectors.iter().map(|vector| (bytes(&vector["hex"]), limits(vector))));
    }
    let mut compared = 0;
    for (input, limits) in inputs {
        let Ok(value) = flotilla_cbor::from_slice::<Value>(&input, &limits) else { continue };
        let re_encoded = flotilla_cbor::to_vec(&value, &limits).unwrap() == input;
        assert_eq!(flotilla_cbor::is_canonical(&input, &limits), re_encoded, "{input:02x?}");
        compared += 1;
    }
    assert!(compared > 100, "{compared}");
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CorpusMessage {
    Hello {},
    Goodbye {},
}

impl Message for CorpusMessage {
    const TYPES: &'static [&'static str] = &["hello", "goodbye"];
}

#[test]
fn frame_vectors() {
    for vector in load("frames.json") {
        let framing = Framing::new(*b"JSTEST01", limits(&vector));
        let expect = vector["expect"].as_str().unwrap();
        check(&vector, "frame reader", expect, framing.decode::<CorpusMessage>(&bytes(&vector["hex"])));
    }
}

/// The subset of CBOR diagnostic notation (RFC 8949 §8) the corpus uses.
mod diag {
    use super::Value;

    pub fn parse(text: &str) -> Value {
        let mut parser = Parser { text, pos: 0 };
        let value = parser.value();
        parser.space();
        assert_eq!(parser.pos, text.len(), "trailing diagnostic text in {text}");
        value
    }

    struct Parser<'a> {
        text: &'a str,
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn rest(&self) -> &'a str {
            &self.text[self.pos..]
        }

        fn space(&mut self) {
            self.pos += self.rest().len() - self.rest().trim_start().len();
        }

        fn eat(&mut self, token: &str) -> bool {
            self.space();
            let found = self.rest().starts_with(token);
            if found {
                self.pos += token.len();
            }
            found
        }

        fn expect(&mut self, token: &str) {
            assert!(self.eat(token), "expected {token} at {:?}", self.rest());
        }

        fn value(&mut self) -> Value {
            self.space();
            for (word, value) in [
                ("true", Value::Bool(true)),
                ("false", Value::Bool(false)),
                ("null", Value::Null),
                ("NaN", Value::Float(f64::NAN)),
                ("Infinity", Value::Float(f64::INFINITY)),
                ("-Infinity", Value::Float(f64::NEG_INFINITY)),
            ] {
                if self.eat(word) {
                    return value;
                }
            }
            if self.eat("[") {
                let mut items = Vec::new();
                while !self.eat("]") {
                    if !items.is_empty() {
                        self.expect(",");
                    }
                    items.push(self.value());
                }
                return Value::Array(items);
            }
            if self.eat("{") {
                let mut entries = Vec::new();
                while !self.eat("}") {
                    if !entries.is_empty() {
                        self.expect(",");
                    }
                    self.space();
                    let key = self.text_string();
                    self.expect(":");
                    entries.push((key, self.value()));
                }
                return Value::Map(entries);
            }
            if self.eat("h'") {
                let end = self.rest().find('\'').unwrap();
                let hex = &self.rest()[..end];
                let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
                self.pos += end + 1;
                return Value::Bytes(bytes);
            }
            if self.rest().starts_with('"') {
                return Value::Text(self.text_string());
            }
            self.number()
        }

        /// A JSON string literal.
        fn text_string(&mut self) -> String {
            let rest = self.rest();
            let mut end = 1;
            let bytes = rest.as_bytes();
            while bytes[end] != b'"' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            self.pos += end + 1;
            serde_json::from_str(&rest[..=end]).unwrap()
        }

        /// An integer, or a float when it has a fraction or exponent.
        fn number(&mut self) -> Value {
            let len =
                self.rest().find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))).unwrap_or(self.rest().len());
            let literal = &self.rest()[..len];
            self.pos += len;
            if literal.contains(['.', 'e', 'E']) {
                Value::Float(literal.parse().unwrap_or_else(|_| panic!("bad float {literal}")))
            } else {
                Value::Integer(literal.parse().unwrap_or_else(|_| panic!("bad integer {literal}")))
            }
        }
    }
}

/// The corpus data model, independent of any CBOR crate.
mod value {
    use std::fmt;

    use serde::{
        de::{self, MapAccess, SeqAccess, Visitor},
        ser::{SerializeMap, SerializeSeq},
        Deserialize, Deserializer, Serialize, Serializer,
    };

    #[derive(Clone, Debug)]
    pub enum Value {
        Null,
        Bool(bool),
        Integer(i128),
        Float(f64),
        Bytes(Vec<u8>),
        Text(String),
        Array(Vec<Value>),
        /// Entries in the order written or read, duplicates included.
        Map(Vec<(String, Value)>),
    }

    /// Floats compare by bit pattern, so `-0.0` and `0.0` differ.
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Value::Null, Value::Null) => true,
                (Value::Bool(a), Value::Bool(b)) => a == b,
                (Value::Integer(a), Value::Integer(b)) => a == b,
                (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
                (Value::Bytes(a), Value::Bytes(b)) => a == b,
                (Value::Text(a), Value::Text(b)) => a == b,
                (Value::Array(a), Value::Array(b)) => a == b,
                (Value::Map(a), Value::Map(b)) => a == b,
                _ => false,
            }
        }
    }

    impl Serialize for Value {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Value::Null => serializer.serialize_unit(),
                Value::Bool(b) => serializer.serialize_bool(*b),
                Value::Integer(n) => match (u64::try_from(*n), i64::try_from(*n)) {
                    (Ok(n), _) => serializer.serialize_u64(n),
                    (_, Ok(n)) => serializer.serialize_i64(n),
                    _ => serializer.serialize_i128(*n),
                },
                Value::Float(f) => serializer.serialize_f64(*f),
                Value::Bytes(bytes) => serializer.serialize_bytes(bytes),
                Value::Text(text) => serializer.serialize_str(text),
                Value::Array(items) => {
                    let mut seq = serializer.serialize_seq(Some(items.len()))?;
                    for item in items {
                        seq.serialize_element(item)?;
                    }
                    seq.end()
                }
                Value::Map(entries) => {
                    let mut map = serializer.serialize_map(Some(entries.len()))?;
                    for (key, value) in entries {
                        map.serialize_entry(key, value)?;
                    }
                    map.end()
                }
            }
        }
    }

    impl<'de> Deserialize<'de> for Value {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(ValueVisitor)
        }
    }

    struct ValueVisitor;

    impl<'de> Visitor<'de> for ValueVisitor {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a value in the profile's data model")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }

        fn visit_none<E: de::Error>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
            Ok(Value::Integer(v.into()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
            Ok(Value::Integer(v.into()))
        }

        fn visit_i128<E: de::Error>(self, v: i128) -> Result<Value, E> {
            Ok(Value::Integer(v))
        }

        fn visit_u128<E: de::Error>(self, v: u128) -> Result<Value, E> {
            i128::try_from(v).map(Value::Integer).map_err(E::custom)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
            Ok(Value::Float(v))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
            Ok(Value::Text(v.to_owned()))
        }

        fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Value, E> {
            Ok(Value::Bytes(v.to_vec()))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element()? {
                items.push(item);
            }
            Ok(Value::Array(items))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry()? {
                entries.push(entry);
            }
            Ok(Value::Map(entries))
        }
    }
}
