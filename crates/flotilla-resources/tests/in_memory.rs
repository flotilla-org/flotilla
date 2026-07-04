mod common;

use std::collections::BTreeMap;

use common::{
    contract::{
        assert_create_get_list_roundtrip, assert_delete_emits_event, assert_metadata_roundtrip, assert_namespace_isolation,
        assert_stale_resource_version_conflicts, assert_watch_from_version_replays, assert_watch_now_semantics, ConvoyFixture,
    },
    convoy_meta, convoy_spec,
};
use flotilla_resources::{Convoy, InMemoryBackend, InputMeta, ResourceBackend};
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
async fn namespaces_are_isolated(#[case] _fixture: ConvoyFixture) {
    assert_namespace_isolation::<ConvoyFixture>().await;
}

#[rstest]
#[case(ConvoyFixture)]
#[tokio::test]
async fn owner_references_roundtrip_through_in_memory_backend(#[case] _fixture: ConvoyFixture) {
    assert_metadata_roundtrip::<ConvoyFixture>().await;
}

#[tokio::test]
async fn delete_with_finalizers_marks_object_for_deletion_instead_of_removing_it() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let resolver = backend.clone().using::<Convoy>("default");
    resolver
        .create(
            &InputMeta::builder().name("alpha".to_string()).finalizers(vec!["flotilla.work/test-finalizer".to_string()]).build(),
            &convoy_spec("template-a"),
        )
        .await
        .expect("create should succeed");

    resolver.delete("alpha").await.expect("delete should succeed");

    let object = resolver.get("alpha").await.expect("object should remain until finalizers are removed");
    assert_eq!(object.metadata.finalizers, vec!["flotilla.work/test-finalizer".to_string()]);
    assert!(object.metadata.deletion_timestamp.is_some(), "delete should set deletion timestamp");
}

#[tokio::test]
async fn list_matching_labels_returns_only_exact_matches() {
    let resolver = resolver("flotilla");

    let mut alpha_meta = convoy_meta("alpha");
    alpha_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    alpha_meta.labels.insert("flotilla.work/task".to_string(), "implement".to_string());
    resolver.create(&alpha_meta, &convoy_spec("template-a")).await.expect("alpha create should succeed");

    let mut beta_meta = convoy_meta("beta");
    beta_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    resolver.create(&beta_meta, &convoy_spec("template-b")).await.expect("beta create should succeed");

    let mut gamma_meta = convoy_meta("gamma");
    gamma_meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-b".to_string());
    gamma_meta.labels.insert("flotilla.work/task".to_string(), "implement".to_string());
    resolver.create(&gamma_meta, &convoy_spec("template-c")).await.expect("gamma create should succeed");

    let selector = BTreeMap::from([
        ("flotilla.work/convoy".to_string(), "convoy-a".to_string()),
        ("flotilla.work/task".to_string(), "implement".to_string()),
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
