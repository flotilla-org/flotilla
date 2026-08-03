use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRequest;

impl Resource for ChangeRequest {
    type Spec = ChangeRequestSpec;
    type Status = ChangeRequestStatus;
    type StatusPatch = ChangeRequestStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "changerequests", kind: "ChangeRequest" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("ChangeRequest subject and observing authority are immutable"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ChangeRequestSpec {
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub observing_authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation<T> {
    /// `None` is the structural Unknown value.
    pub value: Option<T>,
    pub observed_at: DateTime<Utc>,
}

impl<T> Observation<T> {
    pub fn known(value: T, observed_at: DateTime<Utc>) -> Self {
        Self { value: Some(value), observed_at }
    }

    pub fn unknown(observed_at: DateTime<Utc>) -> Self {
        Self { value: None, observed_at }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedChangeRequestState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedChecks {
    Pass,
    Fail,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedMergeability {
    Mergeable,
    Conflicting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestReviewObservation {
    pub actionable_at_head: Observation<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestStatus {
    pub state: Observation<ObservedChangeRequestState>,
    pub head_sha: Observation<String>,
    pub checks: Observation<ObservedChecks>,
    pub review: ChangeRequestReviewObservation,
    pub mergeable: Observation<ObservedMergeability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequestStatusPatch {
    Observed(ChangeRequestStatus),
}

impl StatusPatch<ChangeRequestStatus> for ChangeRequestStatusPatch {
    fn apply(&self, status: &mut ChangeRequestStatus) {
        match self {
            Self::Observed(observation) => *status = observation.clone(),
        }
    }
}

pub fn change_request_record_name(service: &str, scope: &str, number: u64) -> String {
    fn hex(value: &str) -> String {
        value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }
    format!("cr-{}-{}-{number}", hex(service), hex(scope))
}
