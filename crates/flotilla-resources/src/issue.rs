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
    /// Previous-generation status compatibility; remove serde defaults after the next fleet roll.
    #[serde(default)]
    pub title: Observation<String>,
    #[serde(default)]
    pub assignees: Observation<Vec<String>>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_generation_status_decodes_without_presentation_fields() {
        let stored = r#"{"state":{"value":"open","observed_at":"2026-08-03T20:00:00Z"},"labels":{"value":[],"observed_at":"2026-08-03T20:00:00Z"},"updated_at":{"value":"2026-08-03T20:00:00Z","observed_at":"2026-08-03T20:00:00Z"}}"#;
        let status: IssueStatus = serde_json::from_str(stored).expect("decode previous stored status");
        assert_eq!(status.title.value, None);
        assert_eq!(status.assignees.value, None);
    }
}
