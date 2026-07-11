mod common;

use std::collections::BTreeMap;

use chrono::Utc;
use common::{
    create_convoy_with_single_task, create_docker_worktree_policy, create_host_direct_policy, create_policy, create_ready_checkout,
    create_ready_clone, create_ready_docker_environment, create_ready_host_direct_environment, create_stopped_terminal, create_workspace,
    labeled_meta, meta, task_workspace_meta, DockerWorktreePolicyFixture, ReadyCheckoutFixture, StoppedTerminalFixture,
};
use flotilla_controllers::reconcilers::TaskWorkspaceReconciler;
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{Actuation, Reconciler},
    Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus,
    DockerCheckoutStrategy, DockerEnvironmentSpec, DockerPerTaskPlacementPolicySpec, Environment, EnvironmentSpec,
    HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InnerCommandStatus, LifecycleAuthority,
    ObservedCheckoutSpec, PlacementPolicySpec, ProcessDefinition, ProcessSource, ResourceBackend, ResourceError, Selector, SnapshotTask,
    TaskWorkspace, TaskWorkspaceSpec, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec,
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
                if matches!(spec, CheckoutSpec::Worktree(worktree) if worktree.clone_ref == clone_name)
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
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-c",
                "clone-placeholder",
            )))
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
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-f",
                "clone-placeholder",
            )))
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
async fn adopted_checkout_ref_reuses_checkout_without_creating_clone_or_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-adopted", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-adopted", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    create_ready_adopted_checkout(&backend, NAMESPACE, "adopted-checkout-convoy-adopted", "/Users/alice/dev/flotilla-existing").await;
    let workspace = backend
        .clone()
        .using::<TaskWorkspace>(NAMESPACE)
        .create(&task_workspace_meta("workspace-adopted", REPO_URL), &TaskWorkspaceSpec {
            convoy_ref: "convoy-adopted".to_string(),
            task: "implement".to_string(),
            placement_policy_ref: "policy-adopted".to_string(),
            adopted_checkout_ref: Some("adopted-checkout-convoy-adopted".to_string()),
        })
        .await
        .expect("workspace create should succeed");

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, Utc::now());

    assert!(outcome
        .actuations
        .iter()
        .all(|actuation| { !matches!(actuation, Actuation::CreateClone { .. } | Actuation::CreateCheckout { .. }) }));
    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(
            actuation,
            Actuation::CreateTerminalSession { spec, .. }
                if spec.env_ref == host_direct_env_name()
                    && spec.pool == "cleat"
                    && spec.cwd == "/Users/alice/dev/flotilla-existing"
        )
    }));
}

#[tokio::test]
async fn first_agent_is_provisioned_with_a_durable_crew_brief_while_later_agents_remain_latent() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-crew", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").tasks[0].processes = vec![
        ProcessDefinition::builder()
            .role("coder".to_string())
            .source(ProcessSource::Agent {
                selector: Selector { capability: "coding".to_string() },
                prompt: Some("Implement issue 668.".to_string()),
            })
            .build(),
        ProcessDefinition::builder()
            .role("reviewer".to_string())
            .source(ProcessSource::Agent {
                selector: Selector { capability: "review".to_string() },
                prompt: Some("Review the coder's work.".to_string()),
            })
            .build(),
    ];
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-crew", &convoy.metadata.resource_version, &status)
        .await
        .expect("update convoy crew");
    create_host_direct_policy(&backend, NAMESPACE, "policy-crew", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    create_ready_adopted_checkout(&backend, NAMESPACE, "adopted-checkout-convoy-crew", "/Users/alice/dev/flotilla-existing").await;
    let workspace = backend
        .clone()
        .using::<TaskWorkspace>(NAMESPACE)
        .create(&task_workspace_meta("workspace-crew", REPO_URL), &TaskWorkspaceSpec {
            convoy_ref: "convoy-crew".to_string(),
            task: "implement".to_string(),
            placement_policy_ref: "policy-crew".to_string(),
            adopted_checkout_ref: Some("adopted-checkout-convoy-crew".to_string()),
        })
        .await
        .expect("workspace create");

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps");
    let outcome = reconciler.reconcile(&workspace, &deps, Utc::now());

    let Actuation::CreateTerminalSession { spec, .. } = outcome.actuations.first().expect("coder session actuation") else {
        panic!("expected terminal session actuation");
    };
    let TerminalSessionSource::Agent { selector, brief, context, message } = &spec.source else {
        panic!("expected structured agent launch");
    };
    assert_eq!(selector.capability, "coding");
    assert_eq!(brief.path, ".flotilla/briefs/coder.md");
    assert!(brief.content.contains("You are `coder` in convoy `convoy-crew`"));
    assert!(brief.content.contains("- `coder`: active"));
    assert!(brief.content.contains("- `reviewer`: latent"));
    assert!(brief.content.contains("flotilla crew reviewer handoff --message"));
    assert!(brief.content.ends_with("Implement issue 668.\n"));
    assert_eq!(context.namespace, NAMESPACE);
    assert_eq!(context.vessel, "workspace-crew");
    assert_eq!(message, &None);
    assert!(outcome
        .actuations
        .iter()
        .all(|actuation| { !matches!(actuation, Actuation::CreateTerminalSession { spec, .. } if spec.role == "reviewer") }));
}

#[tokio::test]
async fn observed_checkout_at_managed_name_marks_workspace_failed() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-observed", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-observed", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    let canonical_repo = canonicalize_repo_url(REPO_URL).expect("repo canonicalization");
    let clone_name = format!("clone-{}", clone_key(&canonical_repo, &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/Users/alice/dev/flotilla-repos/clone").await;
    create_ready_observed_checkout_without_status_path(&backend, NAMESPACE, "checkout-workspace-observed").await;
    let workspace =
        create_workspace(&backend, NAMESPACE, "workspace-observed", "convoy-observed", "implement", "policy-observed", REPO_URL).await;

    let reconciler = TaskWorkspaceReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::TaskWorkspaceStatusPatch::MarkFailed { ref message })
            if message == "checkout checkout-workspace-observed is ready but has no target path"
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
async fn run_finalizer_preserves_adopted_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-adopted", "convoy-a", "implement", "policy-a", REPO_URL).await;

    create_labeled_adopted_checkout(&backend, NAMESPACE, "checkout-workspace-adopted", "workspace-adopted").await;
    create_labeled_terminal(&backend, NAMESPACE, "terminal-workspace-adopted-coder", "workspace-adopted").await;

    let reconciler = TaskWorkspaceReconciler::new(backend.clone(), NAMESPACE);
    reconciler.run_finalizer(&workspace).await.expect("finalizer should succeed");

    let checkout =
        backend.clone().using::<Checkout>(NAMESPACE).get("checkout-workspace-adopted").await.expect("adopted checkout should be preserved");
    assert_eq!(checkout.metadata.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Adopted));
    assert!(matches!(
        backend.clone().using::<TerminalSession>(NAMESPACE).get("terminal-workspace-adopted-coder").await,
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
            .maybe_worktree(Some(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-labels",
                "clone-placeholder",
            )))
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
            .env_ref(checkout_env_ref.clone())
            .git_ref(GIT_REF.to_string())
            .path(checkout_path.to_string())
            .maybe_worktree(if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
                None
            } else {
                Some(worktree_checkout_spec(&checkout_env_ref, GIT_REF, checkout_path, "clone-placeholder"))
            })
            .maybe_fresh_clone(if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
                Some(fresh_clone_checkout_spec(&checkout_env_ref, GIT_REF, checkout_path, REPO_URL))
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

fn worktree_checkout_spec(env_ref: &str, git_ref: &str, target_path: &str, clone_ref: &str) -> CheckoutWorktreeSpec {
    CheckoutWorktreeSpec {
        env_ref: env_ref.to_string(),
        r#ref: git_ref.to_string(),
        target_path: target_path.to_string(),
        clone_ref: clone_ref.to_string(),
    }
}

fn fresh_clone_checkout_spec(env_ref: &str, git_ref: &str, target_path: &str, url: &str) -> flotilla_resources::FreshCloneCheckoutSpec {
    flotilla_resources::FreshCloneCheckoutSpec {
        env_ref: env_ref.to_string(),
        r#ref: git_ref.to_string(),
        target_path: target_path.to_string(),
        url: url.to_string(),
    }
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
            project_ref: None,
            adopted_checkout_ref: None,
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
            source: flotilla_resources::TerminalSessionSource::Tool { command: command.to_string() },
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
            crew: None,
            launch_command: Some(command.to_string()),
            delivered_message_id: None,
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
        .create(
            &labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())]),
            &CheckoutSpec::Worktree(worktree_checkout_spec(
                &host_direct_env_name(),
                GIT_REF,
                &format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
                "clone-placeholder",
            )),
        )
        .await
        .expect("checkout create should succeed");
}

async fn create_labeled_adopted_checkout(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<Checkout>(namespace)
        .create(
            &labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())])
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: GIT_REF.to_string(),
                path: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
                repo_ref: "repo-flotilla".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("adopted checkout create should succeed");
}

async fn create_ready_adopted_checkout(backend: &ResourceBackend, namespace: &str, name: &str, path: &str) {
    let checkouts = backend.clone().using::<Checkout>(namespace);
    let created = checkouts
        .create(
            &meta(name).with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: GIT_REF.to_string(),
                path: path.to_string(),
                repo_ref: "repo-flotilla".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("adopted checkout create should succeed");
    checkouts
        .update_status(name, &created.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some(path.to_string()),
            commit: Some("abc123".to_string()),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
}

async fn create_ready_observed_checkout_without_status_path(backend: &ResourceBackend, namespace: &str, name: &str) {
    let checkouts = backend.clone().using::<Checkout>(namespace);
    let checkout = checkouts
        .create(
            &meta(name),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: GIT_REF.to_string(),
                path: "/Users/alice/dev/flotilla-repos/github-com-flotilla-org-flotilla.workspace-observed".to_string(),
                repo_ref: "repo-flotilla".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout create should succeed");
    checkouts
        .update_status(name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: None,
            commit: Some("abc123".to_string()),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
}

async fn create_labeled_terminal(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<TerminalSession>(namespace)
        .create(&labeled_meta(name, [(TASK_WORKSPACE_LABEL.to_string(), workspace_name.to_string())]), &TerminalSessionSpec {
            env_ref: host_direct_env_name(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
            pool: "cleat".to_string(),
        })
        .await
        .expect("terminal create should succeed");
}
