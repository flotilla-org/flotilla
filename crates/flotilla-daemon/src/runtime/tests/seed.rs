use super::*;

#[tokio::test]
async fn builtin_workflow_templates_seeded_at_multiple_roots_do_not_raise_authorship_collisions() {
    assert_eq!(WorkflowTemplate::REPLICATION_CLASS, ReplicationClass::Definitions, "code-seeded builtins share the fleet definitions view");
    for root in ["builtin-root-a", "builtin-root-b", "builtin-root-c"] {
        let backend = ResourceBackend::InMemory(Default::default()).with_local_root(NodeId::new(root));
        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("seed builtin workflow templates");
        assert!(
            home_bound_authorship_collisions(&backend, NAMESPACE).await.expect("inspect authored resources").is_empty(),
            "code-seeded builtins at {root} must be exempt from single-home collision enforcement"
        );
    }
}

// #2701: reconciliation removes every orphan still claiming builtin ownership,
// preserves user-owned templates, and is idempotent across startup sweeps.
#[tokio::test]
async fn retired_builtin_records_are_removed_without_deleting_user_templates() {
    let backend = ResourceBackend::InMemory(Default::default());
    let templates = backend.definitions::<WorkflowTemplate>(NAMESPACE);
    for name in ["single-agent-contained", "single-agent-trusted", "unknown-retired"] {
        let mut stale = flotilla_resources::single_agent_workflow_spec();
        stale.turn_delivery.shift_remove("checks-settled");
        templates.create(&mark_builtin_managed(empty_meta(name)), &stale).await.expect("orphan");
    }
    templates.create(&empty_meta("user-workflow"), &flotilla_resources::single_agent_workflow_spec()).await.expect("user template");
    for _ in 0..2 {
        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("reconcile");
        for name in ["single-agent-contained", "single-agent-trusted", "unknown-retired"] {
            assert!(matches!(templates.get(name).await, Err(ResourceError::NotFound { .. })), "orphan {name}");
        }
        assert!(templates.get("user-workflow").await.is_ok());
        assert!(templates.get("single-agent").await.expect("current builtin").spec.turn_delivery.contains_key("checks-settled"));
    }
}

#[tokio::test]
async fn startup_seeding_reconciles_existing_builtin_template_definition() {
    let backend = ResourceBackend::InMemory(Default::default());
    let templates = backend.definitions::<WorkflowTemplate>(NAMESPACE);
    let mut stale = flotilla_resources::single_agent_workflow_spec();
    stale.vessels[0].crew[0].role = "obsolete".to_string();

    templates
        .create(
            &empty_meta_with_labels("single-agent", BTreeMap::from([("example.com/preserved".to_string(), "true".to_string())])),
            &stale,
        )
        .await
        .expect("stale template create should succeed");

    let log_output = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let writer = LogCaptureWriter(Arc::clone(&log_output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("startup reconciliation should succeed");
    }

    let reconciled = templates.get("single-agent").await.expect("template should remain");
    assert_eq!(reconciled.spec, flotilla_resources::single_agent_workflow_spec());
    assert_eq!(reconciled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));
    assert_eq!(reconciled.metadata.labels.get("example.com/preserved").map(String::as_str), Some("true"));
    let logs = String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
    assert!(logs.contains("stored spec diverged from code builtin; overwriting"), "missing overwrite warning: {logs}");
    assert!(logs.contains("single-agent"), "warning should name the template: {logs}");

    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("restart reconciliation should succeed");
    let unchanged = templates.get("single-agent").await.expect("template should remain");
    assert_eq!(unchanged.metadata.resource_version, reconciled.metadata.resource_version);
}

#[tokio::test]
async fn deleted_code_owned_builtin_is_reconciled_back() {
    let backend = ResourceBackend::InMemory(Default::default());
    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("initial builtin reconciliation should succeed");

    let deleted = flotilla_resources::delete_resource_kind(&backend, NAMESPACE, "workflowtemplates", "single-agent")
        .await
        .expect("raw delete should remove builtin");
    assert_eq!(deleted.object.value["metadata"]["name"], "single-agent");
    assert!(matches!(backend.definitions::<WorkflowTemplate>(NAMESPACE).get("single-agent").await, Err(ResourceError::NotFound { .. })));

    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("level-triggered reconciliation should recreate builtin");
    let recreated = backend.using::<WorkflowTemplate>(NAMESPACE).get("single-agent").await.expect("builtin should be recreated");
    assert_eq!(recreated.spec, flotilla_resources::single_agent_workflow_spec());
    assert_eq!(recreated.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));
}

#[tokio::test]
async fn startup_seeding_labels_matching_unlabelled_builtin_template_once() {
    const NAMESPACE: &str = "test";
    let backend = ResourceBackend::InMemory(Default::default());
    let templates = backend.definitions::<WorkflowTemplate>(NAMESPACE);
    templates
        .create(&empty_meta("single-agent"), &flotilla_resources::single_agent_workflow_spec())
        .await
        .expect("matching template create should succeed");
    let existing = templates.get("single-agent").await.expect("template should exist");

    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("startup reconciliation should succeed");

    let labelled = templates.get("single-agent").await.expect("template should remain");
    assert_ne!(labelled.metadata.resource_version, existing.metadata.resource_version);
    assert_eq!(labelled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));

    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("restart reconciliation should succeed");

    let unchanged = templates.get("single-agent").await.expect("template should remain");
    assert_eq!(unchanged.metadata.resource_version, labelled.metadata.resource_version);
}

#[tokio::test]
async fn startup_reconciles_owned_single_agent_shepherd_builtin() {
    let backend = ResourceBackend::InMemory(Default::default());

    reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("builtin reconciliation should succeed");

    let shepherd = backend.using::<WorkflowTemplate>(NAMESPACE).get("single-agent-shepherd").await.expect("shepherd builtin should exist");
    assert_eq!(shepherd.spec, flotilla_resources::single_agent_shepherd_workflow_spec());
    assert_eq!(shepherd.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));
}

#[tokio::test]
async fn migrates_display_name_host_ref_to_canonical_host_id() {
    let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
    let host_id = "b2aac222-host-id";
    backend
        .clone()
        .using::<Host>(NAMESPACE)
        .create(&empty_meta(host_id), &HostSpec { display_name: "udder".into(), connection: Default::default(), ..HostSpec::default() })
        .await
        .expect("seed host");
    for name in ["collision-a", "collision-b"] {
        backend
            .clone()
            .using::<Host>(NAMESPACE)
            .create(
                &empty_meta(name),
                &HostSpec { display_name: "collision".into(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("seed ambiguous host");
    }
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let ambiguous_policy = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec {
            host_ref: "collision".to_string(),
            checkout: HostDirectPlacementPolicyCheckout::Worktree,
        })
        .build();
    policies.create(&empty_meta("a-collision"), &ambiguous_policy).await.expect("seed ambiguous policy");
    let spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref: "udder".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    policies.create(&empty_meta("host-direct-udder"), &spec).await.expect("seed legacy policy");
    let docker_spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            legacy_image_baseline_ref: None,
            memory_policy: Default::default(),
            host_ref: "udder".to_string(),
            image: "crew:test".into(),
            pull_policy: Default::default(),
            agent_adapters: BTreeSet::new(),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
        .build();
    policies.create(&empty_meta("docker-udder"), &docker_spec).await.expect("seed legacy docker policy");
    let legacy_kind = FulfilmentKindSpec::from_policy(&spec, "linux").expect("valid legacy policy");
    backend
        .clone()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(&empty_meta("host-direct-udder"), &legacy_kind)
        .await
        .expect("seed legacy kind");
    backend
        .clone()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(&empty_meta("orphan-udder"), &legacy_kind)
        .await
        .expect("seed standalone legacy kind");
    backend
        .clone()
        .using::<FulfilmentKind>(NAMESPACE)
        .create(&empty_meta("a-collision"), &FulfilmentKindSpec::from_policy(&ambiguous_policy, "linux").expect("valid ambiguous policy"))
        .await
        .expect("seed ambiguous kind");
    let previous =
        BTreeMap::from([("host-direct-udder".to_string(), FulfilmentFacts { observed_at: Utc::now(), ..FulfilmentFacts::default() })]);
    let runner = DiscoveryMockRunner::builder().build();
    let facts = observe_fulfilment_facts(
        &backend,
        NAMESPACE,
        host_id,
        &["cleat".to_string()],
        &previous,
        FulfilmentProbeContext {
            runner: &runner,
            env: &TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]),
            scratch: Path::new("/tmp/flotilla-probe-test"),
        },
        &mut ModelProbeState::default(),
    )
    .await
    .expect("observe legacy kind");
    assert!(facts.contains_key("host-direct-udder"), "display-name kind must receive host facts");
    assert!(!facts.contains_key("a-collision"), "ambiguous kind must not block healthy facts");

    migrate_live_placement_policies(&backend, NAMESPACE, host_id, "linux").await.expect("migrate policy");

    let policy = policies.get("host-direct-udder").await.expect("policy remains");
    assert_eq!(policy.spec.host_direct.expect("direct policy").host_ref, host_id);
    let kind = backend.clone().using::<FulfilmentKind>(NAMESPACE).get("host-direct-udder").await.expect("kind exists");
    assert_eq!(kind.spec.host_ref, host_id);
    let orphan = backend.clone().using::<FulfilmentKind>(NAMESPACE).get("orphan-udder").await.expect("orphan kind exists");
    assert_eq!(orphan.spec.host_ref, host_id);
    let docker = policies.get("docker-udder").await.expect("docker policy remains");
    assert_eq!(docker.spec.docker_per_vessel.expect("docker policy").host_ref, host_id);
    assert_eq!(backend.clone().using::<FulfilmentKind>(NAMESPACE).get("docker-udder").await.expect("docker kind").spec.host_ref, host_id);
    assert_eq!(
        policies.get("a-collision").await.expect("ambiguous policy remains").spec.host_direct.expect("direct").host_ref,
        "collision"
    );
}

#[tokio::test]
async fn conflicting_policy_does_not_block_other_host_ref_migrations() {
    let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
    let host_id = "host-id";
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    hosts.create(&empty_meta(host_id), &HostSpec { display_name: "kiwi".into(), ..HostSpec::default() }).await.expect("seed host");
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref: "kiwi".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    for name in ["a-conflicting", "b-healthy"] {
        policies.create(&empty_meta(name), &spec).await.expect("seed policy");
    }
    let listed_hosts = hosts.list().await.expect("list hosts");
    let listed_policies = policies.list().await.expect("list policies");
    let conflicting = policies.get("a-conflicting").await.expect("read conflicting policy");
    let mut edited_spec = conflicting.spec.clone();
    edited_spec.priority = 1;
    policies
        .update(&InputMeta::from(&conflicting.metadata), &conflicting.metadata.resource_version, &edited_spec)
        .await
        .expect("concurrent policy write");

    migrate_listed_placement_policies(&backend, NAMESPACE, host_id, "linux", &listed_hosts.items, listed_policies.items)
        .await
        .expect("healthy policy still migrates");

    assert_eq!(policies.get("a-conflicting").await.expect("conflicting policy").spec.host_direct.expect("direct").host_ref, "kiwi");
    assert_eq!(policies.get("b-healthy").await.expect("healthy policy").spec.host_direct.expect("direct").host_ref, host_id);
    backend.clone().using::<FulfilmentKind>(NAMESPACE).get("b-healthy").await.expect("healthy kind created");
}

#[tokio::test]
async fn migrates_live_policy_names_to_fulfilment_kinds() {
    let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    for host in ["feta", "kiwi", "udder"] {
        let name = format!("docker-crew-image-{host}");
        let spec = PlacementPolicySpec::builder()
            .pool("cleat".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                legacy_image_baseline_ref: None,
                memory_policy: Default::default(),
                host_ref: host.to_string(),
                image: "crew:test".into(),
                pull_policy: Default::default(),
                agent_adapters: BTreeSet::new(),
                default_cwd: None,
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
            })
            .build();
        policies.create(&empty_meta(&name), &spec).await.expect("seed crew image policy");
    }
    for (name, host, docker) in [("docker-on-kiwi", "kiwi", true), ("host-direct-kiwi", "kiwi", false)] {
        let mut spec = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
        if docker {
            spec.docker_per_vessel = Some(DockerPerVesselPlacementPolicySpec {
                legacy_image_baseline_ref: None,
                memory_policy: Default::default(),
                host_ref: host.to_string(),
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                agent_adapters: BTreeSet::new(),
                default_cwd: None,
                env: BTreeMap::new(),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
            });
        } else {
            spec.host_direct =
                Some(HostDirectPlacementPolicySpec { host_ref: host.to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree });
        }
        policies.create(&empty_meta(name), &spec).await.expect("seed local policy");
    }
    let snapshot_name = "placement-snapshot-012345abcdef";
    let snapshot_spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref: "kiwi".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    policies
        .create(
            &InputMeta::builder()
                .name(snapshot_name.to_string())
                .labels(BTreeMap::from([(
                    flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                    flotilla_resources::PLACEMENT_SNAPSHOT_KIND.to_string(),
                )]))
                .build(),
            &snapshot_spec,
        )
        .await
        .expect("seed frozen placement snapshot");
    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    kinds
        .create(&empty_meta(snapshot_name), &FulfilmentKindSpec::from_policy(&snapshot_spec, "linux").expect("valid frozen policy"))
        .await
        .expect("seed previously migrated snapshot kind");
    let legacy_snapshot = "old-convoy-remote-placement-012345abcdef";
    policies.create(&empty_meta(legacy_snapshot), &snapshot_spec).await.expect("seed legacy snapshot without label");
    let mut invalid = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
    invalid.host_direct =
        Some(HostDirectPlacementPolicySpec { host_ref: "kiwi".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree });
    invalid.docker_per_vessel = Some(DockerPerVesselPlacementPolicySpec {
        legacy_image_baseline_ref: None,
        memory_policy: Default::default(),
        host_ref: "kiwi".to_string(),
        image: "crew:test".into(),
        pull_policy: Default::default(),
        agent_adapters: BTreeSet::new(),
        default_cwd: None,
        env: BTreeMap::new(),
        checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
    });
    policies.create(&empty_meta("invalid-both-realisations"), &invalid).await.expect("seed invalid policy");
    for host in ["feta", "kiwi", "udder"] {
        migrate_live_placement_policies(&backend, NAMESPACE, host, "linux").await.expect("migrate host policies");
    }
    let kinds = kinds.list().await.expect("list kinds").items;
    assert_eq!(kinds.len(), 5);
    assert!(kinds.iter().all(|kind| kind.metadata.name != snapshot_name));
    assert!(kinds.iter().all(|kind| kind.metadata.name != legacy_snapshot));
    for kind in kinds {
        assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::platform("linux".to_string())));
        if kind.metadata.name.starts_with("docker-") {
            assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::network("scoped".to_string())));
        } else {
            assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::host_account_reach()));
            assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::gui_session()));
        }
    }
    assert_eq!(policies.list().await.expect("policies and snapshots stay for A1 admission").items.len(), 8);
}

#[tokio::test]
async fn startup_registration_is_idempotent_and_discovers_existing_clone() {
    let temp = TempDir::new().expect("tempdir");
    let git_repo =
        TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
    let repo = git_repo.path().to_path_buf();

    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"startup-registration-idempotent-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    let profile = manual_profile(&host_id, false);

    register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("first startup registration should succeed");
    register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("second startup registration should succeed");

    let backend = daemon.resource_backend();
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let clones = backend.using::<Clone>(NAMESPACE);

    assert!(hosts.get(&host_id).await.is_ok(), "host resource should exist");
    assert!(environments.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct environment should exist");
    assert!(policies.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct policy should exist");

    let clone_name = format!(
        "clone-{}",
        clone_key(
            &flotilla_resources::canonicalize_repo_url("https://github.com/flotilla-org/flotilla.git").expect("canonical url"),
            &format!("host-direct-{host_id}")
        )
    );
    let clone = clones.get(&clone_name).await.expect("discovered clone should exist");
    assert_eq!(clone.spec.url, "git@github.com:flotilla-org/flotilla.git");
    assert_eq!(clone.metadata.labels.get("flotilla.work/discovered").map(String::as_str), Some("true"));
}

#[tokio::test]
async fn policy_registration_preserves_operator_fields_and_corrects_owned_drift() {
    let temp = TempDir::new().expect("tempdir");
    let database_path = temp.path().join("resources.sqlite");
    let profile = manual_profile("host-test", false);
    {
        let backend = ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("initial sqlite resource backend should open"));
        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("initial policy registration should succeed");

        let policies = backend.using::<PlacementPolicy>(NAMESPACE);
        let registered = policies.get(&profile.host_direct_policy_name()).await.expect("registered host-direct policy");
        policies
            .update(
                &InputMeta::from(&registered.metadata),
                &registered.metadata.resource_version,
                &PlacementPolicySpec::builder()
                    .pool("operator-edited-pool".to_string())
                    .priority(10)
                    .host_direct(HostDirectPlacementPolicySpec {
                        host_ref: "operator-edited-host".to_string(),
                        checkout: HostDirectPlacementPolicyCheckout::Worktree,
                    })
                    .build(),
            )
            .await
            .expect("operator policy apply should succeed");
    }

    let backend = ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("restarted sqlite resource backend should open"));
    ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("policy re-registration should succeed");

    let reconciled =
        backend.using::<PlacementPolicy>(NAMESPACE).get(&profile.host_direct_policy_name()).await.expect("reconciled host-direct policy");
    assert_eq!(reconciled.spec.priority, 10, "operator-owned priority must survive re-registration");
    assert_eq!(reconciled.spec.pool, profile.host_direct_pool, "registration must assert the discovered terminal pool");
    assert_eq!(
        reconciled.spec.host_direct,
        Some(HostDirectPlacementPolicySpec { host_ref: profile.host_id.clone(), checkout: HostDirectPlacementPolicyCheckout::Worktree }),
        "registration must assert its host and checkout strategy"
    );
    assert!(reconciled.spec.docker_per_vessel.is_none());

    ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("steady-state policy registration should succeed");
    let steady =
        backend.using::<PlacementPolicy>(NAMESPACE).get(&profile.host_direct_policy_name()).await.expect("steady host-direct policy");
    assert_eq!(steady.spec.priority, 10);
    assert_eq!(steady.metadata.resource_version, reconciled.metadata.resource_version, "steady registration must not rewrite the policy");
}

#[tokio::test]
async fn docker_policy_registration_preserves_runtime_configuration_and_corrects_owned_drift() {
    let backend = ResourceBackend::InMemory(Default::default());
    let profile = manual_profile("host-test", true);
    ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("initial policy registration should succeed");

    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let registered = policies.get(&profile.docker_policy_name()).await.expect("registered docker policy");
    policies
        .update(
            &InputMeta::from(&registered.metadata),
            &registered.metadata.resource_version,
            &PlacementPolicySpec::builder()
                .pool("operator-edited-pool".to_string())
                .priority(20)
                .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                    legacy_image_baseline_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "operator-edited-host".to_string(),
                    image: "operator/image:latest".to_string().into(),
                    pull_policy: flotilla_resources::DockerImagePullPolicy::Never,
                    agent_adapters: BTreeSet::from(["codex".to_string()]),
                    default_cwd: Some("/operator-workspace".to_string()),
                    env: BTreeMap::from([("OPERATOR_CONFIG".to_string(), "true".to_string())]),
                    checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/operator-clone".to_string() },
                })
                .build(),
        )
        .await
        .expect("operator policy apply should succeed");

    ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("policy re-registration should succeed");

    let reconciled = policies.get(&profile.docker_policy_name()).await.expect("reconciled docker policy");
    assert_eq!(reconciled.spec.priority, 20);
    assert_eq!(reconciled.spec.pool, profile.docker_pool);
    assert!(reconciled.spec.host_direct.is_none());
    assert_eq!(
        reconciled.spec.docker_per_vessel,
        Some(DockerPerVesselPlacementPolicySpec {
            legacy_image_baseline_ref: None,
            memory_policy: Default::default(),
            host_ref: profile.host_id,
            image: "operator/image:latest".to_string().into(),
            pull_policy: flotilla_resources::DockerImagePullPolicy::Never,
            agent_adapters: BTreeSet::from(["codex".to_string()]),
            default_cwd: Some("/operator-workspace".to_string()),
            env: BTreeMap::from([("OPERATOR_CONFIG".to_string(), "true".to_string())]),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
    );
}

#[tokio::test]
async fn policy_registration_leaves_manifest_managed_collision_untouched() {
    let backend = ResourceBackend::InMemory(Default::default());
    let profile = manual_profile("host-test", false);
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let manifest_spec = PlacementPolicySpec::builder()
        .pool("manifest-pool".to_string())
        .priority(25)
        .host_direct(HostDirectPlacementPolicySpec {
            host_ref: "manifest-host".to_string(),
            checkout: HostDirectPlacementPolicyCheckout::Worktree,
        })
        .build();
    let manifest = policies
        .create(
            &empty_meta_with_labels(
                &profile.host_direct_policy_name(),
                BTreeMap::from([(MANAGED_BY_LABEL.to_string(), "manifest".to_string())]),
            ),
            &manifest_spec,
        )
        .await
        .expect("manifest-managed policy should exist");

    ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("registration should tolerate managed collision");

    let unchanged = policies.get(&profile.host_direct_policy_name()).await.expect("manifest-managed policy should remain");
    assert_eq!(unchanged.spec, manifest_spec);
    assert_eq!(unchanged.metadata.resource_version, manifest.metadata.resource_version, "registration must not rewrite managed policy");
}

#[tokio::test]
async fn startup_registration_skips_repos_without_origin_and_gates_docker_policy() {
    let temp = TempDir::new().expect("tempdir");
    let git_repo = TestGitRepo::init(temp.path().join("repo-no-origin"));
    let repo = git_repo.path().to_path_buf();

    let config_base = temp.path().join("config");
    fs::create_dir_all(&config_base).expect("config directory");
    fs::write(config_base.join("daemon.toml"), "machine_id = \"startup-registration-no-origin-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(config_base));
    config.add_observation_root(&ExecutionEnvironmentPath::new(&repo)).expect("persist observation root");
    let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
    let host_id = daemon.local_host_id().expect("local host id").to_string();

    register_startup_resources(&daemon, NAMESPACE, &manual_profile(&host_id, false)).await.expect("startup registration should succeed");

    let backend = daemon.resource_backend();
    let clones = backend.clone().using::<Clone>(NAMESPACE);
    let policies = backend.using::<PlacementPolicy>(NAMESPACE);
    assert!(clones.list().await.expect("clone list").items.is_empty(), "repo without origin should not create a discovered clone");
    assert!(policies.get(&format!("docker-on-{host_id}")).await.is_err(), "docker policy should be absent when docker capability is false");

    let temp2 = TempDir::new().expect("tempdir");
    let config2_base = temp2.path().join("config");
    fs::create_dir_all(&config2_base).expect("config directory");
    fs::write(config2_base.join("daemon.toml"), "machine_id = \"startup-registration-no-origin-test-2\"\n").expect("daemon config");
    let config2 = Arc::new(ConfigStore::with_base(config2_base));
    let daemon2 = in_memory_daemon(Vec::new(), Arc::clone(&config2)).await;
    let host_id2 = daemon2.local_host_id().expect("local host id").to_string();
    register_startup_resources(&daemon2, NAMESPACE, &manual_profile(&host_id2, true))
        .await
        .expect("startup registration with docker capability should succeed");
    let policies2 = daemon2.resource_backend().using::<PlacementPolicy>(NAMESPACE);
    assert!(
        policies2.get(&format!("docker-on-{host_id2}")).await.is_ok(),
        "docker policy should be created when docker capability is true"
    );
}
