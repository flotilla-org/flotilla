use flotilla_resources::{
    image_build_name, single_agent_workflow_spec, DockerImageSource, ImageBuild, ImageDelivery, ImageLayer, ImageLayerSource,
    ImageLayerSpec, ImageLayerTier, Project, ResourceObject,
};

use super::*;

fn layer(name: &str, tier: ImageLayerTier, provides: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0.0".into())
        .tier(tier)
        // Not stdlib: the shipped stdlib layers are built in (#3824 layer 2a); an installation
        // layer of a stdlib name takes its place.
        .source(ImageLayerSource::Installation)
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "a".repeat(64))))
        .fragment(format!("FROM ${{BASE}}\nRUN echo {name}\n"))
        .provides(provides.iter().map(|value| (*value).into()).collect())
        .probe("true".into())
        .build()
}

/// The composed image a convoy's vessel placement froze.
async fn frozen_image(backend: &ResourceBackend, convoy: &ResourceObject<Convoy>) -> DockerImageSource {
    let snapshot = flotilla_resources::vessel_placement_pin(convoy, "work")
        .map(|pin| pin.kind_ref)
        .unwrap_or_else(|| convoy.metadata.annotations[flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION].clone());
    let policy = backend.using::<PlacementPolicy>("flotilla").get(&snapshot).await.expect("placement snapshot");
    policy.spec.docker_per_vessel.expect("docker").image.expect("admission freezes the composed image")
}

// Behaviour (ADR 0062): a composed policy resolves each vessel's image at
// admission on the placement host. ConvoyStart's `--need display:x11`
// resolves by provides lookup; two convoys and a ConvoyCreate whose leg
// declares the same need share ONE ImageBuild named by identity; an
// unresolvable need refuses routing with no convoy; the issuer authors nothing.
async fn composed_image_routing_row(issuer: usize, satisfiable: bool) {
    // The fake provider stands in for the external issue tracker. Both routing
    // and admission resolve the same source-qualified issue through real ports.
    let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
    let reference = IssueRef { source: source.clone(), id: "3397".into() };
    let mut hosts = Vec::new();
    for name in ["image-home", "image-issuer"] {
        let provider = Arc::new(flotilla_discovery_testkit::FakeIssueProvider::new());
        let mut issue = TestIssue::new("Triage label").id("3397").with_labels(vec!["needs:multi-host".into()]).build();
        issue.reference = reference.clone();
        provider.add_issues(vec![(reference.id.clone(), issue)]).await;
        let discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_issue_tracker(provider));
        let config = test_config_store(tempfile::tempdir().unwrap().keep());
        hosts.push(
            InProcessDaemon::new(
                vec![],
                config,
                discovery,
                HostName::new(name),
                Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
            )
            .await,
        );
    }
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
    status.capabilities.insert("docker".into(), serde_json::json!(true));
    status.capabilities.insert("os".into(), serde_json::json!("linux"));
    backend.using::<Host>("flotilla").update_status(&host_id, &host.metadata.resource_version, &status).await.expect("Docker capacity");
    let policy = PlacementPolicySpec::builder()
        .pool("cleat".into())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: host_id.clone(),
            image: None,
            pull_policy: Default::default(),
            memory_policy: Default::default(),
            agent_adapters: BTreeSet::new(),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".into() },
        })
        .build();
    backend
        .using::<PlacementPolicy>("flotilla")
        .create(&InputMeta::builder().name("composed-docker".into()).build(), &policy)
        .await
        .expect("policy");
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(
            &InputMeta::builder().name("composed-docker".into()).build(),
            &FulfilmentKindSpec::from_policy(&policy, "linux").expect("kind"),
        )
        .await
        .expect("kind");
    let catalogue = [
        layer("base-ubuntu-24.04", ImageLayerTier::Base, &["os:ubuntu"]),
        layer("display-x11", ImageLayerTier::Shared, &["display:x11"]),
        layer("crew-utilities", ImageLayerTier::AgentRuntime, &["crew:utilities"]),
        layer("harness-codex", ImageLayerTier::AgentRuntime, &["harness:codex", "harness:codex@0.160.0"]),
        layer("harness-claude", ImageLayerTier::AgentRuntime, &["harness:claude"]),
    ];
    for spec in &catalogue {
        backend
            .definitions::<ImageLayer>("flotilla")
            .apply(&InputMeta::builder().name(spec.resource_name()).build(), &spec.clone().into())
            .await
            .expect("layer definition");
    }
    // Two legs: the first resolves, the second needs a capability nobody
    // provides; admission must refuse without leaving the first's build.
    let mut two_legs = single_agent_workflow_spec();
    let mut demo = two_legs.vessels[0].clone();
    demo.name = "demo".into();
    demo.crew[0].role = "demo".into();
    demo.needs = BTreeSet::from(["display:missing".parse().expect("leg need")]);
    two_legs.vessels.push(demo);
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("two-legs".into()).build(), &two_legs)
        .await
        .expect("two-leg workflow");
    let mut display_workflow = single_agent_workflow_spec();
    display_workflow.vessels[0].needs = BTreeSet::from(["display:x11".parse().expect("leg need")]);
    backend
        .using::<WorkflowTemplate>("flotilla")
        .create(&InputMeta::builder().name("display-workflow".into()).build(), &display_workflow)
        .await
        .expect("leg-need workflow");
    for host in &hosts {
        seed_trusted_remote_convoy_project(host, "flotilla").await;
        let projects = host.resource_backend().using::<Project>("flotilla");
        let mut project = projects.get("flotilla").await.unwrap();
        let mut binding = flotilla_resources::IssueSourceBindingSpec::from(source.clone());
        binding.alias = Some("flotilla".into());
        project.spec.issue_source_bindings = vec![binding];
        projects
            .update(&InputMeta::builder().name("flotilla".into()).build(), &project.metadata.resource_version, &project.spec)
            .await
            .unwrap();
    }
    let mesh = spawn_in_memory_request_mesh(hosts).await.expect("mesh");
    // Run the production demand service on either possible source owner; the
    // paused-clock generated scenarios must not wait on an absent refresher.
    let mut forge_services = Vec::new();
    for host in &mesh.hosts {
        let (service, ready) = flotilla_daemon::runtime::spawn_forge_read_demand_task(host.clone(), "flotilla".into());
        forge_services.push(tokio_util::task::AbortOnDropHandle::new(service));
        ready.await.expect("forge demand watches ready");
    }
    eventually(Duration::from_secs(5), Duration::from_millis(10), "image catalogue and placement replicate", || async {
        let issuer = mesh.hosts[issuer].resource_backend();
        let mut replicated = issuer.including_replicas::<Host>("flotilla").get(&host_id).await.is_ok()
            && issuer.including_replicas::<FulfilmentKind>("flotilla").get("composed-docker").await.is_ok()
            && issuer.including_replicas::<PlacementPolicy>("flotilla").get("composed-docker").await.is_ok();
        for spec in &catalogue {
            replicated &= issuer.definitions::<ImageLayer>("flotilla").get(&spec.resource_name()).await.is_ok();
        }
        replicated
    })
    .await;
    let need = if satisfiable { "display:x11" } else { "display:missing" };
    let start = |name: &str| CommandAction::ConvoyStart {
        intent: Box::new(
            ConvoyStartIntent::builder()
                .project_ref("flotilla".into())
                .name(name.into())
                .branch(format!("test/{name}"))
                .needs(vec![need.into()])
                .issues(vec![flotilla_protocol::IssueSelector::Reference(reference.clone())])
                .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                .build(),
        ),
    };
    let target = mesh.hosts[issuer].resolve_command_target(&start("composed-work"), None).await;
    if !satisfiable {
        let error = target.expect_err("unresolvable need refuses routing").to_string();
        assert!(error.contains("unresolvable image need `display:missing`"), "{error}");
        for host in &mesh.hosts {
            assert!(host.resource_backend().using::<Convoy>("flotilla").list().await.expect("convoys").items.is_empty());
            assert!(host.resource_backend().using::<ImageBuild>("flotilla").list().await.expect("builds").items.is_empty());
        }
        return;
    }
    assert_eq!(
        target.expect("route composed image").host,
        if issuer == 0 { TargetHost::Local } else { TargetHost::Placement(HostId::new(&host_id)) }
    );
    let mut events = mesh.hosts[issuer].subscribe();
    let command_id =
        mesh.clients[issuer].execute(Command::builder().action(start("composed-work")).build()).await.expect("dispatch composed admission");
    let result = await_command_result(&mut events, command_id).await;
    let CommandValue::ConvoyStarted { warnings, .. } = result else { panic!("{result:?}") };
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("3397") && warnings[0].contains("needs:multi-host"), "{warnings:?}");
    let convoys = backend.using::<Convoy>("flotilla").list().await.expect("convoys").items;
    assert_eq!(convoys.len(), 1);
    assert!(!convoys[0].metadata.annotations.contains_key("flotilla.work/frozen-image-layers"), "the retired annotation is never written");
    let DockerImageSource::Built { identity, image_build_ref } = frozen_image(&backend, &convoys[0]).await else {
        panic!("composed placement freezes a Built source");
    };
    assert_eq!(image_build_ref, image_build_name(&identity, "amd64", &ImageDelivery::Local, &host_id));
    let builds = backend.using::<ImageBuild>("flotilla").list().await.expect("builds").items;
    assert_eq!(builds.len(), 1, "admission get-or-creates the identity's build before accepting the convoy");
    let build = builds[0].spec.current().expect("current build");
    assert_eq!(builds[0].metadata.name, image_build_ref);
    assert_eq!(
        build.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(),
        ["base-ubuntu-24.04", "display-x11", "crew-utilities", "harness-codex"]
    );
    assert_eq!((build.host_ref.as_str(), &build.delivery), (host_id.as_str(), &ImageDelivery::Local));

    let command_id = mesh.clients[issuer].execute(Command::builder().action(start("composed-work-second")).build()).await.expect("second");
    assert!(matches!(await_command_result(&mut events, command_id).await, CommandValue::ConvoyStarted { .. }));
    let convoys = backend.using::<Convoy>("flotilla").list().await.expect("both admitted").items;
    assert_eq!(convoys.len(), 2);
    for convoy in &convoys {
        assert!(matches!(frozen_image(&backend, convoy).await, DockerImageSource::Built { identity: ref other, .. } if *other == identity));
    }
    assert_eq!(backend.using::<ImageBuild>("flotilla").list().await.expect("shared build").items.len(), 1);

    // ConvoyCreate freezes the same composition: the leg's need matches the
    // dispatched need, so it reuses the identity's build.
    let mut create_events = mesh.hosts[0].subscribe();
    let create = CommandAction::ConvoyCreate {
        name: "created-composed-work".into(),
        workflow_ref: "display-workflow".into(),
        inputs: Vec::new(),
        repository_url: None,
        r#ref: Some("test/created-composed-work".into()),
        project_ref: Some("flotilla".into()),
        placement_policy: Some("composed-docker".into()),
        adopted_checkout: None,
    };
    let command_id = mesh.clients[0].execute(Command::builder().action(create).build()).await.expect("create admission");
    let result = await_command_result(&mut create_events, command_id).await;
    assert!(matches!(result, CommandValue::ConvoyCreated { .. }), "{result:?}");
    let created = backend
        .using::<Convoy>("flotilla")
        .list()
        .await
        .expect("created convoy")
        .items
        .into_iter()
        .find(|convoy| convoy.spec.role == "created-composed-work")
        .expect("created role");
    assert_eq!(frozen_image(&backend, &created).await, DockerImageSource::Built { identity, image_build_ref });
    assert_eq!(backend.using::<ImageBuild>("flotilla").list().await.expect("shared create build").items.len(), 1);

    // Refusal atomicity: admission resolves every vessel before creating any
    // build, so a refused second leg leaves no build for the first.
    let mut two_leg_events = mesh.hosts[0].subscribe();
    let two_legs = CommandAction::ConvoyStart {
        intent: Box::new(
            ConvoyStartIntent::builder()
                .project_ref("flotilla".into())
                .name("two-legs".into())
                .branch("test/two-legs".into())
                .workflow_ref("two-legs".into())
                .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
                .build(),
        ),
    };
    let command_id = mesh.hosts[0].execute(Command::builder().action(two_legs).build()).await.expect("two-leg admission");
    let result = await_command_result(&mut two_leg_events, command_id).await;
    assert!(matches!(result, CommandValue::Error { ref message } if message.contains("display:missing")), "{result:?}");
    assert_eq!(backend.using::<ImageBuild>("flotilla").list().await.expect("no orphan builds").items.len(), 1);

    assert!(mesh.hosts[1].resource_backend().using::<ImageBuild>("flotilla").list().await.expect("issuer builds").items.is_empty());
    assert!(mesh.hosts[1].resource_backend().using::<Convoy>("flotilla").list().await.expect("issuer authored convoys").items.is_empty());
}

#[tokio::test]
async fn composed_image_routing_pinned_rows() {
    for (issuer, satisfiable) in [(1, true), (0, true), (1, false), (0, false)] {
        composed_image_routing_row(issuer, satisfiable).await;
    }
}

#[hegel::test]
fn generated_composed_image_routing(tc: hegel::TestCase) {
    // Both issuer locations and resolvable/unresolvable dispatched needs.
    let issuer = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
    let satisfiable = tc.draw(gs::booleans());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(composed_image_routing_row(issuer, satisfiable));
}
