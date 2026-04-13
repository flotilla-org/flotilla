mod common;

use std::time::Duration;

use common::{input_meta, spec, status, ConvoyResource};
use flotilla_resources::{InMemoryBackend, ResourceBackend, WatchEvent, WatchStart};
use futures::StreamExt;
use tokio::time::timeout;

fn resolver(namespace: &str) -> flotilla_resources::TypedResolver<ConvoyResource> {
    ResourceBackend::InMemory(InMemoryBackend::default()).using::<ConvoyResource>(namespace)
}

#[tokio::test]
async fn create_get_list_roundtrip() {
    let resolver = resolver("flotilla");
    let created = resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");
    assert_eq!(created.metadata.name, "alpha");
    assert_eq!(created.metadata.namespace, "flotilla");
    assert!(!created.metadata.resource_version.is_empty());
    assert_eq!(created.spec.template, "template-a");
    assert!(created.status.is_none());

    let fetched = resolver.get("alpha").await.expect("get should succeed");
    assert_eq!(fetched.metadata.resource_version, created.metadata.resource_version);
    assert_eq!(fetched.spec.template, "template-a");

    let listed = resolver.list().await.expect("list should succeed");
    assert_eq!(listed.resource_version, created.metadata.resource_version);
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
}

#[tokio::test]
async fn update_requires_current_resource_version() {
    let resolver = resolver("flotilla");
    let created = resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");

    let conflict = resolver.update(&input_meta("alpha"), "0", &spec("template-b")).await.err().expect("stale version should conflict");
    match conflict {
        flotilla_resources::ResourceError::Conflict { .. } => {}
        other => panic!("expected conflict, got {other}"),
    }

    let updated = resolver
        .update(&input_meta("alpha"), &created.metadata.resource_version, &spec("template-b"))
        .await
        .expect("update should succeed");
    assert_eq!(updated.metadata.resource_version, "2");
    assert_eq!(updated.spec.template, "template-b");
    assert_eq!(updated.metadata.labels.get("app").expect("label"), "flotilla");
}

#[tokio::test]
async fn update_status_does_not_require_or_change_input_meta() {
    let resolver = resolver("flotilla");
    let created = resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");
    let updated = resolver
        .update_status("alpha", &created.metadata.resource_version, &status("Running"))
        .await
        .expect("status update should succeed");

    assert_eq!(updated.metadata.resource_version, "2");
    assert_eq!(updated.metadata.labels.get("app").expect("label"), "flotilla");
    assert_eq!(updated.status.expect("status").phase, "Running");
}

#[tokio::test]
async fn delete_emits_deleted_event() {
    let resolver = resolver("flotilla");
    let created = resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");
    let mut watch = resolver.watch(WatchStart::FromVersion(created.metadata.resource_version.clone())).await.expect("watch should succeed");

    resolver.delete("alpha").await.expect("delete should succeed");
    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce event")
        .expect("stream should yield item")
        .expect("event should decode");

    match event {
        WatchEvent::Deleted(object) => {
            assert_eq!(object.metadata.name, "alpha");
            assert_eq!(object.metadata.resource_version, "2");
        }
        _ => panic!("expected deleted event"),
    }
}

#[tokio::test]
async fn watch_from_version_replays_gaplessly_after_list() {
    let resolver = resolver("flotilla");
    resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");
    let listed = resolver.list().await.expect("list should succeed");
    let mut watch = resolver.watch(WatchStart::FromVersion(listed.resource_version.clone())).await.expect("watch should succeed");

    resolver
        .update_status("alpha", &listed.items[0].metadata.resource_version, &status("Running"))
        .await
        .expect("status update should succeed");
    let modified = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce modified event")
        .expect("stream should yield item")
        .expect("event should decode");
    match modified {
        WatchEvent::Modified(object) => assert_eq!(object.status.expect("status").phase, "Running"),
        _ => panic!("expected modified event"),
    }

    let latest = resolver.get("alpha").await.expect("get should succeed");
    resolver.delete("alpha").await.expect("delete should succeed");
    let deleted = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce deleted event")
        .expect("stream should yield item")
        .expect("event should decode");
    match deleted {
        WatchEvent::Deleted(object) => assert_eq!(object.metadata.resource_version, "3"),
        _ => panic!("expected deleted event"),
    }

    assert_eq!(latest.metadata.resource_version, "2");
}

#[tokio::test]
async fn watch_now_only_sees_future_events() {
    let resolver = resolver("flotilla");
    resolver.create(&input_meta("alpha"), &spec("template-a")).await.expect("create should succeed");

    let mut watch = resolver.watch(WatchStart::Now).await.expect("watch should succeed");
    assert!(timeout(Duration::from_millis(100), watch.next()).await.is_err(), "watch-now should not replay existing state");

    let current = resolver.get("alpha").await.expect("get should succeed");
    resolver.update_status("alpha", &current.metadata.resource_version, &status("Running")).await.expect("status update should succeed");
    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce future event")
        .expect("stream should yield item")
        .expect("event should decode");
    match event {
        WatchEvent::Modified(object) => assert_eq!(object.status.expect("status").phase, "Running"),
        _ => panic!("expected modified event"),
    }
}

#[tokio::test]
async fn namespaces_are_isolated() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let alpha = backend.using::<ConvoyResource>("alpha");
    let beta = backend.using::<ConvoyResource>("beta");

    alpha.create(&input_meta("shared"), &spec("template-a")).await.expect("alpha create should succeed");
    beta.create(&input_meta("shared"), &spec("template-b")).await.expect("beta create should succeed");

    let alpha_item = alpha.get("shared").await.expect("alpha get should succeed");
    let beta_item = beta.get("shared").await.expect("beta get should succeed");
    assert_eq!(alpha_item.metadata.namespace, "alpha");
    assert_eq!(beta_item.metadata.namespace, "beta");
    assert_eq!(alpha_item.spec.template, "template-a");
    assert_eq!(beta_item.spec.template, "template-b");
}
