use flotilla_store_memory::InMemoryBackend;
use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    Convoy, ConvoySpec, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta, ResourceError, WorkflowTemplate,
    WorkflowTemplateSpec, CONVOY_LABEL, PLACEMENT_SNAPSHOT_ANNOTATION, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
    WORKFLOW_SNAPSHOT_ANNOTATION, WORKFLOW_SNAPSHOT_KIND,
};
use flotilla_store::{PreparedSnapshotGarbageCollector, ResourceBackend};

fn placement() -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder().host_ref("remote".into()).pool("remote".into()).realisation(FulfilmentRealisation::HostDirect).build()
}

fn prepared_meta(name: &str, kind: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(BTreeMap::from([(PREPARED_SNAPSHOT_LABEL.to_string(), kind.to_string())])).build()
}

/// A snapshot of `kind` as admission writes it: labelled with its convoy.
fn owned_meta(name: &str, kind: &str, convoy: &str) -> InputMeta {
    let mut meta = prepared_meta(name, kind);
    meta.labels.insert(CONVOY_LABEL.to_string(), convoy.to_string());
    meta
}

fn placement_meta(name: &str, convoy: &str) -> InputMeta {
    owned_meta(name, PLACEMENT_SNAPSHOT_KIND, convoy)
}

fn workflow_meta(name: &str, convoy: &str) -> InputMeta {
    owned_meta(name, WORKFLOW_SNAPSHOT_KIND, convoy)
}

fn workflow() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder().vessels(Vec::new()).build()
}

async fn create_convoy(
    backend: &ResourceBackend,
    name: &str,
    workflow_snapshot: &str,
    placement_snapshot: &str,
) -> Result<(), ResourceError> {
    backend
        .clone()
        .using::<Convoy>("flotilla")
        .create(
            &InputMeta::builder()
                .name(name.to_string())
                .annotations(BTreeMap::from([
                    (WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_snapshot.to_string()),
                    (PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_snapshot.to_string()),
                ]))
                .build(),
            &ConvoySpec::builder()
                .workflow_ref("logical-workflow".to_string())
                .placement_policy("logical-placement".to_string())
                .project_ref("flotilla".to_string())
                .build(),
        )
        .await
        .map(|_| ())
}

#[tokio::test]
async fn replicated_convoy_keeps_an_authored_snapshot_alive() {
    let author = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("author"));
    let remote = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("remote"));
    let workflow_name = "workflow-snapshot-012345abcdef";
    let placement_name = "placement-snapshot-012345abcdef";
    author
        .using::<WorkflowTemplate>("flotilla")
        .create(&workflow_meta(workflow_name, "remote-convoy"), &workflow())
        .await
        .expect("author workflow snapshot");
    author
        .using::<FulfilmentKind>("flotilla")
        .create(&placement_meta(placement_name, "remote-convoy"), &placement())
        .await
        .expect("author placement snapshot");
    create_convoy(&remote, "remote-convoy", workflow_name, placement_name).await.expect("remote convoy");
    create_convoy(&author, "remote-convoy", workflow_name, placement_name).await.expect("same-named local convoy being deleted");
    let replica = author.replica_writer::<Convoy>(NodeId::new("remote"), "flotilla");
    replica
        .replace(&remote.using::<Convoy>("flotilla").list().await.expect("remote convoys"), Utc::now())
        .await
        .expect("replicate remote convoy");

    let collector = PreparedSnapshotGarbageCollector::new(author.clone(), "flotilla");
    let retained = collector.collect("remote-convoy").await.expect("collect while same-named local convoy is deleting");
    assert_eq!(retained.workflows_deleted, 0);
    assert_eq!(retained.placements_deleted, 0);

    remote.using::<Convoy>("flotilla").delete("remote-convoy").await.expect("remove remote convoy");
    replica
        .replace(&remote.using::<Convoy>("flotilla").list().await.expect("empty remote convoys"), Utc::now())
        .await
        .expect("replicate removal");
    let collected = collector.collect("remote-convoy").await.expect("collect after remote release");
    assert_eq!(collected.workflows_deleted, 1);
    assert_eq!(collected.placements_deleted, 1);
}

// Admission writes a convoy's workflow and placement snapshots before it admits the convoy, each
// its own though another convoy has the same workflow: another convoy's release meanwhile leaves
// them to their own convoy. A snapshot no convoy's label owns, as the previous generation wrote
// workflow snapshots, is never released.
#[tokio::test]
async fn a_snapshot_written_before_its_convoy_is_released_only_by_that_convoy() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let (ending_workflow, admitting_workflow, unowned) =
        ("workflow-snapshot-012345abcdef", "workflow-snapshot-fedcba543210", "workflow-snapshot-aaaaaaaaaaaa");
    let (ending_placement, admitting_placement) = ("placement-snapshot-012345abcdef", "placement-snapshot-fedcba543210");
    let workflows = backend.clone().using::<WorkflowTemplate>("flotilla");
    let placements = backend.clone().using::<FulfilmentKind>("flotilla");
    workflows.create(&workflow_meta(ending_workflow, "ending"), &workflow()).await.expect("the ending convoy's workflow");
    workflows.create(&workflow_meta(admitting_workflow, "admitting"), &workflow()).await.expect("the admitting convoy's workflow");
    workflows.create(&prepared_meta(unowned, WORKFLOW_SNAPSHOT_KIND), &workflow()).await.expect("a previous generation's workflow");
    placements.create(&placement_meta(ending_placement, "ending"), &placement()).await.expect("the ending convoy's placement");
    placements.create(&placement_meta(admitting_placement, "admitting"), &placement()).await.expect("the admitting convoy's placement");
    create_convoy(&backend, "ending", ending_workflow, ending_placement).await.expect("create ending convoy");

    let collector = PreparedSnapshotGarbageCollector::new(backend.clone(), "flotilla");
    let released = collector.collect("ending").await.expect("the ending convoy's teardown");
    assert_eq!((released.workflows_deleted, released.placements_deleted), (1, 1));
    workflows.get(admitting_workflow).await.expect("the admitting convoy keeps its workflow");
    placements.get(admitting_placement).await.expect("the admitting convoy keeps its placement");

    let released = collector.collect("admitting").await.expect("the admitting convoy's release");
    assert_eq!((released.workflows_deleted, released.placements_deleted), (1, 1));
    assert!(matches!(
        backend.clone().definitions::<WorkflowTemplate>("flotilla").get(admitting_workflow).await,
        Err(ResourceError::NotFound { .. })
    ));
    workflows.get(unowned).await.expect("an unowned snapshot is left to the operator");
}
