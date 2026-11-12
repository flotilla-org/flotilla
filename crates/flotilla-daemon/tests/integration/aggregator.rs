//! Integration test: Aggregator wired into DaemonRuntime.
//!
//! Verifies that creating a Convoy resource causes a ResultSet event to
//! reach subscribed clients through the daemon's broadcast event bus.

use flotilla_store_memory::InMemoryBackend;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_daemon_testkit::engine::{admit_convoy, session_request};
use flotilla_discovery_testkit::fake_discovery;
use flotilla_discovery_testkit::fake_discovery_with_provider_set;
use flotilla_discovery_testkit::FakeDiscoveryProviders;
use flotilla_discovery_testkit::FakeIssueProvider;
use flotilla_discovery_testkit::FakeTerminalPool;
use flotilla_protocol::result_set::ConvoyRow;
use flotilla_protocol::result_set::IndependentRow;
use flotilla_protocol::result_set::QueryId;
use flotilla_protocol::result_set::ResultSet;
use flotilla_protocol::DaemonEvent;
use flotilla_protocol::HostName;
use flotilla_protocol::LifecycleAuthority;
use flotilla_protocol::QueryCursor;
use flotilla_protocol::QueryScope;
use flotilla_protocol::TerminalStatus;
use flotilla_protocol_testkit::TestIssue;
use flotilla_resources::terminal_records::TerminalLaunchRequest;
use flotilla_resources::{
    CheckoutSpec, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus, Environment, EnvironmentSpec,
    HostDirectEnvironmentSpec, InputMeta, ObservedCheckoutSpec, Project, ProjectRepositorySpec, ProjectSpec, Repository, RepositorySpec,
    TerminalSessionSource, TerminalSessionSpec, VesselRequirement, WorkPhase as ResourceWorkPhase, WorkState, WorkflowSnapshot,
};
use flotilla_store::ResourceBackend;
use flotilla_terminal::{TerminalSession as PoolTerminalSession, TerminalSessionLiveness};

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
        continuation: None,
        subjects: Vec::new(),
        role: String::new(),
        generation: 1,
        workflow_ref: workflow_ref.to_string(),
        dispatching_principal_ref: Default::default(),
        inputs: BTreeMap::new(),
        placement_policy: None,
        repositories: Vec::new(),
        r#ref: None,
        project_ref: None,
        adopted_checkout_refs: BTreeMap::new(),
        issues: Vec::new(),
        change_request: None,
        instruction: None,
        promises: Vec::new(),
    }
}

/// Convoy `name` as admission hands it to the engine: `spec`, with one agent seat to owe.
fn engine_convoy(name: &str, spec: ConvoySpec) -> flotilla_resources::ResourceObject<Convoy> {
    flotilla_engine_convoys_testkit::convoy("flotilla", name, spec, flotilla_engine_convoys_testkit::single_agent("coder"))
}

fn convoy_rows(result_set: &ResultSet) -> &[ConvoyRow] {
    result_set.rows.as_convoys().expect("convoy rows")
}

/// The next convoys-query event: its result set or delta, by sequence.
async fn next_convoys_event(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, what: &str) -> DaemonEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == QueryId::Convoys { scope: None } => {
                    return DaemonEvent::ResultSet(result_set);
                }
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Convoys { scope: None } => {
                    return DaemonEvent::ResultDelta(delta);
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// The convoys result set a new subscriber is sent.
async fn convoys_result_set(daemon: &InProcessDaemon) -> ResultSet {
    let events = daemon
        .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: QueryId::Convoys { scope: None }, since: None }])
        .await
        .expect("subscribe_queries");
    events
        .into_iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys { scope: None } => Some(*result_set),
            _ => None,
        })
        .expect("a convoys result set")
}

fn independent_rows(result_set: &ResultSet) -> &[IndependentRow] {
    result_set.rows.as_independents().expect("independent rows")
}

#[tokio::test]
async fn scoped_checkout_queries_emit_observed_rows_and_removal_deltas() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        ResourceBackend::new(InMemoryBackend::default()),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // Repository and Project are the daemon's engine records, written through its store.
    let backend = daemon.resource_backend();
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
                .default_workflow_ref("single-agent".to_string())
                .repositories(vec![ProjectRepositorySpec::builder().repo(repository_key.clone()).build()])
                .build(),
        )
        .await
        .expect("create project");
    let options = RuntimeOptions {
        namespace: "flotilla".into(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        // The engine's checkouts below are stepped by the test, not by a Checkout controller.
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let fleet_query = QueryId::Checkouts { scope: None };
    let project_query = QueryId::Checkouts { scope: Some(QueryScope::new("flotilla", "widgets")) };
    let subscriber = uuid::Uuid::new_v4();

    let initial_sequences = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = daemon
                .subscribe_queries(
                    subscriber,
                    &[QueryCursor { query: fleet_query.clone(), since: None }, QueryCursor { query: project_query.clone(), since: None }],
                )
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
    // An adopted checkout is the engine's, held for its convoy; its row follows its launches, and
    // goes once its launch has ended.
    let engine = flotilla_daemon_testkit::engine::runtime_checkouts(&daemon);
    engine.open_convoy("ship-it");
    let ship_it = engine.holder("ship-it");
    let host_ref = daemon.local_host_id().expect("local host id").to_string();
    let adopted = |branch: &str, path: &str| {
        CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref(branch.to_string())
                .path(path.to_string())
                .repo_ref(repository_key.clone())
                .host_ref(host_ref.clone())
                .is_main(false)
                .build(),
        )
    };
    let launch = engine.put_ready("checkout-widgets", &adopted("feature/query", "/work/widgets"), Some(&ship_it));

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
    for query in [&fleet_query, &project_query] {
        let rows = additions.get(query).expect("scoped addition").changes.as_checkouts().expect("checkout changes");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/work/widgets");
        assert_eq!(rows[0].branch, "feature/query");
        assert_eq!(rows[0].authority, LifecycleAuthority::Adopted);
        assert_eq!(rows[0].host, HostName::new("local"));
        assert_eq!(rows[0].for_convoy.as_deref(), Some("ship-it"));
    }

    // A checkout's spec is its launch's: the revision is the checkout's next launch.
    engine.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
    let launch = engine.put_ready("checkout-widgets", &adopted("feature/revised", "/work/widgets-revised"), Some(&ship_it));
    let mut modifications = HashMap::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while modifications.len() < 2 {
            if let DaemonEvent::ResultDelta(delta) = events.recv().await.expect("checkout event") {
                // The adoption's status projection may still be arriving; wait for the revision.
                if matches!(delta.query(), QueryId::Checkouts { .. })
                    && delta.changes.as_checkouts().is_some_and(|rows| rows.iter().any(|row| row.branch == "feature/revised"))
                {
                    modifications.insert(delta.query(), delta);
                }
            }
        }
    })
    .await
    .expect("repository and project checkout modifications");
    for query in [&fleet_query, &project_query] {
        let rows = modifications.get(query).expect("scoped modification").changes.as_checkouts().expect("checkout changes");
        assert_eq!(rows[0].path, "/work/widgets-revised");
        assert_eq!(rows[0].branch, "feature/revised");
        assert_eq!(rows[0].for_convoy.as_deref(), Some("ship-it"));
    }

    let stale_replay = daemon
        .subscribe_queries(
            subscriber,
            &[
                QueryCursor { query: fleet_query.clone(), since: initial_sequences.get(&fleet_query).copied() },
                QueryCursor { query: project_query.clone(), since: initial_sequences.get(&project_query).copied() },
            ],
        )
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
        .subscribe_queries(
            subscriber,
            &[
                QueryCursor { query: fleet_query.clone(), since: modifications.get(&fleet_query).map(|delta| delta.seq) },
                QueryCursor { query: project_query.clone(), since: modifications.get(&project_query).map(|delta| delta.seq) },
            ],
        )
        .await
        .expect("subscribe with current checkout cursors");
    assert!(current_replay.is_empty());

    engine.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
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
    for query in [&fleet_query, &project_query] {
        assert_eq!(removals.get(query).expect("scoped removal").changes.removed_resources().expect("checkout removals").len(), 1);
    }
}

#[tokio::test]
async fn runtime_start_republishes_engine_adopted_checkouts_before_query_bootstrap() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let repository_spec = RepositorySpec::remote("https://github.com/widgets/api.git").expect("remote repository");
    let repository_key = repository_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository");
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // The engine held the adopted checkout before this start.
    let engine = flotilla_daemon_testkit::engine::runtime_checkouts(&daemon);
    let launch = engine.put_ready(
        "adopted-checkout-restart",
        &CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("feature/restart".to_string())
                .path("/work/widgets".to_string())
                .repo_ref(repository_key.clone())
                .host_ref("host-before-restart".to_string())
                .is_main(false)
                .build(),
        ),
        None,
    );
    let options = RuntimeOptions::builder()
        .namespace("flotilla".to_string())
        .heartbeat_interval(Duration::from_secs(300))
        .controller_resync_interval(Duration::from_secs(300))
        .controller_supervision(Default::default())
        .start_controllers(false)
        .build();
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let query = QueryId::Checkouts { scope: None };

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
    let held = engine.record(&launch);
    assert!(held.is_ready(), "the engine's adopted checkout should remain ready: {held:?}");
    assert_eq!(held.ready_path(), Some("/work/widgets"));
}

#[tokio::test]
async fn project_issue_subscription_materializes_window_and_ephemeral_search() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
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
        FakeDiscoveryProviders::new().with_issue_tracker(Arc::clone(&provider) as Arc<dyn flotilla_issue_tracker::IssueProvider>),
    );
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        discovery,
        HostName::new("local"),
        backend,
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // Repository and Project are the daemon's engine records, written through its store.
    let repository_spec = RepositorySpec::remote("https://github.com/widgets/api.git").expect("remote repository");
    let repository_key = repository_spec.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository resource");
    daemon
        .resource_backend()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("widgets".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Widgets".to_string())
                .default_workflow_ref("single-agent".to_string())
                .repositories(vec![ProjectRepositorySpec::builder().repo(repository_key).build()])
                .build(),
        )
        .await
        .expect("create project resource");
    let options = RuntimeOptions {
        namespace: "flotilla".into(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("start runtime");
    let mut events = daemon.subscribe();
    let scope = QueryScope::new("flotilla", "widgets");
    let query = QueryId::Issues { scope: scope.clone(), search: None, label: None };

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
    assert_eq!(rows[0].reference.id, "WIDGET-049");
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

    let search = QueryId::Issues { scope, search: Some("WIDGET-050".into()), label: None };
    daemon
        .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: search.clone(), since: None }])
        .await
        .expect("subscribe ephemeral search");
    let search_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::ResultSet(set) = events.recv().await.expect("daemon event") {
                if set.query() == search && set.rows.as_issues().is_some_and(|rows| !rows.is_empty()) {
                    return set;
                }
            }
        }
    })
    .await
    .expect("materialized issue search");
    assert!(matches!(search_result.rows.as_issues().expect("search rows"), [row] if row.reference.id == "WIDGET-050"));
}

#[tokio::test]
async fn aggregator_emits_result_set_events() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));

    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
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
    // The aggregator publishes its bootstrap result set once it has listed every source.
    let DaemonEvent::ResultSet(bootstrap) = next_convoys_event(&mut rx, "the bootstrap result set").await else {
        panic!("the convoys query starts with a result set")
    };
    assert!(convoy_rows(&bootstrap).is_empty());

    // Admit a convoy on the engine — the Aggregator should pick it up via the watch
    // stream and emit a delta for the convoys query.
    let mut spec = convoy_spec("my-workflow");
    spec.project_ref = Some("my-project".to_string());
    let launch = admit_convoy(&daemon, engine_convoy("test-convoy-1", spec)).await;
    let DaemonEvent::ResultDelta(_) = next_convoys_event(&mut rx, "the convoy's delta").await else { panic!("a delta follows") };

    let found = convoys_result_set(&daemon).await;
    let rows = convoy_rows(&found);
    assert_eq!(rows.len(), 1, "expected exactly one convoy in the result set");
    assert_eq!(rows[0].name, launch);
    assert_eq!(rows[0].project_ref.as_deref(), Some("my-project"));
}

// Behaviour (#3108): a launch whose admission document does not decode, held when the daemon
// starts as after a restart, neither stops it nor hides the other convoys. It is listed with its
// name and why, nothing of its state told and counted; a launch whose admission carries the
// views' unreadable key still reads whole; and a launch admitted later still arrives.
#[tokio::test]
async fn an_undecodable_launch_is_listed_and_the_daemon_serves_the_others() {
    use flotilla_engine_convoys::convoy_map;
    use flotilla_resources::CONVOY_UNREADABLE_ANNOTATION;
    use flotilla_schema::{Frozen, Timestamp};
    use flotilla_state_machines::machines::convoy::AdmissionBytes;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // Held before this start.
    let readable = admit_convoy(&daemon, engine_convoy("readable", convoy_spec("w"))).await;
    let mut forged = engine_convoy("forged", convoy_spec("w"));
    forged.metadata.annotations.insert(CONVOY_UNREADABLE_ANNOTATION.to_owned(), "not the views' to say".to_owned());
    let forged = admit_convoy(&daemon, forged).await;
    let convoys = daemon.engine_convoys().expect("the Convoy path is installed");
    let mut request = convoy_map::request(&engine_convoy("broken", convoy_spec("w"))).expect("request");
    request.admission = Frozen::new(AdmissionBytes::new(vec![0xff, 0x00])).expect("freeze");
    let now = Timestamp::from_datetime(&chrono::Utc::now());
    let broken = convoys.reserve("broken", now).expect("reserve");
    convoys.create_reserved(&broken, request, now).expect("admit an undecodable document");

    let mut rx = daemon.subscribe();
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");
    let DaemonEvent::ResultSet(bootstrap) = next_convoys_event(&mut rx, "the bootstrap result set").await else {
        panic!("the convoys query starts with a result set")
    };
    let rows = convoy_rows(&bootstrap);
    let row = |name: &str| rows.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("{name} is listed: {rows:?}"));
    let unreadable = row(&broken);
    assert!(unreadable.unreadable.as_deref().is_some_and(|reason| reason.contains("does not decode")), "{unreadable:?}");
    assert_eq!((&unreadable.message, &unreadable.started_at, unreadable.vessels.len()), (&None, &None, 0));
    assert_eq!(unreadable.phase, None, "an unreadable launch has no phase: {unreadable:?}");
    for name in [&readable, &forged] {
        let row = row(name);
        assert_eq!(row.unreadable, None, "{row:?}");
        assert!(row.phase.is_some(), "{row:?}");
        assert!(!row.vessels.is_empty(), "{name} reads whole: {row:?}");
    }
    let view = backend.clone().using::<Convoy>("flotilla").get(&forged).await.expect("the forged launch's view");
    assert!(!view.metadata.annotations.contains_key(CONVOY_UNREADABLE_ANNOTATION));
    assert_eq!(convoys.memory().unreadable(), 1);

    let later = admit_convoy(&daemon, engine_convoy("later", convoy_spec("w"))).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let set = convoys_result_set(&daemon).await;
            if convoy_rows(&set).iter().any(|row| row.name == later) {
                return;
            }
            let _ = next_convoys_event(&mut rx, "a convoy row change").await;
        }
    })
    .await
    .expect("a launch admitted later still arrives");
}

// Behaviour (#3108): an attention the engine holds, a Message to a person, marks its convoy's row.
// The aggregator reads attentions from the engine and nothing else, so one raised there after
// bootstrap reaches the result set.
#[tokio::test]
async fn an_attention_raised_on_the_engine_marks_its_convoys_row() {
    use flotilla_resources::{
        attentions::{Attention, Fact},
        FleetDesignation, FleetDesignationSpec, FLEET_DESIGNATION_NAME,
    };
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut rx = daemon.subscribe();
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");
    let DaemonEvent::ResultSet(_) = next_convoys_event(&mut rx, "the bootstrap result set").await else {
        panic!("the convoys query starts with a result set")
    };
    // An attention is addressed through the fleet's roles, so the fleet is designated.
    backend
        .definitions::<Project>("flotilla")
        .create(
            &convoy_meta("fleet"),
            &ProjectSpec::builder().display_name("Fleet".into()).default_workflow_ref("w".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>("flotilla")
        .create(
            &convoy_meta(FLEET_DESIGNATION_NAME),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let mut spec = convoy_spec("w");
    spec.project_ref = Some("flotilla".to_string());
    let demo = admit_convoy(&daemon, engine_convoy("demo", spec)).await;
    let _ = next_convoys_event(&mut rx, "the convoy's row").await;

    let attention = Attention::new(Fact::ReclaimRefused { convoy: demo.clone() }, "reclaim refused: backing alive", chrono::Utc::now());
    daemon.attentions().raise("flotilla", &attention).await.expect("raise on the engine");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let set = convoys_result_set(&daemon).await;
            if convoy_rows(&set).iter().any(|row| {
                row.name == demo
                    && row.surface_state.needs_attention()
                    && row.message.as_deref().is_some_and(|message| message.contains("reclaim refused: backing alive"))
            }) {
                return;
            }
            let _ = next_convoys_event(&mut rx, "a convoy row change").await;
        }
    })
    .await
    .expect("the engine's attention marks the convoy's row");
}

#[tokio::test]
async fn running_convoyless_session_emits_attachable_independent_row() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let pool = Arc::new(FakeTerminalPool::new());
    pool.add_sessions(vec![
        PoolTerminalSession::builder().session_name("cleat-convoy-coder".to_string()).status(TerminalStatus::Disconnected).build(),
        PoolTerminalSession::builder().session_name("cleat-yeoman".to_string()).status(TerminalStatus::Disconnected).build(),
    ])
    .await;
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery_with_provider_set(FakeDiscoveryProviders::new().with_terminal_pool(pool.clone())),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    // Repository and Project are the daemon's engine records, written through its store.
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
    let repository_key = repository_spec.key();
    daemon
        .resource_backend()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("create repository");
    daemon
        .resource_backend()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("flotilla".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Flotilla".to_string())
                .default_workflow_ref("single-agent".to_string())
                .repositories(vec![ProjectRepositorySpec::builder().repo(repository_key.clone()).build()])
                .build(),
        )
        .await
        .expect("create project");
    let mut rx = daemon.subscribe();
    let host_id = daemon.local_host_id().expect("local host id");
    let environment_name = format!("host-direct-{host_id}");
    let environment_spec = EnvironmentSpec {
        cloud_sandbox: None,
        provider_address: None,
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: host_id.to_string(), repo_default_dir: "/tmp".to_string() }),
        docker: None,
    };
    let environments = backend.using::<Environment>("flotilla");
    environments
        .create(&InputMeta::builder().name(environment_name.clone()).build(), &environment_spec)
        .await
        .expect("create attach environment");
    // The convoy is the engine's launch, its work `coder` running.
    let convoy_a = admit_convoy(
        &daemon,
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "convoy-a",
            convoy_spec("scratch"),
            ConvoyStatus {
                phase: ResourceConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement::builder()
                        .name("coder".to_string())
                        .crew(vec![flotilla_engine_convoys_testkit::agent("coder")])
                        .build()],
                }),
                work: BTreeMap::from([("coder".to_string(), WorkState::builder().phase(ResourceWorkPhase::Running).build())]),
                ..Default::default()
            },
        ),
    )
    .await;
    // The sessions are the engine's launches, written at the daemon's home before its runtime
    // starts; the convoy-bound one is held for the daemon's launch of work `coder` of the convoy.
    let vessel = flotilla_provisioning_testkit::VesselsFixture::over_service(&daemon.decided_store().expect("decided store")).request(
        &convoy_a,
        "coder",
        &host_id.to_string(),
    );
    let terminals = flotilla_daemon_testkit::engine::runtime_terminals(&daemon);
    let tool = |env_ref: &str, role: &str, pool: &str| TerminalSessionSpec {
        env_ref: env_ref.to_string(),
        role: role.to_string(),
        source: TerminalSessionSource::Tool { command: "bash".to_string() },
        cwd: "/repo".to_string(),
        env: Default::default(),
        pool: pool.to_string(),
    };
    let convoy_launch = terminals.request(&TerminalLaunchRequest {
        vessel: Some(vessel.clone()),
        ..session_request("terminal-convoy-coder", &tool(&environment_name, "coder", "fake-terminals"))
    });
    terminals.ready(&convoy_launch, "cleat-convoy-coder", &Default::default());
    let unresolvable = terminals.request(&session_request("terminal-unresolvable", &tool("missing-environment", "observer", "fake")));
    terminals.ready(&unresolvable, "cleat-unresolvable", &Default::default());
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");
    // The sessions' environment is the engine's, requested through the reads the runtime installs (#3108).
    daemon
        .lifecycle_reads()
        .environments()
        .and_then(|records| records.request(&environment_name, &environment_spec, None))
        .expect("request attach environment");

    // A launch carries no repository: no writer ever labels an engine session with one, so the
    // independent row's repository is not asserted here (#3108).
    let yeoman = terminals.request(&session_request("terminal-yeoman", &tool(&environment_name, "yeoman", "fake-terminals")));
    terminals.ready(&yeoman, "cleat-yeoman", &Default::default());

    // Resource rows publish before terminal capability I/O finishes. Wait for
    // the enrichment event, then read a full snapshot: a delta only carries
    // changed recipes and need not repeat the unresolvable session.
    let rows = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ready = match rx.recv().await {
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == (QueryId::Independents { scope: None }) => {
                    independent_rows(&result_set).iter().any(|row| row.attach.as_deref() == Some("terminal-yeoman"))
                }
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == (QueryId::Independents { scope: None }) => delta
                    .changes
                    .as_independents()
                    .expect("independent rows")
                    .iter()
                    .any(|row| row.attach.as_deref() == Some("terminal-yeoman")),
                Ok(_) => false,
                Err(err) => panic!("broadcast receive error: {err}"),
            };
            if ready {
                let replay = daemon
                    .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: QueryId::Independents { scope: None }, since: None }])
                    .await
                    .expect("read enriched independents");
                return replay
                    .iter()
                    .find_map(|event| match event {
                        DaemonEvent::ResultSet(set) if set.query() == (QueryId::Independents { scope: None }) => {
                            Some(independent_rows(set).to_vec())
                        }
                        _ => None,
                    })
                    .expect("full independents snapshot");
            }
        }
    })
    .await
    .expect("timed out waiting for independent attach capability");

    assert!(
        rows.iter().all(|row| row.name != "terminal-convoy-coder"),
        "convoy-bound terminal sessions surface on vessel rows, never in independents",
    );
    // Independents and Convoys have separate delivery boundaries. Wait for the
    // coalesced convoy enrichment too, rather than assuming the independent
    // capability event has already advanced every query family.
    let materialized_convoy = |rows: &[ConvoyRow]| {
        rows.iter()
            .find(|row| row.name == convoy_a)
            .filter(|row| {
                row.vessels.iter().any(|vessel| vessel.name == "coder" && vessel.materialize.as_deref() == Some("terminal-convoy-coder"))
            })
            .cloned()
    };
    let convoy = tokio::time::timeout(Duration::from_secs(5), async {
        let replay = daemon
            .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Convoys { scope: None }, since: None }])
            .await
            .expect("subscribe to convoys query");
        for event in replay {
            if let DaemonEvent::ResultSet(set) = event {
                if let Some(row) = set.rows.as_convoys().and_then(materialized_convoy) {
                    return row;
                }
            }
        }
        loop {
            let ready = match rx.recv().await.expect("convoy enrichment event") {
                DaemonEvent::ResultSet(set) => set.rows.as_convoys().and_then(materialized_convoy),
                DaemonEvent::ResultDelta(delta) => delta.changes.as_convoys().and_then(materialized_convoy),
                _ => None,
            };
            if let Some(row) = ready {
                return row;
            }
        }
    })
    .await
    .expect("timed out waiting for convoy materialization capability");
    let vessel = convoy.vessels.iter().find(|vessel| vessel.name == "coder").expect("convoy-bound session vessel row");
    assert_eq!(vessel.materialize.as_deref(), Some("terminal-convoy-coder"));

    let row = rows.iter().find(|row| row.name == "terminal-yeoman").expect("attachable session row");
    // A standalone launch carries no repository, as no production writer ever gave a session one.
    assert_eq!(row.repo, None);
    assert_eq!(row.repository_key, None);
    assert_eq!(row.host, HostName::new("local"));
    assert_eq!(row.attach.as_deref(), Some("terminal-yeoman"));
    assert_eq!(row.phase, flotilla_protocol::SessionPhase::Running);
    let unresolvable = rows.iter().find(|row| row.name == "terminal-unresolvable").expect("unresolvable session row");
    assert_eq!(unresolvable.attach, None);
    assert!(daemon.resolve_attach_command_internal("terminal-yeoman").await.is_ok());

    let replay = daemon
        .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Independents { scope: None }, since: None }])
        .await
        .expect("subscribe to independents query");
    let replayed = replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == (QueryId::Independents { scope: None }) => {
                Some(independent_rows(result_set))
            }
            _ => None,
        })
        .expect("independents replay result set");
    assert_eq!(replayed.len(), 2);
    let unresolvable = replayed.iter().find(|row| row.name == "terminal-unresolvable").expect("unresolvable session row");
    assert_eq!(unresolvable.attach, None);

    let local_sets = daemon.aggregator_projection_state().await.local_result_sets().await;
    let local_independents = local_sets
        .iter()
        .find(|result_set| result_set.query() == (QueryId::Independents { scope: None }))
        .map(independent_rows)
        .expect("local independents result set");
    assert_eq!(local_independents.len(), 2);
    // Withdrawal when the environment ends: test_daemon.rs a_lost_environment_withdraws_its_sessions_attach.

    // With no repository, a standalone launch is in no project's scope.
    let scope = QueryScope::new("flotilla", "flotilla");
    let scoped = daemon
        .subscribe_queries(
            uuid::Uuid::new_v4(),
            &[QueryCursor { query: QueryId::Independents { scope: Some(scope.clone()) }, since: None }],
        )
        .await
        .expect("subscribe to project independents query");
    let scoped_rows = scoped
        .iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == (QueryId::Independents { scope: Some(scope.clone()) }) => {
                Some(independent_rows(result_set))
            }
            _ => None,
        })
        .expect("project independents replay result set");
    assert!(scoped_rows.is_empty(), "{scoped_rows:?}");

    pool.remove_session("cleat-yeoman").await;
    let error = daemon.resolve_attach_command_internal("terminal-yeoman").await.expect_err("missing endpoint");
    assert_eq!(error, "session terminal-yeoman no longer exists on local");

    pool.set_liveness_override(Ok(TerminalSessionLiveness::Lost("socket gone".to_string()))).await;
    let error = daemon.resolve_attach_command_internal("terminal-yeoman").await.expect_err("lost endpoint");
    assert_eq!(error, "session terminal-yeoman is unreachable on local: socket gone");

    pool.set_liveness_override(Err("cleat unavailable".to_string())).await;
    let error = daemon.resolve_attach_command_internal("terminal-yeoman").await.expect_err("unreachable pool");
    assert_eq!(error, "local unreachable: cleat unavailable");
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

    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;

    // Subscribe before starting the runtime.
    let mut rx = daemon.subscribe();

    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        // This test exercises query replay, not convoy reconciliation. Keeping
        // controllers stopped prevents unrelated status transitions in the
        // intentionally incomplete fixtures.
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    let DaemonEvent::ResultSet(_) = next_convoys_event(&mut rx, "the bootstrap result set").await else {
        panic!("the convoys query starts with a result set")
    };

    // Step 1: Admit convoy A and wait for its change.
    admit_convoy(&daemon, engine_convoy("convoy-a", convoy_spec("wf-a"))).await;
    let DaemonEvent::ResultDelta(after_a) = next_convoys_event(&mut rx, "convoy-a's delta").await else { panic!("a delta follows") };

    let cursor_seq = after_a.seq;
    assert!(cursor_seq > 0, "result set seq must be positive");

    // Step 2: Admit convoy B and wait for the ResultDelta.
    let convoy_b = admit_convoy(&daemon, engine_convoy("convoy-b", convoy_spec("wf-b"))).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Convoys { scope: None } => return delta,
                Ok(_) => continue,
                Err(err) => panic!("recv error waiting for delta: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for ResultDelta after convoy-b");

    // Step 3: SubscribeQueries with the cursor from step 1.
    let replay_events = daemon
        .subscribe_queries(uuid::Uuid::nil(), &[QueryCursor { query: QueryId::Convoys { scope: None }, since: Some(cursor_seq) }])
        .await
        .expect("subscribe_queries");

    // The replay must include a ResultSet for the convoys query containing
    // convoy-b (the seq advanced past cursor_seq, so the full set is re-sent).
    let result_set = replay_events
        .iter()
        .find_map(|e| match e {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Convoys { scope: None } => Some(result_set),
            _ => None,
        })
        .expect("expected a ResultSet for the convoys query in subscribe replay");
    assert!(convoy_rows(result_set).iter().any(|row| row.name == convoy_b), "replayed result set must contain convoy-b");
}

// The inspector replaces the Git subprocess boundary. Admitting a rival convoy
// during inspection models an identity becoming occupied after the first check.
struct ConflictCheckoutInspector {
    daemon: std::sync::Weak<InProcessDaemon>,
}

#[async_trait::async_trait]
impl flotilla_admission::repository_inspection::RepositoryInspector for ConflictCheckoutInspector {
    async fn inspect_path(
        &self,
        path: &std::path::Path,
        _remote: Option<&str>,
    ) -> Result<flotilla_admission::repository_inspection::RepositoryInspection, String> {
        use flotilla_admission::repository_inspection::LocalCheckoutInspection;
        use flotilla_admission::repository_inspection::RepositoryInspection;
        let daemon = self.daemon.upgrade().expect("daemon remains alive");
        let rival = ConvoySpec::builder().role("cleanup".to_string()).workflow_ref("cleanup-workflow".to_string()).build();
        admit_convoy(&daemon, engine_convoy("rival", rival)).await;
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote("https://github.com/widgets/api.git")?,
            checkout: LocalCheckoutInspection::builder()
                .path(path.to_path_buf())
                .host_ref("host".to_string())
                .git_ref("feature/cleanup".to_string())
                .is_main(false)
                .build(),
            transport_url: Some("https://github.com/widgets/api.git".to_string()),
            replaces_prior_repository: false,
        })
    }

    async fn resolve_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        // Admission resolves the remote before it commits. The adopted checkout is recorded on
        // the convoy for its vessel to request on the engine (#3108), so nothing adopted is
        // published, nor shown by the real Aggregator, before then.
        let daemon = self.daemon.upgrade().expect("daemon remains alive");
        let rows = cleanup_checkout_rows(&daemon).await;
        assert!(!rows.iter().any(|row| row.authority == LifecycleAuthority::Adopted), "nothing adopted before admission commits: {rows:?}");
        RepositorySpec::remote(remote)
    }
}

async fn cleanup_checkout_rows(daemon: &InProcessDaemon) -> Vec<flotilla_protocol::result_set::CheckoutRow> {
    let query = QueryId::Checkouts { scope: None };
    let events = daemon
        .subscribe_queries(uuid::Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }])
        .await
        .expect("subscribe checkout query");
    events
        .into_iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(set) if set.query() == query => Some(set.rows.as_checkouts().expect("checkout rows").to_vec()),
            _ => None,
        })
        .expect("checkout result set")
}

// #768: both ways an adopted checkout goes remove its Aggregator row without periodic
// reconciliation. Non-adopted observations survive.
#[tokio::test]
async fn ended_adopted_checkout_removes_its_aggregator_row() {
    assert_adopted_checkout_cleanup(false).await;
}

#[tokio::test]
async fn admission_conflict_leaves_no_adopted_aggregator_row() {
    assert_adopted_checkout_cleanup(true).await;
}

async fn assert_adopted_checkout_cleanup(admission_conflict: bool) {
    use flotilla_protocol::{CommandAction, CommandValue};
    use flotilla_resources::{CrewSource, CrewSpec, WorkflowTemplate, WorkflowTemplateSpec};
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let options = RuntimeOptions::builder()
        .namespace("flotilla".to_string())
        .heartbeat_interval(Duration::from_secs(300))
        .controller_resync_interval(Duration::from_secs(300))
        .controller_supervision(Default::default())
        .start_controllers(false)
        .build();
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
    let repository = RepositorySpec::remote("https://github.com/widgets/api.git").expect("repository");
    backend
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .expect("repository");
    let spec = CheckoutSpec::Observed(
        ObservedCheckoutSpec::builder()
            .r#ref("feature/cleanup".to_string())
            .path("/work/widgets".to_string())
            .repo_ref(repository.key())
            .host_ref(daemon.local_host_id().expect("host").to_string())
            .is_main(false)
            .build(),
    );
    let CheckoutSpec::Observed(unrelated) = spec.clone() else { unreachable!("an observation is observed") };
    daemon.discovered_checkouts().replace(
        &repository.key(),
        vec![flotilla_resources::discovered_checkouts::DiscoveredCheckout {
            name: "unrelated".to_string(),
            repo_slug: repository.key().to_string(),
            spec: unrelated,
        }],
    );
    let engine = flotilla_daemon_testkit::engine::runtime_checkouts(&daemon);
    if admission_conflict {
        std::fs::create_dir(tmp.path().join("checkout")).expect("checkout directory");
        backend
            .using::<WorkflowTemplate>("flotilla")
            .create(
                &InputMeta::builder().name("cleanup-workflow".to_string()).build(),
                &WorkflowTemplateSpec::builder()
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
            .expect("workflow");
        daemon.set_repository_inspector(Arc::new(ConflictCheckoutInspector { daemon: Arc::downgrade(&daemon) })).await;
        let result = cleanup_command_result(
            &daemon,
            CommandAction::ConvoyCreate {
                name: "cleanup".to_string(),
                workflow_ref: "cleanup-workflow".to_string(),
                inputs: vec![],
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
                adopted_checkout: Some(Box::new(tmp.path().join("checkout"))),
            },
        )
        .await;
        assert!(matches!(result, CommandValue::Error { ref message } if message.contains("already exists")), "{result:?}");
        use flotilla_resources::checkout_records::CheckoutRecords as _;
        let held = engine.checkouts.here().expect("engine checkouts");
        assert!(held.is_empty(), "a refused admission must not hold an adopted checkout: {held:?}");
        let remaining = daemon.discovered_checkouts().all();
        assert!(matches!(remaining.as_slice(), [checkout] if checkout.name == "unrelated"), "{remaining:?}");
    } else {
        let launch = engine.put_ready("adopted", &spec, None);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if cleanup_checkout_rows(&daemon).await.iter().any(|row| row.authority == LifecycleAuthority::Adopted) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("adopted Aggregator row");
        // The checkout's launch ends: its change alone removes the adopted row.
        engine.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = cleanup_checkout_rows(&daemon).await;
            if rows.len() == 1 && rows[0].authority == LifecycleAuthority::Observed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("only unrelated Aggregator row remains without periodic reconciliation");
}

async fn cleanup_command_result(daemon: &InProcessDaemon, action: flotilla_protocol::CommandAction) -> flotilla_protocol::CommandValue {
    use flotilla_protocol::Command;
    let mut events = daemon.subscribe();
    let id = daemon.execute(Command::builder().action(action).build()).await.expect("command");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let DaemonEvent::CommandFinished { command_id, result, .. } = events.recv().await.expect("event") {
                if command_id == id {
                    break result;
                }
            }
        }
    })
    .await
    .expect("command completes")
}
