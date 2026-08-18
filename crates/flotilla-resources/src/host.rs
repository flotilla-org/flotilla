use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{CanonicalHostId, SleepInhibitionHealth};
use serde::{Deserialize, Serialize};

use crate::{
    checkout::ConditionValue, resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch, ReplicationClass,
};

define_resource!(Host, "hosts", HostSpec, HostStatus, HostStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const AGENT_ADAPTERS_CAPABILITY: &str = "agent_adapters";
pub const HELD_CREDENTIALS_CAPABILITY: &str = "held_credentials";
pub const CREDENTIAL_EXPIRY_CAPABILITY: &str = "credential_expiry";
/// Scope name under [`CREDENTIAL_EXPIRY_CAPABILITY`] for the host's ambient
/// claude login (the credentials file a `claude login` leaves behind), which
/// is held material without a `CredentialSpec` declaring it.
pub const AMBIENT_CLAUDE_CREDENTIAL_SCOPE: &str = "ambient:claude";
pub const TERMINAL_POOLS_CAPABILITY: &str = "terminal_pools";
pub const HEARTBEAT_READY_TTL_SECS: i64 = 60;
pub const SLEEP_INHIBITION_CONDITION_TYPE: &str = "SleepInhibition";

/// Resolve a user-authored host resource name or display-name alias to the
/// stable host resource identity used by comparison surfaces.
pub fn canonical_host_id<'a>(
    hosts: impl IntoIterator<Item = &'a crate::ResourceObject<Host>>,
    host_ref: &str,
) -> Result<Option<CanonicalHostId>, String> {
    let hosts = hosts.into_iter().collect::<Vec<_>>();
    if let Some(host) = hosts.iter().find(|host| host.metadata.name == host_ref) {
        return Ok(Some(CanonicalHostId::resolved(host.metadata.name.clone())));
    }
    let mut matching = hosts.iter().filter(|host| host.spec.display_name == host_ref);
    let Some(host) = matching.next() else {
        return Ok(None);
    };
    if matching.any(|candidate| candidate.metadata.name != host.metadata.name) {
        return Err(format!("host reference `{host_ref}` is ambiguous"));
    }
    Ok(Some(CanonicalHostId::resolved(host.metadata.name.clone())))
}

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

    /// Expiry metadata for held credential material, keyed by scope name —
    /// [`AMBIENT_CLAUDE_CREDENTIAL_SCOPE`] for the ambient claude login, a
    /// `CredentialSpec` name for declared credentials whose adapter can
    /// express expiry. Timestamps only; never material.
    pub fn credential_expiry(&self) -> Result<BTreeMap<String, CredentialExpiry>, serde_json::Error> {
        self.capabilities.get(CREDENTIAL_EXPIRY_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
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

/// Expiry metadata observed for one piece of held credential material.
///
/// Published under [`CREDENTIAL_EXPIRY_CAPABILITY`] in the same capability
/// family as `held_credentials`. Carries timestamps and nothing else — the
/// probe that produces it must never read tokens into these fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CredentialExpiry {
    /// When the primary material (e.g. an access token) expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// When the refresh chain dies, where the material is refreshable. While
    /// this is in the future the primary material can be renewed, so it — not
    /// `expires_at` — bounds the credential's usable life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<DateTime<Utc>>,
}

impl CredentialExpiry {
    /// The latest point the material can still authenticate: the refresh
    /// chain's expiry where one exists, otherwise the primary expiry. `None`
    /// means no expiry metadata was observed.
    pub fn effective_expires_at(&self) -> Option<DateTime<Utc>> {
        self.refresh_expires_at.max(self.expires_at)
    }

    /// The expiry timestamp, if the material is expired at `now`.
    pub fn expired_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.effective_expires_at().filter(|expires_at| *expires_at <= now)
    }

    /// The expiry timestamp, if the material is unexpired at `now` but
    /// expires within `window`.
    pub fn expires_within(&self, now: DateTime<Utc>, window: chrono::Duration) -> Option<DateTime<Utc>> {
        self.effective_expires_at().filter(|expires_at| *expires_at > now && *expires_at <= now + window)
    }
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

    #[test]
    fn credential_expiry_round_trips_through_the_capability_family() {
        let expires_at = Utc::now();
        let refresh_expires_at = expires_at + chrono::Duration::days(30);
        let expiry = CredentialExpiry::builder().expires_at(expires_at).refresh_expires_at(refresh_expires_at).build();
        let status = HostStatus {
            capabilities: BTreeMap::from([(
                CREDENTIAL_EXPIRY_CAPABILITY.to_string(),
                serde_json::json!({ AMBIENT_CLAUDE_CREDENTIAL_SCOPE: expiry }),
            )]),
            ..HostStatus::default()
        };

        let decoded = status.credential_expiry().expect("decode credential expiry");
        assert_eq!(decoded, BTreeMap::from([(AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expiry)]));
        assert_eq!(HostStatus::default().credential_expiry().expect("absent capability decodes"), BTreeMap::new());
    }

    #[test]
    fn credential_expiry_is_bounded_by_the_refresh_chain_where_one_exists() {
        let now = Utc::now();
        let window = chrono::Duration::days(7);
        let refreshable = CredentialExpiry::builder()
            .expires_at(now - chrono::Duration::hours(1))
            .refresh_expires_at(now + chrono::Duration::days(3))
            .build();
        assert_eq!(refreshable.effective_expires_at(), refreshable.refresh_expires_at);
        assert_eq!(refreshable.expired_at(now), None);
        assert_eq!(refreshable.expires_within(now, window), refreshable.refresh_expires_at);

        let dead = CredentialExpiry::builder()
            .expires_at(now - chrono::Duration::days(20))
            .refresh_expires_at(now - chrono::Duration::days(15))
            .build();
        assert_eq!(dead.expired_at(now), dead.refresh_expires_at);
        assert_eq!(dead.expires_within(now, window), None);

        let access_only = CredentialExpiry::builder().expires_at(now + chrono::Duration::days(30)).build();
        assert_eq!(access_only.effective_expires_at(), access_only.expires_at);
        assert_eq!(access_only.expired_at(now), None);
        assert_eq!(access_only.expires_within(now, window), None);

        assert_eq!(CredentialExpiry::default().effective_expires_at(), None);
        assert_eq!(CredentialExpiry::default().expired_at(now), None);
    }
}
