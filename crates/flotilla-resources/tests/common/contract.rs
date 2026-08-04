use std::{collections::BTreeSet, time::Duration};

use chrono::Utc;
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    Convoy, Demand, DemandAddressee, DemandKind, DemandPoolRef, DemandSpec, InMemoryBackend, InputMeta, IssueSource, OwnerReference,
    PrincipalRef, Project, ProjectRepositorySpec, ProjectSpec, Regard, RegardExpiryPolicy, RegardSource, RegardSpec, RepositoryKey,
    Resource, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, TypedResolver, WatchEvent, WatchStart, WorkflowTemplate,
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

#[derive(Clone, Copy, Debug)]
pub struct RegardFixture;

impl ResourceContractFixture for RegardFixture {
    type Resource = Regard;

    fn label() -> &'static str {
        "Regard"
    }

    fn meta(name: &str) -> InputMeta {
        InputMeta::builder().name(name.to_string()).build()
    }

    fn spec() -> <Self::Resource as Resource>::Spec {
        RegardSpec::builder()
            .principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
            .target(resource_ref("Convoy", "alpha"))
            .source(RegardSource::Expressed)
            .expiry(RegardExpiryPolicy::Decaying { expires_after_seconds: 300 })
            .build()
    }

    fn updated_spec() -> <Self::Resource as Resource>::Spec {
        RegardSpec::builder()
            .principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
            .target(resource_ref("Convoy", "alpha"))
            .source(RegardSource::Implicit { policy: "convoy-start".to_string() })
            .expiry(RegardExpiryPolicy::Pin)
            .build()
    }

    fn assert_created(created: &ResourceObject<Self::Resource>) {
        assert_eq!(created.spec.principal_ref, PrincipalRef::implicit_for_namespace("flotilla"));
        assert!(created.status.is_none());
    }

    fn assert_updated(updated: &ResourceObject<Self::Resource>) {
        assert_eq!(updated.spec.expiry, RegardExpiryPolicy::Pin);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DemandFixture;

impl ResourceContractFixture for DemandFixture {
    type Resource = Demand;

    fn label() -> &'static str {
        "Demand"
    }

    fn meta(name: &str) -> InputMeta {
        InputMeta::builder().name(name.to_string()).build()
    }

    fn spec() -> <Self::Resource as Resource>::Spec {
        DemandSpec::builder()
            .originating_work_ref(resource_ref("Vessel", "alpha-implement"))
            .kind(DemandKind::Permission)
            .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
            .build()
    }

    fn updated_spec() -> <Self::Resource as Resource>::Spec {
        DemandSpec::builder()
            .originating_work_ref(resource_ref("Vessel", "alpha-review"))
            .kind(DemandKind::Review)
            .addressee(DemandAddressee::Pool { pool_ref: DemandPoolRef("project/default".to_string()) })
            .build()
    }

    fn assert_created(created: &ResourceObject<Self::Resource>) {
        assert_eq!(created.spec.kind, DemandKind::Permission);
        assert!(created.status.is_none());
    }

    fn assert_updated(updated: &ResourceObject<Self::Resource>) {
        assert_eq!(updated.spec.kind, DemandKind::Review);
    }
}

fn resource_ref(kind: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla.work/v1", kind, "flotilla", name)
}

pub fn in_memory_backend() -> ResourceBackend {
    ResourceBackend::InMemory(InMemoryBackend::default())
}

pub async fn assert_replica_read_view_contract(backend: ResourceBackend) {
    let local = backend.using::<Convoy>("flotilla");
    local.create(&convoy_meta("local"), &convoy_spec("local-template")).await.expect("create local convoy");

    let origin = flotilla_protocol::NodeId::new("feta-root");
    let remote_backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let remote = remote_backend.using::<Convoy>("flotilla");
    remote.create(&convoy_meta("remote"), &convoy_spec("remote-template")).await.expect("create remote convoy");
    remote.create(&convoy_meta("untouched"), &convoy_spec("untouched-template")).await.expect("create untouched remote convoy");
    let remote_list = remote.list().await.expect("list remote convoy");
    let synced_at = Utc::now();

    backend.replica_writer::<Convoy>(origin.clone(), "flotilla").replace(&remote_list, synced_at).await.expect("store remote replica");

    let local_only = local.list().await.expect("list local-only resources");
    assert_eq!(local_only.items.iter().map(|item| item.metadata.name.as_str()).collect::<Vec<_>>(), ["local"]);

    let all = backend.including_replicas::<Convoy>("flotilla").list().await.expect("list resources including replicas");
    assert_eq!(all.items.len(), 3);
    assert!(matches!(all.items[0].provenance, ResourceProvenance::Local));
    let replica = all.items.iter().find(|item| item.object.metadata.name == "remote").expect("remote replica row");
    assert_eq!(replica.provenance, ResourceProvenance::Replica { origin_root: origin.clone(), last_synced_at: synced_at });
    let wire =
        flotilla_resources::list_resource_kind_including_replicas(&backend, "flotilla", "convoys").await.expect("list replica wire view");
    let wire_replica = wire.value["items"]
        .as_array()
        .expect("wire items")
        .iter()
        .find(|item| item["metadata"]["name"] == "remote")
        .expect("wire replica row");
    assert_eq!(wire_replica["metadata"]["annotations"]["flotilla.work/origin-root"], "feta-root");
    assert!(wire_replica["metadata"]["annotations"]["flotilla.work/last-synced-at"].is_string());

    let cursor =
        backend.replica_writer::<Convoy>(origin.clone(), "flotilla").cursor().await.expect("read replica cursor").expect("replica cursor");
    assert_eq!(cursor.resource_version, remote_list.resource_version);
    assert_eq!(cursor.generation, remote_list.generation);

    let mut watch = backend.including_replicas::<Convoy>("flotilla").watch().await.expect("watch resources including replicas");
    let remote_object = remote.get("remote").await.expect("get remote convoy before update");
    remote
        .update(&convoy_meta("remote"), &remote_object.metadata.resource_version, &convoy_spec("updated-template"))
        .await
        .expect("update one remote convoy");
    let modified = remote.watch(WatchStart::resuming_from(&remote_list)).await.expect("watch remote update").next().await;
    let modified = modified.expect("remote update event").expect("valid remote update");
    let updated_sync = synced_at + chrono::Duration::seconds(1);
    backend.replica_writer::<Convoy>(origin.clone(), "flotilla").apply(modified, updated_sync).await.expect("apply remote update");
    let modified_event = timeout(Duration::from_secs(1), watch.next()).await.expect("replica update watch timeout");
    assert!(matches!(modified_event, Some(Ok(flotilla_resources::ReadWatchEvent::Modified(_)))));
    let after_update = backend.including_replicas::<Convoy>("flotilla").list().await.expect("list after replica update");
    let updated = after_update.items.iter().find(|item| item.object.metadata.name == "remote").expect("updated replica row");
    assert_eq!(updated.provenance, ResourceProvenance::Replica { origin_root: origin.clone(), last_synced_at: updated_sync });
    let untouched = after_update.items.iter().find(|item| item.object.metadata.name == "untouched").expect("untouched replica row");
    assert_eq!(untouched.provenance, ResourceProvenance::Replica { origin_root: origin.clone(), last_synced_at: synced_at });

    let before_delete = remote.list().await.expect("list before remote delete");
    remote.delete("remote").await.expect("delete remote convoy");
    let deleted = remote.watch(WatchStart::resuming_from(&before_delete)).await.expect("watch remote delete").next().await;
    let deleted = deleted.expect("remote delete event").expect("valid remote delete");
    backend
        .replica_writer::<Convoy>(origin, "flotilla")
        .apply(deleted, updated_sync + chrono::Duration::seconds(1))
        .await
        .expect("apply remote delete");

    let event = timeout(Duration::from_secs(1), watch.next()).await.expect("replica delete watch timeout");
    assert!(matches!(event, Some(Ok(flotilla_resources::ReadWatchEvent::Deleted(_)))));
    let remaining = backend.including_replicas::<Convoy>("flotilla").list().await.expect("list after replica delete");
    assert_eq!(remaining.items.len(), 2);

    let mut new_generation = remote.list().await.expect("list new origin generation");
    new_generation.generation = Some("generation-2".to_string());
    new_generation.resource_version = "0".to_string();
    backend
        .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("feta-root"), "flotilla")
        .replace(&new_generation, Utc::now())
        .await
        .expect("replace origin generation");
    let cursor = backend
        .replica_writer::<Convoy>(flotilla_protocol::NodeId::new("feta-root"), "flotilla")
        .cursor()
        .await
        .expect("read replaced cursor")
        .expect("replaced cursor");
    assert_eq!(cursor.generation.as_deref(), Some("generation-2"));
}

pub async fn assert_replica_events_ignore_stale_writes_and_deletes_with_backend(backend: ResourceBackend) {
    let origin = flotilla_protocol::NodeId::new("feta-root");
    let source_backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let source = source_backend.using::<Convoy>("flotilla");
    let meta = convoy_meta("remote");
    let first = source.create(&meta, &convoy_spec("first")).await.expect("create source convoy");
    let first_synced_at = Utc::now();
    let writer = backend.replica_writer::<Convoy>(origin, "flotilla");
    writer.apply(WatchEvent::Added(first.clone()), first_synced_at).await.expect("apply initial replica event");

    let second = source.update(&meta, &first.metadata.resource_version, &convoy_spec("second")).await.expect("update source convoy");
    let stale_delete_synced_at = first_synced_at + chrono::Duration::seconds(1);
    let second_synced_at = first_synced_at + chrono::Duration::seconds(2);
    writer.apply(WatchEvent::Modified(second.clone()), second_synced_at).await.expect("apply newer replica update");
    writer.apply(WatchEvent::Deleted(first), stale_delete_synced_at).await.expect("ignore stale replica delete");

    let after_stale_delete = backend.including_replicas::<Convoy>("flotilla").list().await.expect("list after stale delete");
    assert_eq!(after_stale_delete.items.len(), 1);
    assert_eq!(after_stale_delete.items[0].object.spec.workflow_ref, "second");

    let delete_synced_at = first_synced_at + chrono::Duration::seconds(3);
    writer.apply(WatchEvent::Deleted(second.clone()), delete_synced_at).await.expect("apply current replica delete");
    writer.apply(WatchEvent::Modified(second.clone()), second_synced_at).await.expect("ignore stale replica update after delete");
    assert!(
        backend.including_replicas::<Convoy>("flotilla").list().await.expect("list after stale update").items.is_empty(),
        "a retained delete tombstone must prevent resurrection by an older write"
    );

    writer
        .apply(WatchEvent::Added(second), delete_synced_at + chrono::Duration::seconds(1))
        .await
        .expect("apply recreation newer than tombstone");
    assert_eq!(
        backend.including_replicas::<Convoy>("flotilla").list().await.expect("list recreated replica").items.len(),
        1,
        "a write newer than the delete tombstone should recreate the replica"
    );
}

fn project_spec(display_name: &str, workflow: &str) -> ProjectSpec {
    ProjectSpec::builder()
        .display_name(display_name.to_string())
        .default_workflow_ref(workflow.to_string())
        .repositories(vec![ProjectRepositorySpec {
            repo: RepositoryKey("github.com/acme/widgets".to_string()),
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }])
        .build()
}

pub async fn assert_project_definition_edit_converges_with_backend(backend: ResourceBackend) {
    let kiwi_root = flotilla_protocol::NodeId::new("kiwi-root");
    let feta_root = flotilla_protocol::NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = backend.with_local_root(feta_root.clone());
    let meta = InputMeta::builder().name("widgets".to_string()).build();

    kiwi.definitions::<Project>("flotilla").apply(&meta, &project_spec("Widgets", "default")).await.expect("create Project on kiwi");
    let kiwi_log = kiwi.using::<Project>("flotilla").list().await.expect("list kiwi Project log");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla")
        .replace(&kiwi_log, Utc::now())
        .await
        .expect("replicate kiwi Project to feta");

    let visible_on_feta = feta.definitions::<Project>("flotilla").get("widgets").await.expect("Project should be visible on feta");
    assert_eq!(visible_on_feta.spec.display_name, "Widgets");
    assert!(feta.using::<Project>("flotilla").list().await.expect("list feta local log").items.is_empty());

    feta.definitions::<Project>("flotilla")
        .apply(&InputMeta::from(&visible_on_feta.metadata), &project_spec("Feta Widgets", "default"))
        .await
        .expect("edit kiwi-authored Project on feta");
    let feta_log = feta.using::<Project>("flotilla").list().await.expect("list feta Project log");
    kiwi.replica_writer::<Project>(feta_root, "flotilla").replace(&feta_log, Utc::now()).await.expect("replicate feta Project to kiwi");

    let converged_on_kiwi = kiwi.definitions::<Project>("flotilla").get("widgets").await.expect("merged Project on kiwi");
    assert_eq!(converged_on_kiwi.spec.display_name, "Feta Widgets");
    assert!(converged_on_kiwi.metadata.merge.as_ref().expect("definition merge metadata").conflicts.is_empty());
}

pub async fn assert_project_definition_metadata_edit_converges_with_backend(backend: ResourceBackend) {
    let backend = backend.with_local_root(flotilla_protocol::NodeId::new("local-root"));
    let projects = backend.definitions::<Project>("flotilla");
    let spec = project_spec("Widgets", "default");
    let created = projects.apply(&InputMeta::builder().name("widgets".to_string()).build(), &spec).await.expect("create Project baseline");
    let mut labelled_meta = InputMeta::from(&created.metadata);
    labelled_meta.labels.insert("flotilla.work/managed-by".to_string(), "generator".to_string());

    let labelled = projects.apply(&labelled_meta, &spec).await.expect("apply Project metadata");
    assert_ne!(labelled.metadata.resource_version, created.metadata.resource_version);
    assert_eq!(labelled.metadata.labels.get("flotilla.work/managed-by").map(String::as_str), Some("generator"));

    let unchanged = projects.apply(&labelled_meta, &spec).await.expect("reapply matching Project metadata");
    assert_eq!(unchanged.metadata.resource_version, labelled.metadata.resource_version);
}

pub async fn assert_project_definition_causal_merge_with_backend(backend: ResourceBackend) {
    let kiwi_root = flotilla_protocol::NodeId::new("kiwi-root");
    let feta_root = flotilla_protocol::NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = backend.with_local_root(feta_root.clone());
    let meta = InputMeta::builder().name("widgets".to_string()).build();

    kiwi.definitions::<Project>("flotilla").apply(&meta, &project_spec("Widgets", "default")).await.expect("create Project baseline");
    let baseline = kiwi.using::<Project>("flotilla").list().await.expect("list Project baseline");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla").replace(&baseline, Utc::now()).await.expect("replicate baseline to feta");

    kiwi.definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Kiwi Widgets", "default"))
        .await
        .expect("concurrent kiwi display-name edit");
    feta.definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Feta Widgets", "feta-flow"))
        .await
        .expect("concurrent feta display-name and workflow edit");

    let kiwi_log = kiwi.using::<Project>("flotilla").list().await.expect("list updated kiwi log");
    let feta_log = feta.using::<Project>("flotilla").list().await.expect("list updated feta log");
    kiwi.replica_writer::<Project>(feta_root.clone(), "flotilla")
        .replace(&feta_log, Utc::now())
        .await
        .expect("replicate concurrent feta edit to kiwi");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla")
        .replace(&kiwi_log, Utc::now())
        .await
        .expect("replicate concurrent kiwi edit to feta");

    let merged_on_kiwi = kiwi.definitions::<Project>("flotilla").get("widgets").await.expect("merged Project on kiwi");
    let merged_on_feta = feta.definitions::<Project>("flotilla").get("widgets").await.expect("merged Project on feta");
    assert_eq!(
        serde_json::to_value(&merged_on_kiwi).expect("serialize kiwi merge"),
        serde_json::to_value(&merged_on_feta).expect("serialize feta merge"),
        "the same source set must produce the same merged view on every root"
    );

    for merged in [merged_on_kiwi, merged_on_feta] {
        assert_eq!(merged.spec.default_workflow_ref, "feta-flow", "disjoint-field edit should merge without conflict");
        let conflicts = &merged.metadata.merge.as_ref().expect("definition merge metadata").conflicts;
        let display_conflict = conflicts.get("spec.display_name").expect("concurrent same-field edits should conflict");
        assert_eq!(display_conflict.len(), 2);
        assert_eq!(
            display_conflict.iter().map(|sibling| sibling.value.as_str().expect("string sibling")).collect::<BTreeSet<_>>(),
            BTreeSet::from(["Feta Widgets", "Kiwi Widgets"])
        );
        assert!(!conflicts.contains_key("spec.default_workflow_ref"));
    }

    feta.definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Feta Widgets", "feta-flow"))
        .await
        .expect("resolve conflict by ordinarily writing an existing sibling");
    let resolved_log = feta.using::<Project>("flotilla").list().await.expect("list resolution");
    kiwi.replica_writer::<Project>(feta_root, "flotilla").replace(&resolved_log, Utc::now()).await.expect("replicate resolution to kiwi");

    for merged in [
        kiwi.definitions::<Project>("flotilla").get("widgets").await.expect("resolved Project on kiwi"),
        feta.definitions::<Project>("flotilla").get("widgets").await.expect("resolved Project on feta"),
    ] {
        assert_eq!(merged.spec.display_name, "Feta Widgets");
        assert!(merged.metadata.merge.as_ref().expect("definition merge metadata").conflicts.is_empty());
    }
}

pub async fn assert_project_definition_optional_field_can_be_cleared_with_backend(backend: ResourceBackend) {
    let kiwi_root = flotilla_protocol::NodeId::new("kiwi-root");
    let feta_root = flotilla_protocol::NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = backend.with_local_root(feta_root.clone());
    let meta = InputMeta::builder().name("widgets".to_string()).build();
    let mut original = project_spec("Widgets", "default");
    original.issue_source = Some(IssueSource { service: "github".to_string(), scope: "acme/widgets".to_string() });

    kiwi.definitions::<Project>("flotilla").apply(&meta, &original).await.expect("create Project with issue source");
    let baseline = kiwi.using::<Project>("flotilla").list().await.expect("list Project baseline");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla").replace(&baseline, Utc::now()).await.expect("replicate baseline");

    let mut cleared = feta.definitions::<Project>("flotilla").get("widgets").await.expect("get replicated Project").spec;
    cleared.issue_source = None;
    feta.definitions::<Project>("flotilla").apply(&meta, &cleared).await.expect("clear replicated issue source");
    assert_eq!(
        feta.definitions::<Project>("flotilla").get("widgets").await.expect("get locally cleared Project").spec.issue_source,
        None,
        "an explicit null must causally supersede the replicated value"
    );

    let feta_log = feta.using::<Project>("flotilla").list().await.expect("list cleared Project log");
    kiwi.replica_writer::<Project>(feta_root, "flotilla").replace(&feta_log, Utc::now()).await.expect("replicate cleared Project");
    for merged in [
        kiwi.definitions::<Project>("flotilla").get("widgets").await.expect("cleared Project on kiwi"),
        feta.definitions::<Project>("flotilla").get("widgets").await.expect("cleared Project on feta"),
    ] {
        assert_eq!(merged.spec.issue_source, None);
        assert!(merged.metadata.merge.as_ref().expect("definition merge metadata").conflicts.is_empty());
    }
}

pub async fn assert_project_definition_edit_preserves_unrelated_conflict_with_backend(backend: ResourceBackend) {
    let kiwi_root = flotilla_protocol::NodeId::new("kiwi-root");
    let feta_root = flotilla_protocol::NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = backend.with_local_root(feta_root.clone());
    let meta = InputMeta::builder().name("widgets".to_string()).build();

    kiwi.definitions::<Project>("flotilla").apply(&meta, &project_spec("Widgets", "default")).await.expect("create Project baseline");
    let baseline = kiwi.using::<Project>("flotilla").list().await.expect("list Project baseline");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla").replace(&baseline, Utc::now()).await.expect("replicate baseline");

    kiwi.definitions::<Project>("flotilla").apply(&meta, &project_spec("Kiwi Widgets", "kiwi-flow")).await.expect("concurrent kiwi edit");
    feta.definitions::<Project>("flotilla").apply(&meta, &project_spec("Feta Widgets", "feta-flow")).await.expect("concurrent feta edit");
    let kiwi_log = kiwi.using::<Project>("flotilla").list().await.expect("list kiwi edit");
    let feta_log = feta.using::<Project>("flotilla").list().await.expect("list feta edit");
    kiwi.replica_writer::<Project>(feta_root.clone(), "flotilla").replace(&feta_log, Utc::now()).await.expect("sync feta edit");
    feta.replica_writer::<Project>(kiwi_root, "flotilla").replace(&kiwi_log, Utc::now()).await.expect("sync kiwi edit");

    let conflicted = feta.definitions::<Project>("flotilla").get("widgets").await.expect("get conflicted Project");
    let conflicts = &conflicted.metadata.merge.as_ref().expect("definition merge metadata").conflicts;
    assert!(conflicts.contains_key("spec.display_name"));
    assert!(conflicts.contains_key("spec.default_workflow_ref"));

    let mut resolved_display_name = conflicted.spec;
    resolved_display_name.display_name = "Resolved Widgets".to_string();
    feta.definitions::<Project>("flotilla").apply(&meta, &resolved_display_name).await.expect("resolve only the display-name conflict");
    let resolution_log = feta.using::<Project>("flotilla").list().await.expect("list partial resolution");
    kiwi.replica_writer::<Project>(feta_root, "flotilla").replace(&resolution_log, Utc::now()).await.expect("replicate partial resolution");

    for merged in [
        kiwi.definitions::<Project>("flotilla").get("widgets").await.expect("partially resolved Project on kiwi"),
        feta.definitions::<Project>("flotilla").get("widgets").await.expect("partially resolved Project on feta"),
    ] {
        let conflicts = &merged.metadata.merge.as_ref().expect("definition merge metadata").conflicts;
        assert!(!conflicts.contains_key("spec.display_name"), "the edited field should be resolved");
        assert!(
            conflicts.contains_key("spec.default_workflow_ref"),
            "editing one field must not resolve an unrelated conflict echoed from the merged view"
        );
    }
}

pub async fn assert_project_definition_delete_conflicts_with_concurrent_edit_with_backend(backend: ResourceBackend) {
    let kiwi_root = flotilla_protocol::NodeId::new("kiwi-root");
    let feta_root = flotilla_protocol::NodeId::new("feta-root");
    let kiwi = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(kiwi_root.clone());
    let feta = backend.with_local_root(feta_root.clone());
    let meta = InputMeta::builder().name("widgets".to_string()).build();

    kiwi.definitions::<Project>("flotilla").apply(&meta, &project_spec("Widgets", "default")).await.expect("create Project baseline");
    let baseline = kiwi.using::<Project>("flotilla").list().await.expect("list baseline");
    feta.replica_writer::<Project>(kiwi_root.clone(), "flotilla").replace(&baseline, Utc::now()).await.expect("replicate baseline");

    kiwi.definitions::<Project>("flotilla").delete("widgets").await.expect("delete on kiwi");
    feta.definitions::<Project>("flotilla")
        .apply(&meta, &project_spec("Edited on Feta", "default"))
        .await
        .expect("concurrent edit on feta");
    let kiwi_log = kiwi.using::<Project>("flotilla").list().await.expect("list delete");
    let feta_log = feta.using::<Project>("flotilla").list().await.expect("list edit");
    kiwi.replica_writer::<Project>(feta_root, "flotilla").replace(&feta_log, Utc::now()).await.expect("sync edit");
    feta.replica_writer::<Project>(kiwi_root, "flotilla").replace(&kiwi_log, Utc::now()).await.expect("sync delete");

    let conflicted = feta.definitions::<Project>("flotilla").get("widgets").await.expect("delete/edit conflict remains visible");
    let deletion = conflicted
        .metadata
        .merge
        .as_ref()
        .expect("definition merge metadata")
        .conflicts
        .get("$deleted")
        .expect("concurrent delete and edit should conflict");
    assert_eq!(
        deletion.iter().map(|sibling| sibling.value.as_bool().expect("boolean deletion sibling")).collect::<BTreeSet<_>>(),
        [false, true,].into_iter().collect()
    );
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
