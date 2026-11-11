//! A stored CBOR document read as JSON. JSON has no byte string, so a byte string inside the
//! document reads as standard base64 text, as a whole blob column does.
use std::fmt;

use base64::Engine;
use serde::{
    de::{MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::{Map, Number, Value};

/// `bytes`, a CBOR document, as JSON.
pub(crate) fn cbor_to_json(bytes: &[u8], limits: &flotilla_cbor::Limits) -> Result<Value, flotilla_cbor::Error> {
    flotilla_cbor::from_slice::<Json>(bytes, limits).map(|json| json.0)
}

struct Json(Value);

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a CBOR data item")
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Json, E> {
        Ok(Json(Value::Bool(value)))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Json, E> {
        Ok(Json(value.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Json, E> {
        Ok(Json(value.into()))
    }
    fn visit_i128<E: serde::de::Error>(self, value: i128) -> Result<Json, E> {
        // The profile's integers reach -2^64, below i64; JSON text carries them exactly.
        Ok(Json(Value::String(value.to_string())))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Json, E> {
        Ok(Json(Number::from_f64(value).map_or(Value::Null, Value::Number)))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Json, E> {
        Ok(Json(Value::String(value.to_owned())))
    }
    fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Json, E> {
        Ok(Json(Value::String(base64::engine::general_purpose::STANDARD.encode(value))))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Json, E> {
        Ok(Json(Value::Null))
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Json, E> {
        Ok(Json(Value::Null))
    }
    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        Json::deserialize(deserializer)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(Json(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json(Value::Array(items)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        // The profile's readers refuse any map key but text.
        let mut entries = Map::new();
        while let Some((key, Json(value))) = map.next_entry::<String, Json>()? {
            entries.insert(key, value);
        }
        Ok(Json(Value::Object(entries)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::cbor_to_json;

    #[derive(serde::Serialize)]
    struct Document {
        name: &'static str,
        admission: flotilla_cbor::Bytes,
        items: Vec<Option<i64>>,
    }

    #[test]
    fn byte_strings_read_as_base64_and_the_rest_as_json() {
        let bytes = flotilla_cbor::to_vec(
            &Document { name: "convoy", admission: flotilla_cbor::Bytes(vec![0, 1, 2, 255]), items: vec![Some(-3), None] },
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            cbor_to_json(&bytes, &Default::default()).unwrap(),
            json!({"name": "convoy", "admission": "AAEC/w==", "items": [-3, null]})
        );
    }
}
