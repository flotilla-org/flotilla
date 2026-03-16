use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, providers::discovery::test_support::git_process_discovery};
use flotilla_daemon::server::DaemonServer;
use flotilla_protocol::{Command, CommandAction, CommandResult, DaemonEvent, HostName};
use tokio::time::Instant;

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
    assert_eq!(repos[0].path, repo);

    // get_state — should return a snapshot for our repo
    let snapshot = client.get_state(&repo).await.expect("get_state");
    assert_eq!(snapshot.repo, repo);

    // Subscribe BEFORE refresh to avoid race — event may fire before subscribe
    let mut rx = client.subscribe();

    // refresh — should succeed (triggers a re-scan)
    client.refresh(&repo).await.expect("refresh");
    let event = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("timeout waiting for event").expect("recv");
    // The refresh should produce a snapshot event (full or delta)
    assert!(matches!(event, DaemonEvent::RepoSnapshot(_) | DaemonEvent::RepoDelta(_)), "expected snapshot event, got {:?}", event);

    // replay_since with current seq — should return empty (up to date)
    let snapshot = client.get_state(&repo).await.expect("get_state");
    let last_seen = HashMap::from([(snapshot.repo_identity.clone(), snapshot.seq)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    assert!(replay.is_empty(), "should be empty when up to date, got {} events", replay.len());

    // replay_since with bogus seq — should return full snapshot
    let last_seen = HashMap::from([(snapshot.repo_identity, 999999)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    assert_eq!(replay.len(), 1, "should get one full snapshot");
    assert!(matches!(&replay[0], DaemonEvent::RepoSnapshot(snap) if snap.repo == repo), "expected RepoSnapshot, got {:?}", replay[0]);

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
        if client.get_state(&repo).await.is_ok() {
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

    // Test get_repo_detail by repo directory name
    let repo_name = repo.file_name().expect("repo file_name").to_str().expect("repo name utf8");
    let detail = client.get_repo_detail(repo_name).await.expect("get_repo_detail");
    assert_eq!(detail.path, repo);

    // Test get_repo_providers
    let providers = client.get_repo_providers(repo_name).await.expect("get_repo_providers");
    assert_eq!(providers.path, repo);
    assert!(!providers.providers.is_empty(), "should have at least VCS provider");

    // Test get_repo_work
    let work = client.get_repo_work(repo_name).await.expect("get_repo_work");
    assert_eq!(work.path, repo);

    // Test host query commands against the local daemon state
    let hosts = client.list_hosts().await.expect("list_hosts");
    assert!(hosts.hosts.iter().any(|entry| entry.host == HostName::local() && entry.is_local));

    let local_host = HostName::local().to_string();
    let host_status = client.get_host_status(&local_host).await.expect("get_host_status");
    assert!(host_status.is_local, "local host query should resolve to local host");

    let host_providers = client.get_host_providers(&local_host).await.expect("get_host_providers");
    assert_eq!(host_providers.summary.host_name, HostName::local());

    let topology = client.get_topology().await.expect("get_topology");
    assert_eq!(topology.local_host, HostName::local());

    // Test slug resolution error for nonexistent repo
    let err = client.get_repo_detail("nonexistent").await;
    assert!(err.is_err(), "nonexistent slug should return error");

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
        if client.get_state(&repo).await.is_ok() {
            break;
        }
        if Instant::now() >= data_deadline {
            server_handle.abort();
            panic!("timed out waiting for initial snapshot data");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut rx = client.subscribe();
    let command_id = client
        .execute(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } })
        .await
        .expect("execute refresh all");

    let lifecycle = tokio::time::timeout(Duration::from_secs(10), async {
        let mut started = None;
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted { command_id: id, host, repo: event_repo, description, .. }) if id == command_id => {
                    assert_eq!(host, flotilla_protocol::HostName::local());
                    assert_eq!(event_repo, repo);
                    assert_eq!(description, "Refreshing...");
                    started = Some(id);
                }
                Ok(DaemonEvent::CommandFinished { command_id: id, host, repo: event_repo, result, .. }) if id == command_id => {
                    assert_eq!(host, flotilla_protocol::HostName::local());
                    assert_eq!(event_repo, repo);
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
    assert_eq!(result, CommandResult::Refreshed { repos: vec![repo.clone()] });

    server_handle.abort();
}
