use std::fmt;

use serde::{Deserialize, Serialize};

/// A host resource name after resolving a user-authored host reference.
///
/// This deliberately has no `From<String>` implementation: spec-facing host
/// references must pass through the shared canonical host resolver before
/// they reach identity comparison surfaces.
///
/// ```compile_fail
/// use flotilla_protocol::CanonicalHostId;
///
/// let canonical = CanonicalHostId::resolved("host-01");
/// let raw_spec_host_ref = String::from("host-01");
/// assert!(canonical == raw_spec_host_ref);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonicalHostId(String);

impl CanonicalHostId {
    /// Construct the result of canonical host resolution.
    #[doc(hidden)]
    pub fn resolved(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CanonicalHostId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementTargetHost {
    #[serde(rename = "ref")]
    pub reference: CanonicalHostId,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementRefusal {
    pub policy_name: String,
    pub target_host: PlacementTargetHost,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementViableCandidate {
    pub policy_name: String,
    pub target_host: PlacementTargetHost,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementDecision {
    pub policy_name: String,
    pub target_host: PlacementTargetHost,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub minimal_alternatives: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused_candidates: Vec<PlacementRefusal>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viable_not_selected: Vec<PlacementViableCandidate>,
    /// Live inputs and the allocation judgement frozen at admission.
    /// Absent in previous-generation records (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<FulfilmentAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentAllocation {
    pub chosen_kind: String,
    pub candidates: Vec<FulfilmentAllocationCandidate>,
    /// Why admission used reserved platform capacity without escalation.
    /// Absent in previous-generation convoy statuses (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentAllocationCandidate {
    pub kind: String,
    pub host: String,
    pub cost_class: String,
    pub host_ready: bool,
    pub sleeping_until: Option<chrono::DateTime<chrono::Utc>>,
    pub free_vessel_slots: Option<u32>,
    pub reserved_for_platform: bool,
    pub minimal: bool,
    pub available: bool,
}

#[cfg(test)]
mod tests {
    use super::FulfilmentAllocation;

    #[test]
    fn previous_generation_allocation_decodes_without_reservation_reason() {
        let allocation: FulfilmentAllocation =
            serde_json::from_str(r#"{"chosen_kind":"linux","candidates":[]}"#).expect("previous generation allocation");
        assert_eq!(allocation.reservation_reason, None);
    }
}

/// Limits actually configured at provisioning or terminal launch; absence means
/// unknown, rather than unlimited. These are configuration facts, not usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredResourceLimits {
    pub cpus: Option<usize>,
    pub build_jobs: Option<usize>,
    pub linker_threads: Option<usize>,
}

impl ConfiguredResourceLimits {
    /// Apply known launch limits without erasing recorded environment limits
    /// when a launch leaves a field unknown (notably a container's CPU quota).
    pub fn with_overrides(self, overrides: Self) -> Self {
        Self {
            cpus: overrides.cpus.or(self.cpus),
            build_jobs: overrides.build_jobs.or(self.build_jobs),
            linker_threads: overrides.linker_threads.or(self.linker_threads),
        }
    }
}
