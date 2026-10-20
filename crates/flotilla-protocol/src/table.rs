//! ADR 0067 table vocabulary. A sequence consists of independently encoded messages;
//! HTTP uses JSON Lines or RFC 8742 CBOR sequences. WebSocket carriers wrap a message
//! in a request/subscription envelope; routing ids belong to that envelope (O10).
//!
//! Cell profile decision (O1): cells and schemas deliberately stay JSON-shaped in this
//! version. Blobs are base64 strings in both carriers. CBOR retains signed/unsigned
//! 64-bit integers; JSON uses the shared HTTP tagged-integer profile above 2^53.
//! Native CBOR byte-string cells require a versioned profile, not a silent frame change.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Row = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Table {
        table: String,
        schema: Value,
        incarnation: String,
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
    Rows {
        rows: Vec<Row>,
        #[serde(deserialize_with = "integer")]
        sequence: u64,
        partial: bool,
        cursor: Option<String>,
    },
    Synced {
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
    Upsert {
        key: Vec<Value>,
        row: Row,
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
    Delete {
        key: Vec<Value>,
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
    Progress {
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
    Resync {
        reason: String,
    },
    Count {
        #[serde(deserialize_with = "integer")]
        count: u64,
        estimate: bool,
        #[serde(deserialize_with = "integer")]
        sequence: u64,
    },
}

fn integer<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| value.get("u64").and_then(Value::as_str).and_then(|value| value.parse().ok()))
        .ok_or_else(|| serde::de::Error::custom("expected unsigned integer or JSON u64 tag"))
}
