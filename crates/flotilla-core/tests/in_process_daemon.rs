use std::collections::HashMap;
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

#[tokio::test]
async fn replay_since_returns_full_snapshot_for_unknown_seq() {
    let repo = std::env::current_dir().unwrap();
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config).await;

    // Wait for at least one broadcast so the daemon has state
    let mut rx = daemon.subscribe();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");

    // Request replay with a seq that won't be in the delta log
    let last_seen = HashMap::from([(repo.clone(), 999999)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    assert_eq!(events.len(), 1, "should get exactly one event");
    match &events[0] {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected SnapshotFull, got {:?}", other),
    }
}

#[tokio::test]
async fn replay_since_returns_full_snapshot_for_new_repo() {
    let repo = std::env::current_dir().unwrap();
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config).await;

    // Wait for at least one broadcast
    let mut rx = daemon.subscribe();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");

    // Request replay with empty last_seen (new client)
    let events = daemon
        .replay_since(&HashMap::new())
        .await
        .expect("replay_since");

    assert_eq!(events.len(), 1, "should get one event per tracked repo");
    match &events[0] {
        DaemonEvent::SnapshotFull(snap) => {
            assert_eq!(snap.repo, repo);
        }
        other => panic!("expected SnapshotFull, got {:?}", other),
    }
}

#[tokio::test]
async fn replay_since_returns_empty_when_up_to_date() {
    let repo = std::env::current_dir().unwrap();
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config).await;

    // Wait for the first snapshot to get the current seq
    let mut rx = daemon.subscribe();
    let event = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout")
        .expect("recv");

    let current_seq = match event {
        DaemonEvent::SnapshotFull(snap) => snap.seq,
        DaemonEvent::SnapshotDelta(delta) => delta.seq,
        other => panic!("expected snapshot event, got {:?}", other),
    };

    // Request replay at current seq — should return nothing
    let last_seen = HashMap::from([(repo.clone(), current_seq)]);
    let events = daemon.replay_since(&last_seen).await.expect("replay_since");

    assert!(events.is_empty(), "should be empty when up to date");
}
