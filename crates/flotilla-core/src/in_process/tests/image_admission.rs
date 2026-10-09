use std::collections::BTreeSet;
use std::sync::Arc;

use flotilla_protocol::HostName;
use flotilla_resources::{
    CrewSource, CrewSpec, InMemoryBackend, InputMeta, PlacementPolicy, ResourceBackend, VesselRequirement, WorkflowTemplateSpec,
};

use super::support::{create_docker_placement, test_meta};
use crate::config::ConfigStore;
use crate::in_process::InProcessDaemon;
use crate::providers::discovery::test_support::fake_discovery;

#[tokio::test]
async fn image_baseline_admission_fails_without_agents_and_pins_resolved_image() {
    use flotilla_resources::{CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource};

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-machine\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let mut policy = policies.get("crew-policy").await.expect("policy");
    policy.spec.docker_per_vessel.as_mut().expect("docker").image =
        DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".to_string() };
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec).await.expect("reference baseline");
    let workflow = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false)
        .await
        .expect_err("missing baseline must fail");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let error = daemon
        .resolve_convoy_placement("flotilla", None, &[], &workflow, None, false)
        .await
        .expect_err("default selection must reject missing baseline");
    assert!(error.contains("image-baseline `fleet-crew` missing/unresolved"), "{error}");
    let baselines = backend.definitions::<CrewImageBaseline>("flotilla");
    baselines
        .apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v1".to_string(), layers: None })
        .await
        .expect("baseline");
    let admitted = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("admit");
    baselines.apply(&test_meta("fleet-crew"), &CrewImageBaselineSpec { image: "crew:v2".to_string(), layers: None }).await.expect("bump");
    assert_eq!(admitted.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v1"));
    let next = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("next admission");
    assert_eq!(next.selected.expect("placement").spec.docker_per_vessel.expect("docker").image, DockerImageSource::from("crew:v2"));
}

// Behaviour (#2727): admission freezes a need-selected composition while the
// generation-1 baseline stays the running image; a missing need refuses by name.
#[tokio::test]
async fn image_layer_admission_freezes_inputs_and_keeps_baseline_authoritative() {
    use flotilla_resources::{
        CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource, ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec,
        ImageLayerStage,
    };
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"test-image-composition\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let mut policy = policies.get("crew-policy").await.expect("policy");
    policy.spec.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() };
    policies.update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec).await.expect("baseline policy");
    let base = ImageLayerSpec::builder()
        .stage(ImageLayerStage::Base)
        .parent(ImageLayerParent::Image(format!("debian@sha256:{}", "a".repeat(64))))
        .repository("https://example.test/images".into())
        .revision("1".repeat(40))
        .fragment("Dockerfile.base".into())
        .provides(BTreeSet::from(["os:debian-family".into()]))
        .build();
    let display = ImageLayerSpec::builder()
        .stage(ImageLayerStage::Capability)
        .parent(ImageLayerParent::Rebasable)
        .repository("https://example.test/images".into())
        .revision("2".repeat(40))
        .fragment("Dockerfile.display".into())
        .provides(BTreeSet::from(["display:headless-x11".into()]))
        .requires(BTreeSet::from(["os:debian-family".into()]))
        .build();
    let layers = backend.definitions::<ImageLayer>("flotilla");
    layers.apply(&test_meta("base"), &base).await.expect("base layer");
    layers.apply(&test_meta("display"), &display).await.expect("display layer");
    let baselines = backend.definitions::<CrewImageBaseline>("flotilla");
    baselines
        .apply(
            &test_meta("fleet-crew"),
            &CrewImageBaselineSpec { image: "crew:v1".into(), layers: Some(ImageLayerSelection::builder().base("base".into()).build()) },
        )
        .await
        .expect("baseline alongside layers");
    let crew = CrewSpec::builder()
        .role("tool".into())
        .source(CrewSource::Tool { command: "true".into() })
        .needs(BTreeSet::from(["display:headless-x11".parse().expect("open need")]))
        .build();
    let workflow =
        WorkflowTemplateSpec::builder().vessels(vec![VesselRequirement::builder().name("work".into()).crew(vec![crew]).build()]).build();
    let admitted = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("admission");
    let image = admitted.selected.expect("selected").spec.docker_per_vessel.expect("docker").image;
    let DockerImageSource::Composition { composition } = &image else { panic!("admission must store composition") };
    assert_eq!(composition.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), ["base", "display"]);
    assert_eq!(composition.layers[0].spec, base);
    assert!(composition.identity.is_none());
    assert_eq!(image.resolve(&baselines).await.expect("running image"), "crew:v1");
    let mut next_base = base;
    next_base.revision = "3".repeat(40);
    layers.apply(&test_meta("base"), &next_base).await.expect("update base");
    assert_eq!(composition.layers[0].spec.revision, "1".repeat(40));
    // A placement-bound composition keeps its concrete identity on readmission.
    let mut bound = composition.clone();
    bound.bind(flotilla_resources::PlacedImageIdentity { local_image_id: "sha256:placed".into(), registry_digest: None }).expect("bind");
    let mut live = policies.get("crew-policy").await.expect("live policy");
    live.spec.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Composition { composition: bound.clone() };
    let live = policies.update(&InputMeta::from(&live.metadata), &live.metadata.resource_version, &live.spec).await.expect("bound policy");
    let repeated =
        daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("readmission");
    let DockerImageSource::Composition { composition: repeated } =
        repeated.selected.expect("selected").spec.docker_per_vessel.expect("docker").image
    else {
        panic!("composition")
    };
    assert_eq!(repeated.identity, bound.identity);
    assert_eq!(repeated.layers, bound.layers);
    let mut restored = live.spec;
    restored.docker_per_vessel.as_mut().expect("docker").image = DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() };
    policies
        .update(&InputMeta::from(&live.metadata), &live.metadata.resource_version, &restored)
        .await
        .expect("restore baseline selection");
    let next = daemon.resolve_convoy_placement("flotilla", None, &[], &workflow, Some("crew-policy"), false).await.expect("next admission");
    let DockerImageSource::Composition { composition: next } =
        next.selected.expect("selected").spec.docker_per_vessel.expect("docker").image
    else {
        panic!("composition")
    };
    assert_eq!(next.layers[0].spec.revision, "3".repeat(40));
    let mut missing = workflow;
    missing.vessels[0].crew[0].needs = BTreeSet::from(["display:missing".parse().expect("need")]);
    let error =
        daemon.resolve_convoy_placement("flotilla", None, &[], &missing, Some("crew-policy"), false).await.expect_err("missing need");
    assert!(error.contains("display:missing"));
}
