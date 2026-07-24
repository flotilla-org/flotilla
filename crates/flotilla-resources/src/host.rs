use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch, ReplicationClass};

define_resource!(Host, "hosts", HostSpec, HostStatus, HostStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const AGENT_ADAPTERS_CAPABILITY: &str = "agent_adapters";
pub const TERMINAL_POOLS_CAPABILITY: &str = "terminal_pools";
pub const HEARTBEAT_READY_TTL_SECS: i64 = 60;

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

    pub fn apply_heartbeat_readiness(&mut self, now: DateTime<Utc>) {
        self.ready = self.ready
            && self
                .heartbeat_at
                .is_some_and(|heartbeat_at| now.signed_duration_since(heartbeat_at) <= chrono::Duration::seconds(HEARTBEAT_READY_TTL_SECS));
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
