use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, providers::discovery::test_support::git_process_discovery};
use flotilla_daemon::server::DaemonServer;
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, RepoSelector, StreamKey};
use tokio::time::Instant;

/// Execute a query command via execute_query and return the result directly.
async fn run_query(daemon: &dyn DaemonHandle, action: CommandAction) -> CommandValue {
    let command = Command { node_id: None, provisioning_target: None, context_repo: None, action };
    daemon.execute_query(command, uuid::Uuid::nil()).await.expect("execute_query")
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn socket_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");

    // Use workspace root (a real git repo) as the test repo.
    // CARGO_MANIFEST_DIR points to crates/flotilla-daemon; go up two levels.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir.parent().unwrap().parent().unwrap().to_path_buf();

    // Start daemon server
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let server = DaemonServer::new(vec![repo.clone()], config, git_process_discovery(false), socket_path.clone(), Duration::from_secs(300))
        .await
        .expect("server config should be valid");

    let server_handle = tokio::spawn(async move { server.run().await });

    // Connect client with retry.
    // Using real connect attempts is more reliable than checking socket path
    // existence in slower/sandboxed environments.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let client = loop {
        match flotilla_client::SocketDaemon::connect(&socket_path).await {
            Ok(client) => break client,
            Err(connect_err) => {
                if server_handle.is_finished() {
                    match server_handle.await {
                        Ok(Ok(())) => panic!("daemon server exited before client connected (last connect error: {connect_err})"),
                        Ok(Err(server_err)) => {
                            // Some CI/sandbox environments disallow binding Unix
                            // sockets entirely (EPERM). Skip in that case.
                            if server_err.contains("Operation not permitted") {
                                eprintln!("skipping socket_roundtrip: unix socket bind not permitted in this environment: {server_err}");
                                return;
                            }
                            panic!("daemon server failed before client connected: {server_err} (last connect error: {connect_err})")
                        }
                        Err(join_err) => {
                            panic!("daemon server task panicked before client connected: {join_err} (last connect error: {connect_err})")
                        }
                    }
                }
                if Instant::now() >= connect_deadline {
                    server_handle.abort();
                    panic!("timed out connecting to daemon: {connect_err}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // list_repos — should have at least our repo
    let repos = client.list_repos().await.expect("list_repos");
    assert!(!repos.is_empty(), "should have at least one repo");
    assert_eq!(repos[0].path.as_deref(), Some(repo.as_path()));

    // get_state — should return a snapshot for our repo
    let snapshot = client.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state");
    assert_eq!(snapshot.repo.as_deref(), Some(repo.as_path()));

    // Subscribe BEFORE refresh to avoid race — event may fire before subscribe
    let mut rx = client.subscribe();

    // refresh — should succeed (triggers a re-scan)
    client
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::Refresh { repo: Some(RepoSelector::Path(repo.clone())) },
        })
        .await
        .expect("refresh");
    // Wait for a snapshot or delta event (skip command lifecycle events)
    let _snapshot_event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("recv");
            if matches!(event, DaemonEvent::RepoSnapshot(_) | DaemonEvent::RepoDelta(_)) {
                return event;
            }
        }
    })
    .await
    .expect("timeout waiting for snapshot event");

    // replay_since with current seq — should return no repo events (up to date), but may include HostSnapshots
    let snapshot = client.get_state(&RepoSelector::Path(repo.clone())).await.expect("get_state");
    let last_seen = HashMap::from([(StreamKey::Repo { identity: snapshot.repo_identity.clone() }, snapshot.seq)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    let repo_events: Vec<_> = replay.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_) | DaemonEvent::RepoDelta(_))).collect();
    assert!(repo_events.is_empty(), "should have no repo events when up to date, got {} events", repo_events.len());

    let host_replay = client.replay_since(&HashMap::new()).await.expect("replay_since");
    let local_node_id = host_replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) if snap.is_local => Some(snap.node.node_id.clone()),
            _ => None,
        })
        .expect("expected local host snapshot");
    let local_host_seq = host_replay
        .iter()
        .find_map(|event| match event {
            DaemonEvent::HostSnapshot(snap) if snap.node.node_id == local_node_id => Some(snap.seq),
            _ => None,
        })
        .expect("expected local host snapshot seq");
    let replay = client
        .replay_since(&HashMap::from([(StreamKey::Host { node_id: local_node_id.clone() }, local_host_seq)]))
        .await
        .expect("replay_since");
    let host_events: Vec<_> =
        replay.iter().filter(|event| matches!(event, DaemonEvent::HostSnapshot(snap) if snap.node.node_id == local_node_id)).collect();
    assert!(host_events.is_empty(), "should have no host events when the local host cursor is current");

    // replay_since with bogus seq — should return full snapshot
    let last_seen = HashMap::from([(StreamKey::Repo { identity: snapshot.repo_identity }, 999999)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    let repo_snapshots: Vec<_> = replay.iter().filter(|e| matches!(e, DaemonEvent::RepoSnapshot(_))).collect();
    assert_eq!(repo_snapshots.len(), 1, "should get one full repo snapshot");
    assert!(
        matches!(repo_snapshots[0], DaemonEvent::RepoSnapshot(snap) if snap.repo.as_deref() == Some(repo.as_path())),
        "expected RepoSnapshot for our repo"
    );

    // Clean up
    server_handle.abort();
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn query_commands_roundtrip() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("test.sock");

    // Use workspace root (a real git repo) as the test repo.
    // CARGO_MANIFEST_DIR points to crates/flotilla-daemon; go up two levels.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir.parent().expect("parent").parent().expect("grandparent").to_path_buf();

    // Start daemon server
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let server = DaemonServer::new(vec![repo.clone()], config, git_process_discovery(false), socket_path.clone(), Duration::from_secs(300))
        .await
        .expect("server config should be valid");

    let server_handle = tokio::spawn(async move { server.run().await });

    // Connect client with retry.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let client = loop {
        match flotilla_client::SocketDaemon::connect(&socket_path).await {
            Ok(client) => break client,
            Err(connect_err) => {
                if server_handle.is_finished() {
                    match server_handle.await {
                        Ok(Ok(())) => panic!("daemon server exited before client connected (last connect error: {connect_err})"),
                        Ok(Err(server_err)) => {
                            if server_err.contains("Operation not permitted") {
                                eprintln!(
                                    "skipping query_commands_roundtrip: unix socket bind not permitted in this environment: {server_err}"
                                );
                                return;
                            }
                            panic!("daemon server failed before client connected: {server_err} (last connect error: {connect_err})")
                        }
                        Err(join_err) => {
                            panic!("daemon server task panicked before client connected: {join_err} (last connect error: {connect_err})")
                        }
                    }
                }
                if Instant::now() >= connect_deadline {
                    server_handle.abort();
                    panic!("timed out connecting to daemon: {connect_err}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // Wait for initial data to be available by polling get_state until it
    // returns a snapshot. The initial snapshot event fires during/before
    // connect, so subscribe+recv would race and miss it.
    let data_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client.get_state(&RepoSelector::Path(repo.clone())).await.is_ok() {
            break;
        }
        if Instant::now() >= data_deadline {
            server_handle.abort();
            panic!("timed out waiting for initial snapshot data");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Test get_status
    let status = client.get_status().await.expect("get_status");
    assert!(!status.repos.is_empty(), "status should list at least one repo");
    assert_eq!(status.repos[0].path, repo);

    // Test repo query commands via execute_query() + directed responses
    let repo_name = repo.file_name().expect("repo file_name").to_str().expect("repo name utf8");

    let detail_result = run_query(&*client, CommandAction::QueryRepoDetail { repo: RepoSelector::Query(repo_name.to_string()) }).await;
    match detail_result {
        CommandValue::RepoDetail(detail) => assert_eq!(detail.path, repo),
        other => panic!("expected RepoDetail, got {other:?}"),
    }

    let providers_result =
        run_query(&*client, CommandAction::QueryRepoProviders { repo: RepoSelector::Query(repo_name.to_string()) }).await;
    match providers_result {
        CommandValue::RepoProviders(providers) => {
            assert_eq!(providers.path, repo);
            assert!(!providers.providers.is_empty(), "should have at least VCS provider");
        }
        other => panic!("expected RepoProviders, got {other:?}"),
    }

    let work_result = run_query(&*client, CommandAction::QueryRepoWork { repo: RepoSelector::Query(repo_name.to_string()) }).await;
    match work_result {
        CommandValue::RepoWork(work) => assert_eq!(work.path, repo),
        other => panic!("expected RepoWork, got {other:?}"),
    }

    // Test host query commands via execute_query()
    let hosts_result = run_query(&*client, CommandAction::QueryHostList {}).await;
    let local_node_id = match hosts_result {
        CommandValue::HostList(hosts) => {
            hosts.hosts.iter().find(|entry| entry.is_local).map(|entry| entry.node.node_id.clone()).expect("expected local host entry")
        }
        other => panic!("expected HostList, got {other:?}"),
    };
    let host_status_result = run_query(&*client, CommandAction::QueryHostStatus { target_node_id: local_node_id.clone() }).await;
    match host_status_result {
        CommandValue::HostStatus(status) => assert!(status.is_local, "local host query should resolve to local host"),
        other => panic!("expected HostStatus, got {other:?}"),
    }

    let host_providers_result = run_query(&*client, CommandAction::QueryHostProviders { target_node_id: local_node_id.clone() }).await;
    match host_providers_result {
        CommandValue::HostProviders(providers) => assert_eq!(providers.summary.node.node_id, local_node_id),
        other => panic!("expected HostProviders, got {other:?}"),
    }

    let topology = client.get_topology().await.expect("get_topology");
    assert_eq!(topology.local_node.node_id, local_node_id);

    // Test slug resolution error for nonexistent repo
    let err_result = run_query(&*client, CommandAction::QueryRepoDetail { repo: RepoSelector::Query("nonexistent".to_string()) }).await;
    assert!(matches!(err_result, CommandValue::Error { .. }), "nonexistent slug should return error");

    // Clean up
    server_handle.abort();
}

#[tokio::test]
#[cfg_attr(feature = "skip-no-sandbox-tests", ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include")]
async fn execute_refresh_all_roundtrip_emits_lifecycle_events() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let socket_path = tmp.path().join("test.sock");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir.parent().expect("parent").parent().expect("grandparent").to_path_buf();

    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let server = DaemonServer::new(vec![repo.clone()], config, git_process_discovery(false), socket_path.clone(), Duration::from_secs(300))
        .await
        .expect("server config should be valid");

    let server_handle = tokio::spawn(async move { server.run().await });

    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let client = loop {
        match flotilla_client::SocketDaemon::connect(&socket_path).await {
            Ok(client) => break client,
            Err(connect_err) => {
                if server_handle.is_finished() {
                    match server_handle.await {
                        Ok(Ok(())) => panic!("daemon server exited before client connected (last connect error: {connect_err})"),
                        Ok(Err(server_err)) => {
                            if server_err.contains("Operation not permitted") {
                                eprintln!(
                                    "skipping execute_refresh_all_roundtrip_emits_lifecycle_events: unix socket bind not permitted in this environment: {server_err}"
                                );
                                return;
                            }
                            panic!("daemon server failed before client connected: {server_err} (last connect error: {connect_err})")
                        }
                        Err(join_err) => {
                            panic!("daemon server task panicked before client connected: {join_err} (last connect error: {connect_err})")
                        }
                    }
                }
                if Instant::now() >= connect_deadline {
                    server_handle.abort();
                    panic!("timed out connecting to daemon: {connect_err}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    let data_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client.get_state(&RepoSelector::Path(repo.clone())).await.is_ok() {
            break;
        }
        if Instant::now() >= data_deadline {
            server_handle.abort();
            panic!("timed out waiting for initial snapshot data");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut rx = client.subscribe();
    let local_node_id = match run_query(&*client, CommandAction::QueryHostList {}).await {
        CommandValue::HostList(hosts) => {
            hosts.hosts.iter().find(|entry| entry.is_local).map(|entry| entry.node.node_id.clone()).expect("expected local host entry")
        }
        other => panic!("expected HostList, got {other:?}"),
    };
    let command_id = client
        .execute(Command { node_id: None, provisioning_target: None, context_repo: None, action: CommandAction::Refresh { repo: None } })
        .await
        .expect("execute refresh all");

    let lifecycle = tokio::time::timeout(Duration::from_secs(10), async {
        let mut started = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, node_id, repo: event_repo, description, .. }) if id == command_id => {
                    assert_eq!(node_id, local_node_id);
                    assert_eq!(event_repo.as_deref(), Some(repo.as_path()));
                    assert_eq!(description, "Refreshing...");
                    started = Some(id);
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, node_id, repo: event_repo, result, .. }) if id == command_id => {
                    assert_eq!(node_id, local_node_id);
                    assert_eq!(event_repo.as_deref(), Some(repo.as_path()));
                    break (started, Some((id, result)));
                }
                Ok(_) => {}
                Err(err) => panic!("event stream closed unexpectedly: {err}"),
            }
        }
    })
    .await
    .expect("timeout waiting for command lifecycle");

    assert_eq!(lifecycle.0, Some(command_id));
    let (finished_id, result) = lifecycle.1.expect("command finished event");
    assert_eq!(finished_id, command_id);
    assert_eq!(result, CommandValue::Refreshed { repos: vec![repo.clone()] });

    server_handle.abort();
}
