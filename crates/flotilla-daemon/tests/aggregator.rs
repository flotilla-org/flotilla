//! Integration test: Aggregator wired into DaemonRuntime.
//!
//! Verifies that creating a Convoy resource causes a ResultSet event to
//! reach subscribed clients through the daemon's broadcast event bus.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{
    result_set::{ConvoyRow, QueryId, ResultSet, SessionRow},
    DaemonEvent, HostName, QueryCursor,
};
use flotilla_resources::{
    ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend, TerminalSession, TerminalSessionPhase, TerminalSessionSource,
    TerminalSessionSpec, TerminalSessionStatus, CONVOY_LABEL, REPO_LABEL,
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

fn session_rows(result_set: &ResultSet) -> &[SessionRow] {
    result_set.rows.as_sessions().expect("session rows")
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
async fn running_convoyless_session_emits_attachable_session_row() {
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
    let mut rx = daemon.subscribe();
    let sessions = backend.using::<TerminalSession>("flotilla");
    let convoy_session = sessions
        .create(
            &InputMeta::builder()
                .name("terminal-convoy-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "host-direct-local".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: "cleat".to_string(),
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
                env_ref: "host-direct-local".to_string(),
                role: "yeoman".to_string(),
                source: TerminalSessionSource::Tool { command: "bash".to_string() },
                cwd: "/repo".to_string(),
                pool: "cleat".to_string(),
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
                Ok(DaemonEvent::ResultSet(result_set)) if result_set.query() == QueryId::Sessions => {
                    let rows = session_rows(&result_set);
                    if !rows.is_empty() {
                        return rows.to_vec();
                    }
                }
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Sessions => {
                    let rows = delta.changed.as_sessions().expect("session rows");
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
    .expect("timed out waiting for sessions result rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "terminal-yeoman");
    assert_eq!(rows[0].repo.as_ref().map(|repo| repo.0.as_str()), Some("flotilla-org/flotilla"));
    assert_eq!(rows[0].host, HostName::new("local"));
    assert_eq!(rows[0].attach.as_deref(), Some("terminal-yeoman"));
    assert_eq!(rows[0].phase, flotilla_protocol::SessionPhase::Running);

    let replay =
        daemon.subscribe_queries(&[QueryCursor { query: QueryId::Sessions, since: None }]).await.expect("subscribe to sessions query");
    let replayed = replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::ResultSet(result_set) if result_set.query() == QueryId::Sessions => Some(session_rows(result_set)),
            _ => None,
        })
        .expect("sessions replay result set");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].name, "terminal-yeoman");

    let replica = daemon.fleet_replica_snapshot_internal().await.expect("fleet replica snapshot");
    let local_sessions = replica
        .result_sets
        .iter()
        .find(|result_set| result_set.query() == QueryId::Sessions)
        .map(session_rows)
        .expect("local sessions result set");
    assert_eq!(local_sessions.len(), 1);

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
                Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == QueryId::Sessions && !delta.removed.is_empty() => return delta,
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
