//! Integration test: Aggregator wired into DaemonRuntime.
//!
//! Verifies that creating a Convoy resource causes a ResultSet event to
//! reach subscribed clients through the daemon's broadcast event bus.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{
        fake_discovery, fake_discovery_with_provider_set, FakeDiscoveryProviders, FakeIssueProvider, FakeTerminalPool,
    },
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{
    result_set::{ConvoyRow, IndependentRow, QueryId, ResultSet},
    test_support::TestIssue,
    DaemonEvent, HostName, LifecycleAuthority, QueryCursor, QueryScope,
};
use flotilla_resources::{
    Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus,
    Environment, EnvironmentSpec, HostDirectEnvironmentSpec, InMemoryBackend, InputMeta, ObservedCheckoutSpec, Project,
    ProjectRepositorySpec, ProjectSpec, Repository, RepositorySpec, ResourceBackend, TerminalSession, TerminalSessionPhase,
    TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus, VesselRequirement, WorkPhase as ResourceWorkPhase, WorkState,
    WorkflowSnapshot, CONVOY_LABEL, REPO_LABEL, VESSEL_LABEL,
};

fn test_config(dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"test-convoy\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(dir))
}

fn convoy_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: vec![],
        finalizers: vec![],
        deletion_timestamp: None,
    }
}

fn convoy_spec(workflow_ref: &str) -> ConvoySpec {
    ConvoySpec {
        workflow_ref: workflow_ref.to_string(),
        inputs: BTreeMap::new(),
        placement_policy: None,
        repositories: Vec::new(),
        r#ref: None,
        project_ref: None,
        adopted_checkout_refs: BTreeMap::new(),
        issue: None,
        instruction: None,
    }
}

fn convoy_rows(result_set: &ResultSet) -> &[ConvoyRow] {
    result_set.rows.as_convoys().expect("convoy rows")
}

fn independent_rows(result_set: &ResultSet) -> &[IndependentRow] {
    result_set.rows.as_independents().expect("independent rows")
}

#[tokio::test]
async fn scoped_checkout_queries_emit_observed_rows_and_removal_deltas() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/widgets/api.git").expect("remote repository");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository");
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("widgets".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Widgets".to_string())
                .default_workflow_ref("single-agent-contained".to_string())
                .repositories(vec![ProjectRepositorySpec::builder().repo(repository_key.clone()).build()])
                .build(),
        )
        .await
        .expect("create project");
    let daemon =
        InProcessDaemon::new_with_resource_backend(vec![], Arc::clone(&config), fake_discovery(false), HostName::new("local"), backend)
            .await;
    let observed = daemon.observed_resource_backend();
    let options = RuntimeOptions {
        namespace: "flotilla".into(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let repository_query = QueryId::Checkouts { scope: QueryScope::Repository(repository_key.clone()) };
    let project_query = QueryId::Checkouts { scope: QueryScope::Project { namespace: "flotilla".into(), name: "widgets".into() } };
    let subscriber = uuid::Uuid::new_v4();

    let initial_sequences = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = daemon
                .subscribe_queries(subscriber, &[QueryCursor { query: repository_query.clone(), since: None }, QueryCursor {
                    query: project_query.clone(),
                    since: None,
                }])
                .await
                .expect("subscribe checkout queries");
            let result_sets = events
                .iter()
                .filter_map(|event| match event {
                    DaemonEvent::ResultSet(set) if matches!(set.query(), QueryId::Checkouts { .. }) => Some(set),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if result_sets.len() == 2 && result_sets.iter().all(|set| set.state.conditions.is_empty()) {
                break result_sets.iter().map(|set| (set.query(), set.seq)).collect::<HashMap<_, _>>();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("checkout scopes become available");

    let mut events = daemon.subscribe();
    let checkouts = observed.using::<Checkout>("flotilla");
    let checkout = checkouts
        .create(
            &InputMeta::builder().name("checkout-widgets".to_string()).build().with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("feature/query".to_string())
                    .path("/work/widgets".to_string())
                    .repo_ref(repository_key.clone())
                    .host_ref(daemon.local_host_id().expect("local host id").to_string())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("create observed checkout");

    let mut additions = HashMap::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while additions.len() < 2 {
            if let DaemonEvent::ResultDelta(delta) = events.recv().await.expect("checkout event") {
                if matches!(delta.query(), QueryId::Checkouts { .. }) {
                    additions.insert(delta.query(), delta);
                }
            }
        }
    })
    .await
    .expect("repository and project checkout additions");
    for query in [&repository_query, &project_query] {
        let rows = additions.get(query).expect("scoped addition").changes.as_checkouts().expect("checkout changes");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/work/widgets");
        assert_eq!(rows[0].branch, "feature/query");
        assert_eq!(rows[0].authority, LifecycleAuthority::Adopted);
        assert_eq!(rows[0].host, HostName::new("local"));
    }

    let checkout = checkouts
        .update(
            &InputMeta::builder().name(checkout.metadata.name.clone()).build().with_lifecycle_authority(LifecycleAuthority::Adopted),
            &checkout.metadata.resource_version,
            &CheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("feature/revised".to_string())
                    .path("/work/widgets-revised".to_string())
                    .repo_ref(repository_key)
                    .host_ref(daemon.local_host_id().expect("local host id").to_string())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("modify observed checkout");
    let mut modifications = HashMap::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while modifications.len() < 2 {
            if let DaemonEvent::ResultDelta(delta) = events.recv().await.expect("checkout event") {
                if matches!(delta.query(), QueryId::Checkouts { .. }) && delta.changes.as_checkouts().is_some_and(|rows| !rows.is_empty()) {
                    modifications.insert(delta.query(), delta);
                }
            }
        }
    })
    .await
    .expect("repository and project checkout modifications");
    for query in [&repository_query, &project_query] {
        let rows = modifications.get(query).expect("scoped modification").changes.as_checkouts().expect("checkout changes");
        assert_eq!(rows[0].path, "/work/widgets-revised");
        assert_eq!(rows[0].branch, "feature/revised");
    }

    let stale_replay = daemon
        .subscribe_queries(subscriber, &[
            QueryCursor { query: repository_query.clone(), since: initial_sequences.get(&repository_query).copied() },
            QueryCursor { query: project_query.clone(), since: initial_sequences.get(&project_query).copied() },
        ])
        .await
        .expect("replay stale checkout cursors");
    assert_eq!(stale_replay.iter().filter(|event| matches!(event, DaemonEvent::ResultSet(_))).count(), 2);
    assert!(stale_replay
        .iter()
        .filter_map(|event| match event {
            DaemonEvent::ResultSet(set) => set.rows.as_checkouts(),
            _ => None,
        })
        .all(|rows| rows.len() == 1 && rows[0].branch == "feature/revised"));

    let current_replay = daemon
        .subscribe_queries(subscriber, &[
            QueryCursor { query: repository_query.clone(), since: modifications.get(&repository_query).map(|delta| delta.seq) },
            QueryCursor { query: project_query.clone(), since: modifications.get(&project_query).map(|delta| delta.seq) },
        ])
        .await
        .expect("subscribe with current checkout cursors");
    assert!(current_replay.is_empty());

    checkouts.delete(&checkout.metadata.name).await.expect("delete observed checkout");
    let mut removals = HashMap::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while removals.len() < 2 {
            if let DaemonEvent::ResultDelta(delta) = events.recv().await.expect("checkout event") {
                if matches!(delta.query(), QueryId::Checkouts { .. }) && !delta.changes.removed_resources().unwrap_or_default().is_empty() {
                    removals.insert(delta.query(), delta);
                }
            }
        }
    })
    .await
    .expect("repository and project checkout removals");
    for query in [&repository_query, &project_query] {
        assert_eq!(removals.get(query).expect("scoped removal").changes.removed_resources().expect("checkout removals").len(), 1);
    }
}

#[tokio::test]
async fn runtime_start_republishes_durable_adopted_checkouts_before_query_bootstrap() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/widgets/api.git").expect("remote repository");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository");
    backend
        .clone()
        .using::<Checkout>("flotilla")
        .create(
            &InputMeta::builder()
                .name("adopted-checkout-restart".to_string())
                .build()
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(
                ObservedCheckoutSpec::builder()
                    .r#ref("feature/restart".to_string())
                    .path("/work/widgets".to_string())
                    .repo_ref(repository_key.clone())
                    .host_ref("host-before-restart".to_string())
                    .is_main(false)
                    .build(),
            ),
        )
        .await
        .expect("persist durable adopted checkout without status");

    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let options = RuntimeOptions::builder()
        .namespace("flotilla".to_string())
        .heartbeat_interval(Duration::from_secs(300))
        .controller_resync_interval(Duration::from_secs(300))
        .controller_supervision(Default::default())
        .start_controllers(false)
        .build();
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let query = QueryId::Checkouts { scope: QueryScope::Repository(repository_key) };

    let result_set = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = daemon
                .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }])
                .await
                .expect("subscribe checkout query");
            if let Some(result_set) = events.into_iter().find_map(|event| match event {
                DaemonEvent::ResultSet(result_set)
                    if result_set.query() == query && result_set.rows.as_checkouts().is_some_and(|rows| !rows.is_empty()) =>
                {
                    Some(result_set)
                }
                _ => None,
            }) {
                break result_set;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("adopted checkout should be present in the bootstrapped query");

    let rows = result_set.rows.as_checkouts().expect("checkout rows");
    assert!(matches!(rows, [row] if row.authority == LifecycleAuthority::Adopted && row.branch == "feature/restart"));
    let durable =
        backend.using::<Checkout>("flotilla").get("adopted-checkout-restart").await.expect("durable adopted checkout should remain");
    assert_eq!(durable.status, Some(CheckoutStatus::builder().phase(CheckoutPhase::Ready).path("/work/widgets".to_string()).build()));
}

#[tokio::test]
async fn repository_issue_subscription_materializes_from_an_in_memory_provider() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/widgets/api.git").expect("remote repository");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository resource");
    let provider = Arc::new(FakeIssueProvider::new());
    provider
        .add_issues(
            (0..51)
                .map(|index| {
                    let id = format!("WIDGET-{index:03}");
                    (id.clone(), TestIssue::new(&format!("Materialized {id}")).build())
                })
                .collect(),
        )
        .await;
    let discovery = fake_discovery_with_provider_set(
        FakeDiscoveryProviders::new()
            .with_issue_tracker(Arc::clone(&provider) as Arc<dyn flotilla_core::providers::issue_tracker::IssueProvider>),
    );
    let daemon = InProcessDaemon::new_with_resource_backend(vec![], Arc::clone(&config), discovery, HostName::new("local"), backend).await;
    let options = RuntimeOptions {
        namespace: "flotilla".into(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let mut events = daemon.subscribe();
    let query = QueryId::Issues { scope: QueryScope::Repository(repository_key) };

    daemon
        .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }])
        .await
        .expect("subscribe issue query");

    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::ResultSet(set) = events.recv().await.expect("daemon event") {
                if set.query() == query && set.rows.as_issues().is_some_and(|rows| !rows.is_empty()) {
                    return set;
                }
            }
        }
    })
    .await
    .expect("materialized issue window");
    let rows = result.rows.as_issues().expect("issue rows");
    assert_eq!(rows.len(), 50);
    assert_eq!(rows[0].reference.id, "WIDGET-000");
    assert!(result.state.demand.as_ref().expect("demand metadata").has_more);
    assert!(result.state.conditions.is_empty());

    daemon.fetch_more(&query).await.expect("request next page");
    let delta = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::ResultDelta(delta) = events.recv().await.expect("daemon event") {
                if delta.query() == query {
                    return delta;
                }
            }
        }
    })
    .await
    .expect("fetch-more delta");
    assert_eq!(delta.changes.as_issues().expect("appended issue rows")[0].reference.id, "WIDGET-050");
    assert!(!delta.state.as_ref().and_then(|state| state.demand.as_ref()).expect("updated demand metadata").has_more);
}

#[tokio::test]
async fn aggregator_emits_result_set_events() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));

    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;

    // Subscribe before starting the runtime so we don't miss the first event.
    let mut rx = daemon.subscribe();

    // Start with fast resync to avoid test flakiness.
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    // Create a Convoy resource — the Aggregator should pick it up via the watch
    // stream and emit a ResultSet for the convoys query.
    let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");
    let mut spec = convoy_spec("my-workflow");
    spec.project_ref = Some("my-project".to_string());
    convoys.create(&convoy_meta("test-convoy-1"), &spec).await.expect("create convoy");

    // Wait for the convoys result set.
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == QueryId::Convoys => {
                    return result_set;
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ResultSet for convoys query");

    let rows = convoy_rows(&found);
    assert_eq!(rows.len(), 1, "expected exactly one convoy in the result set");
    assert_eq!(rows[0].name, "test-convoy-1");
    assert_eq!(rows[0].project_ref.as_deref(), Some("my-project"));
}

#[tokio::test]
async fn running_convoyless_session_emits_attachable_independent_row() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(Arc::new(FakeTerminalPool::new()))),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let mut rx = daemon.subscribe();
    let host_id = daemon.local_host_id().expect("local host id");
    let environment_name = format!("host-direct-{host_id}");
    let environment_spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_id.to_string(), repo_default_dir: "/tmp".to_string() }),
        docker: None,
    };
    let environments = backend.using::<Environment>("flotilla");
    environments
        .create(&InputMeta::builder().name(environment_name.clone()).build(), &environment_spec)
        .await
        .expect("create attach environment");
    let sessions = daemon.observed_resource_backend().using::<TerminalSession>("flotilla");
    let convoy_session = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-convoy-coder".to_string())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                    (VESSEL_LABEL.to_string(), "coder".to_string()),
                ]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "host-direct-local".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: "fake-terminals".to_string(),
            },
        )
        .await
        .expect("create convoy terminal session");
    sessions
        .update_status(&convoy_session.metadata.name, &convoy_session.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("cleat-convoy-coder".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark convoy terminal session running");
    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.create(&convoy_meta("convoy-a"), &convoy_spec("scratch")).await.expect("create convoy for bound terminal session");
    convoys
        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement::builder().name("coder".to_string()).crew(Vec::new()).build()],
            }),
            work: BTreeMap::from([("coder".to_string(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
            ..Default::default()
        })
        .await
        .expect("mark convoy vessel running");
    let unresolvable = sessions
        .create(&InputMeta::builder().name("terminal-unresolvable".to_string()).build(), &TerminalSessionSpec {
            env_ref: "missing-environment".to_string(),
            role: "observer".to_string(),
            source: TerminalSessionSource::Tool { command: "bash".to_string() },
            cwd: "/repo".to_string(),
            pool: "fake".to_string(),
        })
        .await
        .expect("create unresolvable terminal session");
    sessions
        .update_status(&unresolvable.metadata.name, &unresolvable.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("cleat-unresolvable".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark unresolvable terminal session running");
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    let created = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-yeoman".to_string())
                .labels(BTreeMap::from([(REPO_LABEL.to_string(), "flotilla-org/flotilla".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: environment_name.clone(),
                role: "yeoman".to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: "fake-terminals".to_string(),
            },
        )
        .await
        .expect("create terminal session");
    sessions
        .update_status(&created.metadata.name, &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some("cleat-yeoman".to_string()),
            ..Default::default()
        })
        .await
        .expect("mark terminal session running");

    let rows = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == QueryId::Independents => {
                    let rows = independent_rows(&result_set);
                    if !rows.is_empty() {
                        return rows.to_vec();
                    }
                }
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Independents => {
                    let rows = delta.changes.as_independents().expect("independent rows");
                    if !rows.is_empty() {
                        return rows.to_vec();
                    }
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for independents result rows");

    assert!(
        rows.iter().all(|row| row.name != "terminal-convoy-coder"),
        "convoy-bound terminal sessions surface on vessel rows, never in independents",
    );
    let convoy_replay = daemon
        .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Convoys, since: None }])
        .await
        .expect("subscribe to convoys query");
    let convoy_rows = convoy_replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys => Some(convoy_rows(result_set)),
            _ => None,
        })
        .expect("convoys replay result set");
    let convoy = convoy_rows.iter().find(|row| row.name == "convoy-a").expect("convoy row for bound terminal session");
    assert!(convoy.vessels.iter().any(|vessel| vessel.name == "coder"), "convoy-bound terminal session surfaces on its vessel row");

    let row = rows.iter().find(|row| row.name == "terminal-yeoman").expect("attachable session row");
    assert_eq!(row.repo.as_ref().map(|repo| repo.0.as_str()), Some("flotilla-org/flotilla"));
    assert_eq!(row.host, HostName::new("local"));
    assert_eq!(row.attach.as_deref(), Some("terminal-yeoman"));
    assert_eq!(row.phase, flotilla_protocol::SessionPhase::Running);
    let unresolvable = rows.iter().find(|row| row.name == "terminal-unresolvable").expect("unresolvable session row");
    assert_eq!(unresolvable.attach, None);
    assert!(daemon.resolve_attach_command_internal("terminal-yeoman").await.is_ok());

    let replay = daemon
        .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Independents, since: None }])
        .await
        .expect("subscribe to independents query");
    let replayed = replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Independents => Some(independent_rows(result_set)),
            _ => None,
        })
        .expect("independents replay result set");
    assert_eq!(replayed.len(), 2);
    let unresolvable = replayed.iter().find(|row| row.name == "terminal-unresolvable").expect("unresolvable session row");
    assert_eq!(unresolvable.attach, None);

    let replica = daemon.fleet_replica_snapshot_internal().await.expect("fleet replica snapshot");
    let local_independents = replica
        .result_sets
        .iter()
        .find(|result_set| result_set.query() == QueryId::Independents)
        .map(independent_rows)
        .expect("local independents result set");
    assert_eq!(local_independents.len(), 2);

    environments.delete(&environment_name).await.expect("delete attach environment");
    let unavailable = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Independents => {
                    if let Some(row) = delta
                        .changes
                        .as_independents()
                        .expect("independent rows")
                        .iter()
                        .find(|row| row.name == "terminal-yeoman" && row.attach.is_none())
                    {
                        return row.clone();
                    }
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for attach capability removal");
    assert_eq!(unavailable.attach, None);

    environments
        .create(&InputMeta::builder().name(environment_name.clone()).build(), &environment_spec)
        .await
        .expect("recreate attach environment");
    let available = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Independents => {
                    if let Some(row) = delta
                        .changes
                        .as_independents()
                        .expect("independent rows")
                        .iter()
                        .find(|row| row.name == "terminal-yeoman" && row.attach.as_deref() == Some("terminal-yeoman"))
                    {
                        return row.clone();
                    }
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for attach capability restoration");
    assert_eq!(available.attach.as_deref(), Some("terminal-yeoman"));

    let running = sessions.get("terminal-yeoman").await.expect("running terminal session");
    sessions
        .update(
            &InputMeta::builder()
                .name(running.metadata.name.clone())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                    (VESSEL_LABEL.to_string(), "yeoman".to_string()),
                ]))
                .build(),
            &running.metadata.resource_version,
            &running.spec,
        )
        .await
        .expect("adopt terminal session into convoy");
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta))
                    if delta.query() == QueryId::Independents
                        && delta.changes.removed_resources().is_some_and(|removed| !removed.is_empty()) =>
                {
                    return delta;
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for adopted session removal");
    let removed = removed.changes.removed_resources().expect("independent removals");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].name, "terminal-yeoman");
}

/// Verifies the causal chain:
///   1. Create convoy A  → ResultSet arrives; record cursor seq.
///   2. Create convoy B  → ResultDelta arrives.
///   3. SubscribeQueries with the cursor from step 1 → response must include
///      a full ResultSet for the convoys query that reflects convoy B.
#[tokio::test]
async fn subscribe_queries_replays_result_set_after_seq() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));

    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;

    // Subscribe before starting the runtime.
    let mut rx = daemon.subscribe();

    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");

    // Step 1: Create convoy A and wait for the ResultSet.
    convoys.create(&convoy_meta("convoy-a"), &convoy_spec("wf-a")).await.expect("create convoy-a");

    let result_set_after_a = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == QueryId::Convoys => return result_set,
                Ok(_) => continue,
                Err(err) => panic!("recv error waiting for result set: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ResultSet after convoy-a");

    let cursor_seq = result_set_after_a.seq;
    assert!(cursor_seq > 0, "result set seq must be positive");

    // Step 2: Create convoy B and wait for the ResultDelta.
    convoys.create(&convoy_meta("convoy-b"), &convoy_spec("wf-b")).await.expect("create convoy-b");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Convoys => return delta,
                Ok(_) => continue,
                Err(err) => panic!("recv error waiting for delta: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ResultDelta after convoy-b");

    // Step 3: SubscribeQueries with the cursor from step 1.
    let replay_events = daemon
        .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Convoys, since: Some(cursor_seq) }])
        .await
        .expect("subscribe_queries");

    // The replay must include a ResultSet for the convoys query containing
    // convoy-b (the seq advanced past cursor_seq, so the full set is re-sent).
    let result_set = replay_events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys => Some(result_set),
            _ => None,
        })
        .expect("expected a ResultSet for the convoys query in subscribe replay");
    assert!(convoy_rows(result_set).iter().any(|row| row.name == "convoy-b"), "replayed result set must contain convoy-b");
}
