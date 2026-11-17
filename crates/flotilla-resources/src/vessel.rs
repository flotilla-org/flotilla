use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{ConfiguredResourceLimits, LifecycleReading, PlacementDecision};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, ReplicationClass, RepositoryKey, Stance};

// A vessel is read from its launch on the engine (#3108): nothing patches its status.
define_resource!(
    Vessel,
    "vessels",
    VesselSpec,
    VesselStatus,
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &["metadata.name", "status.phase"]
);

pub const ACTUATOR_HOST_REF_ANNOTATION: &str = "flotilla.work/actuator-host-ref";
pub const ACTUATOR_SOURCE_ROOT_ANNOTATION: &str = "flotilla.work/actuator-source-root";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselSpec {
    pub convoy_ref: String,
    /// The within-convoy vessel name (the requirement / work key, e.g. `implement`).
    pub vessel_name: String,
    // Drop the alias one roll after the rename.
    #[serde(alias = "placement_policy_ref")]
    pub fulfilment_kind_ref: String,
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
    /// Ended on purpose: its lease was given back. No stored record holds it; the engine's
    /// reading is its only source.
    Released,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselStatus {
    /// ADR 0047: previous-generation vessels have no degraded checkout conditions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub degraded: BTreeMap<String, String>,
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_limits: Option<ConfiguredResourceLimits>,
    pub phase: VesselPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    // Drop the alias one roll after the rename.
    #[serde(default, alias = "observed_policy_ref", skip_serializing_if = "Option::is_none")]
    pub observed_kind_ref: Option<String>,
    // Drop the alias one roll after the rename.
    #[serde(default, alias = "observed_policy_version", skip_serializing_if = "Option::is_none")]
    pub observed_kind_version: Option<String>,
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
    /// The composed image this vessel runs, kept until relaunch (ADR 0062).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<VesselImage>,
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
    // Retired with credential delivery's controller (#3108): `held_credentials`,
    // `credential_delivery_retry` and `credential_refresh_retry`. Retired with the vessel's
    // move onto the engine (#3108): `runtime_observation`, now its environment's home memory.
    // Earlier records carrying them still decode, as the struct accepts and drops unknown fields.
}

/// A vessel's frozen image: the composed identity, then the built digest once
/// provisioning resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VesselImage {
    pub identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl VesselPhase {
    /// The phase the engine's reading decides; without a reading, the stored phase.
    pub fn from_reading(reading: Option<&LifecycleReading>, status: Option<&VesselStatus>) -> Self {
        match reading {
            Some(LifecycleReading::Lost { .. }) => Self::Lost,
            Some(LifecycleReading::Released) => Self::Released,
            // Exhaustion is the failed judgement; the engine's GiveUp write follows it.
            Some(LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. } | LifecycleReading::Exhausted { .. }) => Self::Failed,
            Some(LifecycleReading::Ready { .. }) if status.is_some_and(|status| !status.interrupted_roles.is_empty()) => Self::Interrupted,
            Some(LifecycleReading::Ready { .. }) => Self::Ready,
            Some(LifecycleReading::InProgress | LifecycleReading::Retrying { .. }) => Self::Provisioning,
            None => status.map(|status| status.phase).unwrap_or_default(),
        }
    }
}

/// Name a convoy Vessel `<convoy>-<vessel>`: the name its work's launches are of.
pub fn vessel_resource_name(convoy_name: &str, vessel: &str) -> String {
    format!("{convoy_name}-{vessel}")
}

#[cfg(test)]
mod retired_field_tests {
    use super::*;

    // ADR 0047: a status stored before the move onto the engine (#3108) still decodes; its
    // runtime observation, now home memory, is dropped.
    #[test]
    fn a_status_with_its_retired_runtime_observation_decodes() {
        let stored = serde_json::json!({
            "phase": "Ready",
            "environment_ref": "env-a",
            "runtime_observation": { "memory_usage_bytes": 123, "memory_observed_at": "2026-10-05T10:13:00Z" },
        });
        let status: VesselStatus = serde_json::from_value(stored).expect("old status decodes");
        assert_eq!((status.phase, status.environment_ref.as_deref()), (VesselPhase::Ready, Some("env-a")));
        assert!(!serde_json::to_value(&status).expect("encode").as_object().expect("object").contains_key("runtime_observation"));
    }
}
