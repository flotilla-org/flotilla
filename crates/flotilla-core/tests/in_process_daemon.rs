use std::sync::Arc;

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::DaemonEvent;

#[tokio::test]
async fn daemon_broadcasts_snapshots() {
    let repo = std::env::current_dir().unwrap();
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config).await;
    let mut rx = daemon.subscribe();

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout waiting for snapshot")
        .expect("recv error");

    match event {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
            assert!(snap.seq > 0);
        }
        DaemonEvent::SnapshotDelta(delta) => {
            assert_eq!(delta.repo, repo);
            assert!(delta.seq > 0);
        }
        other => panic!("expected SnapshotFull or SnapshotDelta, got {:?}", other),
    }
}
