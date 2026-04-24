//! Integration test: ConvoyProjection wired into DaemonRuntime.
//!
//! Verifies that creating a Convoy resource causes a NamespaceSnapshot event to
//! reach subscribed clients through the daemon's broadcast event bus.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{DaemonEvent, HostName, StreamKey};
use flotilla_resources::{ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend};

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
    ConvoySpec { workflow_ref: workflow_ref.to_string(), inputs: BTreeMap::new(), placement_policy: None, repository: None, r#ref: None }
}

#[tokio::test]
async fn convoy_projection_emits_namespace_events() {
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
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    // Create a Convoy resource — the projection should pick it up via the watch
    // stream and emit a NamespaceSnapshot for "flotilla".
    let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");
    convoys.create(&convoy_meta("test-convoy-1"), &convoy_spec("my-workflow")).await.expect("create convoy");

    // Wait for a NamespaceSnapshot for the "flotilla" namespace.
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::NamespaceSnapshot(snap)) if snap.namespace == "flotilla" => {
                    return snap;
                }
                Ok(_) => continue,
                Err(err) => panic!("broadcast receive error: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for NamespaceSnapshot for 'flotilla' namespace");

    assert_eq!(found.namespace, "flotilla");
    assert_eq!(found.convoys.len(), 1, "expected exactly one convoy in the snapshot");
    assert_eq!(found.convoys[0].name, "test-convoy-1");
}

/// Verifies the causal chain:
///   1. Create convoy A  → NamespaceSnapshot arrives; record cursor seq.
///   2. Create convoy B  → NamespaceDelta arrives.
///   3. ReplaySince with the cursor from step 1 → response must include at
///      least one NamespaceSnapshot or NamespaceDelta for namespace "flotilla"
///      that reflects convoy B.
#[tokio::test]
async fn replay_since_returns_namespace_events_after_seq() {
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
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    let convoys = backend.using::<flotilla_resources::Convoy>("flotilla");

    // Step 1: Create convoy A and wait for the NamespaceSnapshot.
    convoys.create(&convoy_meta("convoy-a"), &convoy_spec("wf-a")).await.expect("create convoy-a");

    let snapshot_after_a = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::NamespaceSnapshot(snap)) if snap.namespace == "flotilla" => return snap,
                Ok(_) => continue,
                Err(err) => panic!("recv error waiting for snapshot: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for NamespaceSnapshot after convoy-a");

    let cursor_seq = snapshot_after_a.seq;
    assert!(cursor_seq > 0, "snapshot seq must be positive");

    // Step 2: Create convoy B and wait for the NamespaceDelta.
    convoys.create(&convoy_meta("convoy-b"), &convoy_spec("wf-b")).await.expect("create convoy-b");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(DaemonEvent::NamespaceDelta(delta)) if delta.namespace == "flotilla" => return delta,
                Ok(_) => continue,
                Err(err) => panic!("recv error waiting for delta: {err}"),
            }
        }
    })
    .await
    .expect("timed out waiting for NamespaceDelta after convoy-b");

    // Step 3: ReplaySince with the cursor from step 1.
    let cursors = std::collections::HashMap::from([(StreamKey::Namespace { name: "flotilla".to_string() }, cursor_seq)]);
    let replay_events = daemon.replay_since(&cursors).await.expect("replay_since");

    // The replay must include a NamespaceSnapshot for "flotilla" that contains convoy-b
    // (because the seq advanced past cursor_seq, so the full snapshot is re-sent).
    let namespace_snap = replay_events.iter().find_map(|e| match e {
        DaemonEvent::NamespaceSnapshot(snap) if snap.namespace == "flotilla" => Some(snap),
        _ => None,
    });

    let snap = namespace_snap.expect("expected a NamespaceSnapshot for 'flotilla' in replay response");
    assert!(
        snap.convoys.iter().any(|c| c.name == "convoy-b"),
        "replay snapshot for 'flotilla' must contain convoy-b; got: {:?}",
        snap.convoys.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}
