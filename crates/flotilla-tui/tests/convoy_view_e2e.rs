//! End-to-end test: Convoy resource creation flows through InProcessDaemon →
//! Aggregator → PanelSnapshot broadcast → App.convoys("flotilla").
//!
//! The test subscribes the TUI App directly to the daemon's broadcast channel
//! and polls app.convoys() — no real socket needed.

#![cfg(unix)]

use flotilla_store_memory::InMemoryBackend;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_testkit::fake_discovery;
use flotilla_protocol::HostName;
use flotilla_resources::{
    Convoy, ConvoySpec, ConvoyStatus, ResourceObject, VesselRequirement, WorkflowSnapshot, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_tui::{app::App, theme::Theme};

fn test_config(dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"tui-convoy-e2e\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(dir))
}

/// Convoy `name` as admission hands it to the engine, with `status`'s frozen workflow.
fn convoy(name: &str, spec: ConvoySpec, status: ConvoyStatus) -> ResourceObject<Convoy> {
    let mut convoy = flotilla_engine_convoys_testkit::convoy("flotilla", name, spec, status);
    convoy.metadata.labels = BTreeMap::from([
        (PROJECT_LABEL.to_string(), "flotilla".to_string()),
        (ROLE_LABEL.to_string(), name.to_string()),
        (GENERATION_LABEL.to_string(), "1".to_string()),
    ]);
    convoy
}

fn convoy_spec(workflow_ref: &str, role: &str) -> ConvoySpec {
    ConvoySpec {
        continuation: None,
        subjects: Vec::new(),
        role: role.to_string(),
        generation: 1,
        workflow_ref: workflow_ref.to_string(),
        dispatching_principal_ref: Default::default(),
        inputs: BTreeMap::new(),
        placement_policy: None,
        repositories: Vec::new(),
        r#ref: None,
        project_ref: "flotilla".to_string(),
        adopted_checkout_refs: BTreeMap::new(),
        issues: Vec::new(),
        change_request: None,
        instruction: None,
        promises: Vec::new(),
    }
}

#[tokio::test]
async fn tui_shows_convoys_from_daemon() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));

    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;

    // Subscribe before starting the runtime so we don't miss the first snapshot.
    let mut daemon_rx = daemon.subscribe();

    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: true,
        ..RuntimeOptions::default()
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

    // Admit a Convoy on the engine — the Aggregator should broadcast a panel snapshot.
    flotilla_daemon_testkit::engine::admit_convoy(
        &daemon,
        convoy("my-convoy", convoy_spec("my-workflow", "my-convoy"), flotilla_engine_convoys_testkit::single_agent("coder")),
    )
    .await;

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

#[tokio::test]
async fn generated_table_action_completes_work() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));

    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;

    let mut daemon_rx = daemon.subscribe();

    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        // This test admits the convoy itself so it can isolate the panel/TUI command path.
        // Controller integration is covered by the daemon runtime flow tests.
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");

    let daemon_handle: Arc<dyn DaemonHandle> = Arc::clone(&daemon) as Arc<dyn DaemonHandle>;
    let repos = daemon_handle.list_repos().await.expect("list repos");
    let tui_config = Arc::new(ConfigStore::with_base(tmp.path().join("tui-config")));
    let mut app = App::new(Arc::clone(&daemon_handle), repos, tui_config, Theme::classic());

    for event in daemon_handle.replay_since(&HashMap::new()).await.expect("replay_since") {
        app.handle_daemon_event(event);
    }

    // Admit a convoy whose frozen workflow has work ready for completion: on the engine a work
    // owes its agent seats' promises, so it has a coder's seat.
    let snapshot = WorkflowSnapshot {
        cascade: None,
        stall_nudges: Default::default(),
        supervision: None,
        exit: None,
        turn_delivery: Default::default(),
        vessels: vec![VesselRequirement {
            needs: Default::default(),
            name: "implement".into(),
            depends_on: vec![],
            repository_refs: None,
            credential_refs: Default::default(),
            credential_scopes: Default::default(),
            credential_permissions: Default::default(),
            crew: vec![flotilla_engine_convoys_testkit::agent("coder")],
        }],
    };
    let status = ConvoyStatus { workflow_snapshot: Some(snapshot), ..ConvoyStatus::default() };
    flotilla_daemon_testkit::engine::admit_convoy(&daemon, convoy("fix-bug-123", convoy_spec("review-and-fix", "fix-bug-123"), status))
        .await;

    // Drain events until the convoy's vessel appears in the App.
    let drain = |app: &mut App, daemon_rx: &mut tokio::sync::broadcast::Receiver<flotilla_protocol::DaemonEvent>| loop {
        match daemon_rx.try_recv() {
            Ok(event) => app.handle_daemon_event(event),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => panic!("lagged by {n}"),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => panic!("closed"),
        }
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain(&mut app, &mut daemon_rx);
        if app.convoys("flotilla").iter().any(|convoy| !convoy.vessels.is_empty()) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for convoy vessel to appear in app: {:?}", app.convoys("flotilla"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Switch the TUI into the Convoys tab and drive the keybinding flow.
    app.open_view(flotilla_protocol::ViewAddress::Convoys { namespace: "flotilla".to_string(), scope: None });

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }
    fn enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
    }

    app.handle_key(enter()); // drill from the convoy row to its vessels
    app.handle_key(key('.')); // open the selected vessel's generated actions
    app.handle_key(enter()); // dispatch ConvoyWorkForceComplete

    // Dispatch the queued command through the daemon.
    let mut took_one = false;
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    while let Some((cmd, pending_ctx)) = app.proto_commands.take_next() {
        flotilla_tui::app::executor::dispatch(cmd, &mut app, pending_ctx, event_tx.clone());
        took_one = true;
    }
    assert!(took_one, "expected at least one command to be dispatched after Enter");

    // Wait for completion to land back in the app via PanelDelta.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain(&mut app, &mut daemon_rx);
        if let Some(c) = app.convoys("flotilla").first() {
            if let Some(vessel) = c.vessels.iter().find(|vessel| vessel.name == "implement") {
                if vessel.phase == flotilla_tui::convoy_model::WorkPhase::Complete {
                    break;
                }
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out: work did not transition to Complete in app: {:?}", app.convoys("flotilla"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
