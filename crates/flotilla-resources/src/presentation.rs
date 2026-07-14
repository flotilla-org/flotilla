use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(Presentation, "presentations", PresentationSpec, PresentationStatus, PresentationStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PresentationSpec {
    pub convoy_ref: String,
    pub presentation_policy_ref: String,
    pub name: String,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub process_selector: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresentationPhase {
    #[default]
    Pending,
    Active,
    TornDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationStatus {
    pub phase: PresentationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workspace_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_presentation_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_spec_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationStatusPatch {
    /// Duplicate-safe while already active; from another phase this records a new realization attempt.
    MarkActive {
        presentation_manager: String,
        workspace_ref: String,
        spec_hash: String,
        ready_at: DateTime<Utc>,
    },
    MarkTornDown {
        message: Option<String>,
    },
    MarkFailed {
        message: String,
    },
}

impl StatusPatch<PresentationStatus> for PresentationStatusPatch {
    fn apply(&self, status: &mut PresentationStatus) {
        match self {
            Self::MarkActive { presentation_manager, workspace_ref, spec_hash, ready_at } => {
                let was_active = status.phase == PresentationPhase::Active;
                status.phase = PresentationPhase::Active;
                status.observed_presentation_manager = Some(presentation_manager.clone());
                status.observed_workspace_ref = Some(workspace_ref.clone());
                status.observed_spec_hash = Some(spec_hash.clone());
                if !was_active {
                    status.ready_at = Some(*ready_at);
                } else {
                    status.ready_at.get_or_insert(*ready_at);
                }
                status.message = None;
            }
            Self::MarkTornDown { message } => {
                status.phase = PresentationPhase::TornDown;
                status.observed_presentation_manager = None;
                status.observed_workspace_ref = None;
                status.observed_spec_hash = None;
                status.message = message.clone();
            }
            Self::MarkFailed { message } => {
                status.phase = PresentationPhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}
