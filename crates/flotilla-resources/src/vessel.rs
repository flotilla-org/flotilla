use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::PlacementDecision;
use serde::{Deserialize, Serialize};

use crate::{
    environment::EnvironmentWaitReason, resource::define_resource, status_patch::StatusPatch, LandingCredentialScope, ReplicationClass,
    RepositoryKey, Stance,
};

define_resource!(Vessel, "vessels", VesselSpec, VesselStatus, VesselStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const ACTUATOR_HOST_REF_ANNOTATION: &str = "flotilla.work/actuator-host-ref";
pub const ACTUATOR_SOURCE_ROOT_ANNOTATION: &str = "flotilla.work/actuator-source-root";

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
    Interrupted,
    TearingDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselStatus {
    pub phase: VesselPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<EnvironmentWaitReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checkout_refs: BTreeMap<RepositoryKey, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_session_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub interrupted_roles: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_stance: Option<Stance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_stance: Option<Stance>,
    /// Landing material actually staged in this vessel. This is deliberately
    /// status, not desired spec: absence is the pre-approval invariant.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held_credentials: BTreeMap<String, LandingCredentialScope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VesselStatusPatch {
    MarkProvisioning {
        observed_policy_ref: String,
        observed_policy_version: String,
        placement_decision: Option<PlacementDecision>,
        started_at: DateTime<Utc>,
        message: Option<String>,
        wait_reason: Option<EnvironmentWaitReason>,
    },
    MarkReady {
        placement_decision: Option<PlacementDecision>,
        environment_ref: Option<String>,
        image_ref: Option<String>,
        image_digest: Option<String>,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
        ready_at: DateTime<Utc>,
    },
    MarkInterrupted {
        roles: BTreeSet<String>,
        message: String,
    },
    StageLandingCredentials {
        credentials: BTreeMap<String, LandingCredentialScope>,
    },
    MarkTearingDown,
    MarkFailed {
        message: String,
    },
}

impl StatusPatch<VesselStatus> for VesselStatusPatch {
    fn apply(&self, status: &mut VesselStatus) {
        match self {
            Self::MarkProvisioning {
                observed_policy_ref,
                observed_policy_version,
                placement_decision,
                started_at,
                message,
                wait_reason,
            } => {
                status.phase = VesselPhase::Provisioning;
                status.observed_policy_ref = Some(observed_policy_ref.clone());
                status.observed_policy_version = Some(observed_policy_version.clone());
                if let Some(placement_decision) = placement_decision {
                    status.placement_decision.get_or_insert_with(|| placement_decision.clone());
                }
                status.started_at.get_or_insert(*started_at);
                status.message = message.clone();
                status.wait_reason = wait_reason.clone();
            }
            Self::MarkReady {
                placement_decision,
                environment_ref,
                image_ref,
                image_digest,
                checkout_refs,
                terminal_session_refs,
                requested_stance,
                effective_stance,
                ready_at,
            } => {
                status.phase = VesselPhase::Ready;
                if let Some(placement_decision) = placement_decision {
                    status.placement_decision.get_or_insert_with(|| placement_decision.clone());
                }
                status.environment_ref = environment_ref.clone();
                status.image_ref = image_ref.clone();
                status.image_digest = image_digest.clone();
                status.checkout_refs = checkout_refs.clone();
                status.terminal_session_refs = terminal_session_refs.clone();
                status.interrupted_roles.clear();
                status.requested_stance = Some(*requested_stance);
                status.effective_stance = Some(*effective_stance);
                status.ready_at.get_or_insert(*ready_at);
                status.message = None;
                status.wait_reason = None;
            }
            Self::MarkInterrupted { roles, message } => {
                status.phase = VesselPhase::Interrupted;
                status.interrupted_roles = roles.clone();
                status.message = Some(message.clone());
                status.wait_reason = None;
            }
            Self::StageLandingCredentials { credentials } => {
                status.held_credentials.extend(credentials.clone());
            }
            Self::MarkTearingDown => {
                status.phase = VesselPhase::TearingDown;
                status.wait_reason = None;
            }
            Self::MarkFailed { message } => {
                status.phase = VesselPhase::Failed;
                status.message = Some(message.clone());
                status.wait_reason = None;
            }
        }
    }
}
