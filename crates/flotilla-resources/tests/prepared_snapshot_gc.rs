use std::collections::BTreeMap;

use flotilla_resources::{
    Convoy, ConvoySpec, InMemoryBackend, InputMeta, PlacementPolicy, PlacementPolicySpec, PreparedSnapshotGarbageCollector,
    ResourceBackend, ResourceError, WorkflowTemplate, WorkflowTemplateSpec, PLACEMENT_SNAPSHOT_ANNOTATION, PLACEMENT_SNAPSHOT_KIND,
    PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_ANNOTATION, WORKFLOW_SNAPSHOT_KIND,
};

fn prepared_meta(name: &str, kind: &str) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(BTreeMap::from([(PREPARED_SNAPSHOT_LABEL.to_string(), kind.to_string())])).build()
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
            &ConvoySpec::builder().workflow_ref("logical-workflow".to_string()).placement_policy("logical-placement".to_string()).build(),
        )
        .await
        .map(|_| ())
}

#[tokio::test]
async fn shared_snapshots_are_collected_only_after_the_last_convoy_releases_them() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        .using::<PlacementPolicy>("flotilla")
        .create(&prepared_meta(placement_name, PLACEMENT_SNAPSHOT_KIND), &PlacementPolicySpec::builder().pool("remote".to_string()).build())
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
    assert!(matches!(backend.using::<PlacementPolicy>("flotilla").get(placement_name).await, Err(ResourceError::NotFound { .. })));
}

#[tokio::test]
async fn sweep_removes_unreferenced_legacy_snapshots() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let legacy_workflow = "old-convoy-remote-workflow-012345abcdef";
    let legacy_placement = "old-convoy-remote-placement-fedcba543210";
    backend
        .clone()
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &InputMeta::builder().name(legacy_workflow.to_string()).build(),
            &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
        )
        .await
        .expect("create legacy workflow");
    backend
        .clone()
        .using::<PlacementPolicy>("flotilla")
        .create(
            &InputMeta::builder().name(legacy_placement.to_string()).build(),
            &PlacementPolicySpec::builder().pool("remote".to_string()).build(),
        )
        .await
        .expect("create legacy placement");

    let result = PreparedSnapshotGarbageCollector::new(backend, "flotilla").collect(None).await.expect("sweep legacy snapshots");
    assert_eq!(result.workflows_deleted, 1);
    assert_eq!(result.placements_deleted, 1);
}
