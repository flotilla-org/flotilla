use super::*;

pub(super) async fn publish_merged_change_request(backend: &ResourceBackend, number: u64, authority: &str) {
    let records = backend.clone().using::<flotilla_resources::ChangeRequest>(NAMESPACE);
    let name = flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", number);
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
            .get(&flotilla_resources::change_request_record_name("github.com", "flotilla-org/flotilla", number))
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

pub(super) async fn declare_mapping_forge(backend: &ResourceBackend, namespace: &str) {
    let spec = ForgeSpec::builder()
        .forge_id("lab".to_string())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(BTreeSet::from(["lab.example".to_string()]))
        .https_url("https://lab.example".to_string())
        .git_ssh_host("lab.example".to_string())
        .build();
    backend.using::<Forge>(namespace).create(&empty_meta("lab"), &spec).await.expect("declare Forge");
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

pub(super) async fn create_credential_test_session(backend: &ResourceBackend, name: &str, convoy: &str, vessel_ref: &str, env_ref: &str) {
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let session = sessions
        .create(
            &empty_meta(name),
            &TerminalSessionSpec {
                env_ref: env_ref.to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Agent {
                    selector: Selector { capability: "code".to_string(), adapter: None, model: None },
                    brief: flotilla_resources::TerminalBrief {
                        artifact_digest: None,
                        path: ".flotilla/briefs/coder.md".to_string(),
                        content: "Work on the issue".to_string(),
                        copies: Vec::new(),
                    },
                    context: Box::new(flotilla_resources::TerminalCrewContext {
                        namespace: NAMESPACE.to_string(),
                        convoy: convoy.to_string(),
                        vessel_ref: vessel_ref.to_string(),
                    }),
                    message: None,
                },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "test".to_string(),
            },
        )
        .await
        .expect("create live crew session");
    sessions
        .update_status(
            name,
            &session.metadata.resource_version,
            &TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..TerminalSessionStatus::default() },
        )
        .await
        .expect("mark crew session live");
}

pub(super) fn manual_profile(host_id: &str, docker_available: bool) -> LocalProvisioningProfile {
    LocalProvisioningProfile {
        host_id: host_id.to_string(),
        display_name: "kiwi".to_string(),
        repo_default_dir: "/Users/tester/dev/flotilla-repos".to_string(),
        host_direct_pool: "passthrough".to_string(),
        docker_pool: "passthrough".to_string(),
        available_pools: vec!["passthrough".to_string()],
        available_agent_adapters: BTreeSet::new(),
        docker_available,
    }
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
    daemon_with_backend(tracked_repos, config, ResourceBackend::InMemory(Default::default())).await
}

pub(super) async fn sqlite_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
    std::fs::create_dir_all(config.state_dir()).expect("state dir");
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(config.state_dir().join("resources.sqlite")).expect("sqlite backend"));
    daemon_with_backend(tracked_repos, config, backend).await
}

pub(super) fn insert_undecodable_resource<T: Resource>(connection: &rusqlite::Connection, name: &str) {
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
    crew_daemon_with_backend(config, ResourceBackend::InMemory(Default::default())).await
}

pub(super) async fn crew_daemon_with_backend(
    config: Arc<ConfigStore>,
    backend: ResourceBackend,
) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
    let pool = Arc::new(FakeTerminalPool::new());
    let mut discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_core::providers::terminal::TerminalPool>),
    );
    discovery.factories.vcs.push(Box::new(flotilla_core::providers::discovery::factories::git::GitVcsFactory));
    let daemon =
        InProcessDaemon::new_with_resource_backend(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy"), backend)
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
        FakeDiscoveryProviders::new().with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_core::providers::terminal::TerminalPool>),
    );
    discovery.factories.vcs.push(Box::new(flotilla_core::providers::discovery::factories::git::GitVcsFactory));
    discovery.runner = Arc::new(ProcessCommandRunner);
    let daemon = InProcessDaemon::new(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy")).await;
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
fn test_agent_material_registry(env: Arc<dyn EnvVars>) -> AgentMaterialRegistry {
    AgentMaterialRegistry::with_codex_delivery_support(env, true)
}
