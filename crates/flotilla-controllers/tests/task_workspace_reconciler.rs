mod common;

use std::collections::BTreeMap;

use chrono::Utc;
use common::{
    create_convoy_with_single_task, create_docker_worktree_policy, create_host_direct_policy, create_policy, create_ready_checkout,
    create_ready_clone, create_ready_docker_environment, create_ready_host_direct_environment, create_stopped_terminal, create_workspace,
    labeled_meta, meta, DockerWorktreePolicyFixture, ReadyCheckoutFixture, StoppedTerminalFixture,
};
use flotilla_controllers::reconcilers::TaskWorkspaceReconciler;
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{Actuation, Reconciler},
    Checkout, CheckoutSpec, CheckoutWorktreeSpec, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, DockerCheckoutStrategy,
    DockerEnvironmentSpec, DockerPerTaskPlacementPolicySpec, Environment, EnvironmentSpec, HostDirectEnvironmentSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InnerCommandStatus, PlacementPolicySpec, ProcessDefinition,
    ProcessSource, ResourceBackend, ResourceError, SnapshotTask, TaskWorkspace, TerminalSession, TerminalSessionPhase, TerminalSessionSpec,
    TerminalSessionStatus, WorkflowSnapshot, CONVOY_LABEL, PROCESS_ORDINAL_LABEL, ROLE_LABEL, TASK_LABEL, TASK_ORDINAL_LABEL,
    TASK_WORKSPACE_LABEL,
};
use rstest::rstest;

const NAMESPACE: &str = "flotilla";
const REPO_URL: &str = "git@github.com:flotilla-org/flotilla.git";
const GIT_REF: &str = "feat/task-provisioning";
const HOST_REF: &str = "01HXYZ";

#[tokio::test]
async fn missing_placement_policy_marks_workspace_failed() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", REPO_URL, GIT_REF).await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "policy-missing", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TaskWorkspaceStatusPatch::MarkFailed { ref message })
            if message.contains("placement policy policy-missing not found")
    ));
}

#[tokio::test]
async fn reuses_existing_clone_by_deterministic_name() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-b", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-a", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone").await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-b", "convoy-b", "implement", "policy-a", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(outcome.actuations.iter().all(|actuation| !matches!(actuation, Actuation::CreateClone { .. })));
    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(
            actuation,
            Actuation::CreateCheckout { spec, .. }
                if spec.worktree.as_ref().map(|worktree| worktree.clone_ref.as_str()) == Some(clone_name.as_str())
        )
    }));
}

#[tokio::test]
async fn docker_worktree_waits_for_checkout_before_creating_environment() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-c", "implement", REPO_URL, GIT_REF).await;
    create_docker_worktree_policy(
        &backend,
        NAMESPACE,
        DockerWorktreePolicyFixture::builder()
            .name("policy-worktree".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .mount_path("/workspace".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone").await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-c", "convoy-c", "implement", "policy-worktree", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend.clone(), NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());
    assert!(outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateCheckout { .. })));
    assert!(outcome.actuations.iter().all(|actuation| !matches!(actuation, Actuation::CreateEnvironment { .. })));

    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-workspace-c".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c".to_string())
            .maybe_worktree(Some(CheckoutWorktreeSpec { clone_ref: "clone-placeholder".to_string() }))
            .build(),
    )
    .await;
    let current = backend.clone().using::<TaskWorkspace>(NAMESPACE).get("workspace-c").await.expect("workspace get should succeed");
    let deps = reconciler.fetch_dependencies(&current).await.expect("deps should reload");
    let outcome = reconciler.reconcile(&current, &deps, chrono::Utc::now());

    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(
            actuation,
            Actuation::CreateEnvironment { spec, .. }
                if spec.docker.as_ref().map(|docker| docker.mounts.as_slice()) == Some(&[flotilla_resources::EnvironmentMount {
                    source_path: "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c".to_string(),
                    target_path: "/workspace".to_string(),
                    mode: flotilla_resources::EnvironmentMountMode::Rw,
                }])
        )
    }));
}

#[rstest]
#[case::host_direct(
    "workspace-host",
    PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec {
            host_ref: HOST_REF.to_string(),
            checkout: HostDirectPlacementPolicyCheckout::Worktree,
        })
        .build(),
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-host",
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-host",
    None,
)]
#[case::docker_worktree(
    "workspace-docker-worktree",
    PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_task(DockerPerTaskPlacementPolicySpec {
            host_ref: HOST_REF.to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            default_cwd: None,
            env: Default::default(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
        .build(),
    "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-docker-worktree",
    "/workspace",
    Some(DockerEnvironmentSpec {
        host_ref: HOST_REF.to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        mounts: vec![flotilla_resources::EnvironmentMount {
            source_path: "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-docker-worktree".to_string(),
            target_path: "/workspace".to_string(),
            mode: flotilla_resources::EnvironmentMountMode::Rw,
        }],
        env: Default::default(),
    }),
)]
#[case::docker_fresh_clone(
    "workspace-docker-fresh",
    PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_task(DockerPerTaskPlacementPolicySpec {
            host_ref: HOST_REF.to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            default_cwd: Some("/app".to_string()),
            env: Default::default(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
        })
        .build(),
    "/workspace",
    "/app",
    Some(DockerEnvironmentSpec {
        host_ref: HOST_REF.to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        mounts: Vec::new(),
        env: Default::default(),
    }),
)]
#[tokio::test]
async fn terminal_sessions_use_strategy_specific_cwd(
    #[case] workspace_name: &str,
    #[case] policy_spec: PlacementPolicySpec,
    #[case] checkout_path: &str,
    #[case] expected_cwd: &str,
    #[case] docker_env: Option<DockerEnvironmentSpec>,
) {
    assert_terminal_cwd_for_strategy(workspace_name, policy_spec, checkout_path, expected_cwd, docker_env).await;
}

#[tokio::test]
async fn child_failure_propagates_to_workspace_failure() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-f", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-f", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone").await;
    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-workspace-f".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-f".to_string())
            .maybe_worktree(Some(CheckoutWorktreeSpec { clone_ref: "clone-placeholder".to_string() }))
            .build(),
    )
    .await;
    create_stopped_terminal(
        &backend,
        NAMESPACE,
        StoppedTerminalFixture::builder()
            .name("terminal-workspace-f-coder".to_string())
            .env_ref(host_direct_env_name())
            .role("coder".to_string())
            .command("cargo test".to_string())
            .cwd("/workspace".to_string())
            .pool("cleat".to_string())
            .message("boom".to_string())
            .build(),
    )
    .await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-f", "convoy-f", "implement", "policy-f", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TaskWorkspaceStatusPatch::MarkFailed { ref message }) if message == "boom"
    ));
}

#[tokio::test]
async fn run_finalizer_deletes_all_labeled_children() {
    let backend = ResourceBackend::InMemory(Default::default());
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-finalize", "convoy-a", "implement", "policy-a", REPO_URL).await;

    create_labeled_environment(&backend, NAMESPACE, "env-workspace-finalize", "workspace-finalize").await;
    create_labeled_checkout(&backend, NAMESPACE, "checkout-workspace-finalize", "workspace-finalize").await;
    create_labeled_terminal(&backend, NAMESPACE, "terminal-workspace-finalize-coder", "workspace-finalize").await;

    let reconciler = TaskWorkspaceReconciler::new(backend.clone(), NAMESPACE);
    reconciler.run_finalizer(&workspace).await.expect("finalizer should succeed");

    assert!(matches!(
        backend.clone().using::<Environment>(NAMESPACE).get("env-workspace-finalize").await,
        Err(ResourceError::NotFound { .. })
    ));
    assert!(matches!(
        backend.clone().using::<Checkout>(NAMESPACE).get("checkout-workspace-finalize").await,
        Err(ResourceError::NotFound { .. })
    ));
    assert!(matches!(
        backend.clone().using::<TerminalSession>(NAMESPACE).get("terminal-workspace-finalize-coder").await,
        Err(ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn run_finalizer_ignores_missing_children_and_cleans_partial_workspace() {
    let backend = ResourceBackend::InMemory(Default::default());
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-partial", "convoy-a", "implement", "policy-a", REPO_URL).await;

    create_labeled_environment(&backend, NAMESPACE, "env-workspace-partial", "workspace-partial").await;

    let reconciler = TaskWorkspaceReconciler::new(backend.clone(), NAMESPACE);
    reconciler.run_finalizer(&workspace).await.expect("finalizer should succeed");

    assert!(matches!(
        backend.clone().using::<Environment>(NAMESPACE).get("env-workspace-partial").await,
        Err(ResourceError::NotFound { .. })
    ));
}

#[tokio::test]
async fn terminal_session_actuation_includes_system_and_user_labels() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_labeled_processes(&backend, NAMESPACE, "convoy-labels", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-labels", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone").await;
    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-workspace-labels".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path("/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels".to_string())
            .maybe_worktree(Some(CheckoutWorktreeSpec { clone_ref: "clone-placeholder".to_string() }))
            .build(),
    )
    .await;
    create_running_terminal(
        &backend,
        NAMESPACE,
        "terminal-workspace-labels-build",
        &host_direct_env_name(),
        "build",
        "cargo check",
        "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels",
        "cleat",
    )
    .await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-labels", "convoy-labels", "review", "policy-labels", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, Utc::now());

    let terminal = outcome
        .actuations
        .iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateTerminalSession { meta, spec } => Some((meta, spec)),
            _ => None,
        })
        .expect("terminal actuation should be created");

    assert_eq!(terminal.1.role, "test");
    assert_eq!(terminal.0.labels.get("service").map(String::as_str), Some("api"));
    assert_eq!(terminal.0.labels.get("team").map(String::as_str), Some("platform"));
    assert_eq!(terminal.0.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-labels"));
    assert_eq!(terminal.0.labels.get(TASK_LABEL).map(String::as_str), Some("review"));
    assert_eq!(terminal.0.labels.get(TASK_WORKSPACE_LABEL).map(String::as_str), Some("workspace-labels"));
    assert_eq!(terminal.0.labels.get(ROLE_LABEL).map(String::as_str), Some("test"));
    assert_eq!(terminal.0.labels.get(TASK_ORDINAL_LABEL).map(String::as_str), Some("001"));
    assert_eq!(terminal.0.labels.get(PROCESS_ORDINAL_LABEL).map(String::as_str), Some("001"));
}

async fn assert_terminal_cwd_for_strategy(
    workspace_name: &str,
    policy_spec: PlacementPolicySpec,
    checkout_path: &str,
    expected_cwd: &str,
    docker_env: Option<DockerEnvironmentSpec>,
) {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-cwd", "implement", REPO_URL, GIT_REF).await;
    create_policy(&backend, NAMESPACE, "policy-cwd", policy_spec).await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    if checkout_path != "/workspace" || docker_env.is_none() {
        create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone")
            .await;
    }
    if let Some(docker) = docker_env {
        create_ready_docker_environment(&backend, NAMESPACE, &format!("env-{workspace_name}"), docker).await;
    }
    let checkout_env_ref = if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
        format!("env-{workspace_name}")
    } else {
        host_direct_env_name()
    };
    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name(format!("checkout-{workspace_name}"))
            .env_ref(checkout_env_ref)
            .git_ref(GIT_REF.to_string())
            .path(checkout_path.to_string())
            .maybe_worktree(if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
                None
            } else {
                Some(CheckoutWorktreeSpec { clone_ref: "clone-placeholder".to_string() })
            })
            .maybe_fresh_clone(if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
                Some(flotilla_resources::FreshCloneCheckoutSpec { url: REPO_URL.to_string() })
            } else {
                None
            })
            .build(),
    )
    .await;
    let workspace = create_workspace(&backend, NAMESPACE, workspace_name, "convoy-cwd", "implement", "policy-cwd", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    let cwd = outcome
        .actuations
        .iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateTerminalSession { spec, .. } => Some(spec.cwd.as_str()),
            _ => None,
        })
        .expect("terminal actuation should be created");
    assert_eq!(cwd, expected_cwd);
}

fn host_direct_env_name() -> String {
    format!("host-direct-{HOST_REF}")
}

async fn create_convoy_with_labeled_processes(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    repo_url: &str,
    git_ref: &str,
) -> flotilla_resources::ResourceObject<Convoy> {
    let convoys = backend.clone().using::<Convoy>(namespace);
    let convoy = convoys
        .create(&meta(name), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: Default::default(),
            placement_policy: None,
            repository: Some(ConvoyRepositorySpec { url: repo_url.to_string() }),
            r#ref: Some(git_ref.to_string()),
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status(name, &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                tasks: vec![
                    SnapshotTask {
                        name: "implement".to_string(),
                        depends_on: Vec::new(),
                        processes: vec![ProcessDefinition::builder()
                            .role("coder".to_string())
                            .source(ProcessSource::Tool { command: "cargo fmt --check".to_string() })
                            .build()],
                    },
                    SnapshotTask {
                        name: "review".to_string(),
                        depends_on: vec!["implement".to_string()],
                        processes: vec![
                            ProcessDefinition::builder()
                                .role("build".to_string())
                                .source(ProcessSource::Tool { command: "cargo check".to_string() })
                                .build(),
                            ProcessDefinition::builder()
                                .role("test".to_string())
                                .source(ProcessSource::Tool { command: "cargo test".to_string() })
                                .labels(BTreeMap::from([
                                    ("service".to_string(), "api".to_string()),
                                    ("team".to_string(), "platform".to_string()),
                                    (CONVOY_LABEL.to_string(), "wrong-convoy".to_string()),
                                    (TASK_LABEL.to_string(), "wrong-task".to_string()),
                                    (TASK_WORKSPACE_LABEL.to_string(), "wrong-workspace".to_string()),
                                    (ROLE_LABEL.to_string(), "wrong-role".to_string()),
                                    (TASK_ORDINAL_LABEL.to_string(), "999".to_string()),
                                    (PROCESS_ORDINAL_LABEL.to_string(), "999".to_string()),
                                ]))
                                .build(),
                        ],
                    },
                ],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status update should succeed");
    convoys.get(name).await.expect("convoy get should succeed")
}

#[allow(clippy::too_many_arguments)]
async fn create_running_terminal(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    env_ref: &str,
    role: &str,
    command: &str,
    cwd: &str,
    pool: &str,
) -> flotilla_resources::ResourceObject<TerminalSession> {
    let sessions = backend.clone().using::<TerminalSession>(namespace);
    let created = sessions
        .create(&meta(name), &TerminalSessionSpec {
            env_ref: env_ref.to_string(),
            role: role.to_string(),
            command: command.to_string(),
            cwd: cwd.to_string(),
            pool: pool.to_string(),
        })
        .await
        .expect("terminal create should succeed");
    sessions
        .update_status(name, &created.metadata.resource_version, &TerminalSessionStatus {
            phase: TerminalSessionPhase::Running,
            session_id: Some(format!("session-{name}")),
            pid: Some(42),
            started_at: Some(Utc::now()),
            stopped_at: None,
            inner_command_status: Some(InnerCommandStatus::Running),
            inner_exit_code: None,
            message: None,
        })
        .await
        .expect("terminal status update should succeed");
    sessions.get(name).await.expect("terminal get should succeed")
}

async fn create_labeled_environment(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<Environment>(namespace)
        .create(&labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())]), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: HOST_REF.to_string(),
                repo_default_dir: "/Users/alice/dev/flotilla-repos".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("environment create should succeed");
}

async fn create_labeled_checkout(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<Checkout>(namespace)
        .create(&labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())]), &CheckoutSpec {
            env_ref: host_direct_env_name(),
            r#ref: GIT_REF.to_string(),
            target_path: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
            worktree: Some(CheckoutWorktreeSpec { clone_ref: "clone-placeholder".to_string() }),
            fresh_clone: None,
        })
        .await
        .expect("checkout create should succeed");
}

async fn create_labeled_terminal(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<TerminalSession>(namespace)
        .create(&labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())]), &TerminalSessionSpec {
            env_ref: host_direct_env_name(),
            role: "coder".to_string(),
            command: "cargo test".to_string(),
            cwd: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
            pool: "cleat".to_string(),
        })
        .await
        .expect("terminal create should succeed");
}
