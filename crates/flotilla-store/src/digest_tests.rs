use crate::*;

#[tokio::test]
async fn truncated_bucket_and_tree_are_rejected() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let local = backend.using::<Convoy>("ns");
    local
        .create(&InputMeta::builder().name("retained".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .expect("create");
    let root = local.digest(&DigestQuery::Root).await.expect("root");
    let children = local.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.expect("tree");
    let bucket = digest_bucket("retained");
    let mut snapshot = local.digest(&DigestQuery::Snapshot { expected_root: root.root.clone(), bucket }).await.expect("snapshot");
    snapshot.snapshot::<Convoy>(&children, bucket).expect("complete snapshot");
    let mut duplicate = snapshot.clone();
    duplicate.items.as_mut().expect("items").push(snapshot.items.as_ref().expect("items")[0].clone());
    assert!(duplicate.snapshot::<Convoy>(&children, bucket).is_err(), "duplicate keys are invalid");
    snapshot.items.as_mut().expect("items").clear();
    assert!(snapshot.snapshot::<Convoy>(&children, bucket).is_err(), "truncated live set is invalid");
    let mut truncated = children;
    truncated.children = Some(vec![]);
    assert!(snapshot.snapshot::<Convoy>(&truncated, bucket).is_err(), "an incomplete tree cannot prove absence");
}
