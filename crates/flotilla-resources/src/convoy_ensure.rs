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
    pub retry_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEnsureStatusPatch {
    Running { convoy_ref: String },
    BackingOff { retry_at: DateTime<Utc>, failure: String },
}

impl StatusPatch<ConvoyEnsureStatus> for ConvoyEnsureStatusPatch {
    fn apply(&self, status: &mut ConvoyEnsureStatus) {
        match self {
            Self::Running { convoy_ref } => {
                status.convoy_ref = Some(convoy_ref.clone());
                status.retry_at = None;
                status.last_failure = None;
            }
            Self::BackingOff { retry_at, failure } => {
                status.convoy_ref = None;
                status.restart_count = status.restart_count.saturating_add(1);
                status.retry_at = Some(*retry_at);
                status.last_failure = Some(failure.clone());
            }
        }
    }
}
