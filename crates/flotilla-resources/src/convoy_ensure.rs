use chrono::{DateTime, Utc};
use flotilla_protocol::AgentOverride;
use serde::{Deserialize, Serialize};

use crate::{
    checkout::ConditionValue, resource::define_resource, status_patch::NoStatusPatch, ControllerRetry, ReplicationClass, RepositoryKey,
    StalledCondition,
};

// No status is written: a standing role's state is derived from its generations (#3108).
define_resource!(
    ConvoyEnsure,
    "convoyensures",
    ConvoyEnsureSpec,
    ConvoyEnsureStatus,
    NoStatusPatch,
    replication = ReplicationClass::Definitions,
    queryable = &["metadata.name", "spec.project_ref", "spec.role", "metadata.annotations.\"flotilla.work/materialized-project\""]
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
    /// A FulfilmentKind's name.
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

/// How `declared` differs from the configuration that admitted a role's current generation, one
/// line per change; `admitted` is `None` when that generation recorded none. Empty when they agree.
/// A role's drift is derived from its generation, never held (#3108).
pub fn config_changes(declared: &ConvoyEnsureSpec, admitted: Option<&ConvoyEnsureSpec>) -> Vec<String> {
    let Some(old) = admitted else {
        // Never infer a previous generation's admission baseline from the
        // latest observed declaration: it may already have silently drifted.
        return vec!["admission configuration unknown; explicitly roll to establish a baseline".into()];
    };
    let mut changes = Vec::new();
    for repo in declared.repositories.iter().filter(|repo| !old.repositories.contains(repo)) {
        changes.push(format!("repository added: {repo}"));
    }
    for repo in old.repositories.iter().filter(|repo| !declared.repositories.contains(repo)) {
        changes.push(format!("repository removed: {repo}"));
    }
    if old.repositories != declared.repositories && changes.is_empty() {
        changes.push("repository order changed".into());
    }
    if old.workflow_ref != declared.workflow_ref {
        changes.push(format!("workflow: {} -> {}", old.workflow_ref, declared.workflow_ref));
    }
    if old.placement_policy != declared.placement_policy {
        changes.push(format!("placement: {:?} -> {:?}", old.placement_policy, declared.placement_policy));
    }
    if old.agent_overrides != declared.agent_overrides {
        changes.push(format!("agents: {:?} -> {:?}", old.agent_overrides, declared.agent_overrides));
    }
    if old.driver_ref != declared.driver_ref {
        changes.push(format!("driver: {:?} -> {:?}", old.driver_ref, declared.driver_ref));
    }
    if old.presents_as != declared.presents_as {
        changes.push(format!("presentation: {:?} -> {:?}", old.presents_as, declared.presents_as));
    }
    if old.escalation_reason != declared.escalation_reason {
        changes.push("placement escalation reason changed".into());
    }
    if old.project_ref != declared.project_ref {
        changes.push(format!("project: {} -> {}", old.project_ref, declared.project_ref));
    }
    if old.role != declared.role {
        changes.push(format!("role: {} -> {}", old.role, declared.role));
    }
    changes
}

/// The previous generation's stored status: the first-start import reads it and drops it, as a
/// role's state is derived from its generations (#3108). Remove one roll after that import.
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
    /// Consecutive failed generations in the current strike run, as the last pass derived them
    /// from the role's generations; it is shown, and no decision reads it.
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    #[test]
    fn the_previous_generations_status_decodes_its_operator_vocabulary() {
        let status: ConvoyEnsureStatus =
            serde_json::from_value(json!({ "strikes": 3, "next_attempt": "2026-08-25T21:30:00Z", "hold_reason": "restart_limit" }))
                .expect("decode status");

        assert_eq!(status.restart_count, 3);
        assert_eq!(status.retry_at, Some(Utc.with_ymd_and_hms(2026, 8, 25, 21, 30, 0).single().expect("timestamp")));
        assert_eq!(status.hold_reason, Some(ConvoyEnsureHoldReason::RestartLimit));
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
