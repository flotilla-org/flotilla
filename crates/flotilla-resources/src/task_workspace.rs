use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(TaskWorkspace, "taskworkspaces", TaskWorkspaceSpec, TaskWorkspaceStatus, TaskWorkspaceStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorkspaceSpec {
    pub convoy_ref: String,
    pub task: String,
    pub placement_policy_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_checkout_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskWorkspacePhase {
    #[default]
    Pending,
    Provisioning,
    Ready,
    TearingDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorkspaceStatus {
    pub phase: TaskWorkspacePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_session_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskWorkspaceStatusPatch {
    MarkProvisioning { observed_policy_ref: String, observed_policy_version: String, started_at: DateTime<Utc> },
    MarkReady { environment_ref: Option<String>, checkout_ref: Option<String>, terminal_session_refs: Vec<String>, ready_at: DateTime<Utc> },
    MarkTearingDown,
    MarkFailed { message: String },
}

impl StatusPatch<TaskWorkspaceStatus> for TaskWorkspaceStatusPatch {
    fn apply(&self, status: &mut TaskWorkspaceStatus) {
        match self {
            Self::MarkProvisioning { observed_policy_ref, observed_policy_version, started_at } => {
                status.phase = TaskWorkspacePhase::Provisioning;
                status.observed_policy_ref = Some(observed_policy_ref.clone());
                status.observed_policy_version = Some(observed_policy_version.clone());
                status.started_at = Some(*started_at);
                status.message = None;
            }
            Self::MarkReady { environment_ref, checkout_ref, terminal_session_refs, ready_at } => {
                status.phase = TaskWorkspacePhase::Ready;
                status.environment_ref = environment_ref.clone();
                status.checkout_ref = checkout_ref.clone();
                status.terminal_session_refs = terminal_session_refs.clone();
                status.ready_at = Some(*ready_at);
                status.message = None;
            }
            Self::MarkTearingDown => {
                status.phase = TaskWorkspacePhase::TearingDown;
            }
            Self::MarkFailed { message } => {
                status.phase = TaskWorkspacePhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}
