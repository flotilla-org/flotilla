use chrono::{DateTime, Utc};
use flotilla_protocol::AgentOverride;
use serde::{Deserialize, Serialize};

use crate::{
    checkout::ConditionValue, resource::define_resource, status_patch::StatusPatch, ControllerRetry, LeafMaker, ReplicationClass,
    RepositoryKey, RetryCeiling, StallEvidenceSource, StallRung, StalledCondition,
};

pub const DRIVER_ADMISSION_CONDITION_TYPE: &str = "DriverAdmission";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_ref: Option<String>,
    /// Empty means resolve the standing role workflow through its parent chain.
    #[builder(default)]
    #[serde(default)]
    pub workflow_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    pub repositories: Vec<RepositoryKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presents_as: Option<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_overrides: Vec<AgentOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyEnsureStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_config_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_drift: Option<ConvoyEnsureConfigDrift>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration_refused: Option<crate::DeclarationRefusedCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convoy_ref: Option<String>,
    /// Consecutive failed generations in the current retry episode.
    #[serde(default, rename = "strikes")]
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_since: Option<DateTime<Utc>>,
    /// Wall-clock time at which automatic admission will next be attempted.
    #[serde(default, rename = "next_attempt", skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<ConvoyEnsureHoldReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_config_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ConvoyEnsureCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ControllerRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<StalledCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyEnsureConfigDrift {
    pub changes: Vec<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyEnsureCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub value: ConditionValue,
    pub reason: String,
    pub message: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvoyEnsureHoldReason {
    BackingUnverified,
    RestartLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEnsureStatusPatch {
    ConfigDrift { admitted_hash: Option<String>, observed_hash: String, drift: Option<ConvoyEnsureConfigDrift> },
    DeclarationRefused { condition: Option<crate::DeclarationRefusedCondition> },
    Running { convoy_ref: String, observed_at: DateTime<Utc> },
    BackingOff { retry_at: DateTime<Utc>, failure: String },
    Retrying { retry_at: DateTime<Utc>, failure: String },
    Holding { convoy_ref: String, failure: String },
    RestartLimitReached { convoy_ref: String, failure: String },
    ObserveConfig { config_hash: String, changed: bool },
    BackoffState { strikes: u32, retry_at: DateTime<Utc>, failure: String },
    ResetBackoff,
    DriverManaged,
    DriverAdmission { condition: Option<ConvoyEnsureCondition> },
}

impl StatusPatch<ConvoyEnsureStatus> for ConvoyEnsureStatusPatch {
    fn apply(&self, status: &mut ConvoyEnsureStatus) {
        match self {
            Self::ConfigDrift { admitted_hash, observed_hash, drift } => {
                status.admitted_config_hash.clone_from(admitted_hash);
                status.observed_config_hash = Some(observed_hash.clone());
                status.config_drift.clone_from(drift);
            }
            Self::DeclarationRefused { condition } => status.declaration_refused.clone_from(condition),
            Self::Running { convoy_ref, observed_at } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.running_since = Some(*observed_at);
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
            Self::BackingOff { retry_at, failure } => {
                status.convoy_ref = None;
                status.restart_count = status.restart_count.saturating_add(1);
                status.running_since = None;
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
                status.hold_reason = None;
                status.retry = Some(ControllerRetry::retryable_at(status.retry.as_ref(), Utc::now(), *retry_at));
            }
            Self::Retrying { retry_at, failure } => {
                status.running_since = None;
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
                status.hold_reason = None;
                status.retry = Some(ControllerRetry::retryable_at(status.retry.as_ref(), Utc::now(), *retry_at));
            }
            Self::Holding { convoy_ref, failure } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.running_since = None;
                status.retry_at = None;
                status.last_failure = Some(failure.clone());
                status.hold_reason = Some(ConvoyEnsureHoldReason::BackingUnverified);
                status.retry = Some(ControllerRetry::terminal(status.retry.as_ref(), Utc::now(), failure.clone()));
            }
            Self::RestartLimitReached { convoy_ref, failure } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.restart_count = status.restart_count.saturating_add(1);
                status.running_since = None;
                status.retry_at = None;
                status.last_failure = Some(failure.clone());
                status.hold_reason = Some(ConvoyEnsureHoldReason::RestartLimit);
                status.retry = Some(ControllerRetry::terminal(status.retry.as_ref(), Utc::now(), failure.clone()));
            }
            Self::ObserveConfig { config_hash, changed } => {
                status.observed_config_hash = Some(config_hash.clone());
                if *changed {
                    status.restart_count = 0;
                    status.retry_at = None;
                    status.last_failure = None;
                    status.hold_reason = None;
                    status.retry = None;
                }
            }
            Self::BackoffState { strikes, retry_at, failure } => {
                status.restart_count = *strikes;
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
                status.hold_reason = None;
                status.retry = Some(ControllerRetry::retryable_at(status.retry.as_ref(), Utc::now(), *retry_at));
            }
            Self::ResetBackoff => {
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
            Self::DriverManaged => {
                status.convoy_ref = None;
                status.running_since = None;
                // Keep the observed hash: driver changes must not erase drift evidence.
                status.retry = None;
            }
            Self::DriverAdmission { condition } => {
                status.conditions.retain(|existing| existing.condition_type != DRIVER_ADMISSION_CONDITION_TYPE);
                if let Some(condition) = condition {
                    status.conditions.push(condition.clone());
                }
            }
        }
        let now = Utc::now();
        let reason = status.retry.as_ref().and_then(|retry| retry.stall_reason(now, RetryCeiling::default()));
        status.stalled = reason.map(|evidence| StalledCondition {
            leaves: Vec::new(),
            maker: status.retry.clone().map(|retry| LeafMaker::Controller {
                resource_kind: "ConvoyEnsure".into(),
                name: None,
                retry,
                ceiling: RetryCeiling::default(),
            }),
            evidence,
            source: StallEvidenceSource::LeafEngine,
            cause: None,
            began_at: status.stalled.as_ref().map_or(now, |stalled| stalled.began_at),
            rung: StallRung::Operator,
            supervisor: None,
            supervision_index: None,
            supervision_exhausted: false,
            reason: None,
            proposed_disposition: None,
            nudge_history: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    #[test]
    fn operator_ensure_retry_rows_stall_on_terminal_failure_or_ceiling() {
        let now = Utc::now();
        let mut status = ConvoyEnsureStatus::default();
        ConvoyEnsureStatusPatch::Retrying { retry_at: now + chrono::Duration::seconds(30), failure: "provider unavailable".into() }
            .apply(&mut status);
        assert!(status.retry.is_some());
        assert!(status.stalled.is_none(), "a transient controller failure keeps an able maker");

        ConvoyEnsureStatusPatch::Holding { convoy_ref: "convoy-a".into(), failure: "operator must verify backing".into() }
            .apply(&mut status);
        assert_eq!(status.stalled.as_ref().map(|stalled| stalled.evidence.as_str()), Some("operator must verify backing"));
        assert_eq!(status.stalled.as_ref().map(|stalled| stalled.rung), Some(StallRung::Operator));

        ConvoyEnsureStatusPatch::ResetBackoff.apply(&mut status);
        assert!(status.stalled.is_none());
        for attempt in 0..RetryCeiling::default().attempts {
            ConvoyEnsureStatusPatch::Retrying {
                retry_at: now + chrono::Duration::seconds(30 + attempt as i64),
                failure: "provider unavailable".into(),
            }
            .apply(&mut status);
        }
        assert_eq!(status.stalled.as_ref().map(|stalled| stalled.evidence.as_str()), Some("retrying without progress"));
        ConvoyEnsureStatusPatch::ResetBackoff.apply(&mut status);
        assert!(status.retry.is_none() && status.stalled.is_none(), "explicit reconcile-now clears the retry episode");
    }

    #[test]
    fn serialized_backoff_uses_operator_vocabulary() {
        let value = serde_json::to_value(ConvoyEnsureStatus {
            restart_count: 3,
            retry_at: Some(Utc.with_ymd_and_hms(2026, 8, 25, 21, 30, 0).single().expect("timestamp")),
            ..Default::default()
        })
        .expect("serialize status");

        assert_eq!(value["strikes"], 3);
        assert_eq!(value["next_attempt"], "2026-08-25T21:30:00Z");
        assert!(value.get("restart_count").is_none());
        assert!(value.get("retry_at").is_none());
    }

    #[test]
    fn agent_overrides_are_optional_and_empty_values_serialize_away() {
        let spec: ConvoyEnsureSpec = serde_json::from_value(json!({
            "project_ref": "flotilla",
            "role": "governor",
            "workflow_ref": "govern",
            "repositories": []
        }))
        .expect("decode ensure without agent overrides");

        assert!(spec.agent_overrides.is_empty());
        assert!(serde_json::to_value(spec).expect("serialize ensure").get("agent_overrides").is_none());
    }
}
