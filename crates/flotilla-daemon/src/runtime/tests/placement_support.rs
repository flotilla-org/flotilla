use super::*;

pub(super) async fn replicate_local_kind<T: Resource>(source: &InProcessDaemon, target: &InProcessDaemon) {
    let listed = source
        .resource_backend()
        .using::<T>(NAMESPACE)
        .list()
        .await
        .unwrap_or_else(|error| panic!("list {} for test replication: {error}", T::API_PATHS.kind));
    target
        .resource_backend()
        .replica_writer::<T>(source.node_id().clone(), NAMESPACE)
        .replace(&listed, Utc::now())
        .await
        .unwrap_or_else(|error| panic!("replicate {} in test: {error}", T::API_PATHS.kind));
}

pub(super) async fn bridge_runtime_resource_stores(first: &InProcessDaemon, second: &InProcessDaemon) {
    replicate_local_kind::<Convoy>(first, second).await;
    replicate_local_kind::<Vessel>(first, second).await;
    replicate_local_kind::<PlacementPolicy>(first, second).await;
    replicate_local_kind::<ResourceCheckout>(first, second).await;
    replicate_local_kind::<TerminalSession>(first, second).await;

    replicate_local_kind::<Convoy>(second, first).await;
    replicate_local_kind::<Vessel>(second, first).await;
    replicate_local_kind::<PlacementPolicy>(second, first).await;
    replicate_local_kind::<ResourceCheckout>(second, first).await;
    replicate_local_kind::<TerminalSession>(second, first).await;
}

pub(super) async fn remote_shared_clone_placement_reaches_running(docker: bool) {
    let temp = TempDir::new().expect("tempdir");
    let kiwi_config_path = temp.path().join("kiwi-config");
    let feta_config_path = temp.path().join("feta-config");
    fs::create_dir_all(&kiwi_config_path).expect("kiwi config");
    fs::create_dir_all(&feta_config_path).expect("feta config");
    fs::write(kiwi_config_path.join("daemon.toml"), "machine_id = \"kiwi-root\"\n").expect("kiwi daemon config");
    fs::write(feta_config_path.join("daemon.toml"), "machine_id = \"feta-root\"\n").expect("feta daemon config");
    let kiwi_config = Arc::new(ConfigStore::with_base(kiwi_config_path));
    let feta_config = Arc::new(ConfigStore::with_base(feta_config_path));
    let kiwi = in_memory_daemon(Vec::new(), Arc::clone(&kiwi_config)).await;
    let feta = in_memory_daemon(Vec::new(), Arc::clone(&feta_config)).await;
    let kiwi_host_ref = kiwi.local_host_id().expect("kiwi Host identity").to_string();
    let feta_host_ref = feta.local_host_id().expect("feta Host identity").to_string();
    assert_ne!(kiwi_host_ref, feta_host_ref);

    let kiwi_profile = LocalProvisioningProfile {
        repo_default_dir: temp.path().join("kiwi-repos").display().to_string(),
        ..manual_profile(&kiwi_host_ref, false)
    };
    let feta_profile = LocalProvisioningProfile {
        repo_default_dir: temp.path().join("feta-repos").display().to_string(),
        ..manual_profile(&feta_host_ref, docker)
    };
    fs::create_dir_all(&kiwi_profile.repo_default_dir).expect("kiwi repo root");
    fs::create_dir_all(&feta_profile.repo_default_dir).expect("feta repo root");
    register_startup_resources(&kiwi, NAMESPACE, &kiwi_profile).await.expect("register kiwi resources");
    register_startup_resources(&feta, NAMESPACE, &feta_profile).await.expect("register feta resources");
    assert!(
        feta.resource_backend()
            .replica_writer::<Convoy>(kiwi.node_id().clone(), NAMESPACE)
            .cursor()
            .await
            .expect("read initial Convoy replica cursor")
            .is_none(),
        "the placement host must begin with an empty replica store"
    );
    assert!(
        feta.resource_backend()
            .replica_writer::<Vessel>(kiwi.node_id().clone(), NAMESPACE)
            .cursor()
            .await
            .expect("read initial Vessel replica cursor")
            .is_none(),
        "the placement host must begin with an empty replica store"
    );

    let kiwi_state = Arc::new(ControllerRuntimeState::new(
        Arc::clone(&kiwi),
        Arc::clone(&kiwi_config),
        passthrough_registry(),
        None,
        kiwi_host_ref.clone(),
        kiwi_profile.host_direct_environment_name(),
    ));
    let feta_registry = if docker {
        let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
            id: EnvironmentId::new("env-remote-placement-work"),
            image: ImageId::new("test-image"),
            runner: Arc::new(ProcessCommandRunner),
            env_vars: HashMap::from([("HOME".to_string(), temp.path().join("container-home").display().to_string())]),
            destroyed: Arc::new(AtomicBool::new(false)),
        });
        passthrough_registry_with_environment(handle)
    } else {
        passthrough_registry()
    };
    let feta_state = Arc::new(
        ControllerRuntimeState::new(
            Arc::clone(&feta),
            Arc::clone(&feta_config),
            feta_registry,
            docker.then(|| DaemonHostPath::new(temp.path().join("flotilla.sock"))),
            feta_host_ref.clone(),
            feta_profile.host_direct_environment_name(),
        )
        .with_environment_tools(fixed_environment_tools(temp.path())),
    );
    let mut controller_handles = spawn_controller_loops(
        kiwi_state,
        NAMESPACE,
        Duration::from_millis(25),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    );
    controller_handles.extend(spawn_controller_loops(
        feta_state,
        NAMESPACE,
        Duration::from_millis(25),
        ControllerSupervision::default(),
        RuntimeHealth::default(),
    ));
    let bridge = tokio::spawn({
        let kiwi = Arc::clone(&kiwi);
        let feta = Arc::clone(&feta);
        async move {
            loop {
                bridge_runtime_resource_stores(&kiwi, &feta).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });

    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let repo_url = format!("file://{}", source.path().display());
    let repository_spec = RepositorySpec::remote(&repo_url).expect("test repository spec");
    let repository_key = repository_spec.key();
    let workflow_name = if docker { "remote-docker" } else { "remote-host-direct" };
    kiwi.resource_backend()
        .using::<WorkflowTemplate>(NAMESPACE)
        .create(
            &empty_meta(workflow_name),
            &WorkflowTemplateSpec::builder()
                .exit(flotilla_resources::ExitDeclaration::standard_table())
                .inputs(Vec::new())
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .crew(vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "true".to_string() })
                        .build()])
                    .build()])
                .build(),
        )
        .await
        .expect("create workflow");
    let policy_name = if docker { "remote-docker-feta" } else { "remote-host-direct-feta" };
    let policy = if docker {
        PlacementPolicySpec::builder()
            .pool("passthrough".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                legacy_image_baseline_ref: None,
                memory_policy: Default::default(),
                host_ref: feta_host_ref.clone(),
                image: "test-image".to_string().into(),
                pull_policy: Default::default(),
                agent_adapters: BTreeSet::new(),
                default_cwd: Some("/workspace".to_string()),
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
            })
            .build()
    } else {
        PlacementPolicySpec::builder()
            .pool("passthrough".to_string())
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: feta_host_ref.clone(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build()
    };
    kiwi.resource_backend()
        .using::<PlacementPolicy>(NAMESPACE)
        .create(&empty_meta(policy_name), &policy)
        .await
        .expect("create remote placement policy");
    let convoys = kiwi.resource_backend().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &empty_meta("remote-placement"),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: workflow_name.to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: Some(policy_name.to_string()),
                repositories: vec![ConvoyRepositorySpec {
                    url: repo_url,
                    repo_ref: repository_key,
                    source_ref: "main".to_string(),
                    target_ref: "main".to_string(),
                    workspace_slug: repository_spec.leaf_slug(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("remote-placement".to_string()),
                project_ref: None,
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
            },
        )
        .await
        .expect("create admitting Convoy");
    convoys
        .update_status(
            "remote-placement",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                placement_decision: Some(PlacementDecision {
                    minimal_alternatives: Vec::new(),
                    escalation_reason: None,
                    policy_name: policy_name.to_string(),
                    target_host: PlacementTargetHost {
                        reference: CanonicalHostId::resolved(feta_host_ref.clone()),
                        display_name: "feta".to_string(),
                    },
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                }),
                ..ConvoyStatus::default()
            },
        )
        .await
        .expect("record remote placement");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let running = convoys.get("remote-placement").await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
            status.phase == ConvoyPhase::Active && status.work.get("work").is_some_and(|work| work.phase == WorkPhase::Running)
        });
        if running {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let kiwi_convoy = convoys.get("remote-placement").await.expect("kiwi Convoy");
            let kiwi_vessels = kiwi.resource_backend().using::<Vessel>(NAMESPACE).list().await.expect("kiwi Vessels");
            let feta_vessels = feta.resource_backend().using::<Vessel>(NAMESPACE).list().await.expect("feta Vessels");
            let feta_clones = feta.resource_backend().using::<Clone>(NAMESPACE).list().await.expect("feta Clones");
            let feta_checkouts = feta.resource_backend().using::<ResourceCheckout>(NAMESPACE).list().await.expect("feta Checkouts");
            let feta_terminals = feta.resource_backend().using::<TerminalSession>(NAMESPACE).list().await.expect("feta TerminalSessions");
            let feta_environments = feta.resource_backend().using::<Environment>(NAMESPACE).list().await.expect("feta Environments");
            panic!(
                "remote placement did not reach Running: convoy={kiwi_convoy:?} kiwi_vessels={kiwi_vessels:?} \
                     feta_vessels={feta_vessels:?} feta_clones={feta_clones:?} feta_checkouts={feta_checkouts:?} \
                     feta_terminals={feta_terminals:?} feta_environments={feta_environments:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let actuator = feta.resource_backend().using::<Vessel>(NAMESPACE).get("remote-placement-work").await.expect("feta actuator Vessel");
    assert_eq!(actuator.status.as_ref().map(|status| status.phase), Some(flotilla_resources::VesselPhase::Ready));
    assert_eq!(actuator.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).map(String::as_str), Some(feta_host_ref.as_str()));
    let owner = kiwi.resource_backend().using::<Vessel>(NAMESPACE).get("remote-placement-work").await.expect("kiwi owner Vessel");
    assert!(
        owner.status.as_ref().is_none_or(|status: &VesselStatus| status.phase != flotilla_resources::VesselPhase::Failed),
        "the admitting-side Vessel must not attempt remote actuation"
    );
    assert!(
        matches!(
            kiwi.resource_backend().using::<Environment>(NAMESPACE).get(&feta_profile.host_direct_environment_name()).await,
            Err(ResourceError::NotFound { .. })
        ),
        "the placement host's Environment must remain host-local"
    );
    let ready_checkouts = feta
        .resource_backend()
        .using::<ResourceCheckout>(NAMESPACE)
        .list()
        .await
        .expect("feta Checkouts")
        .items
        .into_iter()
        .filter(|checkout| checkout.status.as_ref().map(|status| status.phase) == Some(ResourceCheckoutPhase::Ready))
        .count();
    assert_eq!(ready_checkouts, 1);

    bridge.abort();
    for handle in controller_handles {
        handle.abort();
    }
}
