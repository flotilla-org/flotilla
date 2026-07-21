//! Integration tests for PlacementPolicyApply, in particular round-tripping the
//! `docker_per_vessel.agent_adapters` catalog through create and update.

use std::{path::PathBuf, sync::Arc, time::Duration};

use flotilla_core::{config::ConfigStore, daemon::DaemonHandle, in_process::InProcessDaemon, providers::discovery::test_support::fake_discovery};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName};
use flotilla_resources::{InMemoryBackend, PlacementPolicy, ResourceBackend};

fn test_config(dir: PathBuf) -> Arc<ConfigStore> {
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(dir.join("daemon.toml"), "machine_id = \"test-placement-policy-cli\"\n").expect("write daemon config");
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

fn docker_policy_yaml(agent_adapters: &str) -> String {
    format!(
        "pool: docker\ndocker_per_vessel:\n  host_ref: host-1\n  image: ghcr.io/flotilla/dev:latest\n  checkout:\n    fresh_clone_in_container:\n      clone_path: /workspace\n{agent_adapters}"
    )
}

#[tokio::test]
async fn placement_policy_apply_creates_then_updates_agent_adapter_catalog() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let create_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::PlacementPolicyApply {
                name: "docker-worktree".into(),
                spec_yaml: docker_policy_yaml("  agent_adapters:\n    - codex\n"),
            },
        })
        .await
        .expect("apply execute");
    assert_eq!(await_command_result(&mut rx, create_id).await, CommandValue::PlacementPolicyApplied {
        name: "docker-worktree".into()
    });

    let policies = backend.clone().using::<PlacementPolicy>("flotilla");
    let created = policies.get("docker-worktree").await.expect("policy should exist");
    let created_docker = created.spec.docker_per_vessel.as_ref().expect("docker_per_vessel");
    assert_eq!(created_docker.agent_adapters, vec!["codex".to_string()]);

    let update_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::PlacementPolicyApply {
                name: "docker-worktree".into(),
                spec_yaml: docker_policy_yaml("  agent_adapters:\n    - codex\n    - claude-code\n"),
            },
        })
        .await
        .expect("update execute");
    assert_eq!(await_command_result(&mut rx, update_id).await, CommandValue::PlacementPolicyApplied {
        name: "docker-worktree".into()
    });

    let updated = policies.get("docker-worktree").await.expect("policy should still exist");
    let updated_docker = updated.spec.docker_per_vessel.as_ref().expect("docker_per_vessel");
    assert_eq!(updated_docker.agent_adapters, vec!["codex".to_string(), "claude-code".to_string()]);
    assert_ne!(created.metadata.resource_version, updated.metadata.resource_version);
}

#[tokio::test]
async fn placement_policy_apply_defaults_to_no_adapters_when_omitted() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::PlacementPolicyApply { name: "docker-stock".into(), spec_yaml: docker_policy_yaml("") },
        })
        .await
        .expect("apply execute");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::PlacementPolicyApplied { name: "docker-stock".into() });

    let policy = backend.using::<PlacementPolicy>("flotilla").get("docker-stock").await.expect("policy should exist");
    assert_eq!(policy.spec.docker_per_vessel.expect("docker_per_vessel").agent_adapters, Vec::<String>::new());
}

#[tokio::test]
async fn placement_policy_apply_rejects_blank_and_duplicate_adapters() {
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    for agent_adapters in ["  agent_adapters:\n    - ''\n", "  agent_adapters:\n    - codex\n    - codex\n"] {
        let id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::PlacementPolicyApply {
                    name: "docker-invalid".into(),
                    spec_yaml: docker_policy_yaml(agent_adapters),
                },
            })
            .await
            .expect("apply execute");
        assert!(matches!(
            await_command_result(&mut rx, id).await,
            CommandValue::Error { message } if message.contains("placement policy validation failed")
        ));
    }
}

#[tokio::test]
async fn placement_policy_apply_rejects_malformed_yaml() {
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::PlacementPolicyApply {
                name: "docker-broken".into(),
                spec_yaml: "this is: not {valid yaml structure for: a placement policy".into(),
            },
        })
        .await
        .expect("apply execute");
    assert!(matches!(
        await_command_result(&mut rx, id).await,
        CommandValue::Error { message } if message.contains("invalid placement policy YAML")
    ));
}
