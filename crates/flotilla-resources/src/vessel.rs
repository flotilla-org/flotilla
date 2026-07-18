use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, RepositoryKey, Stance};

define_resource!(Vessel, "vessels", VesselSpec, VesselStatus, VesselStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselSpec {
    pub convoy_ref: String,
    /// The within-convoy vessel name (the requirement / work key, e.g. `implement`).
    pub vessel_name: String,
    pub placement_policy_ref: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VesselPhase {
    #[default]
    Pending,
    Provisioning,
    Ready,
    TearingDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselStatus {
    pub phase: VesselPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ref: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checkout_refs: BTreeMap<RepositoryKey, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_session_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_stance: Option<Stance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_stance: Option<Stance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VesselStatusPatch {
    MarkProvisioning {
        observed_policy_ref: String,
        observed_policy_version: String,
        started_at: DateTime<Utc>,
    },
    MarkReady {
        environment_ref: Option<String>,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
        ready_at: DateTime<Utc>,
    },
    MarkTearingDown,
    MarkFailed {
        message: String,
    },
}

impl StatusPatch<VesselStatus> for VesselStatusPatch {
    fn apply(&self, status: &mut VesselStatus) {
        match self {
            Self::MarkProvisioning { observed_policy_ref, observed_policy_version, started_at } => {
                status.phase = VesselPhase::Provisioning;
                status.observed_policy_ref = Some(observed_policy_ref.clone());
                status.observed_policy_version = Some(observed_policy_version.clone());
                status.started_at.get_or_insert(*started_at);
                status.message = None;
            }
            Self::MarkReady { environment_ref, checkout_refs, terminal_session_refs, requested_stance, effective_stance, ready_at } => {
                status.phase = VesselPhase::Ready;
                status.environment_ref = environment_ref.clone();
                status.checkout_refs = checkout_refs.clone();
                status.terminal_session_refs = terminal_session_refs.clone();
                status.requested_stance = Some(*requested_stance);
                status.effective_stance = Some(*effective_stance);
                status.ready_at.get_or_insert(*ready_at);
                status.message = None;
            }
            Self::MarkTearingDown => {
                status.phase = VesselPhase::TearingDown;
            }
            Self::MarkFailed { message } => {
                status.phase = VesselPhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}
