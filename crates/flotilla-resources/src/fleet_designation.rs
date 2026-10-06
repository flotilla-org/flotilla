use serde::{Deserialize, Serialize};

use crate::{ApiPaths, InputMeta, NoStatusPatch, ReplicationClass, Resource, ResourceError};

/// The fleet store authors this singleton; Definitions federation distributes it.
pub const FLEET_DESIGNATION_NAME: &str = "fleet";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetDesignation;

impl Resource for FleetDesignation {
    type Spec = FleetDesignationSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "fleetdesignations", kind: "FleetDesignation" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;

    fn validate_spec(meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        if meta.name != FLEET_DESIGNATION_NAME || spec.project.trim().is_empty() {
            return Err(ResourceError::invalid("FleetDesignation must be named fleet and have a nonempty Project name"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetDesignationSpec {
    /// Project name in the designation's namespace.
    pub project: String,
}
