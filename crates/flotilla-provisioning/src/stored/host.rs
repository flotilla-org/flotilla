//! Host and HostObservation on the engine (#3824 layer 4c). Host is the configured spec;
//! HostObservation is what the host's daemon, or an SSH host's owning daemon, last observed
//! there, written only when it changes. Both are homed at that daemon. Readiness, the age of an
//! observation and free vessel slots are judgements, so neither record stores them.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::provider::{CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderReadiness};
use flotilla_resources::{
    ConditionValue, FulfilmentFacts, HostCondition, HostConnection, HostSpec, ImageBuildCapacity, ImageBuildReservation,
};
use flotilla_schema::{KindRegistry, RegistryError, Stored, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Connection {
    #[store(code = "dm")]
    Daemon,
    #[store(code = "ss")]
    /// The destination is not carried: its owning daemon reads it from its own profile.
    SshHost { owning_daemon: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Reservation {
    pub cpu: u32,
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum ImageBuilds {
    #[store(code = "no")]
    None,
    #[store(code = "bu")]
    Builder {
        architecture: String,
        slots: u32,
        #[store(flatten)]
        reservation: Reservation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Host", code = "ho")]
pub struct HostRecord {
    pub display_name: String,
    pub connection: Connection,
    /// None builds on the placement host, as before ImageBuild.
    pub image_builds: Option<ImageBuilds>,
    pub max_vessels: Option<u32>,
    pub single_tenant: bool,
}

impl HostRecord {
    /// `expected_concurrent_rust_crews` is not carried: nothing reads it.
    pub fn new(spec: &HostSpec) -> Self {
        Self {
            display_name: spec.display_name.clone(),
            connection: match &spec.connection {
                HostConnection::Daemon => Connection::Daemon,
                HostConnection::SshHost { owning_daemon, .. } => Connection::SshHost { owning_daemon: owning_daemon.clone() },
            },
            image_builds: spec.image_build_capacity.as_ref().map(|capacity| match capacity {
                ImageBuildCapacity::None => ImageBuilds::None,
                ImageBuildCapacity::Builder { architecture, slots, reservation } => ImageBuilds::Builder {
                    architecture: architecture.clone(),
                    slots: *slots,
                    reservation: Reservation { cpu: reservation.cpu, disk_bytes: reservation.disk_bytes },
                },
            }),
            max_vessels: spec.max_vessels,
            single_tenant: spec.single_tenant,
        }
    }

    pub fn spec(&self) -> HostSpec {
        HostSpec {
            display_name: self.display_name.clone(),
            connection: match &self.connection {
                Connection::Daemon => HostConnection::Daemon,
                Connection::SshHost { owning_daemon } => {
                    HostConnection::SshHost { owning_daemon: owning_daemon.clone(), destination: String::new() }
                }
            },
            image_build_capacity: self.image_builds.as_ref().map(|builds| match builds {
                ImageBuilds::None => ImageBuildCapacity::None,
                ImageBuilds::Builder { architecture, slots, reservation } => ImageBuildCapacity::Builder {
                    architecture: architecture.clone(),
                    slots: *slots,
                    reservation: ImageBuildReservation { cpu: reservation.cpu, disk_bytes: reservation.disk_bytes },
                },
            }),
            max_vessels: self.max_vessels,
            single_tenant: self.single_tenant,
            ..HostSpec::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored)]
pub enum Value {
    #[store(code = "t")]
    True,
    #[store(code = "f")]
    False,
    #[store(code = "u")]
    Unknown,
}

impl From<ConditionValue> for Value {
    fn from(value: ConditionValue) -> Self {
        match value {
            ConditionValue::True => Value::True,
            ConditionValue::False => Value::False,
            ConditionValue::Unknown => Value::Unknown,
        }
    }
}

impl From<Value> for ConditionValue {
    fn from(value: Value) -> Self {
        match value {
            Value::True => ConditionValue::True,
            Value::False => ConditionValue::False,
            Value::Unknown => ConditionValue::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Condition {
    pub value: Value,
    pub reason: String,
    pub message: String,
    pub blocks_readiness: bool,
    pub blocks_forge_observation: bool,
}

impl Condition {
    pub fn new(condition: &HostCondition) -> Self {
        Self {
            value: condition.value.into(),
            reason: condition.reason.clone(),
            message: condition.message.clone(),
            blocks_readiness: condition.blocks_readiness,
            blocks_forge_observation: condition.blocks_forge_observation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Stored, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    #[store(code = "hd")]
    HostDirect,
    #[store(code = "ct")]
    Container,
    #[store(code = "mv")]
    MicroVm,
    #[store(code = "cs")]
    CloudSandbox,
    #[store(code = "dy")]
    Dummy,
}

impl Class {
    /// By the class's serialized name, so no code branches on a descriptive class (ADR 0058).
    pub fn new(class: EnvironmentClass) -> Result<Self, String> {
        serde_json::to_value(class)
            .and_then(serde_json::from_value)
            .map_err(|error| format!("class {class:?} has no stored class: {error}"))
    }

    pub fn environment_class(self) -> EnvironmentClass {
        serde_json::to_value(self).and_then(serde_json::from_value).expect("every stored class names an environment class")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Readiness {
    #[store(code = "rd")]
    Ready,
    #[store(code = "mt")]
    MissingTool { reason: String },
    #[store(code = "mc")]
    MissingCredential { reason: String },
    #[store(code = "un")]
    Unknown { reason: String },
}

impl From<&ProviderReadiness> for Readiness {
    fn from(readiness: &ProviderReadiness) -> Self {
        match readiness {
            ProviderReadiness::Ready => Readiness::Ready,
            ProviderReadiness::MissingTool { reason } => Readiness::MissingTool { reason: reason.clone() },
            ProviderReadiness::MissingCredential { reason } => Readiness::MissingCredential { reason: reason.clone() },
            ProviderReadiness::Unknown { reason } => Readiness::Unknown { reason: reason.clone() },
        }
    }
}

/// The advertisement fields that matching and realisation read.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Provider {
    pub class: Option<Class>,
    pub readiness: Readiness,
    pub workspace_root: String,
    #[store(cbor)]
    pub material_modes: Vec<MaterialMode>,
    #[store(cbor)]
    pub checkout_strategies: Vec<CheckoutStrategy>,
}

impl Provider {
    /// Refused for a class with no stored class, or a workspace root that is not UTF-8.
    pub fn new(advertisement: &ProviderAdvertisement) -> Result<Self, String> {
        let root = &advertisement.capabilities.workspace_root;
        Ok(Self {
            class: advertisement.kind.as_ref().map(|kind| Class::new(kind.class)).transpose()?,
            readiness: (&advertisement.readiness).into(),
            workspace_root: root.to_str().ok_or_else(|| format!("workspace root {} is not UTF-8", root.display()))?.to_owned(),
            material_modes: advertisement.capabilities.material_modes.clone(),
            checkout_strategies: advertisement.capabilities.checkout_strategies.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Expiry {
    pub expires_at: Option<Timestamp>,
    pub refresh_expires_at: Option<Timestamp>,
}

/// One provider instance's image cache.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct ProviderImages {
    #[store(child)]
    pub images: BTreeSet<String>,
}

/// What the probe found for one FulfilmentKind here. Its age and free slots are judgements.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct Facts {
    /// The kind's crew terminal pool is missing here, which keeps the kind full.
    pub crew_pool_missing: bool,
    pub gui_session_logged_in: bool,
    /// Version by harness.
    #[store(child)]
    pub harnesses: BTreeMap<String, String>,
    /// Version by toolchain.
    #[store(child)]
    pub toolchains: BTreeMap<String, String>,
}

impl Facts {
    /// No free slots means a missing pool only where no vessel cap applies, so the caller says.
    pub fn new(facts: &FulfilmentFacts, crew_pool_missing: bool) -> Self {
        Self {
            crew_pool_missing,
            gui_session_logged_in: facts.gui_session_logged_in,
            harnesses: facts.harnesses.iter().map(|(harness, facts)| (harness.clone(), facts.version.clone())).collect(),
            toolchains: facts.toolchains.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Stored)]
#[store(kind = "HostObservation", code = "ob")]
pub struct HostObservation {
    pub daemon_generation: Option<String>,
    /// The image architecture (`amd64`, `arm64`); none for an SSH host.
    pub architecture: Option<String>,
    /// Free disk is below admission's floor; false when either figure is unknown.
    pub below_free_space_floor: bool,
    /// An SSH host's owning daemon keeps a central Codex login.
    pub codex_central_login: bool,
    /// Only conditions that can block readiness or forge observation; diagnostics are metrics.
    #[store(child)]
    pub conditions: BTreeMap<String, Condition>,
    #[store(child)]
    pub providers: BTreeMap<String, Provider>,
    #[store(child)]
    pub agent_adapters: BTreeSet<String>,
    /// The adapters of the last daemon generation that did not regress.
    #[store(child)]
    pub agent_adapter_baseline: BTreeSet<String>,
    #[store(child)]
    pub held_credentials: BTreeSet<String>,
    #[store(child)]
    pub credential_expiry: BTreeMap<String, Expiry>,
    /// By provider instance.
    #[store(child)]
    pub image_digests: BTreeMap<String, ProviderImages>,
    /// By FulfilmentKind name, as text: no reference crosses to the kind.
    #[store(child)]
    pub facts: BTreeMap<String, Facts>,
}

/// Registers Host (`ho`) and HostObservation (`ob`) into `registry`: plain kinds.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<HostRecord>()?;
    registry.register::<HostObservation>()?;
    Ok(())
}

/// Host and HostObservation alone, for their DDL snapshot and the import rehearsal.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
