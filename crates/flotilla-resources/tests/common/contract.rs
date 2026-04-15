use std::time::Duration;

use flotilla_resources::{
    Convoy, InMemoryBackend, InputMeta, OwnerReference, Resource, ResourceBackend, ResourceError, ResourceObject, WatchEvent, WatchStart,
    WorkflowTemplate,
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
        assert_eq!(created.spec.tasks.len(), 2);
        assert!(created.status.is_none());
    }

    fn assert_updated(updated: &ResourceObject<Self::Resource>) {
        match &updated.spec.tasks[0].processes[1].source {
            flotilla_resources::ProcessSource::Tool { command } => assert_eq!(command, "cargo check --all-targets"),
            other => panic!("expected tool process, got {other:?}"),
        }
    }
}

pub fn resolver<F: ResourceContractFixture>(namespace: &str) -> flotilla_resources::TypedResolver<F::Resource> {
    ResourceBackend::InMemory(InMemoryBackend::default()).using::<F::Resource>(namespace)
}

pub async fn assert_create_get_list_roundtrip<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
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

pub async fn assert_stale_resource_version_conflicts<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
    let created = resolver.create(&F::meta("alpha"), &F::spec()).await.expect("create should succeed");

    let conflict = resolver.update(&F::meta("alpha"), "0", &F::updated_spec()).await.err().expect("stale version should conflict");
    assert!(matches!(conflict, ResourceError::Conflict { .. }));

    let updated =
        resolver.update(&F::meta("alpha"), &created.metadata.resource_version, &F::updated_spec()).await.expect("update should succeed");
    assert_ne!(updated.metadata.resource_version, created.metadata.resource_version);
    F::assert_updated(&updated);
}

pub async fn assert_delete_emits_event<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
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

pub async fn assert_watch_from_version_replays<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
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

pub async fn assert_watch_now_semantics<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
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

pub async fn assert_namespace_isolation<F: ResourceContractFixture>() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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

pub async fn assert_metadata_roundtrip<F: ResourceContractFixture>() {
    let resolver = resolver::<F>("flotilla");
    let mut meta = F::meta("alpha");
    meta.labels.insert("flotilla.work/convoy".to_string(), "convoy-a".to_string());
    meta.annotations.insert("note".to_string(), "preserve-me".to_string());
    meta.owner_references = vec![OwnerReference {
        api_version: "flotilla.work/v1".to_string(),
        kind: "TaskWorkspace".to_string(),
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
