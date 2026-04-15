mod common;

use common::{
    create_convoy_with_single_task, create_docker_worktree_policy, create_host_direct_policy, create_policy, create_ready_checkout,
    create_ready_clone, create_ready_docker_environment, create_ready_host_direct_environment, create_stopped_terminal, create_workspace,
    DockerWorktreePolicyFixture, ReadyCheckoutFixture, StoppedTerminalFixture,
};
use flotilla_controllers::reconcilers::TaskWorkspaceReconciler;
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{Actuation, Reconciler},
    CheckoutWorktreeSpec, DockerCheckoutStrategy, DockerEnvironmentSpec, DockerPerTaskPlacementPolicySpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, PlacementPolicySpec, ResourceBackend, TaskWorkspace,
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
