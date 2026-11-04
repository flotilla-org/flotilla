//! PlacementPolicy as the previous generation stored it, read only by the import. Remove one roll
//! after the kind retires.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    placement::decode_optional_image_source, realisation_settings, CloudSandboxSize, DockerImagePullPolicy, FulfilmentCostClass,
    FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation, K8sObjectMeta, ObjectMeta, PlacementImageSource, Platform,
};
use serde::Deserialize;
use serde_json::Value;

pub const KIND: &str = "PlacementPolicy";
const API_VERSION: &str = "flotilla.work/v1";

/// One stored policy: its metadata and spec.
#[derive(Debug, Clone)]
pub struct RetiredPolicy {
    pub metadata: ObjectMeta,
    pub spec: PolicySpec,
}

impl RetiredPolicy {
    /// The policy a stored document holds.
    pub fn decode(document: &Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(rename = "apiVersion")]
            api_version: String,
            kind: String,
            metadata: K8sObjectMeta,
            spec: PolicySpec,
        }
        let stored = Stored::deserialize(document).map_err(|error| error.to_string())?;
        if stored.api_version != API_VERSION || stored.kind != KIND {
            return Err(format!("expected {API_VERSION} {KIND}, got {} {}", stored.api_version, stored.kind));
        }
        Ok(Self { metadata: stored.metadata.into(), spec: stored.spec })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PolicySpec {
    pub pool: String,
    #[serde(default)]
    pub provider_requirements: Option<ProviderRequirements>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub host_direct: Option<HostDirect>,
    #[serde(default)]
    pub docker_per_vessel: Option<DockerPerVessel>,
    #[serde(default)]
    pub cloud_sandbox_per_vessel: Option<CloudSandboxPerVessel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRequirements {
    pub instance: Option<String>,
    pub class: Option<flotilla_protocol::provider::EnvironmentClass>,
    pub material_modes: Vec<flotilla_protocol::provider::MaterialMode>,
    pub checkout_strategy: Option<flotilla_protocol::provider::CheckoutStrategy>,
    pub egress_enforcement: Option<flotilla_protocol::provider::EgressEnforcement>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostDirect {
    pub host_ref: String,
    pub checkout: HostDirectCheckout,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostDirectCheckout {
    Worktree,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DockerPerVessel {
    pub host_ref: String,
    /// A snapshot frozen from a retired CrewImageBaseline holds its mutable tag, so it reads as
    /// the vessel's composed image (`legacy_image_baseline_ref`).
    #[serde(default, deserialize_with = "decode_optional_image_source")]
    pub image: Option<PlacementImageSource>,
    #[serde(default)]
    pub pull_policy: DockerImagePullPolicy,
    #[serde(default)]
    pub agent_adapters: BTreeSet<String>,
    #[serde(default)]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub checkout: DockerCheckout,
    #[serde(default)]
    pub legacy_image_baseline_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerCheckout {
    WorktreeOnHostAndMount { mount_path: String },
    LocalCloneOnHostAndMount { mount_path: String },
    FreshCloneInContainer { clone_path: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudSandboxPerVessel {
    #[serde(default, deserialize_with = "decode_optional_image_source")]
    pub image: Option<PlacementImageSource>,
    pub size: CloudSandboxSize,
    pub checkout: CloudSandboxCheckout,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudSandboxCheckout {
    CloneInVessel,
}

impl PolicySpec {
    /// The kind this policy's topology describes, with the grants its realisation implies.
    pub fn kind(&self, host_platform: &str) -> Result<FulfilmentKindSpec, String> {
        let (host_ref, realisation, grants) = match (&self.docker_per_vessel, &self.host_direct, &self.cloud_sandbox_per_vessel) {
            (Some(docker), None, None) => {
                let image = if docker.legacy_image_baseline_ref.is_some() { None } else { docker.image.clone() };
                let grants = BTreeSet::from([
                    FulfilmentGrant::platform(Platform::Linux.to_string()),
                    FulfilmentGrant::network("scoped".to_string()),
                ]);
                (docker.host_ref.clone(), FulfilmentRealisation::DockerPerVessel { image }, grants)
            }
            // A GUI session is granted only once the host's probe observes one.
            (None, Some(host), None) => {
                let grants = BTreeSet::from([
                    FulfilmentGrant::platform(host_platform.to_string()),
                    FulfilmentGrant::host_account_reach(),
                    FulfilmentGrant::network("host".to_string()),
                ]);
                (host.host_ref.clone(), FulfilmentRealisation::HostDirect, grants)
            }
            (None, None, Some(cloud)) => (
                String::new(),
                FulfilmentRealisation::CloudSandboxPerVessel { image: cloud.image.clone(), size: Some(cloud.size.clone()) },
                BTreeSet::from([FulfilmentGrant::platform(Platform::Linux.to_string()), FulfilmentGrant::network("open".into())]),
            ),
            _ => return Err("placement policy must define exactly one realisation".to_string()),
        };
        Ok(FulfilmentKindSpec { host_ref, pool: self.pool.clone(), cost_class: FulfilmentCostClass::default(), grants, realisation })
    }

    /// The settings this policy holds other than the code constants. The memory share is not
    /// compared: it is dropped, not held as a constant.
    pub fn differing_settings(&self) -> Vec<&'static str> {
        let mut differing = Vec::new();
        if let Some(docker) = &self.docker_per_vessel {
            let workspace = realisation_settings::DOCKER_WORKSPACE.to_owned();
            if docker.checkout != (DockerCheckout::WorktreeOnHostAndMount { mount_path: workspace }) {
                differing.push("checkout");
            }
            // An unset cwd is not the constant: a vessel with no repositories would start in `/`.
            if docker.default_cwd.as_deref() != Some(realisation_settings::DOCKER_WORKSPACE) {
                differing.push("default_cwd");
            }
            if !docker.env.is_empty() {
                differing.push("env");
            }
            if docker.pull_policy != realisation_settings::DOCKER_PULL_POLICY {
                differing.push("pull_policy");
            }
        }
        differing
    }
}
