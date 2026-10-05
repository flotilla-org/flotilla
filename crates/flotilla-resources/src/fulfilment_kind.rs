use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, DockerPerVesselPlacementPolicySpec, HostDirectPlacementPolicySpec, NoStatusPatch, PlacementPolicySpec,
    Platform, ReplicationClass,
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FulfilmentGrant(pub String);

impl FulfilmentGrant {
    pub fn platform(value: String) -> Self {
        Self(format!("platform:{value}"))
    }
    pub fn network(value: String) -> Self {
        Self(format!("network:{value}"))
    }
    pub fn toolchain(value: String) -> Self {
        Self(format!("toolchain:{value}"))
    }
    pub fn gui_session() -> Self {
        Self("display:gui-session".into())
    }
    pub fn gpu() -> Self {
        Self("hardware:gpu".into())
    }
    pub fn host_devices() -> Self {
        Self("hardware:host-devices".into())
    }
    pub fn host_account_reach() -> Self {
        Self("account:host".into())
    }
    pub fn container_runtime() -> Self {
        Self("runtime:container".into())
    }
}

impl Serialize for FulfilmentGrant {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FulfilmentGrant {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // ADR 0047: remove legacy tagged variants one roll after generation 1.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Record {
            Capability(String),
            Legacy { kind: String, value: Option<String> },
        }
        let capability = match Record::deserialize(deserializer)? {
            Record::Capability(value) => value,
            Record::Legacy { kind, value } => match kind.as_str() {
                "platform" | "network" | "toolchain" => {
                    format!("{kind}:{}", value.ok_or_else(|| serde::de::Error::custom("grant requires a value"))?)
                }
                "gui_session" => "display:gui-session".into(),
                "gpu" => "hardware:gpu".into(),
                "host_devices" => "hardware:host-devices".into(),
                "host_account_reach" => "account:host".into(),
                "container_runtime" => "runtime:container".into(),
                _ => return Err(serde::de::Error::custom(format!("unknown legacy grant `{kind}`"))),
            },
        };
        crate::validate_capability(&capability).map_err(serde::de::Error::custom)?;
        Ok(Self(capability))
    }
}

/// Namespaced capabilities plus typed forms for host-sensitive placement needs.
/// An open vocabulary of capabilities required by work. The string form is
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
    Capability(String),
}

impl FromStr for CapabilityNeed {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parameter = |prefix: &str| value.strip_prefix(prefix).filter(|part| !part.trim().is_empty()).map(str::to_owned);
        match value {
            "gui_session" | "display:gui-session" => Ok(Self::GuiSession),
            "gpu" | "hardware:gpu" => Ok(Self::Gpu),
            "host_devices" | "hardware:host-devices" => Ok(Self::HostDevices),
            "host_account_reach" | "account:host" => Ok(Self::HostAccountReach),
            "container_runtime" | "runtime:container" => Ok(Self::ContainerRuntime),
            _ if parameter("platform:").is_some() => {
                let platform = parameter("platform:").expect("checked above");
                if platform != Platform::MATRIX_PLACEHOLDER && platform.parse::<Platform>().is_err() {
                    return Err(format!("unknown platform capability `{platform}`"));
                }
                Ok(Self::Platform(platform))
            }
            _ if parameter("network:").is_some() => Ok(Self::Network(parameter("network:").expect("checked above"))),
            _ if parameter("toolchain:").is_some() => Ok(Self::Toolchain(parameter("toolchain:").expect("checked above"))),
            _ if value.starts_with("harness:") && value.contains(">=") => {
                let (adapter, version) = value["harness:".len()..]
                    .split_once(">=")
                    .ok_or_else(|| format!("invalid capability need `{value}`; expected harness:<adapter>>=<version>"))?;
                if adapter.is_empty() || version.is_empty() {
                    return Err(format!("invalid capability need `{value}`; expected harness:<adapter>>=<version>"));
                }
                Ok(Self::Harness { adapter: adapter.to_string(), minimum_version: version.to_string() })
            }
            _ => {
                crate::validate_capability(value)?;
                Ok(Self::Capability(value.to_string()))
            }
        }
    }
}

impl fmt::Display for CapabilityNeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capability(value) => f.write_str(value),
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
        serializer.serialize_str(&self.capability_string())
    }
}

impl<'de> Deserialize<'de> for CapabilityNeed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

impl CapabilityNeed {
    /// The role need that admission expands over the Project's platform matrix.
    pub fn matrix_placeholder() -> Self {
        Self::Platform(Platform::MATRIX_PLACEHOLDER.to_string())
    }

    pub fn capability_string(&self) -> String {
        match self {
            Self::GuiSession => "display:gui-session".into(),
            Self::Gpu => "hardware:gpu".into(),
            Self::HostDevices => "hardware:host-devices".into(),
            Self::HostAccountReach => "account:host".into(),
            Self::ContainerRuntime => "runtime:container".into(),
            _ => self.to_string(),
        }
    }

    /// Image-provided needs are distinct from host privileges and hardware.
    pub fn is_image_need(&self) -> bool {
        match self {
            Self::GuiSession | Self::Toolchain(_) | Self::Harness { .. } => true,
            Self::Capability(value) => !value.starts_with("hardware:") && !value.starts_with("gpu:"),
            _ => false,
        }
    }

    pub fn covered_by(&self, grants: &BTreeSet<FulfilmentGrant>, facts: Option<&crate::FulfilmentFacts>) -> bool {
        match self {
            Self::Capability(value) if value.starts_with("harness:") => facts.is_some_and(|facts| {
                facts
                    .harnesses
                    .iter()
                    .any(|(adapter, harness)| crate::capability_satisfies(&format!("harness:{adapter}@{}", harness.version), value))
            }),
            Self::Capability(value) => grants.iter().any(|grant| crate::capability_satisfies(&grant.0, value)),
            Self::Platform(value) => grants.contains(&FulfilmentGrant::platform(value.clone())),
            Self::GuiSession => grants.contains(&FulfilmentGrant::gui_session()) && facts.is_some_and(|facts| facts.gui_session_logged_in),
            Self::Gpu => grants.contains(&FulfilmentGrant::gpu()),
            Self::HostDevices => grants.contains(&FulfilmentGrant::host_devices()),
            Self::Network(value) => effective_grants(grants).contains(&FulfilmentGrant::network(value.clone())),
            Self::HostAccountReach => grants.contains(&FulfilmentGrant::host_account_reach()),
            Self::ContainerRuntime => grants.contains(&FulfilmentGrant::container_runtime()),
            Self::Toolchain(value) => facts.is_some_and(|facts| {
                facts.toolchains.iter().any(|(name, version)| {
                    crate::capability_satisfies(&format!("toolchain:{name}@{version}"), &format!("toolchain:{value}"))
                })
            }),
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
    if grants.contains(&FulfilmentGrant::network("host".to_string())) {
        expanded.insert(FulfilmentGrant::network("scoped".to_string()));
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
    /// Billing class used after the least-privilege cut. Old records default
    /// to owned capacity for the N to N+1 stored-data window (ADR 0047).
    /// Remove this decoder default one fleet roll after the field is written everywhere.
    #[serde(default)]
    #[builder(default)]
    pub cost_class: FulfilmentCostClass,
    #[builder(default)]
    #[serde(default)]
    pub grants: BTreeSet<FulfilmentGrant>,
    #[serde(flatten)]
    pub realisation: FulfilmentRealisation,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FulfilmentCostClass {
    #[default]
    OwnedIdle,
    SubscriptionIncluded,
    Metered,
}

impl FulfilmentCostClass {
    pub const ALL: [Self; 3] = [Self::OwnedIdle, Self::SubscriptionIncluded, Self::Metered];

    /// The serialized name, which `convoy explain` also displays. A contract
    /// test keeps it equal to serde's `snake_case` name for every variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnedIdle => "owned_idle",
            Self::SubscriptionIncluded => "subscription_included",
            Self::Metered => "metered",
        }
    }
}

impl fmt::Display for FulfilmentCostClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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
                let grants = BTreeSet::from([
                    FulfilmentGrant::platform(Platform::Linux.to_string()),
                    FulfilmentGrant::network("scoped".to_string()),
                ]);
                (host_ref.clone(), FulfilmentRealisation::DockerPerVessel { image: image.clone() }, grants)
            }
            (None, Some(HostDirectPlacementPolicySpec { host_ref, .. })) => {
                let grants = BTreeSet::from([
                    FulfilmentGrant::platform(host_platform.to_string()),
                    FulfilmentGrant::host_account_reach(),
                    FulfilmentGrant::network("host".to_string()),
                    FulfilmentGrant::gui_session(),
                ]);
                (host_ref.clone(), FulfilmentRealisation::HostDirect, grants)
            }
            _ => return Err("placement policy must define exactly one realisation".to_string()),
        };
        Ok(Self { host_ref, pool: policy.pool.clone(), cost_class: FulfilmentCostClass::default(), grants, realisation })
    }
}
