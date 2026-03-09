use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_daemon::server::DaemonServer;
use flotilla_protocol::DaemonEvent;

#[tokio::test]
#[cfg_attr(
    feature = "skip-no-sandbox-tests",
    ignore = "excluded by `skip-no-sandbox-tests`; run without that feature to include"
)]
async fn socket_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");

    // Use workspace root (a real git repo) as the test repo.
    // CARGO_MANIFEST_DIR points to crates/flotilla-daemon; go up two levels.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    // Start daemon server
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));
    let server = DaemonServer::new(
        vec![repo.clone()],
        config,
        socket_path.clone(),
        Duration::from_secs(300),
    )
    .await;

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
                        Ok(Ok(())) => panic!(
                            "daemon server exited before client connected (last connect error: {connect_err})"
                        ),
                        Ok(Err(server_err)) => {
                            // Some CI/sandbox environments disallow binding Unix
                            // sockets entirely (EPERM). Skip in that case.
                            if server_err.contains("Operation not permitted") {
                                eprintln!(
                                    "skipping socket_roundtrip: unix socket bind not permitted in this environment: {server_err}"
                                );
                                return;
                            }
                            panic!(
                                "daemon server failed before client connected: {server_err} (last connect error: {connect_err})"
                            )
                        }
                        Err(join_err) => panic!(
                            "daemon server task panicked before client connected: {join_err} (last connect error: {connect_err})"
                        ),
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
    let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("recv");
    // The refresh should produce a snapshot event (full or delta)
    assert!(
        matches!(
            event,
            DaemonEvent::SnapshotFull(_) | DaemonEvent::SnapshotDelta(_)
        ),
        "expected snapshot event, got {:?}",
        event
    );

    // replay_since with current seq — should return empty (up to date)
    let snapshot = client.get_state(&repo).await.expect("get_state");
    let last_seen = HashMap::from([(repo.clone(), snapshot.seq)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    assert!(
        replay.is_empty(),
        "should be empty when up to date, got {} events",
        replay.len()
    );

    // replay_since with bogus seq — should return full snapshot
    let last_seen = HashMap::from([(repo.clone(), 999999)]);
    let replay = client.replay_since(&last_seen).await.expect("replay_since");
    assert_eq!(replay.len(), 1, "should get one full snapshot");
    assert!(
        matches!(&replay[0], DaemonEvent::SnapshotFull(snap) if snap.repo == repo),
        "expected SnapshotFull, got {:?}",
        replay[0]
    );

    // Clean up
    server_handle.abort();
}
