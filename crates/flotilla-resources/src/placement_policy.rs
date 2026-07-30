use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    field_ownership::serialized_spec_field_value, resource::define_resource, status_patch::NoStatusPatch, FieldOwnedResource,
    FieldOwnership, OwnershipEnforcement, ReplicationClass, ResourceError, WriterRole,
};

define_resource!(
    PlacementPolicy,
    "placementpolicies",
    PlacementPolicySpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime
);

/// Complete PlacementPolicy ownership declaration.
///
/// Strategy topology discovered by registration loops is loop-derived.
/// Scheduling priority and runtime/container configuration are operator-authored.
/// PlacementPolicy has no status fields.
impl FieldOwnedResource for PlacementPolicy {
    const FIELD_OWNERSHIP: &'static [FieldOwnership] = &[
        FieldOwnership::new("spec.pool", WriterRole::ReconcileLoop),
        FieldOwnership::new("spec.priority", WriterRole::Operator),
        FieldOwnership::new("spec.host_direct", WriterRole::ReconcileLoop),
        FieldOwnership::new("spec.docker_per_vessel", WriterRole::ReconcileLoop),
        FieldOwnership::new("spec.docker_per_vessel.host_ref", WriterRole::ReconcileLoop),
        FieldOwnership::new("spec.docker_per_vessel.image", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.pull_policy", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.agent_adapters", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.default_cwd", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.env", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.checkout", WriterRole::ReconcileLoop),
    ];
    const OWNERSHIP_ENFORCEMENT: OwnershipEnforcement = OwnershipEnforcement::Observe;

    fn spec_field_value(spec: &Self::Spec, field: &str) -> Result<Option<serde_json::Value>, ResourceError> {
        match field {
            "spec.priority" => Ok(Some(serde_json::json!(spec.priority))),
            "spec.docker_per_vessel" => Ok(Some(serde_json::json!(spec.docker_per_vessel.is_some()))),
            _ => serialized_spec_field_value::<Self>(spec, field),
        }
    }

    fn spec_field_restore_value(spec: &Self::Spec, field: &str) -> Result<serde_json::Value, ResourceError> {
        match field {
            "spec.docker_per_vessel" => Ok(serde_json::to_value(&spec.docker_per_vessel)
                .map_err(|error| ResourceError::decode(format!("serialize docker_per_vessel: {error}")))?),
            _ => Ok(serialized_spec_field_value::<Self>(spec, field)?.unwrap_or(serde_json::Value::Null)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementPolicySpec {
    pub pool: String,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectPlacementPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_per_vessel: Option<DockerPerVesselPlacementPolicySpec>,
}

fn is_zero(value: &i32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectPlacementPolicySpec {
    pub host_ref: String,
    pub checkout: HostDirectPlacementPolicyCheckout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostDirectPlacementPolicyCheckout {
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerPerVesselPlacementPolicySpec {
    pub host_ref: String,
    pub image: String,
    /// Controls registry access for the literal image tag. Image build recipes
    /// are intentionally outside placement policy and are tracked separately.
    #[serde(default)]
    pub pull_policy: DockerImagePullPolicy,
    /// Agent adapters the image recipe promises will be available after provisioning.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agent_adapters: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub checkout: DockerCheckoutStrategy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerImagePullPolicy {
    #[default]
    Always,
    IfNotPresent,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerCheckoutStrategy {
    WorktreeOnHostAndMount { mount_path: String },
    FreshCloneInContainer { clone_path: String },
}
