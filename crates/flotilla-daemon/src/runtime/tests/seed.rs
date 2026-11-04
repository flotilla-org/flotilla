use super::*;
use crate::startup::busy_store::BusyStore;
use flotilla_resources::{definition_provenance::SEEDING_MANAGER, FulfilmentGrant, PlacementImageSource};
use flotilla_store_sqlite::SqliteBackend;

#[tokio::test]
async fn canonicalizes_display_name_kind_host_refs_to_the_host_id() {
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
    let host_direct = |host_ref: &str| {
        FulfilmentKindSpec::builder()
            .host_ref(host_ref.into())
            .pool("cleat".into())
            .grants(BTreeSet::from([FulfilmentGrant::platform("linux".into())]))
            .realisation(FulfilmentRealisation::HostDirect)
            .build()
    };
    let legacy_kind = host_direct("udder");
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
        .create(&empty_meta("a-collision"), &host_direct("collision"))
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

    canonicalize_kind_hosts(&backend, NAMESPACE, host_id).await.expect("canonicalize kinds");

    let kinds = backend.clone().using::<FulfilmentKind>(NAMESPACE);
    assert_eq!(kinds.get("direct-udder").await.expect("kind exists").spec.host_ref, host_id);
    assert_eq!(kinds.get("orphan-udder").await.expect("orphan kind exists").spec.host_ref, host_id);
    assert_eq!(kinds.get("a-collision").await.expect("ambiguous kind").spec.host_ref, "collision");
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
    let clones = backend.using::<Clone>(NAMESPACE);

    assert!(hosts.get(&host_id).await.is_ok(), "host resource should exist");
    assert!(environments.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct environment should exist");
    assert!(kinds.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct kind should exist");

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
        realisation: FulfilmentRealisation::DockerPerVessel { image: Some(PlacementImageSource::Literal("ubuntu:24.04".into())) },
    }
}

fn seeded_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(MANAGED_BY_LABEL.to_string(), SEEDING_MANAGER.to_string())])
}

// Seeding writes this host's kinds directly, labelled as seeding's.
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
        FulfilmentRealisation::DockerPerVessel { image: Some(PlacementImageSource::Literal("operator/image:1".into())) };
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
