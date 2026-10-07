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
        if let Some(cache) = &spec.image_cache {
            cache.validate().map_err(ResourceError::invalid)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetDesignationSpec {
    /// Project name in the designation's namespace.
    pub project: String,
    /// Optional shared OCI cache; ADR 0047 default may retire after one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_cache: Option<ImageCacheBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCacheBinding {
    /// Registry host and repository prefix, without a tag or digest.
    pub repository: String,
    pub pull_credential: String,
    pub push_credential: String,
}
impl ImageCacheBinding {
    pub fn validate(&self) -> Result<(), String> {
        if self.repository.split('/').count() < 2
            || self.repository.contains(['@', '?', '#'])
            || self.repository.contains("://")
            || self.repository.split('/').any(|part| part.is_empty() || part == "." || part == "..")
            || self.repository.split('/').skip(1).any(|part| part.contains(':'))
            || self.repository.chars().any(char::is_whitespace)
            || self.pull_credential.is_empty()
            || self.push_credential.is_empty()
        {
            return Err("image cache needs a registry/repository and declared pull/push credential references".into());
        }
        Ok(())
    }
}
