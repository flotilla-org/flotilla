use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{ConfiguredResourceLimits, PlacementDecision};
use serde::{Deserialize, Serialize};

use crate::{
    resource::define_resource, status_patch::StatusPatch, ControllerRetry, LandingCredentialScope, ReplicationClass, RepositoryKey, Stance,
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
    // Retired unwritten TearingDown decodes as Interrupted, the nearest live
    // recoverable state, not a semantic rename. ADR 0047: remove one roll after #2917.
    #[serde(alias = "TearingDown")]
    Interrupted,
    Lost,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselStatus {
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub phase: VesselPhase,
    /// ADR 0047: previous-generation records omit this; retain default for one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_observation: Option<flotilla_protocol::EnvironmentRuntimeObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_policy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // ADR 0047: remove image_digest alias one roll after generation 1.
    #[serde(alias = "image_digest")]
    pub local_image_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_digest: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_delivery_retry: Option<ControllerRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_refresh_retry: Option<ControllerRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VesselStatusPatch {
    ObserveRuntime {
        observation: flotilla_protocol::EnvironmentRuntimeObservation,
    },
    CredentialDelivery {
        retry: Option<ControllerRetry>,
    },
    CredentialRefresh {
        retry: Option<ControllerRetry>,
    },
    /// Report immutable mount drift without changing the phase of running crews.
    RequireEnvironmentRecreation {
        message: String,
    },
    MarkProvisioning {
        observed_policy_ref: String,
        observed_policy_version: String,
        placement_decision: Option<PlacementDecision>,
        started_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkReady {
        configured_limits: Option<ConfiguredResourceLimits>,
        placement_decision: Option<PlacementDecision>,
        environment_ref: Option<String>,
        image_ref: Option<String>,
        local_image_id: Option<String>,
        registry_digest: Option<String>,
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
    MarkLost {
        message: String,
    },
    MarkFailed {
        message: String,
    },
}

impl StatusPatch<VesselStatus> for VesselStatusPatch {
    fn apply(&self, status: &mut VesselStatus) {
        match self {
            Self::ObserveRuntime { observation } => status.runtime_observation.get_or_insert_with(Default::default).merge(observation),
            Self::CredentialDelivery { retry } => status.credential_delivery_retry = retry.clone(),
            Self::CredentialRefresh { retry } => status.credential_refresh_retry = retry.clone(),
            Self::RequireEnvironmentRecreation { message } => status.message = Some(message.clone()),
            Self::MarkProvisioning { observed_policy_ref, observed_policy_version, placement_decision, started_at, message } => {
                status.phase = VesselPhase::Provisioning;
                status.observed_policy_ref = Some(observed_policy_ref.clone());
                status.observed_policy_version = Some(observed_policy_version.clone());
                if let Some(placement_decision) = placement_decision {
                    status.placement_decision.get_or_insert_with(|| placement_decision.clone());
                }
                status.started_at.get_or_insert(*started_at);
                status.message = message.clone();
            }
            Self::MarkReady {
                configured_limits,
                placement_decision,
                environment_ref,
                image_ref,
                local_image_id,
                registry_digest,
                checkout_refs,
                terminal_session_refs,
                requested_stance,
                effective_stance,
                ready_at,
            } => {
                status.configured_limits = configured_limits.clone();
                status.phase = VesselPhase::Ready;
                if let Some(placement_decision) = placement_decision {
                    status.placement_decision.get_or_insert_with(|| placement_decision.clone());
                }
                status.environment_ref = environment_ref.clone();
                status.image_ref = image_ref.clone();
                status.local_image_id = local_image_id.clone();
                status.registry_digest = registry_digest.clone();
                status.checkout_refs = checkout_refs.clone();
                status.terminal_session_refs = terminal_session_refs.clone();
                status.interrupted_roles.clear();
                status.requested_stance = Some(*requested_stance);
                status.effective_stance = Some(*effective_stance);
                status.ready_at.get_or_insert(*ready_at);
                status.message = None;
            }
            Self::MarkInterrupted { roles, message } => {
                status.phase = VesselPhase::Interrupted;
                status.interrupted_roles = roles.clone();
                status.message = Some(message.clone());
            }
            Self::StageLandingCredentials { credentials } => {
                status.held_credentials.extend(credentials.clone());
            }
            Self::MarkLost { message } => {
                status.phase = VesselPhase::Lost;
                status.message = Some(message.clone());
            }
            Self::MarkFailed { message } => {
                status.phase = VesselPhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}

/// Per-vessel convoy resources (`Vessel`, `Presentation`) share the name
/// shape `<convoy>-<vessel>`. Resource kinds have separate namespaces, so the
/// shared shape causes no collision and keeps both resources discoverable
/// together by name.
pub fn vessel_resource_name(convoy_name: &str, vessel: &str) -> String {
    format!("{convoy_name}-{vessel}")
}
