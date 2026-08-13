use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::SleepInhibitionHealth;
use serde::{Deserialize, Serialize};

use crate::{
    checkout::ConditionValue, resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch, ReplicationClass,
};

define_resource!(Host, "hosts", HostSpec, HostStatus, HostStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const AGENT_ADAPTERS_CAPABILITY: &str = "agent_adapters";
pub const HELD_CREDENTIALS_CAPABILITY: &str = "held_credentials";
pub const TERMINAL_POOLS_CAPABILITY: &str = "terminal_pools";
pub const HEARTBEAT_READY_TTL_SECS: i64 = 60;
pub const SLEEP_INHIBITION_CONDITION_TYPE: &str = "SleepInhibition";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct HostStatus {
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
    /// Last adapter inventory that did not regress from the preceding
    /// generation. A regressed generation retains this baseline so another
    /// restart cannot silently absorb the loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_adapter_baseline: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_store: Option<ResourceStoreDiagnostics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_started_at: Option<DateTime<Utc>>,
    /// Available bytes on the host-direct checkout root used for convoy admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_free_space_floor_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub conditions: Vec<HostCondition>,
    #[serde(default)]
    #[builder(default)]
    pub sleep_inhibition: SleepInhibitionHealth,
}

impl HostStatus {
    pub fn agent_adapters(&self) -> Result<BTreeSet<String>, serde_json::Error> {
        self.capabilities.get(AGENT_ADAPTERS_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }

    pub fn held_credentials(&self) -> Result<BTreeSet<String>, serde_json::Error> {
        self.capabilities.get(HELD_CREDENTIALS_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }

    pub fn apply_heartbeat_readiness(&mut self, now: DateTime<Utc>) {
        self.ready = self.ready
            && !self.is_degraded()
            && self
                .heartbeat_at
                .is_some_and(|heartbeat_at| now.signed_duration_since(heartbeat_at) <= chrono::Duration::seconds(HEARTBEAT_READY_TTL_SECS));
    }

    pub fn is_degraded(&self) -> bool {
        self.conditions.iter().any(|condition| condition.value == ConditionValue::False)
    }
}

/// A daemon-owned liveness invariant published on its [`Host`] resource.
///
/// `True` means the named subsystem is healthy; `False` makes the host
/// degraded and unavailable for placement until a later observation clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct HostCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub value: ConditionValue,
    pub reason: String,
    pub message: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatusPatch {
    Heartbeat {
        capabilities: BTreeMap<String, serde_json::Value>,
        heartbeat_at: DateTime<Utc>,
        ready: bool,
        daemon_generation: Option<String>,
        daemon_version: Option<String>,
        daemon_started_at: Option<DateTime<Utc>>,
        disk_free_bytes: Option<u64>,
        admission_free_space_floor_bytes: Option<u64>,
    },
    SleepInhibition {
        health: SleepInhibitionHealth,
        observed_at: DateTime<Utc>,
    },
}

impl StatusPatch<HostStatus> for HostStatusPatch {
    fn apply(&self, status: &mut HostStatus) {
        match self {
            Self::Heartbeat {
                capabilities,
                heartbeat_at,
                ready,
                daemon_generation,
                daemon_version,
                daemon_started_at,
                disk_free_bytes,
                admission_free_space_floor_bytes,
            } => {
                status.capabilities = capabilities.clone();
                status.heartbeat_at = Some(*heartbeat_at);
                status.ready = *ready;
                status.daemon_generation.clone_from(daemon_generation);
                status.daemon_version.clone_from(daemon_version);
                status.daemon_started_at = *daemon_started_at;
                status.disk_free_bytes = *disk_free_bytes;
                status.admission_free_space_floor_bytes = *admission_free_space_floor_bytes;
            }
            Self::SleepInhibition { health, observed_at } => {
                status.sleep_inhibition.clone_from(health);
                match health {
                    SleepInhibitionHealth::Failed { message, .. } => {
                        status.conditions.retain(|condition| condition.condition_type != SLEEP_INHIBITION_CONDITION_TYPE);
                        status.conditions.push(
                            HostCondition::builder()
                                .condition_type(SLEEP_INHIBITION_CONDITION_TYPE)
                                .value(ConditionValue::False)
                                .reason("InhibitorNotHeld")
                                .message(format!("sleep inhibition required but not held ({message})"))
                                .observed_at(*observed_at)
                                .build(),
                        );
                    }
                    SleepInhibitionHealth::Held | SleepInhibitionHealth::NotRequired => {
                        status.conditions.retain(|condition| condition.condition_type != SLEEP_INHIBITION_CONDITION_TYPE);
                    }
                    SleepInhibitionHealth::Acquiring { .. } => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_host_condition_makes_fresh_heartbeat_not_ready() {
        let now = Utc::now();
        let mut status = HostStatus {
            heartbeat_at: Some(now),
            ready: true,
            conditions: vec![HostCondition::builder()
                .condition_type("Controller/checkout")
                .value(ConditionValue::False)
                .reason("RestartBudgetExhausted")
                .message("checkout controller stopped")
                .observed_at(now)
                .build()],
            ..HostStatus::default()
        };

        status.apply_heartbeat_readiness(now);

        assert!(!status.ready);
        let encoded = serde_json::to_value(&status).expect("serialize host status");
        assert_eq!(encoded["conditions"][0]["type"], "Controller/checkout");
    }
}
