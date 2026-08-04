use chrono::{DateTime, Utc};
use flotilla_protocol::IssueRef;
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, NoStatusPatch, ReplicationClass, Resource, ResourceError, Stance};

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
    pub stance: Stance,
    pub ready_observed_at: DateTime<Utc>,
    pub dispatched_at: DateTime<Utc>,
    pub time_from_ready_seconds: u64,
    pub observed_at: DateTime<Utc>,
    pub provenance: String,
}
