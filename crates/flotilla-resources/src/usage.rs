use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ApiPaths, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage;

impl Resource for Usage {
    type Spec = UsageSpec;
    type Status = UsageStatus;
    type StatusPatch = UsageStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "usages", kind: "Usage" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("Usage subject is immutable"))
        }
    }

    fn validate_status_update(current: Option<&Self::Status>, requested: &Self::Status) -> Result<(), ResourceError> {
        let Some(current) = current else { return Ok(()) };
        if requested.observed_at > current.observed_at || requested == current {
            return Ok(());
        }
        if requested.observed_at == current.observed_at {
            Err(ResourceError::invalid("Usage observations with equal observed_at must be identical"))
        } else {
            Err(ResourceError::invalid("Usage observed_at must not precede the stored observation"))
        }
    }
}

/// An account at a provider is the stable subject. Plan, organization, and
/// quota data are observations because each can change independently over time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSpec {
    pub provider: String,
    pub account: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageWindow {
    /// Stable lane name such as `session`, `weekly`, or a provider-specific
    /// scoped pool identifier.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsagePace {
    /// Name of the window this projection describes.
    pub window: String,
    pub stage: String,
    pub delta_percent: f64,
    pub expected_used_percent: f64,
    pub will_last_to_reset: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_out_probability: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageProviderCost {
    pub used: f64,
    pub limit: f64,
    pub currency_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct UsageStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Quota lanes remain a set of independently resetting windows. Pollers
    /// must not select one window as the account's scalar usage value.
    #[serde(default)]
    #[builder(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub pace: Vec<UsagePace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_cost: Option<UsageProviderCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits_available: Option<u64>,
    /// Orders observations for this latest-only gauge. A later timestamp
    /// replaces the status; an equal timestamp is valid only for an identical,
    /// idempotent write.
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageStatusPatch {
    Observed(UsageStatus),
}

impl StatusPatch<UsageStatus> for UsageStatusPatch {
    fn apply(&self, status: &mut UsageStatus) {
        match self {
            Self::Observed(observation) => *status = observation.clone(),
        }
    }
}

/// Produce a DNS-safe, fixed-size resource name for a provider/account subject.
pub fn usage_record_name(provider: &str, account: &str) -> String {
    let normalized_provider = provider.trim().to_lowercase();
    let normalized_account = account.trim().to_lowercase();
    let mut hash = Sha256::new();
    hash.update(b"usage-account-v2\0");
    hash.update(normalized_provider.as_bytes());
    hash.update(b"\0");
    hash.update(normalized_account.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!("usage-{}", &digest[..54])
}
