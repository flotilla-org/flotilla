//! Integration tests for the `ConvoyCreate` + `WorkflowTemplateApply` daemon actions
//! and the seeded `scratch` WorkflowTemplate registered at startup.

use std::{sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName};
use flotilla_resources::{Convoy, ConvoyPhase, InMemoryBackend, ResourceBackend, SqliteBackend, WorkflowTemplate};

fn test_config(dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"test-convoy-create-cli\"\n").expect("write daemon config");
    Arc::new(ConfigStore::with_base(dir))
}

async fn start_daemon() -> (Arc<InProcessDaemon>, ResourceBackend, Arc<ConfigStore>, DaemonRuntime, tempfile::TempDir) {
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
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: false,
        ..RuntimeOptions::default()
    };
    let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");
    (daemon, backend, config, runtime, tmp)
}

async fn start_sqlite_daemon(
    start_controllers: bool,
) -> (Arc<InProcessDaemon>, ResourceBackend, Arc<ConfigStore>, DaemonRuntime, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let backend = ResourceBackend::Sqlite(SqliteBackend::open(config.state_dir().as_path().join("resources.sqlite")).expect("sqlite open"));
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_millis(25),
        start_controllers,
        ..RuntimeOptions::default()
    };
    let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options).await.expect("runtime start");
    (daemon, backend, config, runtime, tmp)
}

async fn await_command_result(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv()).await.expect("timed out").expect("recv");
        if let DaemonEvent::CommandFinished { command_id: id, result, .. } = event {
            if id == command_id {
                return result;
            }
        }
    }
}

#[tokio::test]
async fn scratch_workflow_template_is_seeded_at_startup() {
    let (_daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let templates = backend.using::<WorkflowTemplate>("flotilla");
    let scratch = templates.get("scratch").await.expect("scratch template should be seeded");
    assert_eq!(scratch.metadata.name, "scratch");
    assert_eq!(scratch.spec.tasks.len(), 1);
    assert_eq!(scratch.spec.tasks[0].name, "work");
}

#[tokio::test]
async fn convoy_create_command_creates_convoy_resource() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;

    let mut rx = daemon.subscribe();
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "my-scratch".into(),
                workflow_ref: "scratch".into(),
                inputs: vec![("topic".into(), "first-convoy".into())],
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
            },
        })
        .await
        .expect("execute");

    let result = await_command_result(&mut rx, id).await;
    assert_eq!(result, CommandValue::ConvoyCreated { name: "my-scratch".into() });

    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("my-scratch").await.expect("convoy should exist");
    assert_eq!(convoy.spec.workflow_ref, "scratch");
    assert_eq!(convoy.spec.inputs.len(), 1);
    assert!(convoy.spec.repository.is_none());
    assert!(
        convoy.spec.placement_policy.as_deref().is_some_and(|policy| policy.starts_with("host-direct-")),
        "convoy create should default to the seeded host-direct placement policy: {convoy:?}"
    );
}

#[tokio::test]
async fn workflow_template_apply_creates_then_updates() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let templates = backend.using::<WorkflowTemplate>("flotilla");

    let mut rx = daemon.subscribe();

    // First apply: creates a new template.
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::WorkflowTemplateApply {
                name: "custom".into(),
                spec_yaml: "tasks:\n  - name: only\n    processes:\n      - role: shell\n        command: 'echo first'\n".into(),
            },
        })
        .await
        .expect("execute create");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::WorkflowTemplateApplied { name: "custom".into() });
    let v1 = templates.get("custom").await.expect("template should exist");
    let v1_command = match &v1.spec.tasks[0].processes[0].source {
        flotilla_resources::ProcessSource::Tool { command } => command.clone(),
        _ => panic!("expected tool process"),
    };
    assert_eq!(v1_command, "echo first");

    // Second apply with the same name: updates the existing template.
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::WorkflowTemplateApply {
                name: "custom".into(),
                spec_yaml: "tasks:\n  - name: only\n    processes:\n      - role: shell\n        command: 'echo updated'\n".into(),
            },
        })
        .await
        .expect("execute update");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::WorkflowTemplateApplied { name: "custom".into() });
    let v2 = templates.get("custom").await.expect("template should still exist");
    let v2_command = match &v2.spec.tasks[0].processes[0].source {
        flotilla_resources::ProcessSource::Tool { command } => command.clone(),
        _ => panic!("expected tool process"),
    };
    assert_eq!(v2_command, "echo updated");
}

#[tokio::test]
async fn workflow_template_apply_rejects_invalid_yaml() {
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::WorkflowTemplateApply {
                name: "broken".into(),
                spec_yaml: "this is not: {valid yaml structure for: a workflow".into(),
            },
        })
        .await
        .expect("execute");
    let result = await_command_result(&mut rx, id).await;
    assert!(matches!(result, CommandValue::Error { .. }), "expected Error, got {result:?}");
}

#[tokio::test]
async fn sqlite_backed_runtime_reconciles_convoy_create_into_namespace_view() {
    let (daemon, backend, _config, _runtime, _tmp) = start_sqlite_daemon(true).await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "sqlite-scratch".into(),
                workflow_ref: "scratch".into(),
                inputs: vec![("topic".into(), "embedded".into())],
                repository_url: None,
                r#ref: None,
                project_ref: None,
                placement_policy: None,
            },
        })
        .await
        .expect("execute");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ConvoyCreated { name: "sqlite-scratch".into() });

    let convoys = backend.using::<Convoy>("flotilla");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let convoy = convoys.get("sqlite-scratch").await.expect("convoy should exist");
        if matches!(
            convoy.status.as_ref(),
            Some(status) if status.phase == ConvoyPhase::Active && status.observed_workflow_ref.as_deref() == Some("scratch")
        ) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for sqlite-backed convoy reconcile: {convoy:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, rx.recv()).await.expect("timed out waiting for namespace event").expect("recv");
        match event {
            DaemonEvent::NamespaceSnapshot(snapshot)
                if snapshot.namespace == "flotilla"
                    && snapshot.convoys.iter().any(|convoy| convoy.name == "sqlite-scratch" && !convoy.initializing) =>
            {
                break;
            }
            DaemonEvent::NamespaceDelta(delta)
                if delta.namespace == "flotilla"
                    && delta.changed.iter().any(|convoy| convoy.name == "sqlite-scratch" && !convoy.initializing) =>
            {
                break;
            }
            _ => {}
        }
    }
}
