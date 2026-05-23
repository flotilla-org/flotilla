//! Integration tests for the `ProjectCreate` + `ProjectApply` daemon actions
//! and the `project_ref` field on `ConvoyCreate`.

use std::{sync::Arc, time::Duration};

use flotilla_core::{
    config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery,
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName};
use flotilla_resources::{Convoy, InMemoryBackend, Project, ResourceBackend};

fn test_config(dir: std::path::PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"test-project-cli\"\n").expect("write daemon config");
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
async fn project_create_command_creates_project_resource() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectCreate {
                name: "my-project".into(),
                display_name: Some("My Project".into()),
                repository_url: Some("https://github.com/org/repo.git".into()),
                subpath: Some("apps/frontend".into()),
                r#ref: Some("main".into()),
            },
        })
        .await
        .expect("execute");

    let result = await_command_result(&mut rx, id).await;
    assert_eq!(result, CommandValue::ProjectCreated { name: "my-project".into() });

    let projects = backend.using::<Project>("flotilla");
    let project = projects.get("my-project").await.expect("project should exist");
    assert_eq!(project.spec.display_name.as_deref(), Some("My Project"));
    assert_eq!(project.spec.repositories.len(), 1);
    assert_eq!(project.spec.repositories[0].repo, "https://github.com/org/repo.git");
    assert_eq!(project.spec.repositories[0].subpath.as_deref(), Some("apps/frontend"));
    assert_eq!(project.spec.repositories[0].default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn project_apply_supports_multi_repo() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let yaml = r#"
display_name: "Cross-Project Demo"
repositories:
  - repo: https://github.com/org/repo-a.git
    default_branch: main
  - repo: https://github.com/org/repo-b.git
    subpath: services/api
    default_branch: develop
"#;

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply { name: "cross".into(), spec_yaml: yaml.into() },
        })
        .await
        .expect("execute");

    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ProjectApplied { name: "cross".into() });

    let projects = backend.using::<Project>("flotilla");
    let project = projects.get("cross").await.expect("project should exist");
    assert_eq!(project.spec.repositories.len(), 2);
    assert_eq!(project.spec.repositories[0].repo, "https://github.com/org/repo-a.git");
    assert_eq!(project.spec.repositories[1].subpath.as_deref(), Some("services/api"));
}

#[tokio::test]
async fn project_apply_updates_existing() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    // First apply.
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply { name: "iter".into(), spec_yaml: "display_name: V1\nrepositories: []\n".into() },
        })
        .await
        .expect("first apply");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ProjectApplied { name: "iter".into() });

    // Second apply same name, different content.
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply { name: "iter".into(), spec_yaml: "display_name: V2\nrepositories: []\n".into() },
        })
        .await
        .expect("second apply");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ProjectApplied { name: "iter".into() });

    let projects = backend.using::<Project>("flotilla");
    let project = projects.get("iter").await.expect("project should exist");
    assert_eq!(project.spec.display_name.as_deref(), Some("V2"));
}

#[tokio::test]
async fn convoy_create_carries_project_ref() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "linked".into(),
                workflow_ref: "scratch".into(),
                inputs: vec![],
                repository_url: None,
                r#ref: None,
                project_ref: Some("my-project".into()),
            },
        })
        .await
        .expect("execute");

    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ConvoyCreated { name: "linked".into() });

    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("linked").await.expect("convoy should exist");
    assert_eq!(convoy.spec.project_ref.as_deref(), Some("my-project"));
}

#[tokio::test]
async fn project_apply_rejects_invalid_yaml() {
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply {
                name: "broken".into(),
                spec_yaml: "this is: not {valid yaml structure for: a project".into(),
            },
        })
        .await
        .expect("execute");

    let result = await_command_result(&mut rx, id).await;
    assert!(matches!(result, CommandValue::Error { .. }), "expected Error, got {result:?}");
}

#[tokio::test]
async fn project_apply_rejects_wrong_shape_yaml() {
    // Valid YAML, but a `repositories` entry is missing the required `repo` field.
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply {
                name: "shapeless".into(),
                spec_yaml: "repositories:\n  - subpath: apps/x\n    default_branch: main\n".into(),
            },
        })
        .await
        .expect("execute");

    let result = await_command_result(&mut rx, id).await;
    assert!(matches!(result, CommandValue::Error { .. }), "expected Error, got {result:?}");
}
