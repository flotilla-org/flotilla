use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch};

define_resource!(Host, "hosts", HostSpec, HostStatus, HostStatusPatch);

pub const AGENT_ADAPTERS_CAPABILITY: &str = "agent_adapters";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct HostStatus {
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_store: Option<ResourceStoreDiagnostics>,
}

impl HostStatus {
    pub fn agent_adapters(&self) -> Result<BTreeSet<String>, serde_json::Error> {
        self.capabilities.get(AGENT_ADAPTERS_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatusPatch {
    Heartbeat { capabilities: BTreeMap<String, serde_json::Value>, heartbeat_at: DateTime<Utc>, ready: bool },
}

impl StatusPatch<HostStatus> for HostStatusPatch {
    fn apply(&self, status: &mut HostStatus) {
        match self {
            Self::Heartbeat { capabilities, heartbeat_at, ready } => {
                status.capabilities = capabilities.clone();
                status.heartbeat_at = Some(*heartbeat_at);
                status.ready = *ready;
            }
        }
    }
}
