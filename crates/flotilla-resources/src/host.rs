use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch};

define_resource!(Host, "hosts", HostSpec, HostStatus, HostStatusPatch);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
