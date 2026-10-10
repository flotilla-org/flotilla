use chrono::{DateTime, Utc};

use crate::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryBackend, InputMeta, ResourceBackend, SqliteBackend};

    // ADR 0047: a previous-generation spec decodes with no historical links.
    #[test]
    fn previous_generation_spec_decodes_without_history() {
        let spec: ChangeRequestSpec =
            serde_json::from_str(r#"{"service":"github.com","scope":"org/repo","number":42,"observing_authority":"node"}"#)
                .expect("previous spec");
        assert!(spec.subject_of.is_empty());
    }

    #[test]
    fn previous_generation_status_decodes_without_presentation_fields() {
        let stored = r#"{"state":{"value":"open","observed_at":"2026-08-03T20:00:00Z"},"head_sha":{"value":"abc","observed_at":"2026-08-03T20:00:00Z"},"checks":{"value":"pass","observed_at":"2026-08-03T20:00:00Z"},"review":{"actionable_at_head":{"value":false,"observed_at":"2026-08-03T20:00:00Z"}},"mergeable":{"value":"mergeable","observed_at":"2026-08-03T20:00:00Z"}}"#;
        let status: ChangeRequestStatus = serde_json::from_str(stored).expect("decode previous stored status");
        assert_eq!(status.title.value, None);
        assert_eq!(status.author.value, None);
        assert_eq!(status.review_decision.value, None);
        assert_eq!(status.review_requested_from_owner.value, None);
    }

    fn status(state: ObservedChangeRequestState, observed_at: DateTime<Utc>) -> ChangeRequestStatus {
        ChangeRequestStatus {
            title: Default::default(),
            author: Default::default(),
            review_decision: Default::default(),
            review_requested_from_owner: Default::default(),
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
