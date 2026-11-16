use super::*;
use flotilla_store_sqlite::SqliteBackend;

pub(super) async fn publish_merged_change_request(backend: &ResourceBackend, number: u64, authority: &str) {
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>(NAMESPACE);
    let name = cr_name("github.com", "flotilla-org/flotilla", number);
    let record = match records
        .create(
            &InputMeta::builder().name(name).build(),
            &flotilla_resources::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(number)
                .observing_authority(authority.to_string())
                .build(),
        )
        .await
    {
        Ok(record) => record,
        Err(ResourceError::Conflict { .. }) => records
            .get(&cr_name("github.com", "flotilla-org/flotilla", number))
            .await
            .expect("read concurrently materialized CR observation"),
        Err(error) => panic!("create merged CR observation: {error}"),
    };
    let observed_at = Utc::now();
    records
        .update_status(
            &record.metadata.name,
            &record.metadata.resource_version,
            &flotilla_resources::ChangeRequestStatus {
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: flotilla_resources::Observation::known(flotilla_resources::ObservedChangeRequestState::Merged, observed_at),
                head_sha: flotilla_resources::Observation::known("abc".to_string(), observed_at),
                checks: flotilla_resources::Observation::known(flotilla_resources::ObservedChecks::Pass, observed_at),
                review: flotilla_resources::ChangeRequestReviewObservation {
                    actionable_at_head: flotilla_resources::Observation::known(false, observed_at),
                },
                mergeable: flotilla_resources::Observation::known(flotilla_resources::ObservedMergeability::Mergeable, observed_at),
            },
        )
        .await
        .expect("publish merged CR observation");
}

pub(super) fn landed_status_with_placed_checkout(checkout_name: &str) -> ConvoyStatus {
    ConvoyStatus {
        phase: ConvoyPhase::Landed,
        observed_workflow_ref: Some("wf-a".to_string()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState::builder()
                .phase(WorkPhase::Complete)
                .placement(PlacementStatus {
                    fields: BTreeMap::from([(
                        "checkout_refs".to_string(),
                        serde_json::json!(BTreeMap::from([(RepositoryKey("repo-a".to_string()), checkout_name.to_string())])),
                    )]),
                })
                .build(),
        )]),
        ..Default::default()
    }
}

pub(super) fn credential_mapping_config(path: &Path, key: Option<&str>) {
    let mut config = "machine_id = \"credential-mapping-test\"\n".to_string();
    if let Some(key) = key {
        config.push_str(&format!("[credentials.forgejo]\n{key} = \"daemon-token\"\n"));
    }
    fs::write(path.join("daemon.toml"), config).expect("write daemon config");
}

fn mapping_forge_spec() -> ForgeSpec {
    ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["lab.example".to_string()]))
        .https_url("https://lab.example".to_string())
        .git_ssh_host("lab.example".to_string())
        .build()
}

pub(super) async fn declare_mapping_forge(backend: &ResourceBackend, namespace: &str) {
    backend.using::<Forge>(namespace).create(&empty_meta("lab"), &mapping_forge_spec()).await.expect("declare Forge");
}

/// The mapping's Forge declared at another home, `home`, under a record name of its own (a
/// name is held at one home), and replicated to `daemon` as that home's log arrives.
pub(super) async fn declare_mapping_forge_at_home(daemon: &InProcessDaemon, home: &str) {
    let target = daemon.decided_store().expect("decided store");
    let other = flotilla_provisioning_testkit::other_home_for(&target, home);
    other.definition::<flotilla_resources::Forge>(&format!("lab-{home}"), &mapping_forge_spec()).await;
    other.replicate_to(&target);
}

#[derive(Clone)]
pub(super) struct LogCaptureWriter(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log output lock should be healthy").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A live crew session `name`, held by vessel `vessel_ref` in environment `env_ref` at
/// `terminals`: requested and realised under pool session `name`. Returns its launch.
pub(super) fn create_credential_test_session(
    terminals: &flotilla_provisioning_testkit::TerminalsFixture,
    name: &str,
    vessel_ref: &str,
    env_ref: &str,
) -> String {
    let launch = terminals.request(&flotilla_resources::terminal_records::TerminalLaunchRequest {
        session: name.to_string(),
        environment: env_ref.to_string(),
        vessel: Some(vessel_ref.to_string()),
        role: "coder".to_string(),
        pool: "test".to_string(),
        cwd: "/workspace".to_string(),
        env: Default::default(),
        source: flotilla_resources::terminal_records::TerminalLaunchSource::Agent {
            capability: "code".to_string(),
            adapter: None,
            model: None,
            brief: flotilla_resources::TerminalBrief {
                artifact_digest: None,
                path: ".flotilla/briefs/coder.md".to_string(),
                content: "Work on the issue".to_string(),
                copies: Vec::new(),
            },
        },
    });
    terminals.ready(&launch, name, &Default::default());
    launch
}

pub(super) fn manual_profile(host_id: &str, docker_available: bool) -> LocalProvisioningProfile {
    LocalProvisioningProfile {
        host_direct_provider: "host-direct".into(),
        environment_providers: flotilla_discovery_testkit::environment::ready_placement_advertisements()
            .into_iter()
            .filter(|provider| {
                docker_available || provider.kind.as_ref().unwrap().class == flotilla_protocol::provider::EnvironmentClass::HostDirect
            })
            .collect(),
        host_id: host_id.to_string(),
        display_name: "kiwi".to_string(),
        repo_default_dir: "/Users/tester/dev/flotilla-repos".to_string(),
        host_direct_pool: "passthrough".to_string(),
        docker_pool: "passthrough".to_string(),
        available_pools: vec!["passthrough".to_string()],
        available_agent_adapters: BTreeSet::new(),
        docker_available,
        max_vessels: None,
    }
}

/// [`manual_profile`] for `daemon`'s own host, named as the daemon names itself, as discovery
/// builds it: the engine Host's description names the host by that display name.
pub(super) fn own_host_profile(daemon: &InProcessDaemon, docker_available: bool) -> LocalProvisioningProfile {
    let host_id = daemon.local_host_id().expect("local host id").to_string();
    LocalProvisioningProfile { display_name: daemon.host_name().to_string(), ..manual_profile(&host_id, docker_available) }
}

pub(super) async fn daemon_with_backend(
    tracked_repos: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
) -> Arc<InProcessDaemon> {
    daemon_with_backend_and_runner(tracked_repos, config, backend, Arc::new(NoPrProcessRunner)).await
}

pub(super) async fn daemon_with_backend_and_runner(
    tracked_repos: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
    runner: Arc<dyn CommandRunner>,
) -> Arc<InProcessDaemon> {
    daemon_with_backend_runner_and_change_requests(tracked_repos, config, backend, runner, None).await
}

pub(super) async fn daemon_with_backend_runner_and_change_requests(
    tracked_repos: Vec<PathBuf>,
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
    runner: Arc<dyn CommandRunner>,
    requests: Option<Arc<dyn ChangeRequestTracker>>,
) -> Arc<InProcessDaemon> {
    let mut discovery = git_process_discovery(false);
    discovery.runner = runner;
    if let Some(requests) = requests {
        discovery.factories.change_requests = vec![Box::new(FakeChangeRequestFactory(requests))];
    }
    let daemon = InProcessDaemon::new_with_resource_backend(
        tracked_repos,
        config,
        discovery,
        flotilla_protocol::HostName::new("test-host"),
        backend,
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                .with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
        )
        .expect("local environment bag should be replaceable in tests");
    daemon
}

pub(super) async fn in_memory_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
    daemon_with_backend(tracked_repos, config, ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default())).await
}

pub(super) async fn sqlite_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
    std::fs::create_dir_all(config.state_dir()).expect("state dir");
    let backend = ResourceBackend::new(SqliteBackend::open(config.state_dir().join("resources.sqlite")).expect("sqlite backend"));
    daemon_with_backend(tracked_repos, config, backend).await
}

pub(super) fn insert_undecodable_resource<T: Resource>(connection: &rusqlite::Connection, name: &str) {
    SqliteBackend::register_query_functions(connection).expect("configure independent corruption writer");
    connection
        .execute(
            r#"
                INSERT INTO resource_objects
                    (group_name, version, kind, namespace, name, resource_version, body_json)
                VALUES (?1, ?2, ?3, ?4, ?5, 1, '{}')
                "#,
            rusqlite::params![T::API_PATHS.group, T::API_PATHS.version, T::API_PATHS.kind, NAMESPACE, name],
        )
        .expect("insert undecodable resource");
}

pub(super) async fn crew_daemon(config: Arc<ConfigStore>) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
    crew_daemon_with_backend(config, ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default())).await
}

pub(super) async fn crew_daemon_with_backend(
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
    let pool = Arc::new(FakeTerminalPool::new());
    let mut discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_terminal::TerminalPool>),
    );
    discovery.factories.vcs.push(Box::new(flotilla_vcs::factory::GitVcsFactory));
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        config,
        discovery,
        flotilla_protocol::HostName::new("dinghy"),
        backend,
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
                .with(EnvironmentAssertion::binary("codex", "/tools/codex"))
                .with(EnvironmentAssertion::binary("claude", "/tools/claude")),
        )
        .expect("crew environment bag");
    (daemon, pool)
}

pub(super) async fn crew_daemon_with_process_runner(config: Arc<ConfigStore>) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
    let pool = Arc::new(FakeTerminalPool::new());
    let codex_home = config.base_path().join("codex-home");
    let mut discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_terminal::TerminalPool>),
    );
    discovery.factories.vcs.push(Box::new(flotilla_vcs::factory::GitVcsFactory));
    discovery.runner = Arc::new(ProcessCommandRunner);
    let daemon = InProcessDaemon::new(
        Vec::new(),
        config,
        discovery,
        flotilla_protocol::HostName::new("dinghy"),
        std::sync::Arc::new(crate::peer::HostPeerOverlay::default()),
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(
            EnvironmentBag::new()
                .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                .with(EnvironmentAssertion::env_var("CODEX_HOME", codex_home.to_string()))
                .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
                .with(EnvironmentAssertion::binary("codex", "/tools/codex"))
                .with(EnvironmentAssertion::binary("claude", "/tools/claude")),
        )
        .expect("crew environment bag");
    (daemon, pool)
}

pub(super) fn sqlite_max_event_rowid(path: &Path) -> u64 {
    let connection = rusqlite::Connection::open(path).expect("open SQLite store for idle inspection");
    connection
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM resource_events", [], |row| row.get(0))
        .expect("read maximum resource event rowid")
}

pub(super) async fn wait_until<F, Fut>(condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    wait_until_with_timeout(Duration::from_secs(5), condition).await;
}

/// Waits until convoy `name`'s status satisfies `reached`, naming `what` and the last status read
/// when it never does.
pub(super) async fn wait_for_convoy(
    convoys: &flotilla_store::TypedResolver<Convoy>,
    name: &str,
    what: &str,
    reached: impl Fn(&flotilla_resources::ConvoyStatus) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = convoys.get(name).await.ok().and_then(|convoy| convoy.status);
        if status.as_ref().is_some_and(&reached) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "convoy {name} never reached {what}; last status: {status:#?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) async fn wait_until_with_timeout<F, Fut>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for condition");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) async fn wait_for_command_result(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                Ok(_) => {}
                Err(err) => panic!("unexpected event error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for command result")
}

// Runtime scenarios model Linux placement capability independently of the test host.
pub(super) fn test_agent_material_registry(env: Arc<dyn EnvVars>) -> AgentMaterialRegistry {
    AgentMaterialRegistry::with_codex_delivery_support(env, true)
}

use flotilla_orchestration_testkit::InProcessDiscoveryExt;

// Real built-in detection providers over an injected subprocess stand-in.
pub(super) fn detection_registry(runner: Arc<dyn CommandRunner>) -> Arc<ProviderRegistry> {
    use flotilla_discovery::registry::command_provider_registry;
    use flotilla_environment::environment::EnvironmentInput;
    Arc::new(command_provider_registry(&[EnvironmentInput::Docker, EnvironmentInput::HostDirect], runner))
}

/// The record name the observer writes for a stored `(service, scope, number)`
/// change request spelling that needs no Forge directory.
fn cr_name(service: &str, scope: &str, number: u64) -> String {
    flotilla_resources::address::record_name(
        &flotilla_resources::address::stored_change_request_address(service, scope, number, &[]).expect("change request address"),
    )
    .expect("record name")
}

/// Startup seeding as the runtime runs it, over the daemon's [`engine_environments`] (whose
/// readings serve its clones), with its host-direct root realised.
pub(super) async fn register_startup(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let environments = engine_environments(daemon);
    register_startup_resources(daemon, namespace, profile).await?;
    // No Environment controller runs here: the host-direct root is realised as it would realise it.
    let name = profile.host_direct_environment_name();
    if environments.environments.find(&name)?.is_some_and(|environment| !environment.is_ready() && !environment.ended()) {
        environments.ready(&name, &profile.repo_default_dir, Default::default());
    }
    Ok(())
}
