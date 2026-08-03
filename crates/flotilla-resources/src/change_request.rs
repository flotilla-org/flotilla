use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRequest;

impl Resource for ChangeRequest {
    type Spec = ChangeRequestSpec;
    type Status = ChangeRequestStatus;
    type StatusPatch = ChangeRequestStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "changerequests", kind: "ChangeRequest" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Observations;

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("ChangeRequest subject and observing authority are immutable"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ChangeRequestSpec {
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub observing_authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation<T> {
    /// `None` is the structural Unknown value.
    pub value: Option<T>,
    pub observed_at: DateTime<Utc>,
}

impl<T> Observation<T> {
    pub fn known(value: T, observed_at: DateTime<Utc>) -> Self {
        Self { value: Some(value), observed_at }
    }

    pub fn unknown(observed_at: DateTime<Utc>) -> Self {
        Self { value: None, observed_at }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedChangeRequestState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedChecks {
    Pass,
    Fail,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedMergeability {
    Mergeable,
    Conflicting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestReviewObservation {
    pub actionable_at_head: Observation<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestStatus {
    pub state: Observation<ObservedChangeRequestState>,
    pub head_sha: Observation<String>,
    pub checks: Observation<ObservedChecks>,
    pub review: ChangeRequestReviewObservation,
    pub mergeable: Observation<ObservedMergeability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequestStatusPatch {
    Observed(ChangeRequestStatus),
}

impl StatusPatch<ChangeRequestStatus> for ChangeRequestStatusPatch {
    fn apply(&self, status: &mut ChangeRequestStatus) {
        match self {
            Self::Observed(observation) => *status = observation.clone(),
        }
    }
}

pub fn change_request_record_name(service: &str, scope: &str, number: u64) -> String {
    fn hex(value: &str) -> String {
        value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
    }
    format!("cr-{}-{}-{number}", hex(service), hex(scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryBackend, InputMeta, ResourceBackend, SqliteBackend};

    fn status(state: ObservedChangeRequestState, observed_at: DateTime<Utc>) -> ChangeRequestStatus {
        ChangeRequestStatus {
            state: Observation::known(state, observed_at),
            head_sha: Observation::known("abc".to_string(), observed_at),
            checks: Observation::known(ObservedChecks::Pass, observed_at),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
            mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
        }
    }

    async fn assert_observation_history_is_thin(backend: ResourceBackend) {
        let records = backend.using::<ChangeRequest>("flotilla");
        let spec = ChangeRequestSpec::builder()
            .service("github.com".to_string())
            .scope("flotilla-org/flotilla".to_string())
            .number(1366)
            .observing_authority("authority".to_string())
            .build();
        let created = records.create(&InputMeta::builder().name("cr".to_string()).build(), &spec).await.expect("create observation");
        let opened = records
            .update_status(
                "cr",
                &created.metadata.resource_version,
                &status(ObservedChangeRequestState::Open, "2026-08-03T20:00:00Z".parse().expect("time")),
            )
            .await
            .expect("publish open observation");
        records
            .update_status(
                "cr",
                &opened.metadata.resource_version,
                &status(ObservedChangeRequestState::Merged, "2026-08-03T20:01:00Z".parse().expect("time")),
            )
            .await
            .expect("publish merged observation");

        let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded store diagnostics");
        assert_eq!(diagnostics.event_count, 1, "observations retain only the latest watch handoff event");
    }

    async fn assert_local_namespace_enumeration(backend: ResourceBackend) {
        let spec = ChangeRequestSpec::builder()
            .service("github.com".to_string())
            .scope("flotilla-org/flotilla".to_string())
            .number(1366)
            .observing_authority("authority".to_string())
            .build();
        for namespace in ["flotilla", "ops"] {
            backend
                .using::<ChangeRequest>(namespace)
                .create(&InputMeta::builder().name("cr".to_string()).build(), &spec)
                .await
                .expect("create namespaced observation");
        }
        assert_eq!(backend.local_namespaces::<ChangeRequest>().await.expect("local namespaces"), vec!["flotilla", "ops"]);
    }

    #[tokio::test]
    async fn in_memory_observation_history_is_thin() {
        assert_observation_history_is_thin(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_observation_history_is_thin() {
        assert_observation_history_is_thin(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend"))).await;
    }

    #[tokio::test]
    async fn in_memory_enumerates_local_observation_namespaces() {
        assert_local_namespace_enumeration(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_enumerates_local_observation_namespaces() {
        assert_local_namespace_enumeration(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend"))).await;
    }
}
