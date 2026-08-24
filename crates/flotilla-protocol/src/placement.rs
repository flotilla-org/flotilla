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
    pub refused_candidates: Vec<PlacementRefusal>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viable_not_selected: Vec<PlacementViableCandidate>,
}
