//! Integration tests for ProjectAdd/ProjectApply and Project-backed convoy metadata.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    providers::discovery::test_support::fake_discovery,
    repository_inspection::{LocalCheckoutInspection, RepositoryInspection, RepositoryInspector},
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{Command, CommandAction, CommandValue, DaemonEvent, HostName};
use flotilla_resources::{
    Checkout, Convoy, InMemoryBackend, InputMeta, IssueSource, Project, Repository, RepositoryKey, RepositorySpec, RepositoryStatus,
    ResourceBackend,
};

fn test_config(dir: PathBuf) -> Arc<ConfigStore> {
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

#[derive(Clone)]
struct FixedInspector {
    spec: RepositorySpec,
    host_ref: String,
}

#[async_trait]
impl RepositoryInspector for FixedInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: self.spec.clone(),
            checkout: LocalCheckoutInspection {
                path: path.to_path_buf(),
                host_ref: self.host_ref.clone(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: None,
        })
    }
}

#[derive(Clone)]
struct FailingInspector;

#[async_trait]
impl RepositoryInspector for FailingInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Err(format!("cannot inspect {}", path.display()))
    }
}

async fn track_repository(daemon: &Arc<InProcessDaemon>, tmp: &tempfile::TempDir, directory_name: &str, remote: &str) -> RepositoryKey {
    let repository_spec = RepositorySpec::remote(remote).expect("repository spec");
    let repository_key = repository_spec.key();
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: repository_spec, host_ref: "host-01".to_string() })).await;
    let checkout_path = tmp.path().join(directory_name);
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    daemon.add_repo(&checkout_path).await.expect("track repo");
    repository_key
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

async fn execute_project_add(
    daemon: &Arc<InProcessDaemon>,
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    target: String,
    name: Option<&str>,
    display_name: Option<&str>,
) -> CommandValue {
    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectAdd {
                target,
                name: name.map(str::to_string),
                display_name: display_name.map(str::to_string),
                remote: None,
            },
        })
        .await
        .expect("execute");
    await_command_result(rx, id).await
}

#[tokio::test]
async fn tracking_repo_materializes_whole_repo_project() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let repository_key = track_repository(&daemon, &tmp, "tracked", "https://github.com/org/tracked.git").await;

    let project = backend.using::<Project>("flotilla").get("tracked").await.expect("whole-repository project should exist");
    assert_eq!(project.spec.display_name, "tracked");
    assert_eq!(project.spec.default_workflow_ref, "single-agent-contained");
    assert_eq!(project.spec.repositories.as_slice(), [flotilla_resources::ProjectRepositorySpec {
        repo: repository_key,
        subpath: None,
        default_branch: None,
    }]);
}

#[tokio::test]
async fn tracking_repo_fails_when_its_project_cannot_be_materialized() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    daemon.set_repository_inspector(Arc::new(FailingInspector)).await;
    let checkout_path = tmp.path().join("uninspectable");
    std::fs::create_dir(&checkout_path).expect("checkout dir");

    let error = daemon.add_repo(&checkout_path).await.expect_err("tracking should fail");

    assert!(error.contains("cannot inspect"));
    assert!(!daemon.tracked_repo_paths().await.contains(&checkout_path));
    assert!(backend.using::<Project>("flotilla").list().await.expect("project list").items.is_empty());
}

#[tokio::test]
async fn daemon_start_backfills_project_idempotently_and_preserves_edits() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let checkout_path = tmp.path().join("backfilled");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![checkout_path],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let repository_spec = RepositorySpec::remote("https://github.com/org/backfilled.git").expect("repository spec");
    let repository_key = repository_spec.key();
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: repository_spec, host_ref: "host-01".to_string() })).await;
    let options = RuntimeOptions {
        namespace: "flotilla".to_string(),
        heartbeat_interval: Duration::from_secs(300),
        controller_resync_interval: Duration::from_secs(300),
        start_controllers: false,
        ..RuntimeOptions::default()
    };

    let runtime =
        DaemonRuntime::start_with_options(Arc::clone(&daemon), Arc::clone(&config), None, options.clone()).await.expect("runtime start");

    let projects = backend.clone().using::<Project>("flotilla");
    let project = projects.get("backfilled").await.expect("backfilled project should exist");
    assert_eq!(project.spec.repositories[0].repo, repository_key);
    let mut evolved = project.spec;
    evolved.display_name = "Backfilled product".to_string();
    evolved.default_workflow_ref = "custom-workflow".to_string();
    evolved.issue_source = Some(IssueSource { service: "linear".to_string(), scope: "BACK".to_string() });
    evolved.repositories.push(flotilla_resources::ProjectRepositorySpec {
        repo: RepositoryKey("second-repository".to_string()),
        subpath: None,
        default_branch: None,
    });
    projects
        .update(&InputMeta::builder().name("backfilled".to_string()).build(), &project.metadata.resource_version, &evolved)
        .await
        .expect("evolve project");
    drop(runtime);

    let _restarted = DaemonRuntime::start_with_options(daemon, config, None, options).await.expect("runtime restart");

    assert_eq!(projects.get("backfilled").await.expect("evolved project").spec, evolved);
    assert_eq!(projects.list().await.expect("project list").items.len(), 1);
}

#[tokio::test]
async fn daemon_start_skips_a_tracked_repo_that_cannot_be_backfilled() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let checkout_path = tmp.path().join("uninspectable");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![checkout_path],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    daemon.set_repository_inspector(Arc::new(FailingInspector)).await;

    let _runtime =
        DaemonRuntime::start_with_options(daemon, config, None, RuntimeOptions { start_controllers: false, ..RuntimeOptions::default() })
            .await
            .expect("runtime should skip the uninspectable repository");

    assert!(backend.using::<Project>("flotilla").list().await.expect("project list").items.is_empty());
}

#[tokio::test]
async fn tracking_repo_widens_project_name_without_overwriting_custom_project() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let projects = backend.clone().using::<Project>("flotilla");
    let custom_spec = flotilla_resources::ProjectSpec {
        display_name: "Shared product".to_string(),
        default_workflow_ref: "custom-workflow".to_string(),
        issue_source: None,
        repositories: vec![flotilla_resources::ProjectRepositorySpec {
            repo: RepositoryKey("other-repository".to_string()),
            subpath: None,
            default_branch: None,
        }],
    };
    projects.create(&InputMeta::builder().name("shared".to_string()).build(), &custom_spec).await.expect("custom project create");
    let repository_key = track_repository(&daemon, &tmp, "shared", "https://github.com/org-b/shared.git").await;

    assert_eq!(projects.get("shared").await.expect("custom project").spec, custom_spec);
    let generated = projects.get("github-com-org-b-shared").await.expect("collision-aware project should exist");
    assert_eq!(generated.spec.repositories[0].repo, repository_key);
}

#[tokio::test]
async fn tracking_repo_uses_repository_key_when_slug_candidates_collide() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let projects = backend.clone().using::<Project>("flotilla");
    for (name, repo_ref) in [("shared", "first-repository"), ("github-com-org-b-shared", "second-repository")] {
        projects
            .create(&InputMeta::builder().name(name.to_string()).build(), &flotilla_resources::ProjectSpec {
                display_name: name.to_string(),
                default_workflow_ref: "custom-workflow".to_string(),
                issue_source: None,
                repositories: vec![flotilla_resources::ProjectRepositorySpec {
                    repo: RepositoryKey(repo_ref.to_string()),
                    subpath: None,
                    default_branch: None,
                }],
            })
            .await
            .expect("occupied project create");
    }
    let repository_key = track_repository(&daemon, &tmp, "shared", "https://github.com/org-b/shared.git").await;

    let key_prefix = repository_key.0.chars().take(8).collect::<String>();
    let generated_name = format!("github-com-org-b-shared-{key_prefix}");
    let generated = projects.get(&generated_name).await.expect("key-disambiguated project should exist");
    assert_eq!(generated.spec.repositories[0].repo, repository_key);
}

#[tokio::test]
async fn project_add_untracked_path_ensures_repository_checkout_and_whole_repo_project() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let repository_spec = RepositorySpec::remote("https://github.com/org/repo.git").expect("repository spec");
    let repository_key = repository_spec.key();
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: repository_spec.clone(), host_ref: "host-01".to_string() })).await;
    let checkout_path = tmp.path().join("repo");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let mut rx = daemon.subscribe();

    let result =
        execute_project_add(&daemon, &mut rx, checkout_path.to_string_lossy().into_owned(), Some("my-project"), Some("My Project")).await;

    assert_eq!(result, CommandValue::ProjectAdded { name: "my-project".into() });
    let repository =
        backend.clone().using::<Repository>("flotilla").get(&repository_key.to_string()).await.expect("repository should exist");
    assert_eq!(repository.spec, repository_spec);
    repository.spec.verify_key(&repository_key).expect("repository key should verify");
    let checkouts = daemon.observed_resource_backend().using::<Checkout>("flotilla").list().await.expect("checkout list");
    assert_eq!(checkouts.items.len(), 1);
    let project = backend.using::<Project>("flotilla").get("my-project").await.expect("project should exist");
    assert_eq!(project.spec.display_name, "My Project");
    assert_eq!(project.spec.default_workflow_ref, "single-agent-contained");
    assert_eq!(project.spec.repositories.as_slice(), [flotilla_resources::ProjectRepositorySpec {
        repo: repository_key,
        subpath: None,
        default_branch: None,
    }]);
}

#[tokio::test]
async fn project_add_catalog_slug_needs_no_local_checkout() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let spec = RepositorySpec::remote("https://github.com/org/catalog-only.git").expect("repository spec");
    let key = spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("repository create");
    let mut rx = daemon.subscribe();

    let result = execute_project_add(&daemon, &mut rx, "catalog-only".to_string(), None, None).await;

    assert_eq!(result, CommandValue::ProjectAdded { name: "catalog-only".into() });
    assert!(daemon.observed_resource_backend().using::<Checkout>("flotilla").list().await.expect("checkout list").items.is_empty());
    let project = backend.using::<Project>("flotilla").get("catalog-only").await.expect("project should exist");
    assert_eq!(project.spec.repositories[0].repo, key);
}

#[tokio::test]
async fn concurrent_project_adds_of_one_identity_converge_on_one_verified_repository() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let spec = RepositorySpec::remote("https://github.com/org/shared.git").expect("repository spec");
    let key = spec.key();
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: spec.clone(), host_ref: "host-01".to_string() })).await;
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    std::fs::create_dir(&first).expect("first checkout");
    std::fs::create_dir(&second).expect("second checkout");
    let mut first_rx = daemon.subscribe();
    let mut second_rx = daemon.subscribe();
    let command = |target: &Path, name: &str| Command {
        node_id: None,
        provisioning_target: None,
        context_repo: None,
        action: CommandAction::ProjectAdd {
            target: target.to_string_lossy().into_owned(),
            name: Some(name.to_string()),
            display_name: None,
            remote: None,
        },
    };

    let (first_id, second_id) = tokio::join!(daemon.execute(command(&first, "first")), daemon.execute(command(&second, "second")));
    let first_id = first_id.expect("first execute");
    let second_id = second_id.expect("second execute");

    assert_eq!(await_command_result(&mut first_rx, first_id).await, CommandValue::ProjectAdded { name: "first".into() });
    assert_eq!(await_command_result(&mut second_rx, second_id).await, CommandValue::ProjectAdded { name: "second".into() });
    let repositories = backend.using::<Repository>("flotilla").list().await.expect("repository list");
    assert_eq!(repositories.items.len(), 1);
    repositories.items[0].spec.verify_key(&key).expect("repository identity should verify");
}

#[tokio::test]
async fn repeated_project_add_preserves_evolved_definition() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let spec = RepositorySpec::remote("https://github.com/org/repo.git").expect("repository spec");
    let key = spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
        .await
        .expect("repository create");
    let mut rx = daemon.subscribe();
    assert_eq!(execute_project_add(&daemon, &mut rx, "repo".to_string(), Some("core"), None).await, CommandValue::ProjectAdded {
        name: "core".into()
    });
    let projects = backend.clone().using::<Project>("flotilla");
    let original = projects.get("core").await.expect("project");
    let mut evolved = original.spec.clone();
    evolved.display_name = "Evolved".to_string();
    evolved.default_workflow_ref = "governor-refined".to_string();
    evolved.issue_source = Some(IssueSource { service: "linear".to_string(), scope: "FLOT".to_string() });
    projects
        .update(&InputMeta::builder().name("core".to_string()).build(), &original.metadata.resource_version, &evolved)
        .await
        .expect("evolve project");

    assert_eq!(execute_project_add(&daemon, &mut rx, "repo".to_string(), Some("core"), None).await, CommandValue::ProjectAdded {
        name: "core".into()
    });
    assert_eq!(projects.get("core").await.expect("project").spec, evolved);
    assert!(matches!(
        execute_project_add(&daemon, &mut rx, "repo".to_string(), Some("core"), Some("Contradiction")).await,
        CommandValue::Error { message } if message.contains("project apply")
    ));
}

#[tokio::test]
async fn project_apply_normalizes_typed_multi_repo_definition() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let yaml = r#"
display_name: Cross-Project Demo
default_workflow_ref: single-agent-contained
repositories:
  - repo: b
    subpath: ./services/api
  - repo: a
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
    let project = backend.using::<Project>("flotilla").get("cross").await.expect("project should exist");
    assert_eq!(project.spec.repositories[0].repo, RepositoryKey("a".to_string()));
    assert_eq!(project.spec.repositories[1].subpath.as_deref(), Some("services/api"));
}

#[tokio::test]
async fn convoy_create_carries_project_ref() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let repository_spec = RepositorySpec::remote("https://github.com/org/linked-repo.git").expect("repository spec");
    let repository_key = repository_spec.key();
    let repository = backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(repository_key.to_string()).build(), &repository_spec)
        .await
        .expect("repository create");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .update_status(&repository.metadata.name, &repository.metadata.resource_version, &RepositoryStatus {
            default_branch: Some("main".to_string()),
            ..Default::default()
        })
        .await
        .expect("repository status update");
    assert_eq!(
        execute_project_add(&daemon, &mut rx, "linked-repo".to_string(), Some("my-project"), None).await,
        CommandValue::ProjectAdded { name: "my-project".into() }
    );
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
                placement_policy: None,
                adopted_checkout: None,
            },
        })
        .await
        .expect("execute");
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ConvoyCreated { name: "linked".into() });
    let convoy = backend.using::<Convoy>("flotilla").get("linked").await.expect("convoy");
    assert_eq!(convoy.spec.project_ref.as_deref(), Some("my-project"));
    assert_eq!(convoy.spec.repositories.len(), 1);
    assert_eq!(convoy.spec.repositories[0].base_ref, "main");
}

#[tokio::test]
async fn unresolved_replicated_project_refs_store_but_block_convoy_admission() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let apply_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply {
                name: "waiting".into(),
                spec_yaml: "display_name: Waiting\ndefault_workflow_ref: single-agent-contained\nrepositories:\n  - repo: missing\n".into(),
            },
        })
        .await
        .expect("apply execute");
    assert_eq!(await_command_result(&mut rx, apply_id).await, CommandValue::ProjectApplied { name: "waiting".into() });
    assert!(backend.using::<Project>("flotilla").get("waiting").await.is_ok(), "definition should persist before its referent");

    let convoy_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "blocked".into(),
                workflow_ref: "scratch".into(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: Some("waiting".into()),
                placement_policy: None,
                adopted_checkout: None,
            },
        })
        .await
        .expect("convoy execute");
    assert!(matches!(
        await_command_result(&mut rx, convoy_id).await,
        CommandValue::Error { message } if message.contains("project waiting is not ready") && message.contains("missing")
    ));
}

#[tokio::test]
async fn project_apply_rejects_invalid_or_incomplete_definitions() {
    let (daemon, _backend, _config, _runtime, _tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    for spec_yaml in [
        "this is: not {valid yaml structure for: a project",
        "display_name: Missing workflow\nrepositories:\n  - repo: a\n",
        "display_name: Empty repos\ndefault_workflow_ref: wf\nrepositories: []\n",
    ] {
        let id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ProjectApply { name: "broken".into(), spec_yaml: spec_yaml.into() },
            })
            .await
            .expect("execute");
        assert!(matches!(await_command_result(&mut rx, id).await, CommandValue::Error { .. }));
    }
}
