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
        FieldOwnership::new("spec.provider_requirements", WriterRole::Operator),
        FieldOwnership::new("spec.host_direct", WriterRole::ReconcileLoop),
        FieldOwnership::new("spec.cloud_sandbox_per_vessel", WriterRole::Operator),
        FieldOwnership::new("spec.cloud_sandbox_per_vessel.image", WriterRole::Operator),
        FieldOwnership::new("spec.cloud_sandbox_per_vessel.size", WriterRole::Operator),
        FieldOwnership::new("spec.cloud_sandbox_per_vessel.checkout", WriterRole::Operator),
        FieldOwnership::new("spec.cloud_sandbox_per_vessel.env", WriterRole::Operator),
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
    /// Optional data-only provider constraints. Old policies derive their
    /// requirements from their checkout and execution inputs (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_requirements: Option<ProviderRequirements>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectPlacementPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_per_vessel: Option<DockerPerVesselPlacementPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_sandbox_per_vessel: Option<CloudSandboxPerVesselPlacementPolicySpec>,
}

/// Checkout stays inside this input until the generated-table move (#3108).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSandboxPerVesselPlacementPolicySpec {
    pub image: String,
    pub size: crate::CloudSandboxSize,
    pub checkout: CloudSandboxCheckout,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudSandboxCheckout {
    CloneInVessel,
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

/// Decoded through [`StoredDockerPerVessel`]: a new field is added to both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredDockerPerVessel")]
pub struct DockerPerVesselPlacementPolicySpec {
    pub host_ref: String,
    /// Omitted means the vessel's composed image (ADR 0062): admission writes a
    /// `Built` source into each prepared placement snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<DockerImageSource>,
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

/// A snapshot frozen from a retired CrewImageBaseline holds its mutable tag, which Docker refuses,
/// so it decodes as the vessel's composed image.
#[derive(Deserialize)]
struct StoredDockerPerVessel {
    host_ref: String,
    #[serde(default, deserialize_with = "decode_optional_image_source")]
    image: Option<DockerImageSource>,
    #[serde(default)]
    pull_policy: DockerImagePullPolicy,
    #[serde(default)]
    memory_policy: crate::environment::EnvironmentMemoryPolicy,
    #[serde(default)]
    agent_adapters: BTreeSet<String>,
    #[serde(default)]
    default_cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    checkout: DockerCheckoutStrategy,
    #[serde(default)]
    legacy_image_baseline_ref: Option<String>,
}

impl From<StoredDockerPerVessel> for DockerPerVesselPlacementPolicySpec {
    fn from(stored: StoredDockerPerVessel) -> Self {
        let image = match stored.legacy_image_baseline_ref {
            Some(baseline) => {
                tracing::warn!(
                    %baseline,
                    host_ref = %stored.host_ref,
                    "a placement froze its image from a retired CrewImageBaseline; it runs the vessel's composed image"
                );
                None
            }
            None => stored.image,
        };
        Self {
            host_ref: stored.host_ref,
            image,
            pull_policy: stored.pull_policy,
            memory_policy: stored.memory_policy,
            agent_adapters: stored.agent_adapters,
            default_cwd: stored.default_cwd,
            env: stored.env,
            checkout: stored.checkout,
        }
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

/// A placement-specific literal, or the per-identity composed image admission froze into a
/// prepared snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DockerImageSource {
    Literal(String),
    /// The vessel's composed image: its identity and the per-identity ImageBuild
    /// (`image-<hex>-<arch>`) that realises it on the placement architecture.
    Built {
        identity: String,
        image_build_ref: String,
    },
}

/// Decode an optional image source, dropping the ADR 0053
/// `{composition: {selection…}}` form as omitted. ADR 0047: remove the
/// composition shim one roll after ADR 0062 images land. A retired
/// CrewImageBaseline reference decodes as omitted too.
pub(crate) fn decode_optional_image_source<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DockerImageSource>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(object)) if object.contains_key("composition") => {
            Ok(retired_composition_image(&object["composition"]))
        }
        Some(serde_json::Value::Object(object)) if object.contains_key("image_baseline_ref") => {
            tracing::warn!(
                baseline = %object["image_baseline_ref"],
                "an image names a retired CrewImageBaseline; it runs the vessel's composed image"
            );
            Ok(None)
        }
        Some(value) => serde_json::from_value(value).map(Some).map_err(serde::de::Error::custom),
    }
}

/// The image an ADR 0053 composition already carried, with the previous
/// resolution precedence: a bound identity (registry digest, else local ID),
/// then a literal base, then a generation-1 baseline image. A selection-only
/// composition carried none and decodes as omitted. ADR 0047: remove with the
/// composition shim one roll after ADR 0062 images land.
fn retired_composition_image(composition: &serde_json::Value) -> Option<DockerImageSource> {
    fn text(value: Option<&serde_json::Value>) -> Option<&str> {
        value.and_then(serde_json::Value::as_str).filter(|text| !text.trim().is_empty())
    }
    let identity = composition.get("identity");
    text(identity.and_then(|identity| identity.get("registry_digest")))
        .or_else(|| text(identity.and_then(|identity| identity.get("local_image_id"))))
        .or_else(|| text(composition.get("literal_base")))
        .or_else(|| text(composition.get("baseline_image")))
        .map(|image| DockerImageSource::Literal(image.to_string()))
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
            (Some(serde_json::json!({"image_baseline_ref": "crew"})), None),
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
        // ADR 0047: previous-generation compositions that carried an image
        // keep it (old resolve precedence: bound registry digest, bound local
        // ID, literal base, baseline image); selection-only ones are omitted.
        let bound = serde_json::json!({"composition": {"selection": {"base": "base"}, "layers": [], "literal_base": "crew:literal",
            "baseline_image": "crew:baseline", "identity": {"local_image_id": "sha256:local", "registry_digest": "registry.test/crew@sha256:d"}}});
        for (composition, expected) in [
            (bound.clone(), "registry.test/crew@sha256:d"),
            (
                serde_json::json!({"composition": {"selection": {}, "layers": [{"name": "base"}], "identity": {"local_image_id": "sha256:local"}}}),
                "sha256:local",
            ),
            (
                serde_json::json!({"composition": {"selection": {}, "literal_base": "crew:literal", "baseline_image": "crew:baseline"}}),
                "crew:literal",
            ),
            (serde_json::json!({"composition": {"selection": {}, "baseline_image": "crew:baseline"}}), "crew:baseline"),
        ] {
            let spec: PlacementPolicySpec = serde_json::from_value(policy(Some(composition))).expect("old composition");
            assert_eq!(spec.docker_per_vessel.expect("docker").image, Some(DockerImageSource::Literal(expected.into())));
        }
        let kind: crate::FulfilmentKindSpec = serde_json::from_value(serde_json::json!({
            "host_ref": "feta", "pool": "lab", "grants": [],
            "realisation": "docker_per_vessel", "image": {"composition": {"selection": {}}}
        }))
        .expect("old fulfilment kind decodes");
        assert_eq!(kind.realisation, crate::FulfilmentRealisation::DockerPerVessel { image: None });
    }

    #[test]
    fn a_snapshot_frozen_from_a_baseline_runs_the_composed_image() {
        let mut frozen = policy(Some(serde_json::json!("registry.test/crew:baseline")));
        frozen["docker_per_vessel"]["legacy_image_baseline_ref"] = serde_json::json!("fleet-crew");
        let spec: PlacementPolicySpec = serde_json::from_value(frozen).expect("snapshot decodes");
        assert_eq!(spec.docker_per_vessel.as_ref().expect("docker").image, None);
        let encoded = serde_json::to_value(&spec).expect("encode");
        assert!(encoded["docker_per_vessel"].get("image").is_none());
        assert!(encoded["docker_per_vessel"].get("legacy_image_baseline_ref").is_none());
    }
}

/// All constraints are conjunctive. Enforcement is a minimum: advisory admits
/// enforcing providers too. Selection fails closed when several instances match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRequirements {
    pub instance: Option<String>,
    pub class: Option<flotilla_protocol::provider::EnvironmentClass>,
    pub material_modes: Vec<flotilla_protocol::provider::MaterialMode>,
    pub checkout_strategy: Option<flotilla_protocol::provider::CheckoutStrategy>,
    pub egress_enforcement: Option<flotilla_protocol::provider::EgressEnforcement>,
}
