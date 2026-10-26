//! `flotilla image check` (ADR 0062): build and probe a branch's composed
//! image before merge. The daemon resolves the composition and get-or-creates
//! its ImageBuild; these are the request and the reports it returns.
use serde::{Deserialize, Serialize};

/// What to check: the project's composition with its primary repository's
/// `.flotilla/image/` read at `git_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ImageCheckRequest {
    pub project: String,
    /// A branch of the project's primary repository.
    pub git_ref: String,
    /// Image needs (`--need`), as capability strings.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Harness adapters (`--agent`); empty means the adapters the project's
    /// default workflow's roles resolve to.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    /// Canonical architecture (`amd64`, `arm64`); `None` is this host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
}

/// The outcome of one check request. A refused check names its refusals and
/// creates no build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCheckReport {
    pub project: String,
    pub git_ref: String,
    /// The commit the layer directory was read at; `None` when nothing was
    /// read (the project has no primary code repository).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// `<name>@<version>` of the project layer read at the ref; `None` when
    /// the ref has no `.flotilla/image/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_layer: Option<String>,
    /// Harness adapters the composition carries, and where they came from.
    pub agents: Vec<String>,
    pub agents_from: ImageCheckAgentSource,
    /// Definition errors and resolution refusals, each naming its layers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<ImageCheckRefusal>,
    /// Layer names a higher-precedence source shadows in this composition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<ImageCheckShadow>,
    /// The composition's build; `None` when refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<ImageBuildReport>,
    /// Whether the build already existed for this identity (and is reused,
    /// with its result) rather than created by this check.
    #[serde(default)]
    pub reused: bool,
}

impl ImageCheckReport {
    /// A refused check, or a settled build that did not succeed.
    pub fn failed(&self) -> bool {
        !self.refusals.is_empty() || self.build.as_ref().is_some_and(|build| build.phase == ImageBuildReportPhase::Failed && build.settled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageCheckAgentSource {
    /// `--agent`.
    Requested,
    /// The project's default workflow's agent roles.
    DefaultWorkflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCheckRefusal {
    /// The layers (or the layer directory) the refusal names; empty when it
    /// names none (an unresolvable `--need`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCheckShadow {
    pub name: String,
    pub source: String,
    pub shadowed: String,
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageBuildReportPhase {
    Queued,
    Building,
    Built,
    Failed,
}

impl ImageBuildReportPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Building => "building",
            Self::Built => "built",
            Self::Failed => "failed",
        }
    }
}

/// One ImageBuild as a check reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBuildReport {
    pub name: String,
    pub identity: String,
    pub architecture: String,
    pub builder_host: String,
    /// `registry <repository>` or `local`.
    pub delivery: String,
    pub phase: ImageBuildReportPhase,
    /// No further change is coming without an operator: built, a recipe
    /// failure, or an infrastructure failure with no attempts left.
    pub settled: bool,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub layers: Vec<ImageLayerReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ImageBuildFailureReport>,
    /// The build-log artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLayerReport {
    pub layer: String,
    pub source: String,
    pub version: String,
    pub tier: String,
    pub probe: ImageProbeReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ImageProbeReport {
    /// The build has not reported this layer's probe yet.
    Pending,
    Passed,
    Failed {
        reason: String,
    },
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBuildFailureReport {
    /// `infrastructure` or `recipe` for an actuator failure; `lifecycle` for an engine cause or ending.
    pub class: String,
    pub reason: String,
}
