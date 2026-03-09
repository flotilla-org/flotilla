use std::collections::HashMap;
use std::sync::Arc;

use flotilla_core::config::ConfigStore;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{Command, DaemonEvent};

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
async fn execute_broadcasts_lifecycle_events() {
    let repo = std::env::current_dir().unwrap();
    let config = Arc::new(ConfigStore::new());
    let daemon = InProcessDaemon::new(vec![repo.clone()], config).await;
    let mut rx = daemon.subscribe();

    // Execute a command that goes through the spawned task path.
    // ArchiveSession with a non-existent ID returns immediately with
    // "session not found" — no external API calls, deterministic.
    // We only care about the lifecycle events, not the command result.
    let command = Command::ArchiveSession {
        session_id: "nonexistent-session".into(),
    };
    let command_id = daemon
        .execute(&repo, command)
        .await
        .expect("execute should return a command id");

    // Collect CommandStarted and CommandFinished events, skipping any
    // Snapshot events that arrive from the background refresh loop.
    let timeout = std::time::Duration::from_secs(10);
    let mut got_started = false;
    let mut got_finished = false;
    let mut started_id = None;
    let mut finished_id = None;

    let result = tokio::time::timeout(timeout, async {
        while !got_started || !got_finished {
            match rx.recv().await {
                Ok(DaemonEvent::CommandStarted {
                    command_id: id,
                    repo: ref event_repo,
                    ..
                }) => {
                    assert_eq!(
                        event_repo, &repo,
                        "CommandStarted repo should match executed repo"
                    );
                    started_id = Some(id);
                    got_started = true;
                }
                Ok(DaemonEvent::CommandFinished {
                    command_id: id,
                    repo: ref event_repo,
                    ..
                }) => {
                    assert_eq!(
                        event_repo, &repo,
                        "CommandFinished repo should match executed repo"
                    );
                    finished_id = Some(id);
                    got_finished = true;
                }
                Ok(_) => {
                    // Skip snapshot and other events
                }
                Err(e) => panic!("unexpected recv error: {:?}", e),
            }
        }
    })
    .await;

    result.expect("timed out waiting for lifecycle events");

    // Both events must carry the same command ID returned by execute()
    assert_eq!(
        started_id,
        Some(command_id),
        "CommandStarted id should match the id returned by execute()"
    );
    assert_eq!(
        finished_id,
        Some(command_id),
        "CommandFinished id should match the id returned by execute()"
    );
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
