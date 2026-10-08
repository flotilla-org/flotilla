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
    definitions.apply(&meta, &CrewImageBaselineSpec { image: "crew:v2-a".to_string(), layers: None }).await.expect("local bump");
    remote
        .definitions::<CrewImageBaseline>("flotilla")
        .apply(&meta, &CrewImageBaselineSpec { image: "crew:v2-b".to_string(), layers: None })
        .await
        .expect("remote bump");
    replicas
        .replace(&remote.using::<CrewImageBaseline>("flotilla").list().await.expect("list"), Utc::now())
        .await
        .expect("replicate conflict");
    assert!(reference.resolve(&definitions).await.expect_err("conflict").contains("image-baseline `fleet-crew` missing/unresolved"));
    definitions.apply(&meta, &CrewImageBaselineSpec { image: "crew:v3".to_string(), layers: None }).await.expect("resolve conflict");
    assert_eq!(reference.resolve(&definitions).await.expect("resolved baseline"), "crew:v3");

    definitions.apply(&meta, &CrewImageBaselineSpec { image: "  ".to_string(), layers: None }).await.expect("empty image");
    assert!(reference.resolve(&definitions).await.expect_err("empty").contains("missing/unresolved"));
    definitions.delete("fleet-crew").await.expect("delete baseline");
    assert!(reference.resolve(&definitions).await.expect_err("deleted").contains("missing/unresolved"));
    assert_eq!(DockerImageSource::from("independent:v1").resolve(&definitions).await.expect("literal image"), "independent:v1");
}

// ADR 0047/#2941: previous-generation policies decode without provenance;
// frozen snapshots retain it through serialization, and literal tags never
// acquire baseline provenance merely from their spelling.
#[hegel::test]
fn placement_baseline_provenance_round_trips_and_never_infers_literal_tags(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // Cross all four live/frozen provenance combinations, and empty through
    // long literal image values. Frozen provenance takes precedence over live.
    let live = tc.draw(gs::booleans());
    let frozen = tc.draw(gs::booleans());
    let image_len = tc.draw(gs::integers::<usize>().min_value(0).max_value(70));
    let image = if live {
        json!({ "image_baseline_ref": "live-baseline" })
    } else {
        json!(if image_len == 0 { String::new() } else { format!("crew:{}", "a".repeat(image_len)) })
    };
    let mut value = json!({
        "host_ref": "host-a", "image": image,
        "checkout": { "fresh_clone_in_container": { "clone_path": "/workspace" } }
    });
    if frozen {
        value["legacy_image_baseline_ref"] = json!("frozen-baseline");
    }
    let policy: flotilla_resources::DockerPerVesselPlacementPolicySpec = serde_json::from_value(value).expect("stored policy");
    let expected = if frozen {
        Some("frozen-baseline")
    } else if live {
        Some("live-baseline")
    } else {
        None
    };
    assert_eq!(policy.legacy_image_baseline_ref(), expected);
    let encoded = serde_json::to_value(&policy).expect("write policy");
    assert_eq!(encoded.get("legacy_image_baseline_ref"), frozen.then_some(&json!("frozen-baseline")));
    let decoded: flotilla_resources::DockerPerVesselPlacementPolicySpec = serde_json::from_value(encoded).expect("read snapshot");
    assert_eq!(decoded, policy);
    assert_eq!(decoded.legacy_image_baseline_ref(), expected);
}
