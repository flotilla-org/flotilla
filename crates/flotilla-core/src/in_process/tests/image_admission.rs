//! Composed-image admission (ADR 0062): each vessel's ordered layer list comes
//! from the project's `image:` (else the installation defaults), its leg and
//! crew image needs, and the harness layers its roles use; admission
//! get-or-creates one ImageBuild per identity and architecture and freezes
//! `Built { identity, image_build_ref }` into the vessel's placement.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use flotilla_config::ConfigStore;
use flotilla_protocol::{HostName, PlacementDecision};
use flotilla_resources::{
    image_build_name, image_identity, CrewDefaults, CrewDefaultsSpec, CrewSource, CrewSpec, DockerImageSource, FleetDesignation,
    FleetDesignationSpec, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, Host, ImageBuild, ImageBuildCapacity,
    ImageBuildReservation, ImageCacheBinding, ImageDelivery, ImageLayer, ImageLayerSource, ImageLayerSpec, ImageLayerTier, ImageSelection,
    Project, ProjectSpec, VesselRequirement, WorkflowTemplateSpec, FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

use super::support::{create_docker_placement, test_meta};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_admission::convoy_admission::PlacementResolution;

const NS: &str = "flotilla";

fn layer(name: &str, tier: ImageLayerTier, provides: &[&str], requires: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0.0".into())
        .tier(tier)
        .source(if tier == ImageLayerTier::Project { ImageLayerSource::Project } else { ImageLayerSource::Installation })
        .maybe_project((tier == ImageLayerTier::Project).then(|| "flotilla".to_string()))
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "a".repeat(64))))
        .fragment(format!("FROM ${{BASE}}\nRUN echo {name}\n"))
        .provides(provides.iter().map(|capability| capability.to_string()).collect())
        .requires(requires.iter().map(|capability| capability.to_string()).collect())
        .probe("true".into())
        .build()
}

/// Installation layers named like the shipped stdlib layers, so each shadows its always-visible
/// stdlib namesake and the catalogue keeps one base.
fn catalogue() -> Vec<ImageLayerSpec> {
    vec![
        layer("base-ubuntu-24.04", ImageLayerTier::Base, &["os:ubuntu"], &[]),
        layer("rust", ImageLayerTier::Shared, &["toolchain:rust"], &[]),
        layer("zig", ImageLayerTier::Shared, &["toolchain:zig"], &[]),
        layer("display-x11", ImageLayerTier::Shared, &["display:x11"], &[]),
        layer("crew-utilities", ImageLayerTier::AgentRuntime, &["crew:utilities"], &[]),
        layer("harness-codex", ImageLayerTier::AgentRuntime, &["harness:codex", "harness:codex@0.160.0"], &[]),
        layer("harness-claude", ImageLayerTier::AgentRuntime, &["harness:claude"], &[]),
    ]
}

struct Fixture {
    daemon: Arc<InProcessDaemon>,
    backend: ResourceBackend,
    _temp: tempfile::TempDir,
}

async fn fixture(layers: Vec<ImageLayerSpec>, image: Option<ImageSelection>) -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"image-admission\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    create_docker_placement(&backend, "crew-policy", "host-a", BTreeSet::new()).await;
    let kinds = backend.using::<FulfilmentKind>(NS);
    let mut kind = kinds.get("crew-policy").await.expect("kind");
    kind.spec.realisation = FulfilmentRealisation::DockerPerVessel { image: None };
    kinds.update(&test_meta("crew-policy"), &kind.metadata.resource_version, &kind.spec).await.expect("composed kind");
    set_architecture(&backend, "host-a", "x86_64", None).await;
    for spec in layers {
        backend.definitions::<ImageLayer>(NS).apply(&test_meta(&spec.resource_name()), &spec.into()).await.expect("layer");
    }
    backend
        .using::<Project>(NS)
        .create(&test_meta("flotilla"), &ProjectSpec::builder().display_name("flotilla".into()).maybe_image(image).build())
        .await
        .expect("project");
    Fixture { daemon, backend, _temp: temp }
}

async fn set_architecture(backend: &ResourceBackend, name: &str, arch: &str, capacity: Option<ImageBuildCapacity>) {
    let hosts = backend.using::<Host>(NS);
    let host = match hosts.get(name).await {
        Ok(host) => host,
        Err(_) => hosts.create(&test_meta(name), &flotilla_resources::HostSpec::default()).await.expect("host"),
    };
    if capacity.is_some() {
        let mut spec = host.spec.clone();
        spec.image_build_capacity = capacity;
        hosts.update(&test_meta(name), &host.metadata.resource_version, &spec).await.expect("capacity");
    }
    let host = hosts.get(name).await.expect("host");
    let mut status = host.status.clone().unwrap_or_default();
    status.description = Some(
        flotilla_protocol::HostSummary::builder()
            .environment_id(flotilla_protocol::EnvironmentId::new(name))
            .node(flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new(name), name.to_string()))
            .system(flotilla_protocol::SystemInfo { arch: Some(arch.into()), ..Default::default() })
            .build(),
    );
    hosts.update_status(name, &host.metadata.resource_version, &status).await.expect("architecture");
}

fn agent(role: &str, adapter: &str) -> CrewSpec {
    CrewSpec::builder()
        .role(role.into())
        .source(CrewSource::Agent {
            selector: flotilla_resources::Selector { capability: "code".into(), adapter: Some(adapter.into()), model: None },
            prompt: None,
            brief_template: None,
        })
        .build()
}

fn vessel(name: &str, crew: Vec<CrewSpec>, needs: &[&str]) -> VesselRequirement {
    VesselRequirement::builder().name(name.into()).crew(crew).needs(needs.iter().map(|need| need.parse().expect("need")).collect()).build()
}

fn decision() -> PlacementDecision {
    PlacementDecision::builder()
        .kind("crew-policy".into())
        .target_host(flotilla_protocol::PlacementTargetHost {
            reference: flotilla_protocol::CanonicalHostId::resolved("host-a"),
            display_name: "host-a".into(),
        })
        .build()
}

/// Admit `workflow` through ConvoyCreate's placement freeze, returning the
/// convoy-level policy image and any per-vessel pins.
async fn admit(
    fixture: &Fixture,
    workflow: &WorkflowTemplateSpec,
) -> Result<(Option<DockerImageSource>, BTreeMap<String, Option<DockerImageSource>>), String> {
    let kind = fixture.backend.using::<FulfilmentKind>(NS).get("crew-policy").await.expect("kind");
    let mut placement = PlacementResolution::selected(kind);
    let (pins, builds) =
        fixture.daemon.admission.join_convoy_placement_builds(NS, Some("flotilla"), workflow, &mut placement, Some(&decision())).await?;
    assert!(fixture
        .backend
        .using::<ImageBuild>(NS)
        .list()
        .await
        .expect("builds")
        .items
        .iter()
        .all(|build| builds.iter().all(|planned| planned.create.is_none() || planned.name != build.metadata.name)));
    // Admission commits its planned builds with the convoy.
    flotilla_admission::image_build::create_planned_builds(&fixture.backend, NS, &builds).await?;
    let image = docker_image(&placement.selected.expect("selected").spec);
    Ok((image, pins.into_iter().map(|(vessel, (spec, _))| (vessel, docker_image(&spec))).collect()))
}

fn docker_image(spec: &FulfilmentKindSpec) -> Option<DockerImageSource> {
    match &spec.realisation {
        FulfilmentRealisation::DockerPerVessel { image } => image.clone(),
        other => panic!("Docker realisation expected: {other:?}"),
    }
}

fn layer_names(build: &flotilla_resources::ResourceObject<ImageBuild>) -> Vec<String> {
    build.spec.current().expect("current build").layers.iter().map(|layer| layer.name.clone()).collect()
}

async fn build_for(fixture: &Fixture, image: &Option<DockerImageSource>) -> flotilla_resources::ResourceObject<ImageBuild> {
    let Some(DockerImageSource::Built { identity, image_build_ref }) = image else {
        panic!("composed image is frozen as Built: {image:?}")
    };
    let build = fixture.backend.using::<ImageBuild>(NS).get(image_build_ref).await.expect("image build");
    let spec = build.spec.current().expect("current build");
    assert_eq!(&spec.identity, identity);
    assert_eq!(image_identity(&spec.layers), *identity, "the recorded identity is the layers' identity");
    assert_eq!(*image_build_ref, spec.resource_name());
    assert!(image_build_ref.starts_with(&image_build_name(identity, "amd64", &ImageDelivery::Registry { repository: String::new() }, "")));
    build
}

// The project's selection, its project layer and the harness its roles use
// make up the image, in tier order; without a registry the placement host
// builds locally. A second admission of the same composition shares the build.
#[tokio::test]
async fn project_selection_and_harness_derivation_freeze_one_build_per_identity() {
    let mut layers = catalogue();
    layers.push(layer("flotilla-native", ImageLayerTier::Project, &["project:flotilla"], &["toolchain:rust"]));
    let fixture = fixture(layers, Some(ImageSelection { base: None, layers: vec!["rust".into()] })).await;
    let workflow = WorkflowTemplateSpec::builder().vessels(vec![vessel("work", vec![agent("coder", "codex")], &[])]).build();
    let (image, pins) = admit(&fixture, &workflow).await.expect("admit");
    assert!(pins.is_empty(), "one vessel needs no per-vessel pins");
    let build = build_for(&fixture, &image).await;
    assert_eq!(layer_names(&build), ["base-ubuntu-24.04", "rust", "flotilla-native", "crew-utilities", "harness-codex"]);
    let spec = build.spec.current().expect("current");
    assert_eq!(spec.delivery, ImageDelivery::Local);
    assert_eq!(spec.host_ref, "host-a");
    let (again, _) = admit(&fixture, &workflow).await.expect("readmit");
    assert_eq!(again, image);
    assert_eq!(fixture.backend.using::<ImageBuild>(NS).list().await.expect("builds").items.len(), 1);
}

// Installation defaults (fleet-root CrewDefaults) apply only when the project
// declares no `image:`.
#[tokio::test]
async fn installation_defaults_apply_when_the_project_declares_nothing() {
    let fixture = fixture(catalogue(), None).await;
    fixture
        .backend
        .definitions::<CrewDefaults>(NS)
        .apply(
            &test_meta("fleet"),
            &CrewDefaultsSpec { image: Some(ImageSelection { base: None, layers: vec!["zig".into()] }), ..Default::default() },
        )
        .await
        .expect("defaults");
    let workflow = WorkflowTemplateSpec::builder().vessels(vec![vessel("work", vec![agent("coder", "claude")], &[])]).build();
    let (image, _) = admit(&fixture, &workflow).await.expect("admit");
    assert_eq!(layer_names(&build_for(&fixture, &image).await), ["base-ubuntu-24.04", "zig", "crew-utilities", "harness-claude"]);

    let declared = fixture.backend.using::<Project>(NS).get("flotilla").await.expect("project");
    let mut spec = declared.spec.clone();
    spec.image = Some(ImageSelection { base: None, layers: vec!["rust".into()] });
    fixture.backend.using::<Project>(NS).update(&test_meta("flotilla"), &declared.metadata.resource_version, &spec).await.expect("declare");
    let (image, _) = admit(&fixture, &workflow).await.expect("admit with project image");
    assert_eq!(layer_names(&build_for(&fixture, &image).await), ["base-ubuntu-24.04", "rust", "crew-utilities", "harness-claude"]);
}

// Leg needs add shared layers by provides lookup to that vessel only; vessels
// with different images get their own pinned placements.
#[tokio::test]
async fn leg_needs_compose_per_vessel_images() {
    let fixture = fixture(catalogue(), Some(ImageSelection::default())).await;
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![
            vessel("demo", vec![agent("coder", "codex")], &["display:x11"]),
            vessel("review", vec![agent("reviewer", "codex")], &[]),
        ])
        .build();
    let (_, pins) = admit(&fixture, &workflow).await.expect("admit");
    assert_eq!(pins.len(), 2, "different images pin each vessel");
    assert_eq!(
        layer_names(&build_for(&fixture, &pins["demo"]).await),
        ["base-ubuntu-24.04", "display-x11", "crew-utilities", "harness-codex"]
    );
    assert_eq!(layer_names(&build_for(&fixture, &pins["review"]).await), ["base-ubuntu-24.04", "crew-utilities", "harness-codex"]);
}

// Two providers of one need, or none, refuse admission naming the need, and
// no ImageBuild is authored.
#[tokio::test]
async fn duplicate_and_unresolvable_needs_refuse_without_builds() {
    let mut layers = catalogue();
    layers.push(layer("display-wayland", ImageLayerTier::Shared, &["display:x11"], &[]));
    let fixture = fixture(layers, None).await;
    let workflow =
        |need: &str| WorkflowTemplateSpec::builder().vessels(vec![vessel("work", vec![agent("coder", "codex")], &[need])]).build();
    let error = admit(&fixture, &workflow("display:x11")).await.expect_err("duplicate providers");
    assert!(error.contains("duplicate providers for `display:x11`: display-wayland, display-x11"), "{error}");
    let error = admit(&fixture, &workflow("toolchain:go")).await.expect_err("unresolvable");
    assert!(error.contains("unresolvable image need `toolchain:go`"), "{error}");
    assert!(fixture.backend.using::<ImageBuild>(NS).list().await.expect("builds").items.is_empty());
}

// PR #3178 review: a named project that is not defined refuses rather than
// composing from installation defaults the project never selected.
#[tokio::test]
async fn missing_project_refuses_instead_of_using_installation_defaults() {
    let fixture = fixture(catalogue(), None).await;
    fixture.backend.using::<Project>(NS).delete("flotilla").await.expect("delete project");
    let workflow = WorkflowTemplateSpec::builder().vessels(vec![vessel("work", vec![agent("coder", "codex")], &[])]).build();
    let error = admit(&fixture, &workflow).await.expect_err("missing project");
    assert!(error.contains("project `flotilla` is not defined"), "{error}");
    assert!(fixture.backend.using::<ImageBuild>(NS).list().await.expect("builds").items.is_empty());
}

// With a fleet image cache, the lowest-named same-architecture builder pushes
// to the registry, whichever host the vessel is placed on.
#[tokio::test]
async fn registry_delivery_uses_the_deterministic_builder() {
    let fixture = fixture(catalogue(), None).await;
    let builder = |slots| {
        Some(ImageBuildCapacity::Builder {
            architecture: "amd64".into(),
            slots,
            reservation: ImageBuildReservation { cpu: 2, disk_bytes: 1 << 30 },
        })
    };
    let workflow = WorkflowTemplateSpec::builder().vessels(vec![vessel("work", vec![agent("coder", "codex")], &[])]).build();
    let (local, _) = admit(&fixture, &workflow).await.expect("local admission before a registry exists");
    let local = build_for(&fixture, &local).await;
    assert_eq!(local.spec.current().expect("current").delivery, ImageDelivery::Local);
    assert!(local.metadata.name.ends_with("-host-a"), "local builds carry their builder: {}", local.metadata.name);
    set_architecture(&fixture.backend, "udder", "x86_64", builder(1)).await;
    set_architecture(&fixture.backend, "feta", "x86_64", builder(2)).await;
    set_architecture(&fixture.backend, "arm", "aarch64", None).await;
    fixture
        .backend
        .definitions::<FleetDesignation>(NS)
        .apply(
            &test_meta(FLEET_DESIGNATION_NAME),
            &FleetDesignationSpec {
                fleet: None,
                project: "flotilla".into(),
                image_cache: Some(ImageCacheBinding {
                    repository: "registry.test/flotilla/crew-images".into(),
                    pull_credential: "pull".into(),
                    push_credential: "push".into(),
                }),
            },
        )
        .await
        .expect("fleet");
    let (image, _) = admit(&fixture, &workflow).await.expect("admit");
    let build = build_for(&fixture, &image).await;
    assert_ne!(build.metadata.name, local.metadata.name, "a registry configured later creates a registry build");
    assert_eq!(fixture.backend.using::<ImageBuild>(NS).list().await.expect("builds").items.len(), 2);
    let spec = build.spec.current().expect("current");
    assert_eq!(spec.host_ref, "feta");
    assert_eq!(spec.delivery, ImageDelivery::Registry { repository: "registry.test/flotilla/crew-images".into() });
}
