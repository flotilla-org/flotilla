//! Data-only provider declarations shared by stored resources and runtimes.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

/// What a provider reports it chose to reach a vessel's agent: the provider,
/// the connection mode and where the vessel lives. Flotilla records and shows
/// it, and never branches on it (#3980 contract 9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConnectionReport {
    /// The provider instance.
    pub provider: String,
    /// The provider's module, such as `daytona`.
    pub module: String,
    /// The provider's own ID for the vessel.
    pub vessel: String,
    /// The endpoint the agent is reached through, without credentials.
    pub endpoint: String,
    /// The connection mode, such as `reverse_forward` or `local_forward`.
    pub mode: String,
}

impl std::fmt::Display for AgentConnectionReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} via {} to {} vessel {} (provider {})", self.mode, self.endpoint, self.module, self.vessel, self.provider)
    }
}

/// Data consumed without branching on a runtime name or class. Unsupported
/// operations default closed. Optional operation traits remain instance-owned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
    pub receives_host_tools: bool,
    pub per_vessel: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderAdvertisement {
    pub instance: String,
    pub kind: Option<ProviderKind>,
    pub capabilities: ProviderCapabilities,
    pub readiness: ProviderReadiness,
}

/// Readiness is observed on the advertising host, including cloud credentials.
/// ADR 0047: omitted previous-generation readiness stays unknown for one roll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProviderReadiness {
    Ready,
    MissingTool { reason: String },
    MissingCredential { reason: String },
    Unknown { reason: String },
}

impl Default for ProviderReadiness {
    fn default() -> Self {
        Self::Unknown { reason: "provider readiness not observed".into() }
    }
}
