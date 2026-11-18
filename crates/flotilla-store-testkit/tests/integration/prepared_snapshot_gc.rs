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

/// A placement snapshot as admission writes it: labelled with its convoy.
fn placement_meta(name: &str, convoy: &str) -> InputMeta {
    let mut meta = prepared_meta(name, PLACEMENT_SNAPSHOT_KIND);
    meta.labels.insert(CONVOY_LABEL.to_string(), convoy.to_string());
    meta
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
async fn shared_snapshots_are_collected_only_after_the_last_convoy_releases_them() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let workflow_name = "workflow-snapshot-012345abcdef";
    let placement_name = "placement-snapshot-012345abcdef";
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&prepared_meta(workflow_name, WORKFLOW_SNAPSHOT_KIND), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
        .await
        .expect("create prepared workflow");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(&placement_meta(placement_name, "second"), &placement())
        .await
        .expect("create prepared placement");
    create_convoy(&backend, "first", workflow_name, placement_name).await.expect("create first convoy");
    create_convoy(&backend, "second", workflow_name, placement_name).await.expect("create second convoy");

    let collector = PreparedSnapshotGarbageCollector::new(backend.clone(), "flotilla");
    assert_eq!(collector.collect(Some("first")).await.expect("collect while shared").workflows_deleted, 0);

    backend.clone().using::<Convoy>("flotilla").delete("first").await.expect("delete first convoy");
    let result = collector.collect(Some("second")).await.expect("collect final references");
    assert_eq!(result.workflows_deleted, 1);
    assert_eq!(result.placements_deleted, 1);
    assert!(matches!(
        backend.clone().definitions::<WorkflowTemplate>("flotilla").get(workflow_name).await,
        Err(ResourceError::NotFound { .. })
    ));
    assert!(matches!(backend.using::<FulfilmentKind>("flotilla").get(placement_name).await, Err(ResourceError::NotFound { .. })));
}

#[tokio::test]
async fn replicated_convoy_keeps_an_authored_snapshot_alive() {
    let author = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("author"));
    let remote = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("remote"));
    let workflow_name = "workflow-snapshot-012345abcdef";
    let placement_name = "placement-snapshot-012345abcdef";
    author
        .using::<WorkflowTemplate>("flotilla")
        .create(&prepared_meta(workflow_name, WORKFLOW_SNAPSHOT_KIND), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
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
    let retained = collector.collect(Some("remote-convoy")).await.expect("collect while same-named local convoy is deleting");
    assert_eq!(retained.workflows_deleted, 0);
    assert_eq!(retained.placements_deleted, 0);

    remote.using::<Convoy>("flotilla").delete("remote-convoy").await.expect("remove remote convoy");
    replica
        .replace(&remote.using::<Convoy>("flotilla").list().await.expect("empty remote convoys"), Utc::now())
        .await
        .expect("replicate removal");
    let collected = collector.collect(Some("remote-convoy")).await.expect("collect after remote release");
    assert_eq!(collected.workflows_deleted, 1);
    assert_eq!(collected.placements_deleted, 1);
}

// Admission writes a convoy's placement snapshot before it admits the convoy: another
// convoy's teardown, or a sweep, meanwhile must leave it to its own convoy.
#[tokio::test]
async fn a_placement_snapshot_written_before_its_convoy_is_released_only_by_that_convoy() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let workflow_name = "workflow-snapshot-012345abcdef";
    let ending_placement = "placement-snapshot-012345abcdef";
    let admitting_placement = "placement-snapshot-fedcba543210";
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(&prepared_meta(workflow_name, WORKFLOW_SNAPSHOT_KIND), &WorkflowTemplateSpec::builder().vessels(Vec::new()).build())
        .await
        .expect("create prepared workflow");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(&placement_meta(ending_placement, "ending"), &placement())
        .await
        .expect("create ending convoy's placement");
    backend
        .clone()
        .using::<FulfilmentKind>("flotilla")
        .create(&placement_meta(admitting_placement, "admitting"), &placement())
        .await
        .expect("create admitting convoy's placement");
    create_convoy(&backend, "ending", workflow_name, ending_placement).await.expect("create ending convoy");

    let collector = PreparedSnapshotGarbageCollector::new(backend.clone(), "flotilla");
    assert_eq!(collector.collect(Some("ending")).await.expect("ending convoy's teardown").placements_deleted, 1);
    assert_eq!(collector.collect(None).await.expect("sweep").placements_deleted, 0);
    backend.clone().using::<FulfilmentKind>("flotilla").get(admitting_placement).await.expect("the admitting convoy keeps its placement");

    assert_eq!(collector.collect(Some("admitting")).await.expect("admitting convoy's release").placements_deleted, 1);
    assert!(matches!(backend.using::<FulfilmentKind>("flotilla").get(admitting_placement).await, Err(ResourceError::NotFound { .. })));
}
