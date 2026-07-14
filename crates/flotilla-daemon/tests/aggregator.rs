//! Integration test: Aggregator wired into DaemonRuntime.
//!
//! Verifies that creating a Convoy resource causes a ResultSet event to
//! reach subscribed clients through the daemon's broadcast event bus.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::{fake_discovery, fake_discovery_with_provider_set, FakeDiscoveryProviders, FakeTerminalPool},
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{
    result_set::{ConvoyRow, IndependentRow, QueryId, ResultSet},
    DaemonEvent, HostName, QueryCursor,
};
use flotilla_resources::{
    Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoySpec, ConvoyStatus, Environment, EnvironmentSpec, HostDirectEnvironmentSpec,
    InMemoryBackend, InputMeta, ResourceBackend, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec,
    TerminalSessionStatus, VesselRequirement, WorkPhase as ResourceWorkPhase, WorkState, WorkflowSnapshot, CONVOY_LABEL, REPO_LABEL,
    VESSEL_LABEL,
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
        repository: None,
        r#ref: None,
        project_ref: None,
        adopted_checkout_ref: None,
    }
}

fn convoy_rows(result_set: &ResultSet) -> &[ConvoyRow] {
    result_set.rows.as_convoys().expect("convoy rows")
}

fn independent_rows(result_set: &ResultSet) -> &[IndependentRow] {
    result_set.rows.as_independents().expect("independent rows")
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
                    let rows = delta.changed.as_independents().expect("independent rows");
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
    let convoy_replay =
        daemon.subscribe_queries(&[QueryCursor { query: QueryId::Convoys, since: None }]).await.expect("subscribe to convoys query");
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
        .subscribe_queries(&[QueryCursor { query: QueryId::Independents, since: None }])
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
                        .changed
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
                        .changed
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
        .update_status(&running.metadata.name, &running.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Stopped,
            ..Default::default()
        })
        .await
        .expect("stop terminal session");
    let removed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Independents && !delta.removed.is_empty() => return delta,
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for stopped session removal");
    assert_eq!(removed.removed.len(), 1);
    assert_eq!(removed.removed[0].name, "terminal-yeoman");
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
    let replay_events =
        daemon.subscribe_queries(&[QueryCursor { query: QueryId::Convoys, since: Some(cursor_seq) }]).await.expect("subscribe_queries");

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
