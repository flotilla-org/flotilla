use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    apply_resource_document, CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource, InputMeta, ResourceBackend, SqliteBackend,
};
use rstest::rstest;
use serde_json::json;

fn baseline_ref() -> DockerImageSource {
    DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".to_string() }
}

#[rstest]
#[case::memory(ResourceBackend::InMemory(Default::default()))]
#[case::sqlite(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")))]
#[tokio::test]
async fn crew_image_baseline_merged_resolution_contract(#[case] backend: ResourceBackend) {
    let backend = backend.with_local_root(NodeId::new("root-a"));
    let remote = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new("root-b"));
    let definitions = backend.definitions::<CrewImageBaseline>("flotilla");
    let reference = baseline_ref();
    assert!(reference
        .resolve(&definitions)
        .await
        .expect_err("missing baseline")
        .contains("image-baseline `fleet-crew` missing/unresolved"));

    // Dynamic apply is the operator CLI path and must author a Definition.
    apply_resource_document(
        &remote,
        "flotilla",
        json!({
            "apiVersion": "flotilla.work/v1", "kind": "CrewImageBaseline",
            "metadata": {"name": "fleet-crew"}, "spec": {"image": "crew:v1"}
        }),
    )
    .await
    .expect("apply baseline manifest");
    let replicas = backend.replica_writer::<CrewImageBaseline>(NodeId::new("root-b"), "flotilla");
    replicas.replace(&remote.using::<CrewImageBaseline>("flotilla").list().await.expect("list"), Utc::now()).await.expect("replicate");
    assert_eq!(reference.resolve(&definitions).await.expect("replica-only baseline"), "crew:v1");
    assert!(reference.resolve(&backend.definitions("other-namespace")).await.is_err());

    // Concurrent incompatible edits must not silently select a merge winner.
    let meta = InputMeta::builder().name("fleet-crew".to_string()).build();
    definitions.apply(&meta, &CrewImageBaselineSpec { image: "crew:v2-a".to_string() }).await.expect("local bump");
    remote
        .definitions::<CrewImageBaseline>("flotilla")
        .apply(&meta, &CrewImageBaselineSpec { image: "crew:v2-b".to_string() })
        .await
        .expect("remote bump");
    replicas
        .replace(&remote.using::<CrewImageBaseline>("flotilla").list().await.expect("list"), Utc::now())
        .await
        .expect("replicate conflict");
    assert!(reference.resolve(&definitions).await.expect_err("conflict").contains("image-baseline `fleet-crew` missing/unresolved"));
    definitions.apply(&meta, &CrewImageBaselineSpec { image: "crew:v3".to_string() }).await.expect("resolve conflict");
    assert_eq!(reference.resolve(&definitions).await.expect("resolved baseline"), "crew:v3");

    definitions.apply(&meta, &CrewImageBaselineSpec { image: "  ".to_string() }).await.expect("empty image");
    assert!(reference.resolve(&definitions).await.expect_err("empty").contains("missing/unresolved"));
    definitions.delete("fleet-crew").await.expect("delete baseline");
    assert!(reference.resolve(&definitions).await.expect_err("deleted").contains("missing/unresolved"));
    assert_eq!(DockerImageSource::from("independent:v1").resolve(&definitions).await.expect("literal image"), "independent:v1");
}
