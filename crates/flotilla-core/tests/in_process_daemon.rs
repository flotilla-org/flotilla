use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::DaemonEvent;

#[tokio::test]
async fn daemon_broadcasts_snapshots() {
    let repo = std::env::current_dir().unwrap();
    let daemon = InProcessDaemon::new(vec![repo.clone()]).await;
    let mut rx = daemon.subscribe();

    let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout waiting for snapshot")
        .expect("recv error");

    match event {
        DaemonEvent::Snapshot(snap) => {
            assert_eq!(snap.repo, repo);
            assert!(snap.seq > 0);
        }
        other => panic!("expected Snapshot, got {:?}", other),
    }
}
