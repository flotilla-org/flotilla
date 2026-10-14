//! Runs the language-neutral corpus in tests/cbor_corpus/ (see its README).
use std::{collections::BTreeMap, fmt, fs, path::PathBuf};

use flotilla_cbor::{from_slice, to_vec, Bytes, Error, Framing, Limits, Message, DEFAULT_LIMITS};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value as Json;

/// Every vector in a corpus file, across its groups.
fn vectors(file: &str) -> Vec<Json> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cbor_corpus").join(file);
    let corpus: Json = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    corpus["groups"].as_array().unwrap().iter().flat_map(|group| group["vectors"].as_array().unwrap().clone()).collect()
}

fn name(vector: &Json) -> &str {
    vector.get("name").unwrap_or(&vector["hex"]).as_str().unwrap_or("?")
}

/// A hex string, or a list of hex strings and `{"repeat": hex, "count": n}`.
fn bytes(spec: &Json) -> Vec<u8> {
    let unhex = |hex: &str| (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
    match spec {
        Json::String(hex) => unhex(hex),
        Json::Array(parts) => parts
            .iter()
            .flat_map(|part| match part {
                Json::String(hex) => unhex(hex),
                repeat => unhex(repeat["repeat"].as_str().unwrap()).repeat(repeat["count"].as_u64().unwrap() as usize),
            })
            .collect(),
        _ => panic!("bad byte spec {spec}"),
    }
}

fn limits(vector: &Json) -> Limits {
    match vector["limits"].as_str().unwrap() {
        "default" => DEFAULT_LIMITS,
        "small" => Limits { max_message: 64 * 1024, max_byte_string: 4 * 1024, ..DEFAULT_LIMITS },
        other => panic!("unknown limits {other}"),
    }
}

/// The result matches `expect`; an accepted input also matches `json`.
fn check<T: fmt::Debug>(vector: &Json, input: &[u8], result: Result<T, Error>) {
    let expect = vector["expect"].as_str().unwrap();
    match result {
        Ok(_) if expect == "accept" => typed(vector, input, &limits(vector)),
        Ok(value) => panic!("{}: accepted {value:?}, expected {expect}", name(vector)),
        Err(error) => assert_eq!(error.kind().name(), expect, "{}: {error}", name(vector)),
    }
}

/// Floats and integers compare by type and value (`1.0` is not `1`, `-0.0` is
/// not `0.0`). serde_json's default float parser is not exact, so the decoded
/// value is printed and parsed by it too.
fn typed(vector: &Json, input: &[u8], limits: &Limits) {
    if let Some(json) = vector.get("json") {
        let decoded: Json = serde_json::from_str(&from_slice::<Json>(input, limits).unwrap().to_string()).unwrap();
        assert_eq!(decoded.to_string(), json.to_string(), "{}", name(vector));
    }
}

#[test]
fn canonical_vectors_round_trip() {
    for vector in vectors("canonical.json") {
        let input = bytes(&vector["hex"]);
        let value: Value = from_slice(&input, &DEFAULT_LIMITS).unwrap_or_else(|e| panic!("{}: {e}", name(&vector)));
        assert_eq!(to_vec(&value, &DEFAULT_LIMITS).unwrap(), input, "{}", name(&vector));
        typed(&vector, &input, &DEFAULT_LIMITS);
    }
}

#[test]
fn encode_vectors() {
    for vector in vectors("encode.json") {
        assert_eq!(to_vec(&vector["json"], &DEFAULT_LIMITS).unwrap(), bytes(&vector["hex"]), "{}", name(&vector));
    }
}

#[test]
fn decode_vectors() {
    for vector in vectors("decode.json") {
        let input = bytes(&vector["hex"]);
        check(&vector, &input, from_slice::<Value>(&input, &limits(&vector)));
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Hello {
    Hello {},
    Goodbye {},
}

impl Message for Hello {
    const TYPES: &'static [&'static str] = &["hello", "goodbye"];
}

#[test]
fn frame_vectors() {
    for vector in vectors("frames.json") {
        let frame = bytes(&vector["hex"]);
        check(&vector, &[], Framing::new(*b"JSTEST01", limits(&vector)).decode::<Hello>(&frame));
    }
}

/// Any value in the profile's data model, through serde alone. Duplicate
/// keys never reach it: the reader rejects them first.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Value {
    Null,
    Bool(bool),
    Integer(i128),
    Float(f64),
    Bytes(Bytes),
    Text(String),
    Array(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        macro_rules! visit {
            ($($visit:ident($ty:ty) => $make:expr;)*) => {$(
                fn $visit<E>(self, v: $ty) -> Result<Value, E> { Ok($make(v)) }
            )*};
        }
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a CBOR value")
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            visit! {
                visit_bool(bool) => Value::Bool;
                visit_i64(i64) => |v: i64| Value::Integer(v.into());
                visit_u64(u64) => |v: u64| Value::Integer(v.into());
                visit_i128(i128) => Value::Integer;
                visit_f64(f64) => Value::Float;
                visit_str(&str) => |v: &str| Value::Text(v.into());
                visit_bytes(&[u8]) => |v: &[u8]| Value::Bytes(Bytes(v.into()));
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                std::iter::from_fn(|| seq.next_element().transpose()).collect::<Result<_, _>>().map(Value::Array)
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                std::iter::from_fn(|| map.next_entry().transpose()).collect::<Result<_, _>>().map(Value::Map)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
