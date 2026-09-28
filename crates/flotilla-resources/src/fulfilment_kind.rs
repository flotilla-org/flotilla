use std::{collections::BTreeSet, fmt, str::FromStr};

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

/// A closed vocabulary of capabilities required by work. The string form is
/// also used by issue labels and `convoy start --need`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilityNeed {
    Platform(String),
    GuiSession,
    Gpu,
    HostDevices,
    Network(String),
    HostAccountReach,
    ContainerRuntime,
    Toolchain(String),
    Harness { adapter: String, minimum_version: String },
}

impl FromStr for CapabilityNeed {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parameter = |prefix: &str| value.strip_prefix(prefix).filter(|part| !part.trim().is_empty()).map(str::to_owned);
        match value {
            "gui_session" => Ok(Self::GuiSession),
            "gpu" => Ok(Self::Gpu),
            "host_devices" => Ok(Self::HostDevices),
            "host_account_reach" => Ok(Self::HostAccountReach),
            "container_runtime" => Ok(Self::ContainerRuntime),
            _ if parameter("platform:").is_some() => {
                let platform = parameter("platform:").expect("checked above");
                if !matches!(platform.as_str(), "linux" | "macos" | "windows") {
                    return Err(format!("unknown platform capability `{platform}`"));
                }
                Ok(Self::Platform(platform))
            }
            _ if parameter("network:").is_some() => Ok(Self::Network(parameter("network:").expect("checked above"))),
            _ if parameter("toolchain:").is_some() => Ok(Self::Toolchain(parameter("toolchain:").expect("checked above"))),
            _ if value.starts_with("harness:") => {
                let (adapter, version) = value["harness:".len()..]
                    .split_once(">=")
                    .ok_or_else(|| format!("invalid capability need `{value}`; expected harness:<adapter>>=<version>"))?;
                if adapter.is_empty() || version.is_empty() {
                    return Err(format!("invalid capability need `{value}`; expected harness:<adapter>>=<version>"));
                }
                Ok(Self::Harness { adapter: adapter.to_string(), minimum_version: version.to_string() })
            }
            _ => Err(format!("unknown capability need `{value}`")),
        }
    }
}

impl fmt::Display for CapabilityNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(value) => write!(f, "platform:{value}"),
            Self::GuiSession => f.write_str("gui_session"),
            Self::Gpu => f.write_str("gpu"),
            Self::HostDevices => f.write_str("host_devices"),
            Self::Network(value) => write!(f, "network:{value}"),
            Self::HostAccountReach => f.write_str("host_account_reach"),
            Self::ContainerRuntime => f.write_str("container_runtime"),
            Self::Toolchain(value) => write!(f, "toolchain:{value}"),
            Self::Harness { adapter, minimum_version } => write!(f, "harness:{adapter}>={minimum_version}"),
        }
    }
}

impl Serialize for CapabilityNeed {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CapabilityNeed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

impl CapabilityNeed {
    pub fn covered_by(&self, grants: &BTreeSet<FulfilmentGrant>, facts: Option<&crate::FulfilmentFacts>) -> bool {
        match self {
            Self::Platform(value) => grants.contains(&FulfilmentGrant::Platform(value.clone())),
            Self::GuiSession => grants.contains(&FulfilmentGrant::GuiSession) && facts.is_some_and(|facts| facts.gui_session_logged_in),
            Self::Gpu => grants.contains(&FulfilmentGrant::Gpu),
            Self::HostDevices => grants.contains(&FulfilmentGrant::HostDevices),
            Self::Network(value) => effective_grants(grants).contains(&FulfilmentGrant::Network(value.clone())),
            Self::HostAccountReach => grants.contains(&FulfilmentGrant::HostAccountReach),
            Self::ContainerRuntime => grants.contains(&FulfilmentGrant::ContainerRuntime),
            Self::Toolchain(value) => facts.is_some_and(|facts| facts.toolchains.contains_key(value)),
            Self::Harness { adapter, minimum_version } => facts
                .and_then(|facts| facts.harnesses.get(adapter))
                .is_some_and(|harness| version_at_least(&harness.version, minimum_version)),
        }
    }

    pub fn conflicts_with(&self, other: &Self) -> bool {
        matches!((self, other), (Self::Platform(left), Self::Platform(right)) if left != right)
    }
}

/// Normalise implied grants before comparing privilege sets. Host network
/// reach includes scoped network reach.
pub fn effective_grants(grants: &BTreeSet<FulfilmentGrant>) -> BTreeSet<FulfilmentGrant> {
    let mut expanded = grants.clone();
    if grants.contains(&FulfilmentGrant::Network("host".to_string())) {
        expanded.insert(FulfilmentGrant::Network("scoped".to_string()));
    }
    expanded
}

/// Compare dotted numeric versions. Unknown suffixes fail closed.
pub fn version_at_least(actual: &str, required: &str) -> bool {
    let parse = |value: &str| value.split('.').map(str::parse::<u64>).collect::<Result<Vec<_>, _>>();
    let (Ok(actual), Ok(required)) = (parse(actual), parse(required)) else { return false };
    let length = actual.len().max(required.len());
    (0..length).map(|index| actual.get(index).copied().unwrap_or(0)).collect::<Vec<_>>()
        >= (0..length).map(|index| required.get(index).copied().unwrap_or(0)).collect::<Vec<_>>()
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
                    FulfilmentGrant::Network("scoped".to_string()),
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
