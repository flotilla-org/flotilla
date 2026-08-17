//! Integration tests for ProjectAdd/ProjectApply and Project-backed convoy metadata.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    daemon::DaemonHandle,
    in_process::InProcessDaemon,
    ops_entry::{
        MATERIALIZED_PROJECT_ANNOTATION, PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION,
        SOURCE_REPOSITORY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION,
    },
    project_declaration::{BOOTSTRAP_COMMIT_ANNOTATION, BOOTSTRAP_PATH_ANNOTATION, BOOTSTRAP_REPOSITORY_ANNOTATION},
    providers::discovery::test_support::{fake_discovery, git_process_discovery, init_git_repo_with_remote},
    repository_inspection::{LocalCheckoutInspection, ProjectDeclarationInspection, RepositoryInspection, RepositoryInspector},
};
use flotilla_daemon::runtime::{DaemonRuntime, RuntimeOptions};
use flotilla_protocol::{commands::RepositoryIdentityChange, Command, CommandAction, CommandValue, DaemonEvent, HostName, RepoSelector};
use flotilla_resources::{
    Checkout, CheckoutSpec, Convoy, ConvoyEnsure, InMemoryBackend, InputMeta, IssueSource, ObservedCheckoutSpec, Project,
    ProjectRepositoryRole, ProjectSpec, Repository, RepositoryKey, RepositorySpec, RepositoryStatus, ResourceBackend, Stance,
    WorkflowTemplate, WorkflowTemplateSpec, MANAGED_BY_LABEL,
};
use tracing::instrument::WithSubscriber;
#[derive(Clone)]
struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log capture lock should be healthy").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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

#[derive(Clone)]
struct MutableInspector {
    spec: Arc<RwLock<RepositorySpec>>,
}

#[derive(Clone)]
struct DeclarationInspector {
    bootstrap: RepositorySpec,
    commit: Arc<RwLock<String>>,
}

#[async_trait]
impl RepositoryInspector for DeclarationInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: self.bootstrap.clone(),
            checkout: LocalCheckoutInspection {
                path: path.to_path_buf(),
                host_ref: "host-01".to_string(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: None,
        })
    }

    async fn inspect_project_declaration(&self, path: &Path) -> Result<ProjectDeclarationInspection, String> {
        let repository = self.inspect_path(path, None).await?;
        let yaml = std::fs::read_to_string(path.join("project.yaml")).map_err(|error| error.to_string())?;
        let commit = self.commit.read().expect("commit lock should not be poisoned").clone();
        Ok(ProjectDeclarationInspection { repository, yaml, commit })
    }
}

#[async_trait]
impl RepositoryInspector for MutableInspector {
    async fn inspect_path(&self, path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: self.spec.read().expect("repository identity lock should not be poisoned").clone(),
            checkout: LocalCheckoutInspection {
                path: path.to_path_buf(),
                host_ref: "host-01".to_string(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: None,
        })
    }
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

#[derive(Clone)]
struct WorktreeInspector {
    spec: RepositorySpec,
    checkouts: Arc<RwLock<Vec<LocalCheckoutInspection>>>,
}

#[async_trait]
impl RepositoryInspector for WorktreeInspector {
    async fn inspect_path(&self, _path: &Path, _remote: Option<&str>) -> Result<RepositoryInspection, String> {
        Ok(RepositoryInspection {
            spec: self.spec.clone(),
            checkout: self.checkouts.read().expect("checkout inspection lock should not be poisoned")[0].clone(),
            transport_url: None,
        })
    }

    async fn inspect_checkouts(&self, _inspection: &RepositoryInspection) -> Result<Vec<LocalCheckoutInspection>, String> {
        Ok(self.checkouts.read().expect("checkout inspection lock should not be poisoned").clone())
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

async fn execute_project_command(
    daemon: &Arc<InProcessDaemon>,
    rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    action: CommandAction,
) -> CommandValue {
    let id = daemon.execute(Command { node_id: None, provisioning_target: None, context_repo: None, action }).await.expect("execute");
    await_command_result(rx, id).await
}

#[tokio::test]
async fn project_declarations_register_single_and_multi_member_projects_with_provenance() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let bootstrap = RepositorySpec::remote("https://github.com/example/bootstrap").expect("bootstrap spec");
    let commit = Arc::new(RwLock::new("0123456789abcdef".to_string()));
    daemon.set_repository_inspector(Arc::new(DeclarationInspector { bootstrap: bootstrap.clone(), commit })).await;
    let mut rx = daemon.subscribe();

    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: flotilla\nmembers:\n  - alias: flotilla\n    url: https://github.com/flotilla-org/flotilla\n    roles: [code, ops, knowledge]\n",
    )
    .expect("write declaration");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() },)
            .await,
        CommandValue::ProjectRegistered { name: "flotilla".to_string(), members: 1 }
    );
    let flotilla = backend.using::<Project>("flotilla").get("flotilla").await.expect("flotilla project");
    assert_eq!(flotilla.spec.repositories[0].alias.as_deref(), Some("flotilla"));
    assert_eq!(
        flotilla.spec.repositories[0].roles,
        [ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops, ProjectRepositoryRole::Knowledge,].into_iter().collect()
    );
    assert_eq!(flotilla.metadata.annotations.get(BOOTSTRAP_REPOSITORY_ANNOTATION), Some(&bootstrap.key().to_string()));
    assert_eq!(flotilla.metadata.annotations.get(BOOTSTRAP_COMMIT_ANNOTATION).map(String::as_str), Some("0123456789abcdef"));

    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: split\nmembers:\n  - alias: app\n    url: https://github.com/example/app\n    roles: [code]\n  - alias: operations\n    url: https://github.com/example/ops\n    roles: [ops]\n",
    )
    .expect("write declaration");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() },)
            .await,
        CommandValue::ProjectRegistered { name: "split".to_string(), members: 2 }
    );
    let split = backend.using::<Project>("flotilla").get("split").await.expect("split project");
    assert_eq!(split.spec.repositories.iter().map(|member| member.alias.as_deref()).collect::<Vec<_>>(), vec![
        Some("app"),
        Some("operations")
    ]);
    for member in &split.spec.repositories {
        let repository = backend.using::<Repository>("flotilla").get(&member.repo.to_string()).await.expect("member repository");
        assert_eq!(repository.metadata.annotations.get(BOOTSTRAP_COMMIT_ANNOTATION).map(String::as_str), Some("0123456789abcdef"));
        assert!(!repository.metadata.annotations.contains_key(BOOTSTRAP_PATH_ANNOTATION));
    }
}

#[tokio::test]
async fn declaration_adoption_survives_whole_repository_project_reconciliation() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let checkout = tmp.path().join("flotilla");
    std::fs::create_dir(&checkout).expect("checkout dir");
    std::fs::write(
        checkout.join("project.yaml"),
        "name: flotilla\nmembers:\n  - alias: flotilla\n    url: https://github.com/flotilla-org/flotilla\n    roles: [code, ops, knowledge]\n",
    )
    .expect("write declaration");
    let commit = Arc::new(RwLock::new("declaration-commit".to_string()));
    daemon
        .set_repository_inspector(Arc::new(DeclarationInspector {
            bootstrap: RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("bootstrap spec"),
            commit,
        }))
        .await;
    daemon.add_repo(&checkout).await.expect("track bootstrap member");
    let mut rx = daemon.subscribe();
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: checkout.to_string_lossy().into_owned() },)
            .await,
        CommandValue::ProjectRegistered { name: "flotilla".to_string(), members: 1 }
    );
    let projects = backend.definitions::<Project>("flotilla");
    let registered = projects.get("flotilla").await.expect("registered project");
    let registered_repo = &registered.spec.repositories[0].repo;

    for result in [
        execute_project_add(&daemon, &mut rx, checkout.to_string_lossy().into_owned(), Some("flotilla"), None).await,
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectApply {
            name: "flotilla".to_string(),
            spec_yaml: format!(
                "display_name: overwritten\ndefault_workflow_ref: single-agent-contained\nrepositories:\n  - repo: {registered_repo}\n"
            ),
        })
        .await,
    ] {
        assert!(
            matches!(&result, CommandValue::Error { message } if message.contains("managed by a declaration") && message.contains("project refresh")),
            "unexpected command result: {result:?}"
        );
    }

    daemon.materialize_tracked_repo_projects().await.expect("whole-repository reconciliation");

    let reconciled = projects.get("flotilla").await.expect("reconciled project");
    assert_eq!(reconciled.metadata.resource_version, registered.metadata.resource_version);
    assert_eq!(reconciled.spec.repositories[0].alias.as_deref(), Some("flotilla"));
    assert_eq!(reconciled.spec.repositories[0].roles.len(), 3);
}

#[tokio::test]
async fn project_refresh_is_one_way_and_keeps_alias_repository_keys_stable_across_rename() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let commit = Arc::new(RwLock::new("commit-one".to_string()));
    daemon
        .set_repository_inspector(Arc::new(DeclarationInspector {
            bootstrap: RepositorySpec::remote("https://github.com/example/bootstrap").expect("bootstrap spec"),
            commit: Arc::clone(&commit),
        }))
        .await;
    let declaration_path = tmp.path().join("project.yaml");
    std::fs::write(
        &declaration_path,
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/app-old\n    roles: [code]\n  - alias: ops\n    url: https://github.com/example/ops\n    roles: [ops]\n",
    )
    .expect("write declaration");
    let mut rx = daemon.subscribe();
    execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() }).await;
    let projects = backend.definitions::<Project>("flotilla");
    let original = projects.get("demo").await.expect("project");
    let app_key = original.spec.repositories.iter().find(|member| member.alias.as_deref() == Some("app")).expect("app member").repo.clone();
    let mut drifted = original.spec.clone();
    drifted.display_name = "hand edited".to_string();
    projects.apply(&InputMeta::from(&original.metadata), &drifted).await.expect("introduce drift");

    *commit.write().expect("commit lock should not be poisoned") = "commit-two".to_string();
    std::fs::write(
        &declaration_path,
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/app-renamed\n    roles: [code, knowledge]\n  - alias: ops\n    url: https://github.com/example/ops\n    roles: [ops]\n",
    )
    .expect("update declaration");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await,
        CommandValue::ProjectRefreshed { name: "demo".to_string(), members: 2, converged: true, changes: vec!["Project/demo".to_string()] }
    );
    let refreshed = projects.get("demo").await.expect("refreshed project");
    assert_eq!(refreshed.spec.display_name, "demo");
    let app = refreshed.spec.repositories.iter().find(|member| member.alias.as_deref() == Some("app")).expect("app member");
    assert_eq!(app.repo, app_key, "alias should preserve the rename-stable RepositoryKey");
    assert!(app.roles.contains(&ProjectRepositoryRole::Knowledge));
    assert_eq!(refreshed.metadata.annotations.get(BOOTSTRAP_COMMIT_ANNOTATION).map(String::as_str), Some("commit-two"));
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await,
        CommandValue::ProjectRefreshed { name: "demo".to_string(), members: 2, converged: false, changes: Vec::new() }
    );
}

#[tokio::test]
async fn ops_entries_materialize_by_frontmatter_scope_with_provenance_and_converge_drift() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let ops_spec = RepositorySpec::remote("https://github.com/example/project-ops").expect("ops spec");
    let commit = Arc::new(RwLock::new("ops-commit".to_string()));
    daemon.set_repository_inspector(Arc::new(DeclarationInspector { bootstrap: ops_spec.clone(), commit })).await;
    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/app\n    roles: [code]\n  - alias: docs\n    url: https://github.com/example/docs\n    roles: [code]\n  - alias: operations\n    url: https://github.com/example/project-ops\n    roles: [ops]\n",
    )
    .expect("write declaration");
    let misleading_directory = tmp.path().join("verification-commands");
    std::fs::create_dir(&misleading_directory).expect("create misleading directory");
    std::fs::write(
        misleading_directory.join("this-is-a-workflow.md"),
        "---\nkind: workflow_template\nname: scoped\nrepos: [app]\n---\nvessels:\n  - name: work\n    crew:\n      - role: verify\n        command: cargo test\n",
    )
    .expect("write scoped workflow");
    std::fs::write(
        tmp.path().join("all-code.entry"),
        "---\nkind: workflow_template\nname: all-code\n---\nvessels:\n  - name: work\n    crew:\n      - role: verify\n        command: cargo check\n",
    )
    .expect("write default-scoped workflow");
    std::fs::write(
        tmp.path().join("test-command.entry"),
        "---\nkind: verification_command\nname: test\nrepos: [app]\n---\ncommand: cargo test --workspace\n",
    )
    .expect("write verification command");
    let ensure_path = tmp.path().join("quartermaster.entry");
    std::fs::write(
        &ensure_path,
        "---\nkind: ensure\nrole: quartermaster\nrepos: [app]\n---\nworkflow: all-code\nstance: trusted\npresents-as: fleet\n",
    )
    .expect("write standing convoy ensure");

    let mut rx = daemon.subscribe();
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() })
            .await,
        CommandValue::ProjectRegistered { name: "demo".to_string(), members: 3 }
    );
    let project = backend.using::<Project>("flotilla").get("demo").await.expect("project");
    let app = project.spec.repositories.iter().find(|member| member.alias.as_deref() == Some("app")).expect("app");
    let docs = project.spec.repositories.iter().find(|member| member.alias.as_deref() == Some("docs")).expect("docs");
    let workflows = backend.using::<WorkflowTemplate>("flotilla");
    let scoped = workflows.get("scoped").await.expect("scoped workflow");
    assert_eq!(scoped.spec.vessels[0].repository_refs.as_deref(), Some(std::slice::from_ref(&app.repo)));
    assert_eq!(scoped.metadata.annotations.get(SOURCE_REPOSITORY_ANNOTATION), Some(&ops_spec.key().to_string()));
    assert_eq!(scoped.metadata.annotations.get(SOURCE_COMMIT_ANNOTATION).map(String::as_str), Some("ops-commit"));
    assert_eq!(
        scoped.metadata.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str),
        Some("verification-commands/this-is-a-workflow.md")
    );
    let all_code = workflows.get("all-code").await.expect("default-scoped workflow");
    assert_eq!(all_code.spec.vessels[0].repository_refs.as_deref(), Some([app.repo.clone(), docs.repo.clone()].as_slice()));
    let app_repository = backend.using::<Repository>("flotilla").get(&app.repo.to_string()).await.expect("app repository");
    let docs_repository = backend.using::<Repository>("flotilla").get(&docs.repo.to_string()).await.expect("docs repository");
    assert_eq!(app_repository.spec.verification_commands().get("test").map(String::as_str), Some("cargo test --workspace"));
    assert!(docs_repository.spec.verification_commands().is_empty());
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let ensure = ensures.list().await.expect("list ensures").into_iter().next().expect("materialized standing convoy ensure");
    let ensure_name = ensure.metadata.name.clone();
    assert_eq!(ensure.spec.project_ref, "demo");
    assert_eq!(ensure.spec.role, "quartermaster");
    assert_eq!(ensure.spec.workflow_ref, "all-code");
    assert_eq!(ensure.spec.repositories, vec![app.repo.clone()]);
    assert_eq!(ensure.spec.stance, Some(Stance::Trusted));
    assert_eq!(ensure.metadata.annotations.get(SOURCE_COMMIT_ANNOTATION).map(String::as_str), Some("ops-commit"));
    assert_eq!(ensure.metadata.annotations.get(PRESENTS_AS_ANNOTATION).map(String::as_str), Some("fleet"));

    workflows
        .update(
            &InputMeta::from(&scoped.metadata),
            &scoped.metadata.resource_version,
            &WorkflowTemplateSpec::builder().vessels(Vec::new()).build(),
        )
        .await
        .expect("drift workflow");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await,
        CommandValue::ProjectRefreshed {
            name: "demo".to_string(),
            members: 3,
            converged: true,
            changes: vec!["WorkflowTemplate/scoped".to_string()],
        }
    );
    assert_eq!(workflows.get("scoped").await.expect("converged workflow").spec, scoped.spec);

    std::fs::remove_file(ensure_path).expect("remove ensure entry");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await,
        CommandValue::ProjectRefreshed {
            name: "demo".to_string(),
            members: 3,
            converged: true,
            changes: vec![format!("deleted ConvoyEnsure/{ensure_name}")],
        }
    );
    assert!(matches!(ensures.get(&ensure_name).await, Err(flotilla_resources::ResourceError::NotFound { .. })));

    std::fs::remove_file(misleading_directory.join("this-is-a-workflow.md")).expect("remove workflow entry");
    assert_eq!(
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await,
        CommandValue::ProjectRefreshed {
            name: "demo".to_string(),
            members: 3,
            converged: true,
            changes: vec!["deleted WorkflowTemplate/scoped".to_string()],
        }
    );
    assert!(matches!(workflows.get("scoped").await, Err(flotilla_resources::ResourceError::NotFound { .. })));

    std::fs::remove_file(tmp.path().join("test-command.entry")).expect("remove verification entry");
    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/app\n    roles: [knowledge]\n  - alias: docs\n    url: https://github.com/example/docs\n    roles: [code]\n  - alias: operations\n    url: https://github.com/example/project-ops\n    roles: [ops]\n",
    )
    .expect("remove app code role");
    let result = execute_project_command(&daemon, &mut rx, CommandAction::ProjectRefresh { name: "demo".to_string() }).await;
    assert!(
        matches!(&result, CommandValue::ProjectRefreshed { changes, .. }
            if changes.iter().any(|change| change == &format!("Repository/{} verification commands", app.repo))),
        "unexpected command result: {result:?}"
    );
    let released = backend.using::<Repository>("flotilla").get(&app.repo.to_string()).await.expect("released repository");
    assert!(released.spec.verification_commands().is_empty());
    assert!(!released.metadata.annotations.contains_key(VERIFICATION_PROJECT_ANNOTATION));
}

#[tokio::test]
async fn ops_entry_rejects_a_hand_applied_workflow_name_collision() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let ops_spec = RepositorySpec::remote("https://github.com/example/project-ops").expect("ops spec");
    daemon
        .set_repository_inspector(Arc::new(DeclarationInspector {
            bootstrap: ops_spec,
            commit: Arc::new(RwLock::new("ops-commit".to_string())),
        }))
        .await;
    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/project-ops\n    roles: [code, ops]\n",
    )
    .expect("write declaration");
    std::fs::write(
        tmp.path().join("collision.entry"),
        "---\nkind: workflow_template\nname: existing\n---\nvessels:\n  - name: work\n    crew:\n      - role: verify\n        command: cargo test\n",
    )
    .expect("write workflow entry");
    let hand_applied = WorkflowTemplateSpec::builder().vessels(Vec::new()).build();
    let workflows = backend.using::<WorkflowTemplate>("flotilla");
    workflows.create(&InputMeta::builder().name("existing".to_string()).build(), &hand_applied).await.expect("hand apply workflow");

    let mut rx = daemon.subscribe();
    let result =
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() })
            .await;
    assert!(
        matches!(&result, CommandValue::Error { message } if message.contains("already exists") && message.contains("not materialized")),
        "unexpected command result: {result:?}"
    );
    let preserved = workflows.get("existing").await.expect("hand-applied workflow remains");
    assert_eq!(preserved.spec, hand_applied);
    assert!(!preserved.metadata.annotations.contains_key(MATERIALIZED_PROJECT_ANNOTATION));
}

#[tokio::test]
async fn ops_entry_rejects_a_verification_command_targeting_a_non_code_member() {
    let (daemon, _backend, _config, _runtime, tmp) = start_daemon().await;
    let ops_spec = RepositorySpec::remote("https://github.com/example/project-ops").expect("ops spec");
    daemon
        .set_repository_inspector(Arc::new(DeclarationInspector {
            bootstrap: ops_spec,
            commit: Arc::new(RwLock::new("ops-commit".to_string())),
        }))
        .await;
    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/app\n    roles: [code]\n  - alias: operations\n    url: https://github.com/example/project-ops\n    roles: [ops]\n",
    )
    .expect("write declaration");
    std::fs::write(
        tmp.path().join("invalid-command.entry"),
        "---\nkind: verification_command\nname: test\nrepos: [operations]\n---\ncommand: cargo test --workspace\n",
    )
    .expect("write verification command");

    let mut rx = daemon.subscribe();
    let result =
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() })
            .await;
    assert!(
        matches!(&result, CommandValue::Error { message }
            if message.contains("invalid-command.entry")
                && message.contains("repository alias `operations`")
                && message.contains("without the code role")),
        "unexpected command result: {result:?}"
    );
}

#[tokio::test]
async fn ops_entry_rejects_an_ensure_whose_workflow_has_an_exit() {
    let (daemon, _backend, _config, _runtime, tmp) = start_daemon().await;
    let ops_spec = RepositorySpec::remote("https://github.com/example/project-ops").expect("ops spec");
    daemon
        .set_repository_inspector(Arc::new(DeclarationInspector {
            bootstrap: ops_spec,
            commit: Arc::new(RwLock::new("ops-commit".to_string())),
        }))
        .await;
    std::fs::write(
        tmp.path().join("project.yaml"),
        "name: demo\nmembers:\n  - alias: app\n    url: https://github.com/example/project-ops\n    roles: [code, ops]\n",
    )
    .expect("write declaration");
    std::fs::write(
        tmp.path().join("invalid-ensure.entry"),
        "---\nkind: ensure\nrole: finite-work\nrepos: [app]\n---\nworkflow: single-agent-trusted\n",
    )
    .expect("write ensure entry");

    let mut rx = daemon.subscribe();
    let result =
        execute_project_command(&daemon, &mut rx, CommandAction::ProjectRegister { target: tmp.path().to_string_lossy().into_owned() })
            .await;

    assert!(
        matches!(&result, CommandValue::Error { message }
            if message.contains("invalid-ensure.entry")
                && message.contains("single-agent-trusted")
                && message.contains("exit declaration")),
        "unexpected command result: {result:?}"
    );
}

#[tokio::test]
async fn tracking_repo_materializes_whole_repo_project() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let repository_key = track_repository(&daemon, &tmp, "tracked", "https://github.com/org/tracked.git").await;

    let project = backend.using::<Project>("flotilla").get("tracked").await.expect("whole-repository project should exist");
    assert_eq!(project.spec.display_name, "tracked");
    assert_eq!(project.spec.default_workflow_ref, "single-agent-trusted");
    assert_eq!(project.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("whole-repository-project"));
    assert_eq!(project.spec.repositories.as_slice(), [flotilla_resources::ProjectRepositorySpec {
        repo: repository_key,
        alias: None,
        roles: Default::default(),
        subpath: None,
        default_branch: None,
    }]);
}

#[tokio::test]
async fn tracked_repo_reconciles_generator_owned_project_fields_and_preserves_custom_fields() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let checkout_path = tmp.path().join("tracked");
    init_git_repo_with_remote(&checkout_path, "https://github.com/org/tracked.git");
    let repository_spec = RepositorySpec::remote("https://github.com/org/tracked.git").expect("repository spec");
    let repository_key = repository_spec.key();
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![checkout_path],
        config,
        git_process_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: repository_spec, host_ref: "host-01".to_string() })).await;

    let projects = backend.definitions::<Project>("flotilla");
    let log_output = Arc::new(Mutex::new(Vec::new()));
    let writer = LogCaptureWriter(Arc::clone(&log_output));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    projects
        .create(
            &InputMeta::builder()
                .name("tracked".to_string())
                .labels(BTreeMap::from([("example.com/preserved".to_string(), "true".to_string())]))
                .build(),
            &ProjectSpec::builder()
                .display_name("My Tracked Repository".to_string())
                .default_workflow_ref("single-agent-contained".to_string())
                .maybe_issue_source(Some(IssueSource { service: "https://linear.app".to_string(), scope: "TRACK".to_string() }))
                .repositories(vec![flotilla_resources::ProjectRepositorySpec {
                    repo: repository_key,
                    alias: None,
                    roles: Default::default(),
                    subpath: None,
                    default_branch: Some("release".to_string()),
                }])
                .build(),
        )
        .await
        .expect("stale whole-repository Project should be created");

    daemon.materialize_tracked_repo_projects().with_subscriber(subscriber).await.expect("tracked Project reconciliation should succeed");

    let reconciled = projects.get("tracked").await.expect("tracked Project should remain");
    assert_eq!(reconciled.spec.display_name, "My Tracked Repository");
    assert_eq!(reconciled.spec.issue_source, Some(IssueSource { service: "https://linear.app".to_string(), scope: "TRACK".to_string() }));
    assert_eq!(reconciled.spec.default_workflow_ref, "single-agent-trusted");
    assert_eq!(reconciled.spec.repositories.as_slice(), [flotilla_resources::ProjectRepositorySpec {
        repo: RepositorySpec::remote("https://github.com/org/tracked.git").expect("repository spec").key(),
        alias: None,
        roles: Default::default(),
        subpath: None,
        default_branch: None,
    }]);
    assert_eq!(reconciled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("whole-repository-project"));
    assert_eq!(reconciled.metadata.labels.get("example.com/preserved").map(String::as_str), Some("true"));
    let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
    assert!(
        logs.contains("stored generator-owned whole-repository Project fields diverged; overwriting"),
        "expected generator-field divergence warning; captured logs: {logs:?}"
    );
    assert!(logs.contains("tracked"), "expected warning to identify project `tracked`; captured logs: {logs:?}");

    daemon.materialize_tracked_repo_projects().await.expect("steady-state reconciliation should succeed");
    let unchanged = projects.get("tracked").await.expect("tracked Project should remain");
    assert_eq!(unchanged.metadata.resource_version, reconciled.metadata.resource_version);
}

#[tokio::test]
async fn tracked_repo_labels_matching_unlabelled_project_once() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let checkout_path = tmp.path().join("tracked");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let repository_spec = RepositorySpec::remote("https://github.com/org/tracked.git").expect("repository spec");
    let repository_key = repository_spec.key();
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![checkout_path],
        config,
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    daemon.set_repository_inspector(Arc::new(FixedInspector { spec: repository_spec.clone(), host_ref: "host-01".to_string() })).await;

    let projects = backend.definitions::<Project>("flotilla");
    projects
        .create(
            &InputMeta::builder().name("tracked".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("tracked".to_string())
                .default_workflow_ref("single-agent-trusted".to_string())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec {
                    repo: repository_key,
                    alias: None,
                    roles: Default::default(),
                    subpath: None,
                    default_branch: None,
                }])
                .build(),
        )
        .await
        .expect("matching unlabelled Project should be created");
    let unlabelled = projects.get("tracked").await.expect("Project should exist");

    daemon.materialize_tracked_repo_projects().await.expect("ownership reconciliation should succeed");
    let labelled = projects.get("tracked").await.expect("Project should remain");
    assert_ne!(labelled.metadata.resource_version, unlabelled.metadata.resource_version);
    assert_eq!(labelled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("whole-repository-project"));

    daemon.materialize_tracked_repo_projects().await.expect("steady-state reconciliation should succeed");
    let unchanged = projects.get("tracked").await.expect("Project should remain");
    assert_eq!(unchanged.metadata.resource_version, labelled.metadata.resource_version);
}

#[tokio::test]
async fn retracking_path_after_remote_appears_migrates_repository_identity() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let checkout_path = tmp.path().join("andamento");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let local_spec = RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec");
    let local_key = local_spec.key();
    let inspected_spec = Arc::new(RwLock::new(local_spec));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;

    let first_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path.clone() },
        })
        .await
        .expect("initial repo add");
    assert!(matches!(await_command_result(&mut rx, first_id).await, CommandValue::RepoTracked { .. }));

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/andamento").expect("remote repository spec");
    let remote_key = remote_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(remote_key.to_string()).build(), &remote_spec)
        .await
        .expect("stale remote repository generation");
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("github-com-flotilla-org-andamento".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("andamento".to_string())
                .default_workflow_ref("single-agent-trusted".to_string())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder().repo(remote_key.clone()).build()])
                .build(),
        )
        .await
        .expect("stale disambiguated project twin");
    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    let second_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path.clone() },
        })
        .await
        .expect("repo add after remote appears");
    assert_eq!(await_command_result(&mut rx, second_id).await, CommandValue::RepoTracked {
        path: checkout_path.clone(),
        resolved_from: None,
        identity_change: Some(RepositoryIdentityChange {
            previous_display: "local".to_string(),
            current_display: "https://github.com/flotilla-org/andamento".to_string(),
        }),
    });

    let projects = backend.definitions::<Project>("flotilla").list().await.expect("project list");
    assert_eq!(projects.len(), 1, "identity migration must not leave a disambiguated twin");
    assert_eq!(projects[0].metadata.name, "andamento");
    assert_eq!(projects[0].spec.repositories[0].repo, remote_key);
    let repositories = backend.using::<Repository>("flotilla").list().await.expect("repository list");
    assert_eq!(repositories.items.len(), 1, "superseded repository identities should be garbage-collected");
    assert_eq!(repositories.items[0].metadata.name, remote_key.to_string());
    assert!(backend.using::<Repository>("flotilla").get(&local_key.to_string()).await.is_err());

    let repository = backend.clone().using::<Repository>("flotilla").get(&remote_key.to_string()).await.expect("remote repository");
    backend
        .clone()
        .using::<Repository>("flotilla")
        .update_status(&repository.metadata.name, &repository.metadata.resource_version, &RepositoryStatus {
            default_branch: Some("main".to_string()),
            ..Default::default()
        })
        .await
        .expect("repository status update");
    let convoy_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ConvoyCreate {
                name: "identity-migrated".into(),
                workflow_ref: "scratch".into(),
                inputs: Vec::new(),
                repository_url: None,
                r#ref: None,
                project_ref: Some("andamento".into()),
                placement_policy: None,
                adopted_checkout: None,
            },
        })
        .await
        .expect("convoy create");
    assert_eq!(await_command_result(&mut rx, convoy_id).await, CommandValue::ConvoyCreated { name: "identity-migrated@andamento".into() });
}

#[tokio::test]
async fn tracking_after_custom_project_identity_change_does_not_create_generated_twin() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let checkout_path = tmp.path().join("custom-repo");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let local_spec = RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec");
    let inspected_spec = Arc::new(RwLock::new(local_spec));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;

    assert_eq!(
        execute_project_add(
            &daemon,
            &mut rx,
            checkout_path.to_string_lossy().into_owned(),
            Some("my-custom-project"),
            Some("My Custom Project"),
        )
        .await,
        CommandValue::ProjectAdded { name: "my-custom-project".into() }
    );

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/custom-repo").expect("remote repository spec");
    let remote_key = remote_spec.key();
    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    let track_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path },
        })
        .await
        .expect("track repo after remote appears");
    assert!(matches!(await_command_result(&mut rx, track_id).await, CommandValue::RepoTracked { .. }));

    let projects = backend.using::<Project>("flotilla").list().await.expect("project list");
    assert_eq!(projects.items.len(), 1, "identity migration must not create a generated twin for a custom-named project");
    assert_eq!(projects.items[0].metadata.name, "my-custom-project");
    assert_eq!(projects.items[0].spec.display_name, "My Custom Project");
    assert_eq!(projects.items[0].spec.repositories[0].repo, remote_key);
}

#[tokio::test]
async fn identity_change_preserves_migrated_project_when_local_and_remote_names_differ() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let checkout_path = tmp.path().join("z-local-name");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let local_spec = RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec");
    let inspected_spec = Arc::new(RwLock::new(local_spec));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;
    let add_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path.clone() },
        })
        .await
        .expect("initial repo add");
    assert!(matches!(await_command_result(&mut rx, add_id).await, CommandValue::RepoTracked { .. }));

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/a-remote-name").expect("remote repository spec");
    let remote_key = remote_spec.key();
    backend
        .clone()
        .using::<Repository>("flotilla")
        .create(&InputMeta::builder().name(remote_key.to_string()).build(), &remote_spec)
        .await
        .expect("pre-existing remote repository");
    backend
        .clone()
        .using::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("a-remote-name".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("a-remote-name".to_string())
                .default_workflow_ref("single-agent-trusted".to_string())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder().repo(remote_key.clone()).build()])
                .build(),
        )
        .await
        .expect("pre-existing remote project twin");

    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    let second_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path },
        })
        .await
        .expect("repo add after remote appears");
    assert!(matches!(await_command_result(&mut rx, second_id).await, CommandValue::RepoTracked { .. }));

    let projects = backend.definitions::<Project>("flotilla").list().await.expect("project list");
    assert_eq!(projects.len(), 1, "the pre-existing remote twin should be removed");
    assert_eq!(projects[0].metadata.name, "z-local-name", "the migrated Project should retain its identity");
    assert_eq!(projects[0].spec.display_name, "z-local-name");
    assert_eq!(projects[0].spec.repositories[0].repo, remote_key);
}

#[tokio::test]
async fn refresh_surfaces_and_reconciles_repository_identity_change() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let checkout_path = tmp.path().join("refreshed");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let inspected_spec = Arc::new(RwLock::new(
        RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec"),
    ));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;
    let add_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path.clone() },
        })
        .await
        .expect("initial repo add");
    assert!(matches!(await_command_result(&mut rx, add_id).await, CommandValue::RepoTracked { .. }));

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/refreshed").expect("remote repository spec");
    let remote_key = remote_spec.key();
    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    let refresh_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::Refresh { repo: Some(RepoSelector::Path(checkout_path.clone())) },
        })
        .await
        .expect("refresh command");

    assert_eq!(await_command_result(&mut rx, refresh_id).await, CommandValue::Refreshed {
        repos: vec![checkout_path],
        identity_changes: vec![RepositoryIdentityChange {
            previous_display: "local".to_string(),
            current_display: "https://github.com/flotilla-org/refreshed".to_string(),
        }],
    });
    let project = backend.using::<Project>("flotilla").get("refreshed").await.expect("migrated project");
    assert_eq!(project.spec.repositories[0].repo, remote_key);
}

#[tokio::test]
async fn identity_migration_marks_repository_retained_by_durable_checkout() {
    let (daemon, backend, _config, _runtime, tmp) = start_daemon().await;
    let mut rx = daemon.subscribe();
    let checkout_path = tmp.path().join("retained");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let local_spec = RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec");
    let local_key = local_spec.key();
    let inspected_spec = Arc::new(RwLock::new(local_spec));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;
    let add_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path.clone() },
        })
        .await
        .expect("initial repo add");
    assert!(matches!(await_command_result(&mut rx, add_id).await, CommandValue::RepoTracked { .. }));
    backend
        .clone()
        .using::<Checkout>("flotilla")
        .create(
            &InputMeta::builder().name("durable-old-checkout".to_string()).build(),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "main".to_string(),
                path: checkout_path.to_string_lossy().into_owned(),
                repo_ref: local_key.clone(),
                host_ref: "host-01".to_string(),
                is_main: true,
            }),
        )
        .await
        .expect("durable checkout");

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/retained").expect("remote repository spec");
    let remote_key = remote_spec.key();
    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    let second_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::TrackRepoPath { path: checkout_path },
        })
        .await
        .expect("repo add after remote appears");
    assert!(matches!(await_command_result(&mut rx, second_id).await, CommandValue::RepoTracked { .. }));

    let retained = backend.using::<Repository>("flotilla").get(&local_key.to_string()).await.expect("retained old repository");
    assert_eq!(retained.metadata.annotations.get("flotilla.work/superseded-by"), Some(&remote_key.to_string()));
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
async fn daemon_start_backfills_project_idempotently_and_preserves_edits_to_generated_name_occupant() {
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
        alias: None,
        roles: Default::default(),
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
    let materialized = projects.list().await.expect("project list");
    assert_eq!(materialized.items.len(), 2);
    assert!(materialized.items.iter().any(|project| {
        matches!(
            project.spec.repositories.as_slice(),
            [entry] if entry.repo == repository_key && entry.subpath.is_none()
        )
    }));
}

#[tokio::test]
async fn daemon_restart_preserves_whole_repo_project_overlapped_by_applied_project_during_identity_change() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config = test_config(tmp.path().join("config"));
    let checkout_path = tmp.path().join("flotilla");
    std::fs::create_dir(&checkout_path).expect("checkout dir");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        vec![checkout_path.clone()],
        Arc::clone(&config),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let local_spec = RepositorySpec::local("host-01", checkout_path.join(".git").to_string_lossy()).expect("local repository spec");
    let local_key = local_spec.key();
    let inspected_spec = Arc::new(RwLock::new(local_spec));
    daemon.set_repository_inspector(Arc::new(MutableInspector { spec: Arc::clone(&inspected_spec) })).await;
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
    projects.get("flotilla").await.expect("whole-repository project should exist before overlap");
    let second_key = RepositoryKey("second-repository".to_string());
    let mut rx = daemon.subscribe();
    let apply_id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply {
                name: "presentation".into(),
                spec_yaml: format!(
                    "display_name: Presentation\ndefault_workflow_ref: single-agent-contained\nrepositories:\n  - repo: {local_key}\n  - repo: {second_key}\n"
                ),
            },
        })
        .await
        .expect("apply execute");
    assert_eq!(await_command_result(&mut rx, apply_id).await, CommandValue::ProjectApplied { name: "presentation".into() });
    projects.get("flotilla").await.expect("whole-repository project should survive applying the overlap");

    let remote_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("remote repository spec");
    let remote_key = remote_spec.key();
    *inspected_spec.write().expect("repository identity lock should not be poisoned") = remote_spec;
    drop(runtime);

    let _restarted =
        DaemonRuntime::start_with_options(daemon, config, None, options).await.expect("runtime should restart after project overlap");

    let whole_repo = projects.get("flotilla").await.expect("whole-repository project should survive restart");
    assert_eq!(whole_repo.spec.repositories[0].repo, remote_key);
    let presentation = projects.get("presentation").await.expect("overlapping applied project should survive restart");
    assert_eq!(presentation.spec.repositories.iter().map(|repository| &repository.repo).collect::<Vec<_>>(), [&second_key, &remote_key]);
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
        dispatch_policy: None,
        repositories: vec![flotilla_resources::ProjectRepositorySpec {
            repo: RepositoryKey("other-repository".to_string()),
            alias: None,
            roles: Default::default(),
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
                dispatch_policy: None,
                repositories: vec![flotilla_resources::ProjectRepositorySpec {
                    repo: RepositoryKey(repo_ref.to_string()),
                    alias: None,
                    roles: Default::default(),
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
    assert_eq!(project.spec.default_workflow_ref, "single-agent-trusted");
    assert_eq!(project.spec.repositories.as_slice(), [flotilla_resources::ProjectRepositorySpec {
        repo: repository_key,
        alias: None,
        roles: Default::default(),
        subpath: None,
        default_branch: None,
    }]);
}

async fn project_checkout_set_with_store_history(tmp: &tempfile::TempDir, with_history: bool) -> Vec<CheckoutSpec> {
    let config = test_config(tmp.path().join(if with_history { "history-config" } else { "cold-config" }));
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
    let _runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime start");
    let spec = RepositorySpec::remote("https://github.com/org/view-only.git").expect("repository spec");
    let key = spec.key();
    let main_path = tmp.path().join("view-only");
    let worktree_path = tmp.path().join("view-only.feature");
    std::fs::create_dir_all(&main_path).expect("main checkout");
    std::fs::create_dir_all(&worktree_path).expect("worktree checkout");
    let checkouts = Arc::new(RwLock::new(vec![
        LocalCheckoutInspection { path: main_path.clone(), host_ref: "host-01".to_string(), git_ref: "main".to_string(), is_main: true },
        LocalCheckoutInspection { path: worktree_path, host_ref: "host-01".to_string(), git_ref: "feature".to_string(), is_main: false },
    ]));
    daemon.set_repository_inspector(Arc::new(WorktreeInspector { spec: spec.clone(), checkouts })).await;

    if with_history {
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &spec)
            .await
            .expect("historical repository");
        backend
            .clone()
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("view-only".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("view-only".to_string())
                    .default_workflow_ref("single-agent-trusted".to_string())
                    .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder().repo(key).build()])
                    .build(),
            )
            .await
            .expect("historical project");
    }

    let mut rx = daemon.subscribe();
    assert_eq!(
        execute_project_add(&daemon, &mut rx, main_path.to_string_lossy().into_owned(), None, None).await,
        CommandValue::ProjectAdded { name: "view-only".into() }
    );
    assert!(daemon.tracked_repo_paths().await.is_empty(), "project materialization must not depend on Plane-A tracking");
    let mut specs = daemon
        .observed_resource_backend()
        .using::<Checkout>("flotilla")
        .list()
        .await
        .expect("checkout list")
        .items
        .into_iter()
        .map(|checkout| checkout.spec)
        .collect::<Vec<_>>();
    specs.sort_by_key(|spec| spec.target_path().map(str::to_string));
    specs
}

#[tokio::test]
async fn view_only_project_rebuilds_the_same_checkout_set_on_an_empty_store() {
    let tmp = tempfile::TempDir::new().expect("tempdir");

    let cold = project_checkout_set_with_store_history(&tmp, false).await;
    let historical = project_checkout_set_with_store_history(&tmp, true).await;

    assert_eq!(cold, historical);
    assert_eq!(cold.len(), 2);
}

#[tokio::test]
async fn repeated_project_materialization_updates_and_retracts_worktree_observations() {
    let (daemon, _backend, _config, _runtime, tmp) = start_daemon().await;
    let spec = RepositorySpec::remote("https://github.com/org/reconciled.git").expect("repository spec");
    let main_path = tmp.path().join("reconciled");
    let worktree_path = tmp.path().join("reconciled.feature");
    std::fs::create_dir_all(&main_path).expect("main checkout");
    std::fs::create_dir_all(&worktree_path).expect("worktree checkout");
    let checkouts = Arc::new(RwLock::new(vec![
        LocalCheckoutInspection { path: main_path.clone(), host_ref: "host-01".to_string(), git_ref: "main".to_string(), is_main: true },
        LocalCheckoutInspection {
            path: worktree_path.clone(),
            host_ref: "host-01".to_string(),
            git_ref: "feature".to_string(),
            is_main: false,
        },
    ]));
    daemon.set_repository_inspector(Arc::new(WorktreeInspector { spec, checkouts: Arc::clone(&checkouts) })).await;
    let mut rx = daemon.subscribe();

    execute_project_add(&daemon, &mut rx, main_path.to_string_lossy().into_owned(), None, None).await;
    checkouts.write().expect("checkout inspection lock should not be poisoned")[1].git_ref = "renamed-feature".to_string();
    execute_project_add(&daemon, &mut rx, main_path.to_string_lossy().into_owned(), None, None).await;

    let observed = daemon.observed_resource_backend().using::<Checkout>("flotilla");
    let after_update = observed.list().await.expect("updated checkout list").items;
    assert_eq!(after_update.len(), 2);
    assert!(after_update.iter().any(|checkout| {
        matches!(&checkout.spec, CheckoutSpec::Observed(spec) if spec.path == worktree_path.to_string_lossy() && spec.r#ref == "renamed-feature")
    }));

    checkouts.write().expect("checkout inspection lock should not be poisoned").pop();
    execute_project_add(&daemon, &mut rx, main_path.to_string_lossy().into_owned(), None, None).await;

    let after_removal = observed.list().await.expect("checkout list after removal").items;
    assert_eq!(after_removal.len(), 1);
    assert!(matches!(&after_removal[0].spec, CheckoutSpec::Observed(spec) if spec.path == main_path.to_string_lossy()));
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
async fn repeated_project_add_reconciles_owned_fields_and_preserves_custom_fields() {
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
    let reconciled = projects.get("core").await.expect("project");
    assert_eq!(reconciled.spec.display_name, "Evolved");
    assert_eq!(reconciled.spec.issue_source, evolved.issue_source);
    assert_eq!(reconciled.spec.default_workflow_ref, "single-agent-trusted");
    assert_eq!(reconciled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("whole-repository-project"));
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
async fn project_apply_preserves_existing_metadata() {
    let (daemon, backend, _config, _runtime, _tmp) = start_daemon().await;
    let projects = backend.definitions::<Project>("flotilla");
    projects
        .create(
            &InputMeta::builder()
                .name("labelled".to_string())
                .labels(BTreeMap::from([
                    (MANAGED_BY_LABEL.to_string(), "whole-repository-project".to_string()),
                    ("example.com/preserved".to_string(), "true".to_string()),
                ]))
                .annotations(BTreeMap::from([("example.com/note".to_string(), "keep".to_string())]))
                .build(),
            &ProjectSpec::builder()
                .display_name("Before".to_string())
                .default_workflow_ref("single-agent-trusted".to_string())
                .repositories(vec![flotilla_resources::ProjectRepositorySpec {
                    repo: RepositoryKey("repository".to_string()),
                    alias: None,
                    roles: Default::default(),
                    subpath: None,
                    default_branch: None,
                }])
                .build(),
        )
        .await
        .expect("labelled Project should be created");
    let mut rx = daemon.subscribe();
    let yaml = r#"
display_name: After
default_workflow_ref: single-agent-trusted
issue_source:
  service: https://linear.app
  scope: KEEP
repositories:
  - repo: repository
"#;

    let id = daemon
        .execute(Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::ProjectApply { name: "labelled".into(), spec_yaml: yaml.into() },
        })
        .await
        .expect("execute");

    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ProjectApplied { name: "labelled".into() });
    let project = projects.get("labelled").await.expect("Project should remain");
    assert_eq!(project.spec.display_name, "After");
    assert_eq!(project.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some("whole-repository-project"));
    assert_eq!(project.metadata.labels.get("example.com/preserved").map(String::as_str), Some("true"));
    assert_eq!(project.metadata.annotations.get("example.com/note").map(String::as_str), Some("keep"));
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
    assert_eq!(await_command_result(&mut rx, id).await, CommandValue::ConvoyCreated { name: "linked@my-project".into() });
    let convoy = backend
        .using::<Convoy>("flotilla")
        .list_matching_labels(&BTreeMap::from([(flotilla_resources::ROLE_LABEL.to_string(), "linked".to_string())]))
        .await
        .expect("list convoys")
        .items
        .into_iter()
        .next()
        .expect("convoy");
    assert_eq!(convoy.spec.project_ref.as_deref(), Some("my-project"));
    assert_eq!(convoy.spec.repositories.len(), 1);
    assert_eq!(convoy.spec.repositories[0].source_ref, "main");
    assert_eq!(convoy.spec.repositories[0].target_ref, "main");
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
