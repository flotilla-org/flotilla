use super::*;
use crate::startup::busy_store::BusyStore;
use flotilla_resources::{definition_provenance::SEEDING_MANAGER, DockerImageSource, FulfilmentGrant};
use flotilla_store_sqlite::SqliteBackend;

#[tokio::test]
async fn migrates_display_name_host_ref_to_canonical_host_id() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
    policies.create(&empty_meta("direct-udder"), &spec).await.expect("seed legacy policy");
    let docker_spec = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            memory_policy: Default::default(),
            host_ref: "udder".to_string(),
            image: Some("crew:test".into()),
            pull_policy: Default::default(),
            agent_adapters: BTreeSet::new(),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
        .build();
    policies.create(&empty_meta("docker-udder"), &docker_spec).await.expect("seed legacy docker policy");
    let legacy_kind = FulfilmentKindSpec::from_policy(&spec, "linux").expect("valid legacy policy");
    backend.clone().using::<FulfilmentKind>(NAMESPACE).create(&empty_meta("direct-udder"), &legacy_kind).await.expect("seed legacy kind");
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
        BTreeMap::from([("direct-udder".to_string(), FulfilmentFacts { observed_at: Utc::now(), ..FulfilmentFacts::default() })]);
    let runner = Arc::new(DiscoveryMockRunner::builder().build());
    let facts = observe_fulfilment_facts(
        &backend,
        NAMESPACE,
        host_id,
        &["cleat".to_string()],
        &previous,
        FulfilmentProbeContext {
            providers: &detection_registry(runner.clone()),
            runner: runner.as_ref(),
            env: &TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]),
            scratch: Path::new("/tmp/flotilla-probe-test"),
            observation: &Default::default(),
            harnesses: &flotilla_agent::adapter::built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            pinned_harnesses: &Default::default(),
        },
        &mut ModelProbeState::default(),
    )
    .await
    .expect("observe legacy kind");
    assert!(facts.contains_key("direct-udder"), "display-name kind must receive host facts");
    assert!(!facts.contains_key("a-collision"), "ambiguous kind must not block healthy facts");

    migrate_live_placement_policies(&backend, NAMESPACE, host_id, "linux").await.expect("migrate policy");

    let policy = policies.get("direct-udder").await.expect("policy remains");
    assert_eq!(policy.spec.host_direct.expect("direct policy").host_ref, host_id);
    let kind = backend.clone().using::<FulfilmentKind>(NAMESPACE).get("direct-udder").await.expect("kind exists");
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
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
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    for host in ["feta", "kiwi", "udder"] {
        let name = format!("docker-crew-image-{host}");
        let spec = PlacementPolicySpec::builder()
            .pool("cleat".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                memory_policy: Default::default(),
                host_ref: host.to_string(),
                image: Some("crew:test".into()),
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
                memory_policy: Default::default(),
                host_ref: host.to_string(),
                image: Some("ubuntu:24.04".into()),
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
    let frozen_kind = kinds
        .create(
            &InputMeta::builder()
                .name(snapshot_name.to_string())
                .labels(BTreeMap::from([(
                    flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                    flotilla_resources::PLACEMENT_SNAPSHOT_KIND.to_string(),
                )]))
                .build(),
            &FulfilmentKindSpec::from_policy(&snapshot_spec, "linux").expect("valid frozen policy"),
        )
        .await
        .expect("seed admission's snapshot kind");
    let legacy_snapshot = "old-convoy-remote-placement-012345abcdef";
    policies.create(&empty_meta(legacy_snapshot), &snapshot_spec).await.expect("seed legacy snapshot without label");
    let mut invalid = PlacementPolicySpec::builder().pool("cleat".to_string()).build();
    invalid.host_direct =
        Some(HostDirectPlacementPolicySpec { host_ref: "kiwi".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree });
    invalid.docker_per_vessel = Some(DockerPerVesselPlacementPolicySpec {
        memory_policy: Default::default(),
        host_ref: "kiwi".to_string(),
        image: Some("crew:test".into()),
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
    let snapshot_kind = kinds.get(snapshot_name).await.expect("admission's snapshot kind is left alone");
    assert_eq!(snapshot_kind.metadata.resource_version, frozen_kind.metadata.resource_version);
    let kinds = kinds.list().await.expect("list kinds").items;
    let names: BTreeSet<_> = kinds.iter().map(|kind| kind.metadata.name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["docker-crew-image-feta", "docker-crew-image-kiwi", "docker-crew-image-udder", snapshot_name]),
        "snapshots, seeded names and invalid policies do not migrate"
    );
    for kind in kinds {
        assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::platform("linux".to_string())));
        if kind.metadata.name.starts_with("docker-") {
            assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::network("scoped".to_string())));
        } else {
            assert!(kind.spec.grants.contains(&flotilla_resources::FulfilmentGrant::host_account_reach()));
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
    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
    let clones = backend.using::<Clone>(NAMESPACE);

    assert!(hosts.get(&host_id).await.is_ok(), "host resource should exist");
    assert!(environments.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct environment should exist");
    assert!(kinds.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct kind should exist");
    assert!(policies.list().await.expect("list policies").items.is_empty(), "startup seeds no placement policy");

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

fn host_direct_kind_spec(profile: &LocalProvisioningProfile, platform: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec {
        host_ref: profile.host_id.clone(),
        pool: profile.host_direct_pool.clone(),
        cost_class: Default::default(),
        grants: BTreeSet::from([
            FulfilmentGrant::platform(platform.to_string()),
            FulfilmentGrant::host_account_reach(),
            FulfilmentGrant::network("host".to_string()),
        ]),
        realisation: FulfilmentRealisation::HostDirect,
    }
}

fn docker_kind_spec(profile: &LocalProvisioningProfile) -> FulfilmentKindSpec {
    FulfilmentKindSpec {
        host_ref: profile.host_id.clone(),
        pool: profile.docker_pool.clone(),
        cost_class: Default::default(),
        grants: BTreeSet::from([FulfilmentGrant::platform("linux".to_string()), FulfilmentGrant::network("scoped".to_string())]),
        realisation: FulfilmentRealisation::DockerPerVessel { image: Some(DockerImageSource::Literal("ubuntu:24.04".into())) },
    }
}

fn seeded_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(MANAGED_BY_LABEL.to_string(), SEEDING_MANAGER.to_string())])
}

// Seeding writes this host's kinds directly, labelled as seeding's, and seeds no PlacementPolicy.
#[tokio::test]
async fn seeding_writes_host_direct_and_docker_kinds() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let profile = manual_profile("host-test", true);
    ensure_default_kinds(&backend, NAMESPACE, &profile, "macos").await.expect("seed kinds");

    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    let direct = kinds.get(&profile.host_direct_policy_name()).await.expect("host-direct kind");
    assert_eq!(profile.host_direct_policy_name(), "host-direct-host-test");
    assert_eq!(direct.spec, host_direct_kind_spec(&profile, "macos"));
    assert_eq!(direct.metadata.labels, seeded_labels());
    let docker = kinds.get(&profile.docker_policy_name()).await.expect("docker kind");
    assert_eq!(profile.docker_policy_name(), "docker-on-host-test");
    assert_eq!(docker.spec, docker_kind_spec(&profile));
    assert_eq!(docker.metadata.labels, seeded_labels());
    assert!(backend.using::<PlacementPolicy>(NAMESPACE).list().await.expect("list policies").items.is_empty());
}

#[tokio::test]
async fn kind_seeding_keeps_operator_grants_and_corrects_owned_drift() {
    let temp = TempDir::new().expect("tempdir");
    let database_path = temp.path().join("resources.sqlite");
    let profile = manual_profile("host-test", false);
    let operator_grants = BTreeSet::from([
        FulfilmentGrant::platform("windows".to_string()),
        FulfilmentGrant::network("host".to_string()),
        FulfilmentGrant::toolchain("rust".to_string()),
    ]);
    {
        let backend = ResourceBackend::new(SqliteBackend::open(&database_path).expect("initial sqlite resource backend should open"));
        ensure_default_kinds(&backend, NAMESPACE, &profile, "linux").await.expect("initial seeding should succeed");

        let kinds = backend.using::<FulfilmentKind>(NAMESPACE);
        let seeded = kinds.get(&profile.host_direct_policy_name()).await.expect("seeded host-direct kind");
        kinds
            .update(
                &InputMeta::from(&seeded.metadata),
                &seeded.metadata.resource_version,
                &FulfilmentKindSpec {
                    host_ref: "operator-edited-host".to_string(),
                    pool: "operator-edited-pool".to_string(),
                    cost_class: flotilla_resources::FulfilmentCostClass::Metered,
                    grants: operator_grants,
                    realisation: FulfilmentRealisation::DockerPerVessel { image: None },
                },
            )
            .await
            .expect("operator kind apply should succeed");
    }

    let backend = ResourceBackend::new(SqliteBackend::open(&database_path).expect("restarted sqlite resource backend should open"));
    ensure_default_kinds(&backend, NAMESPACE, &profile, "linux").await.expect("re-seeding should succeed");

    let kinds = backend.using::<FulfilmentKind>(NAMESPACE);
    let reconciled = kinds.get(&profile.host_direct_policy_name()).await.expect("reconciled host-direct kind");
    assert_eq!(reconciled.spec.host_ref, profile.host_id, "seeding asserts its host");
    assert_eq!(reconciled.spec.pool, profile.host_direct_pool, "seeding asserts the discovered terminal pool");
    assert_eq!(reconciled.spec.realisation, FulfilmentRealisation::HostDirect, "seeding asserts the realisation");
    assert_eq!(reconciled.spec.cost_class, flotilla_resources::FulfilmentCostClass::Metered, "the operator's cost class survives");
    assert_eq!(
        reconciled.spec.grants,
        BTreeSet::from([
            FulfilmentGrant::platform("linux".to_string()),
            FulfilmentGrant::network("host".to_string()),
            FulfilmentGrant::toolchain("rust".to_string()),
        ]),
        "operator grants survive while the platform is the host's"
    );
    assert_eq!(reconciled.metadata.labels, seeded_labels());

    ensure_default_kinds(&backend, NAMESPACE, &profile, "linux").await.expect("steady-state seeding should succeed");
    let steady = kinds.get(&profile.host_direct_policy_name()).await.expect("steady host-direct kind");
    assert_eq!(steady.metadata.resource_version, reconciled.metadata.resource_version, "steady seeding must not rewrite the kind");
}

#[tokio::test]
async fn kind_seeding_refreshes_platform_and_adopts_unlabelled_kinds() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let profile = manual_profile("host-test", true);
    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    let mut stale_docker = docker_kind_spec(&profile);
    stale_docker.realisation =
        FulfilmentRealisation::DockerPerVessel { image: Some(DockerImageSource::Literal("operator/image:1".into())) };
    stale_docker.grants.insert(FulfilmentGrant::gpu());
    kinds.create(&empty_meta(&profile.docker_policy_name()), &stale_docker).await.expect("unlabelled docker kind");

    ensure_default_kinds(&backend, NAMESPACE, &profile, "macos").await.expect("seed on macos");
    ensure_default_kinds(&backend, NAMESPACE, &profile, "linux").await.expect("seed on linux");

    let direct = kinds.get(&profile.host_direct_policy_name()).await.expect("host-direct kind");
    assert_eq!(direct.spec, host_direct_kind_spec(&profile, "linux"), "the platform grant follows the host's platform");
    let docker = kinds.get(&profile.docker_policy_name()).await.expect("docker kind");
    let mut expected = docker_kind_spec(&profile);
    expected.grants.insert(FulfilmentGrant::gpu());
    assert_eq!(docker.spec, expected, "seeding owns the Docker realisation and keeps the operator's grant");
    assert_eq!(docker.metadata.labels, seeded_labels(), "seeding adopts the unlabelled kind");
}

#[tokio::test]
async fn kind_seeding_leaves_kind_managed_elsewhere_untouched() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let profile = manual_profile("host-test", false);
    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    let mut manifest_spec = host_direct_kind_spec(&profile, "windows");
    manifest_spec.host_ref = "manifest-host".to_string();
    manifest_spec.pool = "manifest-pool".to_string();
    let manifest = kinds
        .create(
            &empty_meta_with_labels(
                &profile.host_direct_policy_name(),
                BTreeMap::from([(MANAGED_BY_LABEL.to_string(), "manifest".to_string())]),
            ),
            &manifest_spec,
        )
        .await
        .expect("manifest-managed kind should exist");

    ensure_default_kinds(&backend, NAMESPACE, &profile, "linux").await.expect("seeding should tolerate managed collision");

    let unchanged = kinds.get(&profile.host_direct_policy_name()).await.expect("manifest-managed kind should remain");
    assert_eq!(unchanged.spec, manifest_spec);
    assert_eq!(unchanged.metadata.resource_version, manifest.metadata.resource_version, "seeding must not rewrite a managed kind");
}

#[tokio::test]
async fn startup_registration_skips_repos_without_origin_and_gates_docker_kind() {
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
    let kinds = backend.using::<FulfilmentKind>(NAMESPACE);
    assert!(clones.list().await.expect("clone list").items.is_empty(), "repo without origin should not create a discovered clone");
    assert!(kinds.get(&format!("docker-on-{host_id}")).await.is_err(), "docker kind should be absent when docker capability is false");

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
    let kinds2 = daemon2.resource_backend().using::<FulfilmentKind>(NAMESPACE);
    assert!(kinds2.get(&format!("docker-on-{host_id2}")).await.is_ok(), "docker kind should be created when docker capability is true");
}

// #3765 S6: a cloud policy migrates only on its uniquely advertised controller;
// its fulfilment kind retains the cloud realisation and resolved host identity.
#[tokio::test]
async fn cloud_policy_migration_uses_unique_controller() {
    use flotilla_protocol::provider::{EnvironmentClass, ProviderAdvertisement, ProviderKind, ProviderReadiness};
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let hosts = backend.using::<Host>(NAMESPACE);
    let host = hosts
        .create(&empty_meta("cloud-controller"), &flotilla_resources::HostSpec { display_name: "controller".into(), ..Default::default() })
        .await
        .unwrap();
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &flotilla_resources::HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::to_value(vec![ProviderAdvertisement {
                        instance: "daytona".into(),
                        kind: Some(ProviderKind::new(EnvironmentClass::CloudSandbox, "daytona")),
                        readiness: ProviderReadiness::Ready,
                        ..Default::default()
                    }])
                    .unwrap(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let spec: flotilla_resources::PlacementPolicySpec = serde_json::from_value(serde_json::json!({
        "pool": "cleat", "provider_requirements": {"instance":"daytona", "class":"cloud_sandbox"},
        "cloud_sandbox_per_vessel": {"image":"image@sha256:digest", "size":{"cpus":1,"memory_bytes":1024,"disk_bytes":2048}, "checkout":"clone_in_vessel", "env":{}}
    })).unwrap();
    backend.using::<PlacementPolicy>(NAMESPACE).create(&empty_meta("cloud"), &spec).await.unwrap();
    let listed_hosts = hosts.list().await.unwrap().items;
    let policies = backend.using::<PlacementPolicy>(NAMESPACE).list().await.unwrap().items;
    migrate_listed_placement_policies(&backend, NAMESPACE, "other-host", "linux", &listed_hosts, policies.clone()).await.unwrap();
    assert!(backend.using::<FulfilmentKind>(NAMESPACE).get("cloud").await.is_err());
    migrate_listed_placement_policies(&backend, NAMESPACE, "cloud-controller", "linux", &listed_hosts, policies).await.unwrap();
    let kind = backend.using::<FulfilmentKind>(NAMESPACE).get("cloud").await.unwrap();
    assert_eq!(kind.spec.host_ref, "cloud-controller");
    assert!(matches!(kind.spec.realisation, flotilla_resources::FulfilmentRealisation::CloudSandboxPerVessel { .. }));
}

// A read-deadline storm lasting longer than five minutes must not terminate startup.
// The real runtime exercises refusals at the memory store boundary.
#[tokio::test(start_paused = true)]
async fn runtime_starts_after_store_read_deadlines_clear() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("daemon.toml"), "machine_id = \"startup-retry-test\"\n").unwrap();
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let refusals = Arc::new(AtomicUsize::new(0));
    let armed = Arc::new(AtomicBool::new(false));
    let backend = ResourceBackend::from_arc(Arc::new(BusyStore {
        inner: Arc::new(flotilla_store_memory::InMemoryBackend::default()),
        armed: Arc::clone(&armed),
        busy_since: Arc::new(std::sync::OnceLock::new()),
        refusals: Arc::clone(&refusals),
    }));
    let daemon = daemon_with_backend(Vec::new(), Arc::clone(&config), backend).await;
    armed.store(true, Ordering::Relaxed);
    let started = tokio::time::Instant::now();
    let runtime = super::super::DaemonRuntime::start_with_options(
        Arc::clone(&daemon),
        config,
        None,
        super::super::RuntimeOptions { start_controllers: false, ..Default::default() },
    )
    .await
    .expect("runtime recovers after store pressure");
    assert!(started.elapsed() >= Duration::from_secs(360), "startup waited through the prolonged store pressure");
    assert!(refusals.load(Ordering::Relaxed) > 0, "startup exercised the deadline refusal");
    assert!(!daemon.resource_backend().using::<Host>(NAMESPACE).list().await.unwrap().items.is_empty());
    runtime.shutdown();
}

// A cloud policy's provider instance selects between two cloud providers on one
// controller, both for a kind it creates and for one migrated before it carried the instance.
#[tokio::test]
async fn cloud_policy_instance_selects_among_controller_providers() {
    use flotilla_protocol::provider::{
        CheckoutStrategy, EnvironmentClass, ProviderAdvertisement, ProviderCapabilities, ProviderKind, ProviderReadiness,
    };
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let hosts = backend.using::<Host>(NAMESPACE);
    let host = hosts
        .create(&empty_meta("cloud-controller"), &flotilla_resources::HostSpec { display_name: "controller".into(), ..Default::default() })
        .await
        .unwrap();
    let advertisement = |instance: &str| ProviderAdvertisement {
        instance: instance.into(),
        kind: Some(ProviderKind::new(EnvironmentClass::CloudSandbox, instance)),
        capabilities: ProviderCapabilities {
            checkout_strategies: vec![CheckoutStrategy::CloneInVessel],
            workspace_root: "/workspace".into(),
            per_vessel: true,
            ..Default::default()
        },
        readiness: ProviderReadiness::Ready,
    };
    hosts
        .update_status(
            &host.metadata.name,
            &host.metadata.resource_version,
            &flotilla_resources::HostStatus {
                capabilities: [(
                    flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(),
                    serde_json::to_value(vec![advertisement("daytona"), advertisement("namespace")]).unwrap(),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let spec: flotilla_resources::PlacementPolicySpec = serde_json::from_value(serde_json::json!({
        "pool": "cleat", "provider_requirements": {"instance":"namespace", "class":"cloud_sandbox"},
        "cloud_sandbox_per_vessel": {"image":"image@sha256:digest", "size":{"cpus":1,"memory_bytes":1024,"disk_bytes":2048}, "checkout":"clone_in_vessel", "env":{}}
    }))
    .unwrap();
    let policies = backend.using::<PlacementPolicy>(NAMESPACE);
    policies.create(&empty_meta("created"), &spec).await.unwrap();
    policies.create(&empty_meta("migrated"), &spec).await.unwrap();
    let mut unlabelled = FulfilmentKindSpec::from_policy(&spec, "linux").unwrap();
    unlabelled.host_ref = "cloud-controller".into();
    backend.using::<FulfilmentKind>(NAMESPACE).create(&empty_meta("migrated"), &unlabelled).await.unwrap();

    migrate_live_placement_policies(&backend, NAMESPACE, "cloud-controller", "linux").await.unwrap();

    for name in ["created", "migrated"] {
        let mut kind = backend.using::<FulfilmentKind>(NAMESPACE).get(name).await.unwrap();
        flotilla_admission::support::resolve_placement_provider(&backend, NAMESPACE, &mut kind).await.unwrap();
        assert_eq!(flotilla_admission::support::provider_instance(&kind), Some("namespace"), "{name}");
    }
}
