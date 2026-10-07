//! Replicated demand and last successful result for an external forge read.
use chrono::{DateTime, Utc};
use flotilla_protocol::{issue_query::IssueQuery, IssueSource};
use serde::{Deserialize, Serialize};

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
    pub value: Option<serde_json::Value>,
    pub error: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
}
impl StatusPatch<ForgeReadStatus> for ForgeReadStatus {
    fn apply(&self, status: &mut ForgeReadStatus) {
        *status = self.clone();
    }
}
pub fn forge_read_name(source: &IssueSource, request: &ForgeReadRequest) -> String {
    format!("forge-read-{}", crate::content_hash(&serde_json::json!([source, request])).expect("forge read key serializes"))
}
