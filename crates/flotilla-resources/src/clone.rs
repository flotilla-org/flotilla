use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, RepositoryKey};

define_resource!(Clone, "clones", CloneSpec, CloneStatus, CloneStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CloneSpec {
    pub repo_ref: RepositoryKey,
    pub url: String,
    pub env_ref: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClonePhase {
    #[default]
    Pending,
    Cloning,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneStatus {
    pub phase: ClonePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneStatusPatch {
    MarkCloning,
    MarkRetrying { message: String },
    MarkReady { default_branch: Option<String> },
    MarkFailed { message: String, failed_at: DateTime<Utc> },
}

impl StatusPatch<CloneStatus> for CloneStatusPatch {
    fn apply(&self, status: &mut CloneStatus) {
        match self {
            Self::MarkCloning => {
                status.phase = ClonePhase::Cloning;
                status.message = None;
                status.failed_at = None;
            }
            Self::MarkRetrying { message } => {
                status.phase = ClonePhase::Cloning;
                status.message = Some(message.clone());
                status.failed_at = None;
            }
            Self::MarkReady { default_branch } => {
                status.phase = ClonePhase::Ready;
                status.default_branch = default_branch.clone();
                status.message = None;
                status.failed_at = None;
            }
            Self::MarkFailed { message, failed_at } => {
                status.phase = ClonePhase::Failed;
                status.message = Some(message.clone());
                status.failed_at = Some(*failed_at);
            }
        }
    }
}
