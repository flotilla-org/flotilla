use flotilla_core::image_build::{ImageBuildInputResolver, ImageBuildSourceInputs};
use flotilla_resources::{
    CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource, FrozenImageLayer, FrozenImageLayers, ImageBuild, ImageComposition,
    ImageInputStability, ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec, ImageLayerStage, IMAGE_LAYERS_ANNOTATION,
};

use super::*;

struct BuildInputs;
#[async_trait::async_trait]
impl ImageBuildInputResolver for BuildInputs {
    async fn resolve(&self, _layer: &FrozenImageLayer, architecture: &str) -> Result<ImageBuildSourceInputs, String> {
        Ok(ImageBuildSourceInputs::builder()
            .architecture(architecture.into())
            .content_hashes(vec![format!("sha256:{}", "2".repeat(64))])
            .stability(ImageInputStability::Pinned)
            .build())
    }
}

fn layer(stage: ImageLayerStage, provides: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .stage(stage)
        .parent(if stage == ImageLayerStage::Base {
            ImageLayerParent::Image(format!("debian@sha256:{}", "a".repeat(64)))
        } else {
            ImageLayerParent::Rebasable
        })
        .repository("https://example.test/images".into())
        .revision("1".repeat(40))
        .fragment("Dockerfile".into())
        .provides(provides.iter().map(|value| (*value).into()).collect())
        .requires(if stage == ImageLayerStage::Base { BTreeSet::new() } else { BTreeSet::from(["os:debian-family".into()]) })
        .build()
}

// Behaviour (#2727/#2728): routing freezes baseline or explicit composition
// on the selected host. Two admitted convoys share a pinned execution; the
// generation-1 baseline remains authoritative during the transition.
async fn layered_baseline_routing_row(issuer: usize, satisfiable: bool, build: bool) {
    let hosts = vec![empty_daemon_named("image-home").await, empty_daemon_named("image-issuer").await];
    let home = Arc::clone(&hosts[0]);
    seed_host_capacity(&home, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    home.set_local_placement_capabilities(&BTreeSet::from(["codex".into()]), &["cleat".into()]).await;
    let host_id = home.local_host_id().expect("home identity").to_string();
    let backend = home.resource_backend();
    let host = backend.using::<Host>("flotilla").get(&host_id).await.expect("host");
    let mut status = host.status.expect("status");
    status.description = Some(
        flotilla_protocol::HostSummary::builder()
            .environment_id(flotilla_protocol::EnvironmentId::new(&host_id))
            .node(flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new(&host_id), host_id.clone()))
            .system(flotilla_protocol::SystemInfo { arch: Some("x86_64".into()), ..Default::default() })
            .build(),
    );
    home.set_image_build_input_resolver(Arc::new(BuildInputs)).await;
    status.capabilities.insert("docker".into(), serde_json::json!(true));
    status.capabilities.insert("os".into(), serde_json::json!("linux"));
    backend.using::<Host>("flotilla").update_status(&host_id, &host.metadata.resource_version, &status).await.expect("Docker capacity");
    let policy = PlacementPolicySpec::builder()
        .pool("cleat".into())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: host_id.clone(),
            image: if build {
                DockerImageSource::Composition {
                    composition: Box::new(
                        ImageComposition::builder()
                            .selection(ImageLayerSelection::builder().base("base".into()).harness("harness".into()).build())
                            .needs(BTreeSet::new())
                            .layers(vec![
                                FrozenImageLayer { name: "base".into(), spec: layer(ImageLayerStage::Base, &["os:debian-family"]) },
                                FrozenImageLayer {
                                    name: "harness".into(),
                                    spec: layer(ImageLayerStage::Harness, &["harness:codex@0.160.0"]),
                                },
                            ])
                            .build(),
                    ),
                }
            } else {
                DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() }
            },
            pull_policy: Default::default(),
            memory_policy: Default::default(),
            agent_adapters: BTreeSet::from(["codex".into()]),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".into() },
        })
        .build();
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(&InputMeta::builder().name("layered-docker".into()).build(), &policy)
        .await
        .expect("policy");
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(
            &InputMeta::builder().name("layered-docker".into()).build(),
            &FulfilmentKindSpec::from_policy(&policy, "linux").expect("kind"),
        )
        .await
        .expect("kind");
    for (name, spec) in [
        ("base", layer(ImageLayerStage::Base, &["os:debian-family"])),
        ("display", layer(ImageLayerStage::Capability, &["display:headless-x11"])),
        ("harness", layer(ImageLayerStage::Harness, &["harness:codex@0.160.0"])),
    ] {
        backend
            .definitions::<ImageLayer>("flotilla")
            .apply(&InputMeta::builder().name(name.into()).build(), &spec)
            .await
            .expect("layer definition");
    }
    backend
        .definitions::<CrewImageBaseline>("flotilla")
        .apply(
            &InputMeta::builder().name("fleet-crew".into()).build(),
            &CrewImageBaselineSpec {
                image: "crew:authoritative".into(),
                layers: Some(ImageLayerSelection::builder().base("base".into()).harness("harness".into()).build()),
            },
        )
        .await
        .expect("baseline alongside layers");
    for host in &hosts {
        seed_trusted_remote_convoy_project(host, "flotilla").await;
    }
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("mesh");
    eventually(Duration::from_secs(5), Duration::from_millis(10), "image catalogue and placement replicate", || async {
        let issuer = mesh.hosts[issuer].resource_backend();
        issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
            && issuer.including_replicas::<FulfilmentKind>("flotilla").get("layered-docker").await.is_ok()
            && issuer.including_replicas::<PlacementPolicy>("flotilla").get("layered-docker").await.is_ok()
            && issuer.definitions::<ImageLayer>("flotilla").get("harness").await.is_ok()
            && issuer.definitions::<ImageLayer>("flotilla").get("display").await.is_ok()
            && issuer.definitions::<ImageLayer>("flotilla").get("base").await.is_ok()
            && issuer.definitions::<CrewImageBaseline>("flotilla").get("fleet-crew").await.is_ok()
    })
    .await;
    let need = if satisfiable { "display:headless-x11" } else { "display:missing" };
    let action = CommandAction::ConvoyStart {
        intent: Box::new(
            ConvoyStartIntent::builder()
                .project_ref("flotilla".into())
                .name("layered-work".into())
                .branch("test/layered-work".into())
                .needs(vec![need.into()])
                .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                .build(),
        ),
    };
    let target = mesh.hosts[issuer].resolve_command_target(&action, None).await;
    if !satisfiable {
        assert!(target.expect_err("unsatisfiable need refuses routing").to_string().contains(need));
        assert!(backend.using::<Convoy>("flotilla").list().await.expect("convoys").items.is_empty());
        return;
    }
    assert_eq!(
        target.expect("route composed image").host,
        if issuer == 0 { TargetHost::Local } else { TargetHost::Placement(HostId::new(&host_id)) }
    );
    let mut events = mesh.hosts[issuer].subscribe();
    let command_id = mesh.clients[issuer].execute(Command::builder().action(action).build()).await.expect("dispatch layered admission");
    let result = await_command_result(&mut events, command_id).await;
    assert!(matches!(result, CommandValue::ConvoyStarted { .. }), "{result:?}");
    let convoys = backend.using::<Convoy>("flotilla").list().await.expect("convoys").items;
    assert_eq!(convoys.len(), 1);
    let frozen: FrozenImageLayers =
        serde_json::from_str(convoys[0].metadata.annotations.get(IMAGE_LAYERS_ANNOTATION).expect("frozen layer record"))
            .expect("decode layers");
    assert_eq!(frozen.layers.keys().map(String::as_str).collect::<Vec<_>>(), ["base", "display", "harness"]);
    if build {
        assert_eq!(frozen.baseline_image, None);
        let builds = backend.using::<ImageBuild>("flotilla").list().await.expect("joined builds").items;
        assert_eq!(builds.len(), 1, "admission joins the first stage before accepting the convoy");
        assert_eq!(builds[0].spec.host_ref, host_id);
        let second = CommandAction::ConvoyStart {
            intent: Box::new(
                ConvoyStartIntent::builder()
                    .project_ref("flotilla".into())
                    .name("layered-work-second".into())
                    .branch("test/layered-work-second".into())
                    .needs(vec![need.into()])
                    .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                    .build(),
            ),
        };
        let command_id = mesh.clients[issuer].execute(Command::builder().action(second).build()).await.expect("second admission");
        assert!(matches!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyStarted { .. }));
        assert_eq!(backend.using::<Convoy>("flotilla").list().await.expect("both admitted").items.len(), 2);
        assert_eq!(backend.using::<ImageBuild>("flotilla").list().await.expect("shared execution").items.len(), 1);

        assert!(mesh.hosts[1].resource_backend().using::<ImageBuild>("flotilla").list().await.expect("issuer builds").items.is_empty());
    } else {
        assert_eq!(frozen.baseline_image.as_deref(), Some("crew:authoritative"));
    }
    assert!(mesh.hosts[1].resource_backend().using::<Convoy>("flotilla").list().await.expect("issuer authored convoys").items.is_empty());
}

#[tokio::test]
async fn layered_baseline_routing_pinned_rows() {
    for (issuer, satisfiable, build) in
        [(1, true, false), (0, true, false), (1, false, false), (1, true, true), (0, true, true), (1, false, true)]
    {
        layered_baseline_routing_row(issuer, satisfiable, build).await;
    }
}

#[hegel::test]
fn generated_layered_baseline_routing(tc: hegel::TestCase) {
    // Both issuer locations and satisfiable/unsatisfiable catalogue needs.
    let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
    let satisfiable = tc.draw(gs::booleans());
    let build = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(layered_baseline_routing_row(issuer, satisfiable, build));
}
