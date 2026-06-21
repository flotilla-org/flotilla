mod common;

use common::{
    contract::{
        assert_create_get_list_roundtrip_with_backend, assert_delete_emits_event_with_backend, assert_metadata_roundtrip_with_backend,
        assert_namespace_isolation_with_backend, assert_stale_resource_version_conflicts_with_backend,
        assert_watch_from_version_replays_with_backend, assert_watch_now_semantics_with_backend, ConvoyFixture,
    },
    convoy_meta, convoy_spec,
};
use flotilla_resources::{Convoy, ResourceBackend, SqliteBackend, WatchEvent, WatchStart};
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
