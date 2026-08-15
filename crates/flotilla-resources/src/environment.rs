use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{placement_policy::DockerImagePullPolicy, resource::define_resource, status_patch::StatusPatch};

define_resource!(Environment, "environments", EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_direct: Option<HostDirectEnvironmentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerEnvironmentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectEnvironmentSpec {
    pub host_ref: String,
    pub repo_default_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerEnvironmentSpec {
    pub host_ref: String,
    pub image: String,
    /// Agent adapters the placement policy expects discovery to find in the image.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declared_agent_adapters: BTreeSet<String>,
    /// Agent adapters this specific vessel workflow will actually launch.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required_agent_adapters: BTreeSet<String>,
    #[serde(default)]
    pub pull_policy: DockerImagePullPolicy,
    #[serde(default)]
    pub mounts: Vec<EnvironmentMount>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentMount {
    pub source_path: String,
    pub target_path: String,
    pub mode: EnvironmentMountMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentMountMode {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentPhase {
    #[default]
    Pending,
    Ready,
    Terminating,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    pub phase: EnvironmentPhase,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<EnvironmentWaitReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EnvironmentWaitReason {
    /// Capacity contention surfaced immediately by Vessel planning rather than
    /// waiting for the generic provisioning-stuck threshold.
    MaterialPoolExhausted { pool_ref: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentStatusPatch {
    MarkReady { docker_container_id: Option<String>, image_ref: Option<String>, image_digest: Option<String> },
    MarkWaiting { message: String, reason: EnvironmentWaitReason },
    MarkFailed { message: String },
    MarkTerminating,
}

impl StatusPatch<EnvironmentStatus> for EnvironmentStatusPatch {
    fn apply(&self, status: &mut EnvironmentStatus) {
        match self {
            Self::MarkReady { docker_container_id, image_ref, image_digest } => {
                status.phase = EnvironmentPhase::Ready;
                status.ready = true;
                status.docker_container_id = docker_container_id.clone();
                status.image_ref = image_ref.clone();
                status.image_digest = image_digest.clone();
                status.message = None;
                status.wait_reason = None;
            }
            Self::MarkWaiting { message, reason } => {
                status.phase = EnvironmentPhase::Pending;
                status.ready = false;
                status.message = Some(message.clone());
                status.wait_reason = Some(reason.clone());
            }
            Self::MarkFailed { message } => {
                status.phase = EnvironmentPhase::Failed;
                status.ready = false;
                status.message = Some(message.clone());
                status.wait_reason = None;
            }
            Self::MarkTerminating => {
                status.phase = EnvironmentPhase::Terminating;
                status.ready = false;
                status.wait_reason = None;
            }
        }
    }
}
