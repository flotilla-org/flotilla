use std::time::Duration;

use flotilla_resources::{
    Convoy, InMemoryBackend, InputMeta, OwnerReference, Resource, ResourceBackend, ResourceError, ResourceObject, TypedResolver,
    WatchEvent, WatchStart, WorkflowTemplate,
};
use futures::StreamExt;
use tokio::time::timeout;

use crate::common::{convoy_meta, convoy_spec, updated_workflow_template_spec, valid_workflow_template_spec, workflow_template_meta};

pub trait ResourceContractFixture {
    type Resource: Resource;

    fn label() -> &'static str;
    fn meta(name: &str) -> InputMeta;
    fn spec() -> <Self::Resource as Resource>::Spec;
    fn updated_spec() -> <Self::Resource as Resource>::Spec;
    fn assert_created(created: &ResourceObject<Self::Resource>);
    fn assert_updated(updated: &ResourceObject<Self::Resource>);
}

#[derive(Clone, Copy, Debug)]
pub struct ConvoyFixture;

impl ResourceContractFixture for ConvoyFixture {
    type Resource = Convoy;

    fn label() -> &'static str {
        "Convoy"
    }

    fn meta(name: &str) -> InputMeta {
        convoy_meta(name)
    }

    fn spec() -> <Self::Resource as Resource>::Spec {
        convoy_spec("template-a")
    }

    fn updated_spec() -> <Self::Resource as Resource>::Spec {
        convoy_spec("template-b")
    }

    fn assert_created(created: &ResourceObject<Self::Resource>) {
        assert_eq!(created.spec.workflow_ref, "template-a");
        assert!(created.status.is_none());
    }

    fn assert_updated(updated: &ResourceObject<Self::Resource>) {
        assert_eq!(updated.spec.workflow_ref, "template-b");
        assert_eq!(updated.metadata.labels.get("app").expect("label"), "flotilla");
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WorkflowTemplateFixture;

impl ResourceContractFixture for WorkflowTemplateFixture {
    type Resource = WorkflowTemplate;

    fn label() -> &'static str {
        "WorkflowTemplate"
    }

    fn meta(name: &str) -> InputMeta {
        workflow_template_meta(name)
    }

    fn spec() -> <Self::Resource as Resource>::Spec {
        valid_workflow_template_spec()
    }

    fn updated_spec() -> <Self::Resource as Resource>::Spec {
        updated_workflow_template_spec()
    }

    fn assert_created(created: &ResourceObject<Self::Resource>) {
        assert_eq!(created.spec.vessels.len(), 2);
        assert!(created.status.is_none());
    }

    fn assert_updated(updated: &ResourceObject<Self::Resource>) {
        match &updated.spec.vessels[0].crew[1].source {
            flotilla_resources::CrewSource::Tool { command } => assert_eq!(command, "cargo check --all-targets"),
            other => panic!("expected tool process, got {other:?}"),
        }
    }
}

pub fn in_memory_backend() -> ResourceBackend {
    ResourceBackend::InMemory(InMemoryBackend::default())
}

pub fn resolver<F: ResourceContractFixture>(backend: ResourceBackend, namespace: &str) -> TypedResolver<F::Resource> {
    backend.using::<F::Resource>(namespace)
}

pub async fn assert_create_get_list_roundtrip_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");

    assert_eq!(created.metadata.name, "alpha", "{} create should preserve name", F::label());
    assert_eq!(created.metadata.namespace, "flotilla", "{} create should preserve namespace", F::label());
    assert!(!created.metadata.resource_version.is_empty(), "{} create should assign resource version", F::label());
    F::assert_created(&created);

    let fetched = resolver.get("alpha").await.expect("get should succeed");
    assert_eq!(fetched.metadata.resource_version, created.metadata.resource_version);

    let listed = resolver.list().await.expect("list should succeed");
    assert_eq!(listed.resource_version, created.metadata.resource_version);
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].metadata.name, "alpha");
}

pub async fn assert_create_get_list_roundtrip<F: ResourceContractFixture>() {
    assert_create_get_list_roundtrip_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_stale_resource_version_conflicts_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");

    let conflict = resolver.update(&F::meta("alpha"), "0", &F::updated_spec()).await.err().expect("stale version should conflict");
    assert!(matches!(conflict, ResourceError::Conflict { .. }));

    let updated =
        resolver.update(&F::meta("alpha"), &created.metadata.resource_version, &F::updated_spec()).await.expect("update should succeed");
    assert_ne!(updated.metadata.resource_version, created.metadata.resource_version);
    F::assert_updated(&updated);
}

pub async fn assert_stale_resource_version_conflicts<F: ResourceContractFixture>() {
    assert_stale_resource_version_conflicts_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_identical_update_is_noop_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let meta = F::meta("alpha");
    let spec = F::spec();
    let created = resolver.create(&meta, &spec).await.expect("create should succeed");
    let mut watch = resolver.watch(WatchStart::FromVersion(created.metadata.resource_version.clone())).await.expect("watch should succeed");

    let unchanged = resolver.update(&meta, &created.metadata.resource_version, &spec).await.expect("identical update should succeed");

    assert_eq!(unchanged.metadata.resource_version, created.metadata.resource_version);
    assert!(timeout(Duration::from_millis(100), watch.next()).await.is_err(), "identical update should not emit a watch event");
}

pub async fn assert_identical_status_update_is_noop_with_backend<F: ResourceContractFixture>(backend: ResourceBackend)
where
    <F::Resource as Resource>::Status: Default,
{
    let resolver = resolver::<F>(backend, "flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");
    let status = <F::Resource as Resource>::Status::default();
    let status_written =
        resolver.update_status("alpha", &created.metadata.resource_version, &status).await.expect("initial status update should succeed");
    let mut watch =
        resolver.watch(WatchStart::FromVersion(status_written.metadata.resource_version.clone())).await.expect("watch should succeed");

    let unchanged = resolver
        .update_status("alpha", &status_written.metadata.resource_version, &status)
        .await
        .expect("identical status update should succeed");

    assert_eq!(unchanged.metadata.resource_version, status_written.metadata.resource_version);
    assert!(timeout(Duration::from_millis(100), watch.next()).await.is_err(), "identical status update should not emit a watch event");
}

pub async fn assert_delete_emits_event_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");
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
            assert_ne!(object.metadata.resource_version, created.metadata.resource_version);
        }
        _ => panic!("expected deleted event"),
    }
}

pub async fn assert_delete_emits_event<F: ResourceContractFixture>() {
    assert_delete_emits_event_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_repeated_delete_with_pending_finalizers_is_noop_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let finalizer = "flotilla.work/test-finalizer";
    let mut meta = F::meta("alpha");
    meta.finalizers = vec![finalizer.to_string()];
    let created = resolver.create(&meta, &F::spec()).await.expect("create should succeed");
    let mut watch = resolver.watch(WatchStart::FromVersion(created.metadata.resource_version.clone())).await.expect("watch should start");

    resolver.delete("alpha").await.expect("first delete should mark the object for deletion");
    let marked = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("first delete should emit an event")
        .expect("watch should yield an item")
        .expect("watch event should decode");
    let WatchEvent::Modified(marked) = marked else { panic!("first delete should mark the object as modified") };
    assert_eq!(marked.metadata.finalizers, vec![finalizer.to_string()]);
    assert!(marked.metadata.deletion_timestamp.is_some(), "first delete should set the deletion timestamp");

    resolver.delete("alpha").await.expect("repeated delete should be idempotent while finalizers are pending");
    let still_marked = resolver.get("alpha").await.expect("pending finalizer should keep the object present");
    assert_eq!(still_marked.metadata.resource_version, marked.metadata.resource_version);
    assert_eq!(still_marked.metadata.finalizers, vec![finalizer.to_string()]);
    assert!(still_marked.metadata.deletion_timestamp.is_some());
    assert!(timeout(Duration::from_millis(100), watch.next()).await.is_err(), "repeated delete should not emit an event");

    let cleared_meta = InputMeta::from(&marked.metadata).without_finalizer(finalizer);
    let removed = resolver
        .update(&cleared_meta, &marked.metadata.resource_version, &marked.spec)
        .await
        .expect("clearing the finalizer should succeed");
    let deleted = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("clearing the finalizer should emit a deleted event")
        .expect("watch should yield an item")
        .expect("watch event should decode");
    let WatchEvent::Deleted(deleted) = deleted else { panic!("clearing the finalizer should delete the object") };
    assert_eq!(deleted.metadata.resource_version, removed.metadata.resource_version);
    assert!(matches!(resolver.get("alpha").await, Err(ResourceError::NotFound { .. })));
}

pub async fn assert_watch_from_version_replays_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");

    let listed = resolver.list().await.expect("list should succeed");
    let mut watch = resolver.watch(WatchStart::FromVersion(listed.resource_version.clone())).await.expect("watch should succeed");

    let updated = resolver
        .update(&F::meta("alpha"), &listed.items[0].metadata.resource_version, &F::updated_spec())
        .await
        .expect("update should succeed");

    let modified = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce modified event")
        .expect("stream should yield item")
        .expect("event should decode");
    match modified {
        WatchEvent::Modified(object) => assert_eq!(object.metadata.resource_version, updated.metadata.resource_version),
        _ => panic!("expected modified event"),
    }

    resolver.delete("alpha").await.expect("delete should succeed");
    let deleted = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce deleted event")
        .expect("stream should yield item")
        .expect("event should decode");
    match deleted {
        WatchEvent::Deleted(object) => assert_ne!(object.metadata.resource_version, updated.metadata.resource_version),
        _ => panic!("expected deleted event"),
    }
}

pub async fn assert_watch_from_version_replays<F: ResourceContractFixture>() {
    assert_watch_from_version_replays_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_watch_now_semantics_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");

    let mut watch = resolver.watch(WatchStart::Now).await.expect("watch should succeed");
    assert!(timeout(Duration::from_millis(100), watch.next()).await.is_err(), "watch-now should not replay existing state");

    let current = resolver.get("alpha").await.expect("get should succeed");
    let updated =
        resolver.update(&F::meta("alpha"), &current.metadata.resource_version, &F::updated_spec()).await.expect("update should succeed");
    let event = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("watch should produce future event")
        .expect("stream should yield item")
        .expect("event should decode");
    match event {
        WatchEvent::Modified(object) => assert_eq!(object.metadata.resource_version, updated.metadata.resource_version),
        _ => panic!("expected modified event"),
    }
}

pub async fn assert_watch_now_semantics<F: ResourceContractFixture>() {
    assert_watch_now_semantics_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_watch_retention_expires_only_versions_below_floor_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");
    let second = resolver
        .update(&F::meta("alpha"), &created.metadata.resource_version, &F::updated_spec())
        .await
        .expect("first update should succeed");
    let third =
        resolver.update(&F::meta("alpha"), &second.metadata.resource_version, &F::spec()).await.expect("second update should succeed");
    let fourth = resolver
        .update(&F::meta("alpha"), &third.metadata.resource_version, &F::updated_spec())
        .await
        .expect("third update should succeed");

    let mut retained = resolver
        .watch(WatchStart::FromVersion(second.metadata.resource_version.clone()))
        .await
        .expect("watch at compaction floor should succeed");
    for expected_version in [&third.metadata.resource_version, &fourth.metadata.resource_version] {
        let event = retained.next().await.expect("retained event").expect("retained event should decode");
        let WatchEvent::Modified(object) = event else { panic!("expected retained modified event") };
        assert_eq!(&object.metadata.resource_version, expected_version);
    }

    let expired = resolver
        .watch(WatchStart::FromVersion(created.metadata.resource_version.clone()))
        .await
        .expect_err("watch below compaction floor should expire");
    assert_eq!(expired, ResourceError::WatchExpired {
        requested_version: created.metadata.resource_version,
        compacted_through: Some(second.metadata.resource_version),
    });
}

pub async fn assert_consumer_relists_after_expired_watch_and_converges_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let first = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");
    let second = resolver
        .update(&F::meta("alpha"), &first.metadata.resource_version, &F::updated_spec())
        .await
        .expect("first update should succeed");
    let third =
        resolver.update(&F::meta("alpha"), &second.metadata.resource_version, &F::spec()).await.expect("second update should succeed");
    resolver.update(&F::meta("alpha"), &third.metadata.resource_version, &F::updated_spec()).await.expect("third update should succeed");

    assert!(matches!(
        resolver.watch(WatchStart::FromVersion(first.metadata.resource_version)).await,
        Err(ResourceError::WatchExpired { .. })
    ));

    let relisted = resolver.list().await.expect("expired consumer should relist");
    let mut local = relisted.items.into_iter().next().expect("relisted object");
    let mut resumed =
        resolver.watch(WatchStart::FromVersion(relisted.resource_version)).await.expect("consumer should resume from relisted version");
    let latest =
        resolver.update(&F::meta("alpha"), &local.metadata.resource_version, &F::spec()).await.expect("post-relist update should succeed");
    let event = resumed.next().await.expect("post-relist event").expect("post-relist event should decode");
    let WatchEvent::Modified(object) = event else { panic!("expected post-relist modified event") };
    local = object;

    assert_eq!(local.metadata.resource_version, latest.metadata.resource_version);
}

pub async fn assert_store_diagnostics_report_retained_events_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend.clone(), "flotilla");
    let first = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");
    let second = resolver
        .update(&F::meta("alpha"), &first.metadata.resource_version, &F::updated_spec())
        .await
        .expect("first update should succeed");
    resolver.update(&F::meta("alpha"), &second.metadata.resource_version, &F::spec()).await.expect("second update should succeed");

    let diagnostics = backend.diagnostics().await.expect("diagnostics should succeed").expect("embedded backend should report diagnostics");
    assert_eq!(diagnostics.object_count, 1);
    assert_eq!(diagnostics.event_count, 2);
    assert_eq!(diagnostics.resource_stream_count, 1);
    assert_eq!(diagnostics.max_retained_events, 2);
    assert!(diagnostics.event_log_within_retention());
    assert!(diagnostics.warnings.is_empty());
}

pub async fn assert_watch_only_does_not_create_resource_stream_diagnostics_with_backend<F: ResourceContractFixture>(
    backend: ResourceBackend,
) {
    let resolver = resolver::<F>(backend.clone(), "flotilla");
    let _watch = resolver.watch(WatchStart::Now).await.expect("watch should start");

    let diagnostics = backend.diagnostics().await.expect("diagnostics should succeed").expect("embedded diagnostics");
    assert_eq!(diagnostics.object_count, 0);
    assert_eq!(diagnostics.event_count, 0);
    assert_eq!(diagnostics.resource_stream_count, 0);
    assert_eq!(diagnostics.max_retained_events, 0);
}

pub async fn assert_namespace_isolation_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let alpha = backend.using::<F::Resource>("alpha");
    let beta = backend.using::<F::Resource>("beta");

    alpha.create(&F::meta("shared"), &F::spec()).await.expect("alpha create should succeed");
    beta.create(&F::meta("shared"), &F::updated_spec()).await.expect("beta create should succeed");

    let alpha_item = alpha.get("shared").await.expect("alpha get should succeed");
    let beta_item = beta.get("shared").await.expect("beta get should succeed");
    assert_eq!(alpha_item.metadata.namespace, "alpha");
    assert_eq!(beta_item.metadata.namespace, "beta");
    assert_ne!(alpha_item.metadata.resource_version, "");
    assert_ne!(beta_item.metadata.resource_version, "");
}

pub async fn assert_namespace_isolation<F: ResourceContractFixture>() {
    assert_namespace_isolation_with_backend::<F>(in_memory_backend()).await;
}

pub async fn assert_metadata_roundtrip_with_backend<F: ResourceContractFixture>(backend: ResourceBackend) {
    let resolver = resolver::<F>(backend, "flotilla");
    let mut meta = F::meta("alpha");
    meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    meta.annotations.insert("note".to_string(), "preserve-me".to_string());
    meta.owner_references = vec![OwnerReference {
        api_version: "flotilla.work/v1".to_string(),
        kind: "Vessel".to_string(),
        name: "alpha-implement".to_string(),
        controller: true,
    }];

    let created = resolver.create(&meta, &F::spec()).await.expect("create should succeed");
    let fetched = resolver.get("alpha").await.expect("get should succeed");

    assert_eq!(created.metadata.labels, meta.labels);
    assert_eq!(fetched.metadata.labels, meta.labels);
    assert_eq!(created.metadata.annotations, meta.annotations);
    assert_eq!(fetched.metadata.annotations, meta.annotations);
    assert_eq!(created.metadata.owner_references, meta.owner_references);
    assert_eq!(fetched.metadata.owner_references, meta.owner_references);
}

pub async fn assert_metadata_roundtrip<F: ResourceContractFixture>() {
    assert_metadata_roundtrip_with_backend::<F>(in_memory_backend()).await;
}
