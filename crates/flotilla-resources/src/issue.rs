use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, Observation, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issue;

impl Resource for Issue {
    type Spec = IssueSpec;
    type Status = IssueStatus;
    type StatusPatch = IssueStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "issues", kind: "Issue" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current.service == requested.service && current.scope == requested.scope && current.number == requested.number {
            Ok(())
        } else {
            Err(ResourceError::invalid("Issue subject is immutable"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct IssueSpec {
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub observing_authority: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedIssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueStatus {
    pub state: Observation<ObservedIssueState>,
    pub labels: Observation<Vec<String>>,
    pub updated_at: Observation<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueStatusPatch {
    Observed(IssueStatus),
}

impl StatusPatch<IssueStatus> for IssueStatusPatch {
    fn apply(&self, status: &mut IssueStatus) {
        match self {
            Self::Observed(observation) => *status = observation.clone(),
        }
    }
}

pub fn issue_record_name(service: &str, scope: &str, number: u64) -> String {
    fn hex(value: &str) -> String {
        value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }
    let (service, scope) = flotilla_relay_protocol::Subject::normalize_scope(service, scope);
    format!("issue-{}-{}-{number}", hex(&service), hex(&scope))
}
