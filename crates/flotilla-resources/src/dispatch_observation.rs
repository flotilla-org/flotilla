use chrono::{DateTime, Utc};
use flotilla_protocol::IssueRef;
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, NoStatusPatch, ReplicationClass, Resource, ResourceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchObservation;

impl Resource for DispatchObservation {
    type Spec = DispatchObservationSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths =
        ApiPaths { group: "flotilla.work", version: "v1", plural: "dispatchobservations", kind: "DispatchObservation" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("DispatchObservation records are immutable"))
        }
    }
}

pub const DISPATCH_RECONCILER_PROVENANCE: &str = "dispatch-reconciler";

/// One immutable record of a real dispatch decision observed after an issue
/// appeared in the dispatchable queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DispatchObservationSpec {
    pub project_ref: String,
    pub convoy_ref: String,
    pub issue: IssueRef,
    pub workflow_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
    pub ready_observed_at: DateTime<Utc>,
    pub dispatched_at: DateTime<Utc>,
    pub time_from_ready_seconds: u64,
    pub observed_at: DateTime<Utc>,
    pub provenance: String,
}

/// Immutable conflict evidence; predicted and actual measurements are retained
/// independently for tuning rather than overwritten when a branch is pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchOverlap;
impl Resource for DispatchOverlap {
    type Spec = DispatchOverlapSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "dispatchoverlaps", kind: "DispatchOverlap" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;
    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("overlap observations are immutable"))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchOverlapSpec {
    #[serde(default)]
    pub outcome: bool,
    pub project_ref: String,
    pub source: flotilla_protocol::IssueSource,
    pub issue: IssueRef,
    pub target: flotilla_protocol::FootprintTarget,
    pub candidate_actual: bool,
    pub target_actual: bool,
    pub revision: String,
    pub weight: u64,
    pub files: Vec<String>,
    pub conflicts: Option<bool>,
    pub observed_at: DateTime<Utc>,
}
