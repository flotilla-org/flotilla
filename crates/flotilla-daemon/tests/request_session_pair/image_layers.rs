use flotilla_resources::{
    CrewImageBaseline, CrewImageBaselineSpec, DockerImageSource, FrozenImageLayers, ImageLayer, ImageLayerParent, ImageLayerSelection,
    ImageLayerSpec, ImageLayerStage, IMAGE_LAYERS_ANNOTATION,
};

use super::*;

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

// Behaviour (#2727): automatic routing can select a layered baseline before
// admission freezes it. Admission belongs to the selected host, freezes the
// canonical composition, and retains the generation-1 running baseline.
async fn layered_baseline_routing_row(issuer: usize, satisfiable: bool) {
    let hosts = vec![empty_daemon_named("image-home").await, empty_daemon_named("image-issuer").await];
    let home = Arc::clone(&hosts[0]);
    seed_host_capacity(&home, 100 * 1024 * 1024 * 1024, 20 * 1024 * 1024 * 1024).await;
    home.set_local_placement_capabilities(&BTreeSet::from(["codex".into()]), &["cleat".into()]).await;
    let host_id = home.local_host_id().expect("home identity").to_string();
    let backend = home.resource_backend();
    let host = backend.using::<Host>("flotilla").get(&host_id).await.expect("host");
    let mut status = host.status.expect("status");
    status.capabilities.insert("docker".into(), serde_json::json!(true));
    status.capabilities.insert("os".into(), serde_json::json!("linux"));
    backend.using::<Host>("flotilla").update_status(&host_id, &host.metadata.resource_version, &status).await.expect("Docker capacity");
    let policy = PlacementPolicySpec::builder()
        .pool("cleat".into())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: host_id.clone(),
            image: DockerImageSource::Baseline { image_baseline_ref: "fleet-crew".into() },
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
        .apply(&InputMeta::builder().name("fleet-crew".into()).build(), &CrewImageBaselineSpec {
            image: "crew:authoritative".into(),
            layers: Some(ImageLayerSelection::builder().base("base".into()).harness("harness".into()).build()),
        })
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
    assert_eq!(frozen.baseline_image.as_deref(), Some("crew:authoritative"));
    assert!(mesh.hosts[1].resource_backend().using::<Convoy>("flotilla").list().await.expect("issuer authored convoys").items.is_empty());
}

#[tokio::test]
async fn layered_baseline_routing_pinned_rows() {
    for (issuer, satisfiable) in [(1, true), (0, true), (1, false)] {
        layered_baseline_routing_row(issuer, satisfiable).await;
    }
}

#[hegel::test]
fn generated_layered_baseline_routing(tc: hegel::TestCase) {
    // Both issuer locations and satisfiable/unsatisfiable catalogue needs.
    let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
    let satisfiable = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(layered_baseline_routing_row(issuer, satisfiable));
}
