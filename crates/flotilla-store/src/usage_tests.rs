use chrono::{DateTime, Utc};

use crate::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryBackend, InputMeta, ResourceBackend, SqliteBackend};

    fn status(used_percent: f64, observed_at: DateTime<Utc>) -> UsageStatus {
        UsageStatus::builder()
            .windows(vec![UsageWindow::builder().name("weekly").used_percent(used_percent).build()])
            .observed_at(observed_at)
            .build()
    }

    async fn assert_observation_history_is_thin(backend: ResourceBackend) {
        let records = backend.using::<Usage>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name("usage-account".to_string()).build(),
                &UsageSpec { provider: "codex".to_string(), account: "user@example.com".to_string() },
            )
            .await
            .expect("create usage observation");
        let first = records
            .update_status("usage-account", &created.metadata.resource_version, &status(8.0, "2026-08-06T10:00:00Z".parse().expect("time")))
            .await
            .expect("publish first usage observation");
        records
            .update_status("usage-account", &first.metadata.resource_version, &status(100.0, "2026-08-06T10:05:00Z".parse().expect("time")))
            .await
            .expect("publish second usage observation");

        let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded store diagnostics");
        assert_eq!(diagnostics.event_count, 1, "usage observations retain only the latest watch handoff event");
    }

    async fn assert_observations_are_monotonically_ordered(backend: ResourceBackend) {
        let records = backend.using::<Usage>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name("usage-account".to_string()).build(),
                &UsageSpec { provider: "codex".to_string(), account: "user@example.com".to_string() },
            )
            .await
            .expect("create usage observation");
        let older = status(8.0, "2026-08-06T10:00:00Z".parse().expect("time"));
        let newer = status(12.0, "2026-08-06T10:05:00Z".parse().expect("time"));
        let stored =
            records.update_status("usage-account", &created.metadata.resource_version, &newer).await.expect("publish newer observation");

        let racing_write = records.update_status("usage-account", &created.metadata.resource_version, &older).await;
        assert!(matches!(racing_write, Err(ResourceError::Conflict { .. })));
        let retried_write = records.update_status("usage-account", &stored.metadata.resource_version, &older).await;
        assert!(matches!(retried_write, Err(ResourceError::Invalid { .. })));

        let current = records.get("usage-account").await.expect("read current usage observation");
        assert_eq!(current.status, Some(newer));
    }

    async fn assert_equal_observation_times_are_idempotent_only(backend: ResourceBackend) {
        let records = backend.using::<Usage>("flotilla");
        let created = records
            .create(
                &InputMeta::builder().name("usage-account".to_string()).build(),
                &UsageSpec { provider: "codex".to_string(), account: "user@example.com".to_string() },
            )
            .await
            .expect("create usage observation");
        let observed_at = "2026-08-06T10:00:00Z".parse().expect("time");
        let first = status(8.0, observed_at);
        let stored =
            records.update_status("usage-account", &created.metadata.resource_version, &first).await.expect("publish first observation");

        let repeated =
            records.update_status("usage-account", &stored.metadata.resource_version, &first).await.expect("repeat identical observation");
        assert_eq!(repeated.metadata.resource_version, stored.metadata.resource_version);

        let divergent = records.update_status("usage-account", &stored.metadata.resource_version, &status(9.0, observed_at)).await;
        assert!(matches!(divergent, Err(ResourceError::Invalid { .. })));
    }

    #[test]
    fn provider_account_record_names_are_stable_case_insensitive_and_bounded() {
        let lower = usage_record_name("codex", "user@example.com");
        assert_eq!(lower, usage_record_name(" CODEX ", " User@Example.COM "));
        assert!(lower.starts_with("usage-"));
        assert_eq!(lower.len(), 60);
    }

    #[test]
    fn same_account_at_different_providers_has_distinct_record_names() {
        assert_ne!(usage_record_name("codex", "user@example.com"), usage_record_name("claude", "user@example.com"));
    }

    #[test]
    fn provider_and_account_are_one_immutable_subject() {
        let current = UsageSpec { provider: "codex".to_string(), account: "user@example.com".to_string() };
        assert!(Usage::validate_spec_update(&current, &current).is_ok());
        assert!(Usage::validate_spec_update(&current, &UsageSpec { provider: "claude".to_string(), ..current.clone() }).is_err());
        assert!(Usage::validate_spec_update(&current, &UsageSpec { account: "other@example.com".to_string(), ..current.clone() }).is_err());
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
    async fn in_memory_observations_are_monotonically_ordered() {
        assert_observations_are_monotonically_ordered(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_observations_are_monotonically_ordered() {
        assert_observations_are_monotonically_ordered(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend")))
            .await;
    }

    #[tokio::test]
    async fn in_memory_equal_observation_times_are_idempotent_only() {
        assert_equal_observation_times_are_idempotent_only(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    }

    #[tokio::test]
    async fn sqlite_equal_observation_times_are_idempotent_only() {
        assert_equal_observation_times_are_idempotent_only(ResourceBackend::Sqlite(
            SqliteBackend::open_in_memory().expect("sqlite backend"),
        ))
        .await;
    }
}
