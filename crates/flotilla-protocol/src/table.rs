//! ADR 0067 table vocabulary. A sequence consists of independently encoded messages;
//! HTTP uses JSON Lines or RFC 8742 CBOR sequences, WebSocket uses one map per frame.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub type Row = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Table { table: String, schema: Value, incarnation: String, sequence: u64 },
    Rows { rows: Vec<Row>, sequence: u64, partial: bool, cursor: Option<String> },
    Synced { sequence: u64 },
    Upsert { key: Vec<Value>, row: Row, sequence: u64 },
    Delete { key: Vec<Value>, sequence: u64 },
    Progress { sequence: u64 },
    Resync { reason: String },
    Count { count: u64, estimate: bool, sequence: u64 },
}
