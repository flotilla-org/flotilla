//! Runtime-only provider vocabulary. Persisted Docker inputs remain at the
//! legacy boundary until the generated Environment table move (ADR 0058).
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentClass {
    HostDirect,
    Container,
    MicroVm,
    CloudSandbox,
    Dummy,
}

/// Implementation names are open; identity never selects runtime behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKind {
    pub class: EnvironmentClass,
    pub implementation: String,
}

impl ProviderKind {
    pub fn new(class: EnvironmentClass, implementation: impl Into<String>) -> Self {
        Self { class, implementation: implementation.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialMode {
    Frozen,
    Pushed,
    Shared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStrategy {
    HostWorktree,
    HostClone,
    CloneInVessel,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressRoute {
    #[default]
    None,
    ExplicitProxy,
    TransparentProxy,
    Open,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressEnforcement {
    Enforced,
    #[default]
    Advisory,
}

/// Data consumed without branching on a runtime name or class. Unsupported
/// operations default closed. Optional operation traits remain instance-owned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub material_modes: Vec<MaterialMode>,
    pub checkout_strategies: Vec<CheckoutStrategy>,
    pub workspace_root: PathBuf,
    pub reference_root: Option<PathBuf>,
    pub suspend_resume: bool,
    pub export: bool,
    pub egress_route: EgressRoute,
    pub egress_enforcement: EgressEnforcement,
    pub fallback_runner: bool,
    pub local_image_cache: bool,
    pub image_builder: bool,
    pub detection: bool,
    pub receives_host_tools: bool,
    pub per_vessel: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProviderReadiness {
    Ready,
    MissingTool { tool: String },
    MissingCredential { credential: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAdvertisement {
    pub instance: String,
    pub kind: ProviderKind,
    pub readiness: ProviderReadiness,
    pub capabilities: ProviderCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedCause {
    SuspendedByRequest,
    PausedByProvider,
    GuestHalted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoneCause {
    LifetimeCapReached,
    EvictedByProvider,
    HostOutOfMemory,
    CgroupOutOfMemory,
    BackingExited { code: i32 },
    VmmExited { code: i32 },
    GuestPoweredOff,
    MissingOnList,
    DestroyedByRequest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    HostUnreachable,
    ProviderApiError { message: String },
    GuestUnresponsive,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObservationState {
    Running,
    Stopped { cause: StoppedCause },
    Gone { cause: GoneCause },
    Unknown { reason: UnknownReason },
}

/// Runtime-only observation payload; not a wire or stored resource shape.
/// Keep all existing Docker memory/exit evidence without changing stored facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObservation {
    pub state: ObservationState,
    pub runtime: Option<flotilla_protocol::EnvironmentRuntimeObservation>,
}
impl ProviderObservation {
    pub fn new(state: ObservationState) -> Self {
        Self { state, runtime: None }
    }
}

/// Tokens are opaque and accepted only by the originating instance.
#[async_trait]
pub trait SuspendResume: Send + Sync {
    async fn suspend(&self, backing_id: &str) -> Result<String, String>;
    async fn resume(&self, token: &str) -> Result<(), String>;
}
#[async_trait]
pub trait StorageExport: Send + Sync {
    async fn export(&self, backing_id: &str) -> Result<String, String>;
}

/// Read-only tool detection, separate from required lifecycle operations.
#[async_trait]
pub trait EnvironmentDetection: Send + Sync {
    async fn detect(
        &self,
        image: Option<&str>,
        binary: &str,
        args: &[&str],
        scratch: &std::path::Path,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String>;
}

/// The stream binds observations to the configured instance and actual image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentDescriptor {
    pub kind: ProviderKind,
    pub instance: String,
    pub image_digest: Option<String>,
    pub provider_versions: std::collections::BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
/// Runtime-only stream event. Publication/serialization belongs to its consumer.
pub struct EnvironmentObservation {
    pub descriptor: EnvironmentDescriptor,
    pub backing_id: Option<String>,
    pub observation: ProviderObservation,
}
