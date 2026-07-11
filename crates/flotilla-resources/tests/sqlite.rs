mod common;

use common::{
    contract::{
        assert_consumer_relists_after_expired_watch_and_converges_with_backend, assert_create_get_list_roundtrip_with_backend,
        assert_delete_emits_event_with_backend, assert_identical_status_update_is_noop_with_backend,
        assert_identical_update_is_noop_with_backend, assert_metadata_roundtrip_with_backend, assert_namespace_isolation_with_backend,
        assert_stale_resource_version_conflicts_with_backend, assert_store_diagnostics_report_retained_events_with_backend,
        assert_watch_from_version_replays_with_backend, assert_watch_now_semantics_with_backend,
        assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend,
        assert_watch_retention_expires_only_versions_below_floor_with_backend, ConvoyFixture,
    },
    convoy_meta, convoy_spec,
};
use flotilla_resources::{Convoy, EventRetention, ResourceBackend, SqliteBackend, WatchEvent, WatchStart};
use futures::StreamExt;
use rstest::rstest;
use tempfile::tempdir;
use tokio::time::{timeout, Duration};

fn backend() -> ResourceBackend {
    ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend should open"))
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn create_get_list_roundtrip(#[case] _fixture: ConvoyFixture) {
    assert_create_get_list_roundtrip_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn update_requires_current_resource_version(#[case] _fixture: ConvoyFixture) {
    assert_stale_resource_version_conflicts_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_update_is_noop_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_status_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_status_update_is_noop_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn delete_emits_deleted_event(#[case] _fixture: ConvoyFixture) {
    assert_delete_emits_event_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_from_version_replays_gaplessly_after_list(#[case] _fixture: ConvoyFixture) {
    assert_watch_from_version_replays_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_now_only_sees_future_events(#[case] _fixture: ConvoyFixture) {
    assert_watch_now_semantics_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_below_retention_floor_expires(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_watch_retention_expires_only_versions_below_floor_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn expired_watch_consumer_relists_and_converges(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_consumer_relists_after_expired_watch_and_converges_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn diagnostics_report_bounded_event_log(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend =
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory_with_event_retention(retention).expect("sqlite backend should open"));
    assert_store_diagnostics_report_retained_events_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_only_diagnostics_match_mutation_based_stream_semantics(#[case] _fixture: ConvoyFixture) {
    assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn namespaces_are_isolated(#[case] _fixture: ConvoyFixture) {
    assert_namespace_isolation_with_backend::<ConvoyFixture>(backend()).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn owner_references_roundtrip_through_sqlite_backend(#[case] _fixture: ConvoyFixture) {
    assert_metadata_roundtrip_with_backend::<ConvoyFixture>(backend()).await;
}

#[tokio::test]
async fn objects_and_resource_versions_survive_restart() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");
    assert_eq!(created.metadata.resource_version, "1");
    drop(resolver);
    drop(backend);

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should reopen"));
    let resolver = backend.using::<Convoy>("flotilla");
    let fetched = resolver.get("alpha").await.expect("object should survive restart");
    assert_eq!(fetched.metadata.resource_version, "1");
    assert_eq!(fetched.spec.workflow_ref, "template-a");

    let updated = resolver
        .update(&convoy_meta("alpha"), &fetched.metadata.resource_version, &convoy_spec("template-b"))
        .await
        .expect("update should succeed after restart");
    assert_eq!(updated.metadata.resource_version, "2");
}

#[test]
fn file_backend_enables_wal_and_busy_timeout() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");
    let backend = SqliteBackend::open(&path).expect("sqlite backend should open");

    let connection = rusqlite::Connection::open(&path).expect("inspection connection should open");
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).expect("read journal mode");
    let busy_timeout_ms: u64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0)).expect("read busy timeout");

    assert_eq!(journal_mode, "wal");
    assert_eq!(busy_timeout_ms, 5_000);
    drop(backend);
}

#[tokio::test]
async fn watch_from_version_replays_events_persisted_before_restart() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");
    let updated = resolver
        .update(&convoy_meta("alpha"), &created.metadata.resource_version, &convoy_spec("template-b"))
        .await
        .expect("update should succeed");
    drop(resolver);
    drop(backend);

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should reopen"));
    let resolver = backend.using::<Convoy>("flotilla");
    let mut watch = resolver.watch(WatchStart::FromVersion(created.metadata.resource_version)).await.expect("watch should succeed");

    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should replay persisted event")
        .expect("stream should yield item")
        .expect("event should decode");
    match event {
        WatchEvent::Modified(object) => assert_eq!(object.metadata.resource_version, updated.metadata.resource_version),
        other => panic!("expected modified event, got {other:?}"),
    }
}

#[tokio::test]
async fn reopening_with_smaller_retention_compacts_existing_events_and_persists_floor() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resources.sqlite");

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&path).expect("sqlite backend should open"));
    let resolver = backend.using::<Convoy>("flotilla");
    let first = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create");
    let second =
        resolver.update(&convoy_meta("alpha"), &first.metadata.resource_version, &convoy_spec("template-b")).await.expect("first update");
    let third =
        resolver.update(&convoy_meta("alpha"), &second.metadata.resource_version, &convoy_spec("template-c")).await.expect("second update");
    resolver.update(&convoy_meta("alpha"), &third.metadata.resource_version, &convoy_spec("template-d")).await.expect("third update");
    drop(resolver);
    drop(backend);

    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::Sqlite(
        SqliteBackend::open_with_event_retention(&path, retention).expect("sqlite backend should reopen with smaller retention"),
    );
    let resolver = backend.using::<Convoy>("flotilla");
    let expired = resolver
        .watch(WatchStart::FromVersion(first.metadata.resource_version.clone()))
        .await
        .expect_err("startup compaction should expire old version");
    assert_eq!(expired, flotilla_resources::ResourceError::WatchExpired {
        requested_version: first.metadata.resource_version,
        compacted_through: second.metadata.resource_version,
    });

    drop(resolver);
    drop(backend);
    let backend = ResourceBackend::Sqlite(
        SqliteBackend::open_with_event_retention(&path, retention).expect("sqlite backend should reopen after compaction"),
    );
    let resolver = backend.using::<Convoy>("flotilla");
    assert!(matches!(
        resolver.watch(WatchStart::FromVersion("1".to_string())).await,
        Err(flotilla_resources::ResourceError::WatchExpired { compacted_through, .. }) if compacted_through == "2"
    ));
}
