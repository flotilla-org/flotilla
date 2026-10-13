use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    field_ownership::serialized_spec_field_value, resource::define_resource, status_patch::NoStatusPatch, FieldOwnedResource,
    FieldOwnership, OwnershipEnforcement, ReplicationClass, ResourceError, WriterRole, HOST_DIRECT_NAME_PREFIX,
};

/// Reserved namespace shared by Docker policy seeding and field ownership.
pub const DOCKER_POLICY_NAME_PREFIX: &str = "docker-on-";

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
/// Docker checkout and strategy presence are loop-owned only in the reserved
/// `docker-on-*` and `host-direct-*` seed-policy namespaces; other names are
/// operator-authored.
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
        FieldOwnership::new("spec.docker_per_vessel.memory_policy", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.agent_adapters", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.default_cwd", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.env", WriterRole::Operator),
        FieldOwnership::new("spec.docker_per_vessel.checkout", WriterRole::ReconcileLoop),
    ];
    const OWNERSHIP_ENFORCEMENT: OwnershipEnforcement = OwnershipEnforcement::Observe;

    fn field_owner(ownership: &FieldOwnership, name: &str) -> WriterRole {
        // These are the only FIELD_OWNERSHIP entries with seed-scoped owners.
        // New Docker fields declare their default owner in that table; add them
        // here only if their owner also differs for operator-authored policies.
        if matches!(ownership.field, "spec.docker_per_vessel" | "spec.docker_per_vessel.checkout")
            && !name.starts_with(DOCKER_POLICY_NAME_PREFIX)
            && !name.starts_with(HOST_DIRECT_NAME_PREFIX)
        {
            WriterRole::Operator
        } else {
            ownership.owner
        }
    }

    fn ownership_rule(field: &str, owner: WriterRole) -> String {
        let route = match owner {
            WriterRole::ReconcileLoop =>
                "author this PlacementPolicy through its managed manifest and ManifestRoot reconciliation, or create a separate crew policy outside the reserved docker-on-* and host-direct-* seed names",
            WriterRole::Operator => "use flotilla resource apply for a local policy, or edit its source manifest for a managed policy",
            WriterRole::Actuator => "use the owning actuator",
        };
        format!("{field} is owned by {owner:?}; {route}")
    }

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
    /// Omitted means the vessel's composed image (ADR 0062): admission writes a
    /// `Built` source into each prepared placement snapshot.
    #[serde(default, deserialize_with = "decode_optional_image_source", skip_serializing_if = "Option::is_none")]
    pub image: Option<DockerImageSource>,
    /// Admission freezes baseline provenance alongside the resolved image.
    /// Operator-authored policies normally omit this field. Explicit values are
    /// accepted as asserted provenance, like the legacy Environment label; use
    /// them only when preserving an image previously resolved from that baseline.
    /// One-generation bridge until #2731; retire after its last writing roll
    /// and the following compatibility generation (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_image_baseline_ref: Option<String>,
    /// Controls registry access for the resolved image tag. Image build recipes
    /// are intentionally outside placement policy and are tracked separately.
    #[serde(default)]
    pub pull_policy: DockerImagePullPolicy,
    /// ADR 0047: default previous-generation policies for one fleet roll.
    #[serde(default)]
    pub memory_policy: crate::environment::EnvironmentMemoryPolicy,
    /// Agent adapters the image recipe promises will be available after provisioning.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agent_adapters: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub checkout: DockerCheckoutStrategy,
}

impl DockerPerVesselPlacementPolicySpec {
    /// Frozen snapshots retain provenance after their image becomes a literal
    /// or composition. Old live policies still carry the baseline source itself.
    pub fn legacy_image_baseline_ref(&self) -> Option<&str> {
        self.legacy_image_baseline_ref.as_deref().or(match &self.image {
            Some(DockerImageSource::Baseline { image_baseline_ref }) => Some(image_baseline_ref.as_str()),
            _ => None,
        })
    }
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
    LocalCloneOnHostAndMount { mount_path: String },
    FreshCloneInContainer { clone_path: String },
}

/// A placement-specific literal, a retired baseline reference, or the
/// per-identity composed image admission froze into a prepared snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DockerImageSource {
    Literal(String),
    // Compatibility reader only: no fleet manifests author CrewImageBaseline.
    // Remove with CrewImageBaseline after the cut-over roll.
    Baseline {
        image_baseline_ref: String,
    },
    /// The vessel's composed image: its identity and the per-identity ImageBuild
    /// (`image-<hex>-<arch>`) that realises it on the placement architecture.
    Built {
        identity: String,
        image_build_ref: String,
    },
}

/// Decode an optional image source, dropping the ADR 0053
/// `{composition: {selection…}}` form as omitted. ADR 0047: remove the
/// composition shim one roll after ADR 0062 images land.
pub(crate) fn decode_optional_image_source<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DockerImageSource>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(object)) if object.contains_key("composition") => Ok(None),
        Some(value) => serde_json::from_value(value).map(Some).map_err(serde::de::Error::custom),
    }
}

impl From<String> for DockerImageSource {
    fn from(image: String) -> Self {
        Self::Literal(image)
    }
}

impl From<&str> for DockerImageSource {
    fn from(image: &str) -> Self {
        Self::Literal(image.to_string())
    }
}

#[cfg(test)]
mod image_source_tests {
    use super::*;

    fn policy(image: Option<serde_json::Value>) -> serde_json::Value {
        let mut docker = serde_json::json!({"host_ref": "feta", "checkout": {"fresh_clone_in_container": {"clone_path": "/w"}}});
        if let Some(image) = image {
            docker["image"] = image;
        }
        serde_json::json!({"pool": "lab", "docker_per_vessel": docker})
    }

    // ADR 0062: policies omit the image (the vessel's composed image); the
    // ADR 0053 composition form decodes as omitted (ADR 0047); literals,
    // baselines and admission's Built source round-trip.
    #[test]
    fn composition_is_dropped_and_built_round_trips() {
        let composition = serde_json::json!({"composition": {"selection": {"base": "base", "utilities": null, "harness": null, "project": null}, "layers": []}});
        for (image, expected) in [
            (None, None),
            (Some(serde_json::Value::Null), None),
            (Some(composition), None),
            (
                Some(serde_json::json!("registry.test/crew@sha256:abc")),
                Some(DockerImageSource::Literal("registry.test/crew@sha256:abc".into())),
            ),
            (
                Some(serde_json::json!({"image_baseline_ref": "crew"})),
                Some(DockerImageSource::Baseline { image_baseline_ref: "crew".into() }),
            ),
            (
                Some(serde_json::json!({"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"})),
                Some(DockerImageSource::Built { identity: "sha256:abc".into(), image_build_ref: "image-abc-amd64".into() }),
            ),
        ] {
            let spec: PlacementPolicySpec = serde_json::from_value(policy(image)).expect("policy decodes");
            let docker = spec.docker_per_vessel.as_ref().expect("docker");
            assert_eq!(docker.image, expected);
            let encoded = serde_json::to_value(&spec).expect("encode");
            assert!(encoded["docker_per_vessel"].get("image").is_none() == expected.is_none());
            assert_eq!(serde_json::from_value::<PlacementPolicySpec>(encoded).expect("round-trip"), spec);
        }
        let kind: crate::FulfilmentKindSpec = serde_json::from_value(serde_json::json!({
            "host_ref": "feta", "pool": "lab", "grants": [],
            "realisation": "docker_per_vessel", "image": {"composition": {"selection": {}}}
        }))
        .expect("old fulfilment kind decodes");
        assert_eq!(kind.realisation, crate::FulfilmentRealisation::DockerPerVessel { image: None });
    }
}
