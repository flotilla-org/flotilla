use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, ReplicationClass, RepositoryKey, Stance};

define_resource!(
    ConvoyEnsure,
    "convoyensures",
    ConvoyEnsureSpec,
    ConvoyEnsureStatus,
    ConvoyEnsureStatusPatch,
    replication = ReplicationClass::Definitions
);

/// Desired state for one standing convoy.
///
/// The referenced workflow must have no exit declaration. `repositories` is
/// the project-member subset selected by the ops entry; an empty set is never
/// materialized by project refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyEnsureSpec {
    pub project_ref: String,
    pub role: String,
    pub workflow_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<Stance>,
    pub repositories: Vec<RepositoryKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presents_as: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyEnsureStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convoy_ref: Option<String>,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<ConvoyEnsureHoldReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_config_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvoyEnsureHoldReason {
    BackingUnverified,
    RestartLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEnsureStatusPatch {
    Running { convoy_ref: String, observed_at: DateTime<Utc> },
    BackingOff { retry_at: DateTime<Utc>, failure: String },
    Retrying { retry_at: DateTime<Utc>, failure: String },
    Holding { convoy_ref: String, failure: String },
    RestartLimitReached { convoy_ref: String, failure: String },
    ObserveConfig { config_hash: String, changed: bool },
    ResetBackoff,
}

impl StatusPatch<ConvoyEnsureStatus> for ConvoyEnsureStatusPatch {
    fn apply(&self, status: &mut ConvoyEnsureStatus) {
        match self {
            Self::Running { convoy_ref, observed_at } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.running_since = Some(*observed_at);
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
            }
            Self::BackingOff { retry_at, failure } => {
                status.convoy_ref = None;
                status.restart_count = status.restart_count.saturating_add(1);
                status.running_since = None;
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
                status.hold_reason = None;
            }
            Self::Retrying { retry_at, failure } => {
                status.running_since = None;
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
                status.hold_reason = None;
            }
            Self::Holding { convoy_ref, failure } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.running_since = None;
                status.retry_at = None;
                status.last_failure = Some(failure.clone());
                status.hold_reason = Some(ConvoyEnsureHoldReason::BackingUnverified);
            }
            Self::RestartLimitReached { convoy_ref, failure } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.restart_count = status.restart_count.saturating_add(1);
                status.running_since = None;
                status.retry_at = None;
                status.last_failure = Some(failure.clone());
                status.hold_reason = Some(ConvoyEnsureHoldReason::RestartLimit);
            }
            Self::ObserveConfig { config_hash, changed } => {
                status.observed_config_hash = Some(config_hash.clone());
                if *changed {
                    status.restart_count = 0;
                    status.retry_at = None;
                    status.last_failure = None;
                    status.hold_reason = None;
                }
            }
            Self::ResetBackoff => {
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
            }
        }
    }
}
