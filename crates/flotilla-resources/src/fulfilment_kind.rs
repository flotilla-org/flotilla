use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicySpec, NoStatusPatch, PlacementPolicySpec,
    ReplicationClass,
};

// A1 keeps policy snapshots for already admitted convoys. Kinds are authored
// on their host and replicated read-only, like today's live policies.
define_resource!(
    FulfilmentKind,
    "fulfilmentkinds",
    FulfilmentKindSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FulfilmentGrant {
    Platform(String),
    GuiSession,
    Gpu,
    HostDevices,
    Network(String),
    HostAccountReach,
    ContainerRuntime,
    Toolchain(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct FulfilmentKindSpec {
    pub host_ref: String,
    pub pool: String,
    #[builder(default)]
    #[serde(default)]
    pub grants: BTreeSet<FulfilmentGrant>,
    #[serde(flatten)]
    pub realisation: FulfilmentRealisation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "realisation", rename_all = "snake_case")]
pub enum FulfilmentRealisation {
    DockerPerVessel { image: crate::DockerImageSource },
    HostDirect,
}

impl FulfilmentKindSpec {
    /// Translate live policy topology without changing admission's policy path.
    /// Operator-authored grants may subsequently refine this initial set.
    pub fn from_policy(policy: &PlacementPolicySpec, host_platform: &str) -> Result<Self, String> {
        let (host_ref, realisation, grants) = match (&policy.docker_per_vessel, &policy.host_direct) {
            (Some(DockerPerVesselPlacementPolicySpec { host_ref, image, .. }), None) => {
                let grants =
                    BTreeSet::from([FulfilmentGrant::Platform("linux".to_string()), FulfilmentGrant::Network("scoped".to_string())]);
                (host_ref.clone(), FulfilmentRealisation::DockerPerVessel { image: image.clone() }, grants)
            }
            (None, Some(HostDirectPlacementPolicySpec { host_ref, .. })) => {
                let grants = BTreeSet::from([
                    FulfilmentGrant::Platform(host_platform.to_string()),
                    FulfilmentGrant::HostAccountReach,
                    FulfilmentGrant::Network("host".to_string()),
                    FulfilmentGrant::GuiSession,
                ]);
                (host_ref.clone(), FulfilmentRealisation::HostDirect, grants)
            }
            _ => return Err("placement policy must define exactly one realisation".to_string()),
        };
        Ok(Self { host_ref, pool: policy.pool.clone(), grants, realisation })
    }
}
