//! Decoding Quint's ITF value encoding, shared by trace replay and graph
//! conformance. `specs/primitives/conformance/dump2json.py` writes TLC graph
//! nodes in the same encoding as `quint run --out-itf` traces.
#![allow(dead_code)]

use flotilla_schema::Address;
use serde_json::Value;

/// An ITF integer (`{"#bigint": "n"}`).
pub fn number(v: &Value) -> i64 {
    v["#bigint"].as_str().unwrap_or_else(|| panic!("ITF integer, found {v}")).parse().expect("model integer")
}

pub fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("ITF string, found {v}"))
}

/// A model string that the engine holds as an address (a convoy's project, a
/// promise's target): the specs keep it opaque text, so it maps to the
/// project-rooted address whose one value is that text, and back through
/// [`Address::name`].
pub fn address(v: &Value) -> Address {
    address_of(text(v))
}

/// The project-rooted address whose one value is `text`.
pub fn address_of(text: &str) -> Address {
    Address::project(text).unwrap_or_else(|error| panic!("model text {text:?} as an address: {error}"))
}

pub fn boolean(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| panic!("ITF boolean, found {v}"))
}

/// The elements of an ITF set (`{"#set": [...]}`).
pub fn set(v: &Value) -> &[Value] {
    v["#set"].as_array().map(Vec::as_slice).unwrap_or_else(|| panic!("ITF set, found {v}"))
}

/// The pairs of an ITF map (`{"#map": [[k, v]]}`). TLC prints an empty
/// function as an empty sequence, so `[]` is also an empty map.
pub fn pairs(v: &Value) -> &[Value] {
    match v {
        Value::Array(items) if items.is_empty() => &[],
        _ => v["#map"].as_array().map(Vec::as_slice).unwrap_or_else(|| panic!("ITF map, found {v}")),
    }
}

/// The value at integer key `key` of an ITF map.
pub fn at(v: &Value, key: i64) -> &Value {
    &pairs(v).iter().find(|p| number(&p[0]) == key).unwrap_or_else(|| panic!("map key {key} in {v}"))[1]
}

/// The elements of an ITF list (a JSON array).
pub fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_else(|| panic!("ITF list, found {v}"))
}

/// A sum value's constructor (`{"tag": T, "value": v}`).
pub fn tag(v: &Value) -> &str {
    v["tag"].as_str().unwrap_or_else(|| panic!("ITF sum, found {v}"))
}

pub fn payload(v: &Value) -> &Value {
    &v["value"]
}

/// `Some(v)` / `None` of an `Absent | Present(State)` slot.
pub fn slot(v: &Value) -> Option<&Value> {
    match tag(v) {
        "Absent" => None,
        "Present" => Some(payload(v)),
        other => panic!("slot {other}"),
    }
}

pub fn int(n: i64) -> Value {
    serde_json::json!({ "#bigint": n.to_string() })
}

pub fn unit() -> Value {
    serde_json::json!({ "#tup": [] })
}

/// A sum value, as ITF encodes it.
pub fn variant(tag: &str, value: Value) -> Value {
    serde_json::json!({ "tag": tag, "value": value })
}

pub fn set_of(items: impl IntoIterator<Item = Value>) -> Value {
    serde_json::json!({ "#set": items.into_iter().collect::<Vec<_>>() })
}

pub fn map_of(items: impl IntoIterator<Item = (Value, Value)>) -> Value {
    serde_json::json!({ "#map": items.into_iter().map(|(k, v)| serde_json::json!([k, v])).collect::<Vec<_>>() })
}

/// A canonical form for comparing values: set and map entries sorted, and
/// an empty sequence read as an empty map where either side is a map.
pub fn normalise(v: &Value) -> Value {
    match v {
        Value::Object(fields) if fields.contains_key("#set") => {
            let mut items: Vec<Value> = set(v).iter().map(normalise).collect();
            items.sort_by_key(|item| item.to_string());
            set_of(items)
        }
        Value::Object(fields) if fields.contains_key("#map") => {
            let mut items: Vec<(Value, Value)> = pairs(v).iter().map(|p| (normalise(&p[0]), normalise(&p[1]))).collect();
            items.sort_by_key(|(k, _)| k.to_string());
            if items.is_empty() {
                Value::Array(vec![])
            } else {
                map_of(items)
            }
        }
        Value::Object(fields) => Value::Object(fields.iter().map(|(k, v)| (k.clone(), normalise(v))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        other => other.clone(),
    }
}

/// A value in Quint's surface syntax, for readable failure messages:
/// `Moved(Open({ promises: Map(0 -> Pending()) }))` rather than raw ITF.
pub fn show(v: &Value) -> String {
    let join = |items: &mut dyn Iterator<Item = String>| items.collect::<Vec<_>>().join(", ");
    match v {
        Value::Object(fields) if fields.contains_key("#bigint") => number(v).to_string(),
        Value::Object(fields) if fields.contains_key("#tup") => format!("({})", join(&mut list(&v["#tup"]).iter().map(show))),
        Value::Object(fields) if fields.contains_key("#set") => format!("Set({})", join(&mut set(v).iter().map(show))),
        Value::Object(fields) if fields.contains_key("#map") => {
            format!("Map({})", join(&mut pairs(v).iter().map(|p| format!("{} -> {}", show(&p[0]), show(&p[1])))))
        }
        Value::Object(fields) if fields.contains_key("tag") && fields.len() == 2 && fields.contains_key("value") => match payload(v) {
            Value::Object(inner) if inner.get("#tup").is_some_and(|t| t.as_array().is_some_and(Vec::is_empty)) => tag(v).to_string(),
            inner => format!("{}({})", tag(v), show(inner)),
        },
        Value::Object(fields) => format!("{{ {} }}", join(&mut fields.iter().map(|(k, v)| format!("{k}: {}", show(v))))),
        Value::Array(items) => format!("[{}]", join(&mut items.iter().map(show))),
        other => other.to_string(),
    }
}
