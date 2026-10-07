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
        if let Some(gc) = &spec.image_gc {
            if !(60..=31536000).contains(&gc.interval_seconds) || !(3600..=31536000).contains(&gc.grace_seconds) {
                return Err(ResourceError::invalid("image GC needs an interval >= 60s and grace >= 3600s"));
            }
            if gc.registry_host.as_ref().is_some_and(|value| value.trim().is_empty())
                || gc.registry_credential.as_ref().is_some_and(|value| value.trim().is_empty())
            {
                return Err(ResourceError::invalid("registry GC host and credential must be nonempty"));
            }
            if gc.registry_host.is_some() != gc.registry_credential.is_some() {
                return Err(ResourceError::invalid("registry GC needs both a host and a credential"));
            }
            if gc.registry_host.is_some() && spec.image_cache.is_none() {
                return Err(ResourceError::invalid("registry GC needs a declared image cache"));
            }
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
    /// Opt-in collection. ADR 0047: remove this decoder default after one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_gc: Option<ImageGcPolicy>,
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

/// Collection uses immutable build identities, never general Docker pruning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageGcPolicy {
    #[serde(default)]
    #[builder(default)]
    pub mode: ImageGcMode,
    pub interval_seconds: u64,
    pub grace_seconds: u64,
    #[serde(default)]
    pub registry_host: Option<String>,
    #[serde(default)]
    pub registry_credential: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageGcMode {
    #[default]
    DryRun,
    Apply,
}
