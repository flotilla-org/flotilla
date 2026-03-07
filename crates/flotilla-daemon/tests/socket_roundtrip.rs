use std::path::PathBuf;
use std::time::Duration;

use flotilla_core::daemon::DaemonHandle;
use flotilla_daemon::server::DaemonServer;
use flotilla_protocol::DaemonEvent;

#[tokio::test]
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
    let server = DaemonServer::new(
        vec![repo.clone()],
        socket_path.clone(),
        Duration::from_secs(300),
    )
    .await;

    let server_handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Wait for socket to appear
    for _ in 0..20 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(socket_path.exists(), "socket file should exist");

    // Connect client
    let client = flotilla_tui::socket::SocketDaemon::connect(&socket_path)
        .await
        .expect("connect should succeed");

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
    // The refresh should produce a Snapshot event
    assert!(
        matches!(event, DaemonEvent::Snapshot(_)),
        "expected Snapshot event, got {:?}",
        event
    );

    // Clean up
    server_handle.abort();
}
