mod common;

use std::collections::BTreeMap;

use common::{
    contract::{
        assert_consumer_relists_after_expired_watch_and_converges_with_backend, assert_create_get_list_roundtrip,
        assert_delete_emits_event, assert_identical_status_update_is_noop_with_backend, assert_identical_update_is_noop_with_backend,
        assert_metadata_roundtrip, assert_namespace_isolation, assert_repeated_delete_with_pending_finalizers_is_noop_with_backend,
        assert_stale_resource_version_conflicts, assert_store_diagnostics_report_retained_events_with_backend,
        assert_watch_from_version_replays, assert_watch_now_semantics,
        assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend,
        assert_watch_retention_expires_only_versions_below_floor_with_backend, ConvoyFixture,
    },
    convoy_meta, convoy_spec,
};
use flotilla_resources::{Convoy, EventRetention, InMemoryBackend, ResourceBackend};
use rstest::rstest;

fn resolver(namespace: &str) -> flotilla_resources::TypedResolver<Convoy> {
    ResourceBackend::InMemory(InMemoryBackend::default()).using::<Convoy>(namespace)
}

// Keep the rstest shape even with a single fixture so this suite can grow into
// shared backend contract coverage without restructuring each test.
#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn create_get_list_roundtrip(#[case] _fixture: ConvoyFixture) {
    assert_create_get_list_roundtrip::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn update_requires_current_resource_version(#[case] _fixture: ConvoyFixture) {
    assert_stale_resource_version_conflicts::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_update_is_noop_with_backend::<ConvoyFixture>(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn identical_status_update_preserves_resource_version_and_emits_no_event(#[case] _fixture: ConvoyFixture) {
    assert_identical_status_update_is_noop_with_backend::<ConvoyFixture>(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn delete_emits_deleted_event(#[case] _fixture: ConvoyFixture) {
    assert_delete_emits_event::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_from_version_replays_gaplessly_after_list(#[case] _fixture: ConvoyFixture) {
    assert_watch_from_version_replays::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_now_only_sees_future_events(#[case] _fixture: ConvoyFixture) {
    assert_watch_now_semantics::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_below_retention_floor_expires(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
    assert_watch_retention_expires_only_versions_below_floor_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn expired_watch_consumer_relists_and_converges(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
    assert_consumer_relists_after_expired_watch_and_converges_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn diagnostics_report_bounded_event_log(#[case] _fixture: ConvoyFixture) {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::InMemory(InMemoryBackend::with_event_retention(retention));
    assert_store_diagnostics_report_retained_events_with_backend::<ConvoyFixture>(backend).await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn watch_only_diagnostics_match_mutation_based_stream_semantics(#[case] _fixture: ConvoyFixture) {
    assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend::<ConvoyFixture>(ResourceBackend::InMemory(
        InMemoryBackend::default(),
    ))
    .await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn namespaces_are_isolated(#[case] _fixture: ConvoyFixture) {
    assert_namespace_isolation::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn owner_references_roundtrip_through_in_memory_backend(#[case] _fixture: ConvoyFixture) {
    assert_metadata_roundtrip::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn repeated_delete_is_noop_while_finalizers_are_pending(#[case] _fixture: ConvoyFixture) {
    assert_repeated_delete_with_pending_finalizers_is_noop_with_backend::<ConvoyFixture>(ResourceBackend::InMemory(
        InMemoryBackend::default(),
    ))
    .await;
}

#[tokio::test]
async fn list_matching_labels_returns_only_exact_matches() {
    let resolver = resolver("flotilla");

    let mut alpha_meta = convoy_meta("alpha");
    alpha_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    alpha_meta.labels.insert("flotilla.work/vessel".to_string(), "implement".to_string());
    resolver.create(&alpha_meta, &convoy_spec("template-a")).await.expect("alpha create should succeed");

    let mut beta_meta = convoy_meta("beta");
    beta_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    resolver.create(&beta_meta, &convoy_spec("template-b")).await.expect("beta create should succeed");

    let mut gamma_meta = convoy_meta("gamma");
    gamma_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-b".to_string());
    gamma_meta.labels.insert("flotilla.work/vessel".to_string(), "implement".to_string());
    resolver.create(&gamma_meta, &convoy_spec("template-c")).await.expect("gamma create should succeed");

    let selector = BTreeMap::from([
        ("flotilla.work/convoy".to_string(), "convoy-a".to_string()),
        ("flotilla.work/vessel".to_string(), "implement".to_string()),
    ]);

    let listed = resolver.list_matching_labels(&selector).await.expect("filtered list should succeed");

    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
}

#[tokio::test]
async fn observed_backend_surfaces_generation_on_list_and_watch() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let resolver = backend.using::<Convoy>("flotilla");
    resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create should succeed");

    let listed = resolver.list().await.expect("list should succeed");
    let generation = listed.generation.clone().expect("observed list should expose generation");
    let watch = resolver
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration {
            generation: generation.clone(),
            resource_version: listed.resource_version.clone(),
        })
        .await
        .expect("watch should start within listed generation");

    assert_eq!(watch.generation(), Some(generation.as_str()));
}

#[tokio::test]
async fn observed_backend_rejects_watch_resume_from_previous_generation() {
    let first_backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let first = first_backend.using::<Convoy>("flotilla");
    let first_list = first.list().await.expect("first list should succeed");
    let stale_generation = first_list.generation.expect("observed list should expose generation");

    let restarted_backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let restarted = restarted_backend.using::<Convoy>("flotilla");
    let restarted_generation =
        restarted.list().await.expect("restarted list should succeed").generation.expect("observed list should expose generation");
    assert_ne!(restarted_generation, stale_generation, "restart should mint a new observed generation");

    let err = restarted
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration { generation: stale_generation, resource_version: "0".to_string() })
        .await
        .expect_err("watch resume from a previous generation should fail");

    assert!(matches!(err, flotilla_resources::ResourceError::Invalid { .. }));
}

#[tokio::test]
async fn observed_backend_rejects_bare_watch_resume_without_generation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed());
    let resolver = backend.using::<Convoy>("flotilla");
    let listed = resolver.list().await.expect("list should succeed");

    let err = resolver
        .watch(flotilla_resources::WatchStart::FromVersion(listed.resource_version))
        .await
        .expect_err("observed watch resume should require generation");

    assert!(matches!(err, flotilla_resources::ResourceError::Invalid { .. }));
}

#[tokio::test]
async fn observed_backend_expires_compacted_version_within_current_generation() {
    let retention = EventRetention::new(2).expect("valid retention");
    let backend = ResourceBackend::InMemory(InMemoryBackend::observed_with_event_retention(retention));
    let resolver = backend.using::<Convoy>("flotilla");
    let created = resolver.create(&convoy_meta("alpha"), &convoy_spec("template-a")).await.expect("create");
    let second =
        resolver.update(&convoy_meta("alpha"), &created.metadata.resource_version, &convoy_spec("template-b")).await.expect("first update");
    let third =
        resolver.update(&convoy_meta("alpha"), &second.metadata.resource_version, &convoy_spec("template-c")).await.expect("second update");
    resolver.update(&convoy_meta("alpha"), &third.metadata.resource_version, &convoy_spec("template-d")).await.expect("third update");
    let generation = resolver.list().await.expect("list").generation.expect("observed generation");

    let err = resolver
        .watch(flotilla_resources::WatchStart::FromVersionInGeneration {
            generation,
            resource_version: created.metadata.resource_version.clone(),
        })
        .await
        .expect_err("compacted version should expire");

    assert_eq!(err, flotilla_resources::ResourceError::WatchExpired {
        requested_version: created.metadata.resource_version,
        compacted_through: Some(second.metadata.resource_version),
    });
}
