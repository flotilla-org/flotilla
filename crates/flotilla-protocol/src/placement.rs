use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementTargetHost {
    #[serde(rename = "ref")]
    pub reference: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PlacementRefusal {
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
}
