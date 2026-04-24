//! End-to-end test: Convoy resource creation flows through InProcessDaemon →
//! ConvoyProjection → NamespaceSnapshot broadcast → App.convoys("flotilla").
//!
//! The test subscribes the TUI App directly to the daemon's broadcast channel
//! and polls app.convoys() — no real socket needed.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::HostName;
use flotilla_resources::{Convoy, ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend};
use flotilla_tui::{app::App, theme::Theme};

fn test_config(dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"tui-convoy-e2e\"\n").expect("write daemon config");
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
async fn tui_shows_convoys_from_daemon() {
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

    // Subscribe before starting the runtime so we don't miss the first snapshot.
    let mut daemon_rx = daemon.subscribe();

    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    // Build a TUI App wired to the same daemon (no repos needed for convoy view).
    let daemon_handle: Arc<dyn DaemonHandle> = Arc::clone(&daemon) as Arc<dyn DaemonHandle>;
    let repos = daemon_handle.list_repos().await.expect("list repos");
    let tui_config = Arc::new(ConfigStore::with_base(tmp.path().join("tui-config")));
    let mut app = App::new(Arc::clone(&daemon_handle), repos, tui_config, Theme::classic());

    // Replay any events already emitted before we constructed App.
    for event in daemon_handle.replay_since(&HashMap::new()).await.expect("replay_since") {
        app.handle_daemon_event(event);
    }

    // Create a Convoy resource — ConvoyProjection should pick it up and broadcast
    // a NamespaceSnapshot that the App will ingest via drain_daemon_events below.
    let convoys = backend.using::<Convoy>("flotilla");
    convoys.create(&convoy_meta("my-convoy"), &convoy_spec("my-workflow")).await.expect("create convoy");

    // Poll until App.convoys("flotilla") is non-empty or we time out.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Drain any pending broadcast events into the App.
        loop {
            match daemon_rx.try_recv() {
                Ok(event) => app.handle_daemon_event(event),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    panic!("broadcast lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => panic!("broadcast closed"),
            }
        }

        if !app.convoys("flotilla").is_empty() {
            break;
        }

        if Instant::now() >= deadline {
            panic!("timed out: app.convoys(\"flotilla\") still empty after 5s");
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let convoy_list = app.convoys("flotilla");
    assert_eq!(convoy_list.len(), 1, "expected exactly one convoy; got {convoy_list:?}");
    assert_eq!(convoy_list[0].name, "my-convoy", "convoy name mismatch");
}
