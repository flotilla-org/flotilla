//! Replicated demand and last successful result for an external forge read.
use chrono::{DateTime, Utc};
use flotilla_protocol::{issue_query::IssueQuery, IssueSource};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{ApiPaths, ReplicationClass, Resource, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeRead;
impl Resource for ForgeRead {
    type Spec = ForgeReadSpec;
    type Status = ForgeReadStatus;
    type StatusPatch = ForgeReadStatus;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "forgereads", kind: "ForgeRead" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ForgeReadRequest {
    Board,
    Query { params: IssueQuery, page: u32, count: usize },
    Issue { id: String },
    Changes { since: String, count: usize },
    Mission { id: String },
    DispatchFacts { id: String },
    Branch { branch: String },
    ChangeRequests { limit: usize },
    ChangeRequest { id: String },
    MergedBranches { limit: usize },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeReadSpec {
    pub source: IssueSource,
    pub request: ForgeReadRequest,
    pub demanded_at: DateTime<Utc>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeReadStatus {
    pub authority: String,
    pub attempted_at: DateTime<Utc>,
    pub observed_at: Option<DateTime<Utc>>,
    // Missing means no successful observation; null is a successful optional
    // result (for example, no PR for a branch). Option<Value>'s default decoder
    // collapses both, so omit None and preserve every present JSON value.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present_value")]
    pub value: Option<serde_json::Value>,
    pub error: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
}

fn present_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}
impl StatusPatch<ForgeReadStatus> for ForgeReadStatus {
    fn apply(&self, status: &mut ForgeReadStatus) {
        *status = self.clone();
    }
}
pub fn forge_read_name(source: &IssueSource, request: &ForgeReadRequest) -> String {
    format!("forge-read-{}", crate::content_hash(&serde_json::json!([source, request])).expect("forge read key serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stored result preserves absence versus successful null, and every
    // other JSON payload shape. Generate absence, null, scalar, array and object.
    #[hegel::test]
    fn stored_forge_result_preserves_successful_null(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let value = match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
            0 => None,
            1 => Some(serde_json::Value::Null),
            2 => Some(serde_json::json!(true)),
            3 => Some(serde_json::json!([0, 1])),
            _ => Some(serde_json::json!({"issues": []})),
        };
        let status = ForgeReadStatus {
            authority: "owner".into(),
            attempted_at: "2026-10-07T00:00:00Z".parse().expect("timestamp"),
            observed_at: value.as_ref().map(|_| "2026-10-07T00:00:00Z".parse().expect("timestamp")),
            value,
            error: None,
            retry_at: None,
        };
        let encoded = serde_json::to_value(&status).expect("serialize stored result");
        assert_eq!(encoded.get("value").is_some(), status.value.is_some());
        let decoded: ForgeReadStatus = serde_json::from_value(encoded).expect("decode stored result");
        assert_eq!(decoded, status);
    }
}
