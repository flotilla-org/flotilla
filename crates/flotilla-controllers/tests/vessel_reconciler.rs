mod common;

use std::collections::BTreeMap;

use chrono::Utc;
use common::{
    create_convoy_with_single_task, create_docker_worktree_policy, create_host_direct_policy, create_policy, create_ready_checkout,
    create_ready_clone, create_ready_docker_environment, create_ready_host_direct_environment, create_stopped_terminal, create_workspace,
    labeled_meta, meta, vessel_meta, work_state, DockerWorktreePolicyFixture, ReadyCheckoutFixture, StoppedTerminalFixture,
};
use flotilla_controllers::reconcilers::VesselReconciler;
use flotilla_protocol::{IssueRef, IssueSource, IssueState};
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{Actuation, Reconciler},
    ensure_repository, Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutWorktreeSpec, Convoy, ConvoyIssue, ConvoyPhase,
    ConvoyReconciler, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, DockerCheckoutStrategy, DockerEnvironmentSpec,
    DockerPerVesselPlacementPolicySpec, Environment, EnvironmentSpec, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout,
    HostDirectPlacementPolicySpec, InnerCommandStatus, InputMeta, IssueSnapshot, LifecycleAuthority, ObservedCheckoutSpec,
    PlacementPolicySpec, Repository, RepositorySpec, ResourceBackend, ResourceError, Selector, Stance, TerminalSession,
    TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus, Vessel, VesselRequirement, VesselSpec,
    WorkPhase, WorkflowSnapshot, WorkflowTemplate, CONVOY_LABEL, CREW_ORDINAL_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_ORDINAL_LABEL,
    VESSEL_REF_LABEL,
};
use rstest::rstest;

const NAMESPACE: &str = "flotilla";
const REPO_URL: &str = "https://github.com/flotilla-org/flotilla.git";
const GIT_REF: &str = "feat/task-provisioning";
const HOST_REF: &str = "01HXYZ";

#[tokio::test]
async fn repositoryless_vessel_runs_tools_without_provisioning_a_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(&meta("convoy-scratch"), &ConvoySpec {
            workflow_ref: "scratch".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: Vec::new(),
            r#ref: None,
            project_ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            issues: Vec::new(),
            instruction: None,
        })
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-scratch", &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: "work".to_string(),
                    stance: Stance::Trusted,
                    depends_on: Vec::new(),
                    repository_refs: None,
                    crew: vec![CrewSpec::builder()
                        .role("shell".to_string())
                        .source(CrewSource::Tool { command: "bash".to_string() })
                        .build()],
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status should update");
    create_host_direct_policy(&backend, NAMESPACE, "policy-scratch", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    let vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-scratch"), &VesselSpec {
            convoy_ref: "convoy-scratch".to_string(),
            vessel_name: "work".to_string(),
            placement_policy_ref: "policy-scratch".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel should create");

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let deps = reconciler.fetch_dependencies(&vessel).await.expect("deps should load");
    let outcome = reconciler.reconcile(&vessel, &deps, Utc::now());

    assert!(outcome
        .actuations
        .iter()
        .all(|actuation| !matches!(actuation, Actuation::CreateClone { .. } | Actuation::CreateCheckout { .. })));
    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(actuation, Actuation::CreateTerminalSession { spec, .. }
            if spec.cwd == "/Users/alice/dev/flotilla-repos" && spec.source == TerminalSessionSource::Tool { command: "bash".to_string() })
    }));
}

#[tokio::test]
async fn contained_requirement_rejects_host_direct_placement() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-contained", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy should have status");
    status.workflow_snapshot.as_mut().expect("convoy should have workflow snapshot").vessels[0].stance = Stance::Contained;
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-contained", &convoy.metadata.resource_version, &status)
        .await
        .expect("convoy status should update");
    create_host_direct_policy(&backend, NAMESPACE, "policy-host", HOST_REF, "cleat").await;
    let workspace =
        create_workspace(&backend, NAMESPACE, "workspace-contained", "convoy-contained", "implement", "policy-host", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkFailed { ref message })
            if message.contains("requires contained stance") && message.contains("host-direct")
    ));
    assert!(outcome.actuations.is_empty());
}

#[tokio::test]
async fn ready_vessel_records_requested_and_effective_stance() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-stance", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-stance", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone").await;
    let checkout_path = "/Users/alice/dev/flotilla-repos/workspace-stance";
    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-stance".to_string())
            .env_ref(host_direct_env_name())
            .git_ref(GIT_REF.to_string())
            .path(checkout_path.to_string())
            .maybe_worktree(Some(worktree_checkout_spec(&host_direct_env_name(), GIT_REF, checkout_path, &clone_name)))
            .build(),
    )
    .await;
    create_running_terminal(
        &backend,
        NAMESPACE,
        "terminal-workspace-stance-coder",
        &host_direct_env_name(),
        "coder",
        "cargo test",
        checkout_path,
        "cleat",
    )
    .await;
    let workspace =
        create_workspace(&backend, NAMESPACE, "workspace-stance", "convoy-stance", "implement", "policy-stance", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkReady { requested_stance: Stance::Trusted, effective_stance: Stance::Trusted, .. })
    ));
}

#[tokio::test]
async fn sequential_vessels_share_a_convoy_owned_worktree_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-shared", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.clone().expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels.push(VesselRequirement {
        name: "review".to_string(),
        stance: Stance::Trusted,
        depends_on: vec!["implement".to_string()],
        repository_refs: None,
        crew: Vec::new(),
    });
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-shared", &convoy.metadata.resource_version, &status)
        .await
        .expect("second vessel should be recorded");
    create_host_direct_policy(&backend, NAMESPACE, "policy-shared", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone").await;
    let implement =
        create_workspace(&backend, NAMESPACE, "workspace-shared-implement", "convoy-shared", "implement", "policy-shared", REPO_URL).await;
    let review =
        create_workspace(&backend, NAMESPACE, "workspace-shared-review", "convoy-shared", "review", "policy-shared", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let implement_deps = reconciler.fetch_dependencies(&implement).await.expect("implement dependencies");
    let implement_outcome = reconciler.reconcile(&implement, &implement_deps, Utc::now());
    let (checkout_meta, checkout_spec) = implement_outcome
        .actuations
        .iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateCheckout { meta, spec } => Some((meta.clone(), spec.clone())),
            _ => None,
        })
        .expect("first vessel should create the shared checkout");
    assert_eq!(checkout_meta.name, "checkout-convoy-shared");
    assert_eq!(checkout_meta.labels.get(CONVOY_LABEL).map(String::as_str), Some("convoy-shared"));
    assert!(!checkout_meta.labels.contains_key(VESSEL_REF_LABEL));

    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let checkout = checkouts.create(&checkout_meta, &checkout_spec).await.expect("shared checkout create");
    checkouts
        .update_status(&checkout_meta.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: checkout_spec.target_path().map(str::to_string),
            commit: None,
            branch_provenance: Default::default(),
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("shared checkout ready");

    let review_deps = reconciler.fetch_dependencies(&review).await.expect("review dependencies");
    let review_outcome = reconciler.reconcile(&review, &review_deps, Utc::now());
    assert!(
        review_outcome.actuations.iter().all(|actuation| !matches!(actuation, Actuation::CreateCheckout { .. })),
        "later vessels should reuse the convoy checkout"
    );

    reconciler.run_finalizer(&implement).await.expect("vessel finalization");
    checkouts.get(&checkout_meta.name).await.expect("vessel finalization must preserve convoy checkout");
    ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>(NAMESPACE))
        .with_checkouts(checkouts.clone())
        .run_finalizer(&convoy)
        .await
        .expect("convoy finalization");
    assert!(matches!(checkouts.get(&checkout_meta.name).await, Err(ResourceError::NotFound { .. })));
}

#[tokio::test]
async fn unrelated_convoys_with_the_same_branch_use_distinct_worktree_paths() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-one", "implement", REPO_URL, GIT_REF).await;
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-two", "implement", REPO_URL, GIT_REF).await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-shared-branch", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    let clone_name =
        format!("clone-{}", clone_key(&canonicalize_repo_url(REPO_URL).expect("repo canonicalization"), &host_direct_env_name()));
    create_ready_clone(&backend, NAMESPACE, &clone_name, REPO_URL, &host_direct_env_name(), "/tmp/clone").await;
    let first = create_workspace(&backend, NAMESPACE, "workspace-one", "convoy-one", "implement", "policy-shared-branch", REPO_URL).await;
    let second = create_workspace(&backend, NAMESPACE, "workspace-two", "convoy-two", "implement", "policy-shared-branch", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let first_deps = reconciler.fetch_dependencies(&first).await.expect("first dependencies");
    let second_deps = reconciler.fetch_dependencies(&second).await.expect("second dependencies");
    let first_outcome = reconciler.reconcile(&first, &first_deps, Utc::now());
    let second_outcome = reconciler.reconcile(&second, &second_deps, Utc::now());
    let checkout_path = |outcome: &flotilla_resources::controller::ReconcileOutcome<_>| {
        outcome
            .actuations
            .iter()
            .find_map(|actuation| match actuation {
                Actuation::CreateCheckout { spec, .. } => spec.target_path().map(str::to_string),
                _ => None,
            })
            .expect("convoy should create a checkout")
    };
    let first_path = checkout_path(&first_outcome);
    let second_path = checkout_path(&second_outcome);

    assert_eq!(first_path, "/Users/alice/dev/flotilla-repos/convoy-one/github-com-flotilla-org-flotilla.feat-task-provisioning");
    assert_eq!(second_path, "/Users/alice/dev/flotilla-repos/convoy-two/github-com-flotilla-org-flotilla.feat-task-provisioning");
    assert_ne!(first_path, second_path);
}

#[tokio::test]
async fn multi_repository_vessel_provisions_every_checkout_and_runs_crew_at_workspace_root() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_specs = [
        RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository"),
        RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository"),
    ];
    for repository in &repository_specs {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(&meta("convoy-multi"), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: vec![
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/cleat".to_string(),
                    repo_ref: repository_specs[1].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "cleat".to_string(),
                    subpaths: Vec::new(),
                },
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repository_specs[0].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                },
            ],
            r#ref: Some("feature/multi".to_string()),
            project_ref: Some("flotilla-suite".to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            issues: vec![
                ConvoyIssue {
                    reference: IssueRef {
                        source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                        id: "732".into(),
                    },
                    repository_ref: Some(repository_specs[0].key()),
                    snapshot: IssueSnapshot {
                        title: "Start convoy from an issue".into(),
                        body: Some("Persist this exact issue body.".into()),
                        state: IssueState::Open,
                        labels: vec!["enhancement".into()],
                        as_of: "2026-07-18T09:30:00Z".parse().expect("timestamp"),
                    },
                },
                ConvoyIssue {
                    reference: IssueRef {
                        source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/cleat".into() },
                        id: "733".into(),
                    },
                    repository_ref: None,
                    snapshot: IssueSnapshot {
                        title: "Fix shared workflow setup".into(),
                        body: Some("This issue applies across the checked-out repositories.".into()),
                        state: IssueState::Open,
                        labels: vec!["enhancement".into()],
                        as_of: "2026-07-18T09:31:00Z".parse().expect("timestamp"),
                    },
                },
            ],
            instruction: Some("Keep the public seam stable.".into()),
        })
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-multi", &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: "implement".to_string(),
                    stance: Stance::Trusted,
                    depends_on: Vec::new(),
                    repository_refs: None,
                    crew: vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Agent {
                            selector: Selector { capability: "coding".to_string() },
                            prompt: Some("Work across both repositories.".to_string()),
                        })
                        .build()],
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status should update");
    create_host_direct_policy(&backend, NAMESPACE, "policy-multi", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    for (repository, url) in [
        (&repository_specs[0], "https://github.com/flotilla-org/flotilla"),
        (&repository_specs[1], "https://github.com/flotilla-org/cleat"),
    ] {
        let clone_name = format!(
            "clone-{}",
            clone_key(
                match repository.identity() {
                    flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote,
                    flotilla_resources::RepositoryIdentity::Local { .. } => panic!("expected remote repository"),
                },
                &host_direct_env_name()
            )
        );
        create_ready_clone(&backend, NAMESPACE, &clone_name, url, &host_direct_env_name(), &format!("/clones/{}", repository.leaf_slug()))
            .await;
    }
    let vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-multi"), &VesselSpec {
            convoy_ref: "convoy-multi".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-multi".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel should create");
    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);

    let deps = reconciler.fetch_dependencies(&vessel).await.expect("deps should load");
    let outcome = reconciler.reconcile(&vessel, &deps, Utc::now());
    let checkout_actuations = outcome
        .actuations
        .iter()
        .filter_map(|actuation| match actuation {
            Actuation::CreateCheckout { meta, spec } => Some((meta.clone(), spec.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkout_actuations.len(), 2);
    assert_eq!(checkout_actuations.iter().map(|(_, spec)| spec.target_path().expect("managed checkout path")).collect::<Vec<_>>(), [
        "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/cleat",
        "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/flotilla",
    ]);
    for (meta, spec) in checkout_actuations {
        let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
        let created = checkouts.create(&meta, &spec).await.expect("checkout should create");
        checkouts
            .update_status(&meta.name, &created.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: spec.target_path().map(str::to_string),
                commit: Some("44982740".to_string()),
                branch_provenance: Default::default(),
                integration: Default::default(),
                message: None,
            })
            .await
            .expect("checkout should become ready");
    }

    let current = backend.clone().using::<Vessel>(NAMESPACE).get("workspace-multi").await.expect("vessel should exist");
    let deps = reconciler.fetch_dependencies(&current).await.expect("deps should reload");
    let outcome = reconciler.reconcile(&current, &deps, Utc::now());
    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(
            actuation,
            Actuation::CreateTerminalSession { spec, .. }
                if spec.cwd == "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi"
                    && matches!(&spec.source, TerminalSessionSource::Agent { brief, .. }
                        if brief.path == ".flotilla/briefs/coder.md"
                            && brief.copies == [
                                "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/cleat",
                                "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi/flotilla"
                            ]
                            && brief.content.contains("https://github.com` / `flotilla-org/flotilla` / `732")
                            && brief.content.contains("https://github.com` / `flotilla-org/cleat` / `733")
                            && brief.content.contains("Snapshot as of `2026-07-18T09:30:00+00:00`")
                            && brief.content.contains("Persist this exact issue body.")
                            && brief.content.contains("This issue applies across the checked-out repositories.")
                            && brief.content.contains("Keep the public seam stable."))
        )
    }));

    create_running_terminal(
        &backend,
        NAMESPACE,
        "terminal-workspace-multi-coder",
        &host_direct_env_name(),
        "coder",
        "cargo test",
        "/Users/alice/dev/flotilla-repos/convoy-multi/feature-multi",
        "cleat",
    )
    .await;
    let current = backend.clone().using::<Vessel>(NAMESPACE).get("workspace-multi").await.expect("vessel should exist");
    let deps = reconciler.fetch_dependencies(&current).await.expect("deps should reload");
    let outcome = reconciler.reconcile(&current, &deps, Utc::now());
    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkReady { checkout_refs, .. }) if checkout_refs.len() == 2
    ));
}

#[tokio::test]
async fn multi_repository_docker_mounts_the_shared_workspace_root_once() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repositories = [
        RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository"),
        RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository"),
    ];
    for repository in &repositories {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(&meta("convoy-multi-docker"), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: vec![
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repositories[0].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                },
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/cleat".to_string(),
                    repo_ref: repositories[1].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "cleat".to_string(),
                    subpaths: Vec::new(),
                },
            ],
            r#ref: Some("feature/multi".to_string()),
            project_ref: Some("flotilla-suite".to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            issues: Vec::new(),
            instruction: None,
        })
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-multi-docker", &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: "implement".to_string(),
                    stance: Stance::Contained,
                    depends_on: Vec::new(),
                    repository_refs: None,
                    crew: vec![CrewSpec::builder()
                        .role("coder".to_string())
                        .source(CrewSource::Tool { command: "cargo test".to_string() })
                        .build()],
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status should update");
    create_docker_worktree_policy(
        &backend,
        NAMESPACE,
        DockerWorktreePolicyFixture::builder()
            .name("policy-multi-docker".to_string())
            .host_ref(HOST_REF.to_string())
            .pool("cleat".to_string())
            .image("ghcr.io/flotilla/dev:latest".to_string())
            .mount_path("/workspace".to_string())
            .build(),
    )
    .await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;

    for (repository, slug) in [(&repositories[0], "flotilla"), (&repositories[1], "cleat")] {
        let name = format!("checkout-convoy-multi-docker-{slug}");
        let path = format!("/Users/alice/dev/flotilla-repos/workspace-multi-docker/{slug}");
        let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
        let created = checkouts
            .create(
                &meta(&name),
                &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                    repo_ref: repository.key(),
                    env_ref: host_direct_env_name(),
                    r#ref: "feature/multi".to_string(),
                    base_ref: Some("main".to_string()),
                    target_path: path.clone(),
                    clone_ref: format!("clone-{slug}"),
                }),
            )
            .await
            .expect("checkout should create");
        checkouts
            .update_status(&name, &created.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: Some(path),
                commit: Some("44982740".to_string()),
                branch_provenance: Default::default(),
                integration: Default::default(),
                message: None,
            })
            .await
            .expect("checkout should become ready");
    }
    let vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-multi-docker"), &VesselSpec {
            convoy_ref: "convoy-multi-docker".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-multi-docker".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel should create");

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let deps = reconciler.fetch_dependencies(&vessel).await.expect("deps should load");
    let outcome = reconciler.reconcile(&vessel, &deps, Utc::now());
    let mounts = outcome.actuations.iter().find_map(|actuation| match actuation {
        Actuation::CreateEnvironment { spec, .. } => spec.docker.as_ref().map(|docker| docker.mounts.as_slice()),
        _ => None,
    });
    let mounts = mounts.expect("docker environment should be created");
    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source_path, "/Users/alice/dev/flotilla-repos/convoy-multi-docker/feature-multi");
    assert_eq!(mounts[0].target_path, "/workspace");

    let mixed = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-multi-mixed"), &VesselSpec {
            convoy_ref: "convoy-multi-docker".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-multi-docker".to_string(),
            adopted_checkout_refs: BTreeMap::from([(repositories[0].key(), "checkout-workspace-multi-docker-flotilla".to_string())]),
        })
        .await
        .expect("mixed vessel should create");
    let deps = reconciler.fetch_dependencies(&mixed).await.expect("mixed deps should load");
    let outcome = reconciler.reconcile(&mixed, &deps, Utc::now());
    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkFailed { message })
            if message == "adopted checkouts are not supported for multi-repository vessel workspaces"
    ));
}

#[tokio::test]
async fn multi_repository_docker_fresh_clone_uses_per_repository_paths() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repositories = [
        RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository"),
        RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository"),
    ];
    for repository in &repositories {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(&meta("convoy-multi-fresh"), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: vec![
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: repositories[0].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                },
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/cleat".to_string(),
                    repo_ref: repositories[1].key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "cleat".to_string(),
                    subpaths: Vec::new(),
                },
            ],
            r#ref: Some("feature/multi".to_string()),
            project_ref: Some("flotilla-suite".to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            issues: Vec::new(),
            instruction: None,
        })
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-multi-fresh", &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: "implement".to_string(),
                    stance: Stance::Contained,
                    depends_on: Vec::new(),
                    repository_refs: None,
                    crew: Vec::new(),
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status should update");
    create_policy(
        &backend,
        NAMESPACE,
        "policy-multi-fresh",
        PlacementPolicySpec::builder()
            .pool("cleat".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                host_ref: HOST_REF.to_string(),
                image: "ghcr.io/flotilla/dev:latest".to_string(),
                agent_adapters: Default::default(),
                default_cwd: None,
                env: Default::default(),
                checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace/".to_string() },
            })
            .build(),
    )
    .await;
    create_ready_docker_environment(&backend, NAMESPACE, "env-workspace-multi-fresh", DockerEnvironmentSpec {
        host_ref: HOST_REF.to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        mounts: Vec::new(),
        env: Default::default(),
    })
    .await;
    let vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-multi-fresh"), &VesselSpec {
            convoy_ref: "convoy-multi-fresh".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-multi-fresh".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel should create");

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&vessel).await.expect("deps should load");
    let outcome = reconciler.reconcile(&vessel, &deps, Utc::now());
    let checkout_paths = outcome
        .actuations
        .iter()
        .filter_map(|actuation| match actuation {
            Actuation::CreateCheckout { spec: CheckoutSpec::FreshClone(spec), .. } => Some(spec.target_path.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkout_paths, ["/workspace/flotilla", "/workspace/cleat"]);
}

#[tokio::test]
async fn vessel_repository_scope_narrows_a_multi_repository_convoy() {
    let backend = ResourceBackend::InMemory(Default::default());
    let flotilla = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository");
    let cleat = RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository");
    for repository in [&flotilla, &cleat] {
        ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository.key(), repository)
            .await
            .expect("repository should create");
    }
    let convoy = backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .create(&meta("convoy-scoped"), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: vec![
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/cleat".to_string(),
                    repo_ref: cleat.key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "cleat".to_string(),
                    subpaths: Vec::new(),
                },
                ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla".to_string(),
                    repo_ref: flotilla.key(),
                    base_ref: "main".to_string(),
                    workspace_slug: "flotilla".to_string(),
                    subpaths: Vec::new(),
                },
            ],
            r#ref: Some("feature/scoped".to_string()),
            project_ref: Some("flotilla-suite".to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            issues: Vec::new(),
            instruction: None,
        })
        .await
        .expect("convoy should create");
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-scoped", &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![VesselRequirement {
                    name: "implement".to_string(),
                    stance: Stance::Trusted,
                    depends_on: Vec::new(),
                    repository_refs: Some(vec![cleat.key()]),
                    crew: Vec::new(),
                }],
            }),
            ..Default::default()
        })
        .await
        .expect("convoy status should update");
    create_host_direct_policy(&backend, NAMESPACE, "policy-scoped", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    let vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-scoped"), &VesselSpec {
            convoy_ref: "convoy-scoped".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-scoped".to_string(),
            adopted_checkout_refs: BTreeMap::new(),
        })
        .await
        .expect("vessel should create");

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let deps = reconciler.fetch_dependencies(&vessel).await.expect("deps should load");
    let outcome = reconciler.reconcile(&vessel, &deps, Utc::now());
    assert_eq!(outcome.actuations.iter().filter(|actuation| matches!(actuation, Actuation::CreateClone { .. })).count(), 1);
    let (checkout_name, checkout) = outcome
        .actuations
        .iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateCheckout { meta, spec } => Some((meta.name.as_str(), spec)),
            _ => None,
        })
        .expect("scoped checkout should be created");
    assert_eq!(checkout_name, "checkout-convoy-scoped-cleat");
    assert_eq!(checkout.repo_ref(), &cleat.key());
    assert_eq!(checkout.target_path(), Some("/Users/alice/dev/flotilla-repos/convoy-scoped/feature-scoped/cleat"));

    let adopted_path = "/Users/alice/dev/cleat-existing";
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let adopted = checkouts
        .create(
            &meta("adopted-cleat-scoped").with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "feature/scoped".to_string(),
                path: adopted_path.to_string(),
                repo_ref: cleat.key(),
                host_ref: HOST_REF.to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("adopted checkout should create");
    checkouts
        .update_status("adopted-cleat-scoped", &adopted.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some(adopted_path.to_string()),
            commit: Some("abc123".to_string()),
            branch_provenance: Default::default(),
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("adopted checkout should become ready");
    let adopted_vessel = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&meta("workspace-scoped-adopted"), &VesselSpec {
            convoy_ref: "convoy-scoped".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-scoped".to_string(),
            adopted_checkout_refs: BTreeMap::from([(cleat.key(), "adopted-cleat-scoped".to_string())]),
        })
        .await
        .expect("adopted vessel should create");

    let deps = reconciler.fetch_dependencies(&adopted_vessel).await.expect("adopted deps should load");
    let outcome = reconciler.reconcile(&adopted_vessel, &deps, Utc::now());
    assert!(outcome
        .actuations
        .iter()
        .all(|actuation| !matches!(actuation, Actuation::CreateClone { .. } | Actuation::CreateCheckout { .. })));
    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkReady { checkout_refs, .. })
            if checkout_refs == BTreeMap::from([(cleat.key(), "adopted-cleat-scoped".to_string())])
    ));
}

#[tokio::test]
async fn contained_requirement_runs_in_contained_docker_placement() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-docker-stance", "implement", REPO_URL, GIT_REF).await;
    let mut status = convoy.status.expect("convoy should have status");
    status.workflow_snapshot.as_mut().expect("convoy should have workflow snapshot").vessels[0].stance = Stance::Contained;
    backend
        .clone()
        .using::<Convoy>(NAMESPACE)
        .update_status("convoy-docker-stance", &convoy.metadata.resource_version, &status)
        .await
        .expect("convoy status should update");
    create_policy(
        &backend,
        NAMESPACE,
        "policy-docker-stance",
        PlacementPolicySpec::builder()
            .pool("cleat".to_string())
            .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                host_ref: HOST_REF.to_string(),
                image: "ghcr.io/flotilla/dev:latest".to_string(),
                agent_adapters: Default::default(),
                default_cwd: None,
                env: Default::default(),
                checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".to_string() },
            })
            .build(),
    )
    .await;
    let environment_ref = "env-workspace-docker-stance";
    create_ready_docker_environment(&backend, NAMESPACE, environment_ref, DockerEnvironmentSpec {
        host_ref: HOST_REF.to_string(),
        image: "ghcr.io/flotilla/dev:latest".to_string(),
        mounts: Vec::new(),
        env: Default::default(),
    })
    .await;
    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-workspace-docker-stance".to_string())
            .env_ref(environment_ref.to_string())
            .git_ref(GIT_REF.to_string())
            .path("/workspace".to_string())
            .maybe_fresh_clone(Some(fresh_clone_checkout_spec(environment_ref, GIT_REF, "/workspace", REPO_URL)))
            .build(),
    )
    .await;
    create_running_terminal(
        &backend,
        NAMESPACE,
        "terminal-workspace-docker-stance-coder",
        environment_ref,
        "coder",
        "cargo test",
        "/workspace",
        "cleat",
    )
    .await;
    let workspace = create_workspace(
        &backend,
        NAMESPACE,
        "workspace-docker-stance",
        "convoy-docker-stance",
        "implement",
        "policy-docker-stance",
        REPO_URL,
    )
    .await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkReady {
            requested_stance: Stance::Contained,
            effective_stance: Stance::Contained,
            ..
        })
    ));
}

#[tokio::test]
async fn missing_placement_policy_marks_workspace_failed() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", REPO_URL, GIT_REF).await;
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "policy-missing", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkFailed { ref message })
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

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
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

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());
    assert!(outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateCheckout { .. })));
    assert!(outcome.actuations.iter().all(|actuation| !matches!(actuation, Actuation::CreateEnvironment { .. })));

    create_ready_checkout(
        &backend,
        NAMESPACE,
        ReadyCheckoutFixture::builder()
            .name("checkout-convoy-c".to_string())
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
    let current = backend.clone().using::<Vessel>(NAMESPACE).get("workspace-c").await.expect("workspace get should succeed");
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
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: HOST_REF.to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            agent_adapters: Default::default(),
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
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: HOST_REF.to_string(),
            image: "ghcr.io/flotilla/dev:latest".to_string(),
            agent_adapters: Default::default(),
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
            .name("checkout-convoy-f".to_string())
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

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, chrono::Utc::now());

    assert!(matches!(
        outcome.patch,
        Some(flotilla_resources::VesselStatusPatch::MarkFailed { ref message }) if message == "boom"
    ));
}

#[tokio::test]
async fn adopted_checkout_ref_reuses_checkout_without_creating_clone_or_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-adopted", "implement", REPO_URL, GIT_REF).await;
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    create_host_direct_policy(&backend, NAMESPACE, "policy-adopted", HOST_REF, "cleat").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, HOST_REF, "/Users/alice/dev/flotilla-repos").await;
    create_ready_adopted_checkout(&backend, NAMESPACE, "adopted-checkout-convoy-adopted", "/Users/alice/dev/flotilla-existing").await;
    let workspace = backend
        .clone()
        .using::<Vessel>(NAMESPACE)
        .create(&vessel_meta("workspace-adopted", REPO_URL), &VesselSpec {
            convoy_ref: "convoy-adopted".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-adopted".to_string(),
            adopted_checkout_refs: BTreeMap::from([(repo_ref, "adopted-checkout-convoy-adopted".to_string())]),
        })
        .await
        .expect("workspace create should succeed");

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
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
    let repo_ref = convoy.spec.repositories[0].repo_ref.clone();
    let mut status = convoy.status.expect("convoy status");
    status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0].crew = vec![
        CrewSpec::builder()
            .role("coder".to_string())
            .source(CrewSource::Agent {
                selector: Selector { capability: "coding".to_string() },
                prompt: Some("Implement issue 668.".to_string()),
            })
            .build(),
        CrewSpec::builder()
            .role("reviewer".to_string())
            .source(CrewSource::Agent {
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
        .using::<Vessel>(NAMESPACE)
        .create(&vessel_meta("workspace-crew", REPO_URL), &VesselSpec {
            convoy_ref: "convoy-crew".to_string(),
            vessel_name: "implement".to_string(),
            placement_policy_ref: "policy-crew".to_string(),
            adopted_checkout_refs: BTreeMap::from([(repo_ref, "adopted-checkout-convoy-crew".to_string())]),
        })
        .await
        .expect("workspace create");

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
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
    assert!(brief.content.contains("flotilla crew complete"));
    assert!(brief.content.contains("## Assignment\n\nImplement issue 668.\n"));
    assert_eq!(context.namespace, NAMESPACE);
    assert_eq!(context.vessel_ref, "workspace-crew");
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
    create_ready_observed_checkout_without_status_path(&backend, NAMESPACE, "checkout-convoy-observed").await;
    let workspace =
        create_workspace(&backend, NAMESPACE, "workspace-observed", "convoy-observed", "implement", "policy-observed", REPO_URL).await;

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
    let deps = reconciler.fetch_dependencies(&workspace).await.expect("deps should load");
    let outcome = reconciler.reconcile(&workspace, &deps, Utc::now());

    assert!(
        matches!(
            outcome.patch,
            Some(flotilla_resources::VesselStatusPatch::MarkFailed { ref message })
                if message == "checkout checkout-convoy-observed is ready but has no target path"
        ),
        "unexpected patch: {:?}",
        outcome.patch
    );
}

#[tokio::test]
async fn run_finalizer_deletes_all_labeled_children() {
    let backend = ResourceBackend::InMemory(Default::default());
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-finalize", "convoy-a", "implement", "policy-a", REPO_URL).await;

    create_labeled_environment(&backend, NAMESPACE, "env-workspace-finalize", "workspace-finalize").await;
    create_labeled_checkout(&backend, NAMESPACE, "checkout-workspace-finalize", "workspace-finalize").await;
    create_labeled_terminal(&backend, NAMESPACE, "terminal-workspace-finalize-coder", "workspace-finalize").await;

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
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
async fn completed_convoy_does_not_repeat_vessel_delete_while_its_finalizer_is_pending() {
    let backend = ResourceBackend::InMemory(Default::default());
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    let terminals = backend.clone().using::<TerminalSession>(NAMESPACE);
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-finalizer", "implement", REPO_URL, GIT_REF).await;

    let mut status = convoy.status.expect("convoy status");
    status.phase = ConvoyPhase::Completed;
    status.observed_workflow_ref = Some("wf".to_string());
    status.work.insert(
        "implement".to_string(),
        work_state().phase(WorkPhase::Complete).finished_at(Utc::now()).message("done".to_string()).call(),
    );
    convoys
        .update_status("convoy-finalizer", &convoy.metadata.resource_version, &status)
        .await
        .expect("convoy completion should be recorded");

    let vessel =
        create_workspace(&backend, NAMESPACE, "convoy-finalizer-implement", "convoy-finalizer", "implement", "policy-finalizer", REPO_URL)
            .await;
    let mut vessel_meta = InputMeta::from(&vessel.metadata);
    vessel_meta.labels.insert(CONVOY_LABEL.to_string(), "convoy-finalizer".to_string());
    vessel_meta.finalizers = vec!["flotilla.work/vessel-workspace-teardown".to_string()];
    vessels
        .update(&vessel_meta, &vessel.metadata.resource_version, &vessel.spec)
        .await
        .expect("vessel finalizer and convoy label should be recorded");
    create_labeled_terminal(&backend, NAMESPACE, "terminal-convoy-finalizer-implement-coder", "convoy-finalizer-implement").await;

    let convoy_reconciler = ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>(NAMESPACE)).with_vessels(vessels.clone());
    let completed = convoys.get("convoy-finalizer").await.expect("completed convoy should exist");
    let first_dependencies = convoy_reconciler.fetch_dependencies(&completed).await.expect("first dependencies should load");
    let first_outcome = convoy_reconciler.reconcile(&completed, &first_dependencies, Utc::now());
    assert!(first_outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-finalizer-implement")));

    vessels.delete("convoy-finalizer-implement").await.expect("first convoy cleanup should mark the vessel for deletion");
    let pending_vessel = vessels.get("convoy-finalizer-implement").await.expect("pending finalizer should retain the vessel");
    assert!(pending_vessel.metadata.deletion_timestamp.is_some());

    let second_dependencies = convoy_reconciler.fetch_dependencies(&completed).await.expect("second dependencies should load");
    let second_outcome = convoy_reconciler.reconcile(&completed, &second_dependencies, Utc::now());
    assert!(
        !second_outcome
            .actuations
            .iter()
            .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-finalizer-implement")),
        "a vessel already awaiting finalization should not be deleted again"
    );

    let vessel_reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
    vessel_reconciler.run_finalizer(&pending_vessel).await.expect("vessel finalizer should delete terminal children");
    let clear_finalizer = InputMeta::from(&pending_vessel.metadata).without_finalizer("flotilla.work/vessel-workspace-teardown");
    vessels
        .update(&clear_finalizer, &pending_vessel.metadata.resource_version, &pending_vessel.spec)
        .await
        .expect("clearing the vessel finalizer should complete deletion");

    assert!(matches!(vessels.get("convoy-finalizer-implement").await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(terminals.get("terminal-convoy-finalizer-implement-coder").await, Err(ResourceError::NotFound { .. })));
}

#[tokio::test]
async fn run_finalizer_preserves_adopted_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
    let workspace = create_workspace(&backend, NAMESPACE, "workspace-adopted", "convoy-a", "implement", "policy-a", REPO_URL).await;

    create_labeled_adopted_checkout(&backend, NAMESPACE, "checkout-workspace-adopted", "workspace-adopted").await;
    create_labeled_terminal(&backend, NAMESPACE, "terminal-workspace-adopted-coder", "workspace-adopted").await;

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
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

    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE);
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
            .name("checkout-convoy-labels".to_string())
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

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
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
    assert_eq!(terminal.0.labels.get(VESSEL_LABEL).map(String::as_str), Some("review"));
    assert_eq!(terminal.0.labels.get(VESSEL_REF_LABEL).map(String::as_str), Some("workspace-labels"));
    assert_eq!(terminal.0.labels.get(ROLE_LABEL).map(String::as_str), Some("test"));
    assert_eq!(terminal.0.labels.get(VESSEL_ORDINAL_LABEL).map(String::as_str), Some("001"));
    assert_eq!(terminal.0.labels.get(CREW_ORDINAL_LABEL).map(String::as_str), Some("001"));
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
            .name(if checkout_path == "/workspace" && workspace_name == "workspace-docker-fresh" {
                format!("checkout-{workspace_name}")
            } else {
                "checkout-convoy-cwd".to_string()
            })
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

    let reconciler = VesselReconciler::new(backend, NAMESPACE);
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
        repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key(
            &canonicalize_repo_url(REPO_URL).expect("repo canonicalization"),
        )),
        env_ref: env_ref.to_string(),
        r#ref: git_ref.to_string(),
        base_ref: None,
        target_path: target_path.to_string(),
        clone_ref: clone_ref.to_string(),
    }
}

fn fresh_clone_checkout_spec(env_ref: &str, git_ref: &str, target_path: &str, url: &str) -> flotilla_resources::FreshCloneCheckoutSpec {
    flotilla_resources::FreshCloneCheckoutSpec {
        repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key(
            &canonicalize_repo_url(REPO_URL).expect("repo canonicalization"),
        )),
        env_ref: env_ref.to_string(),
        r#ref: git_ref.to_string(),
        base_ref: None,
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
    let repository_spec = RepositorySpec::remote(repo_url).expect("repository URL should be canonical");
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
        .await
        .expect("repository create should succeed");
    let convoys = backend.clone().using::<Convoy>(namespace);
    let convoy = convoys
        .create(&meta(name), &ConvoySpec {
            workflow_ref: "wf".to_string(),
            inputs: Default::default(),
            placement_policy: None,
            repositories: vec![ConvoyRepositorySpec {
                url: repo_url.to_string(),
                repo_ref: repository_key,
                base_ref: git_ref.to_string(),
                workspace_slug: repository_spec.leaf_slug(),
                subpaths: Vec::new(),
            }],
            r#ref: Some(git_ref.to_string()),
            project_ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            issues: Vec::new(),
            instruction: None,
        })
        .await
        .expect("convoy create should succeed");
    convoys
        .update_status(name, &convoy.metadata.resource_version, &ConvoyStatus {
            workflow_snapshot: Some(WorkflowSnapshot {
                vessels: vec![
                    VesselRequirement {
                        name: "implement".to_string(),
                        stance: Default::default(),
                        depends_on: Vec::new(),
                        repository_refs: None,
                        crew: vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "cargo fmt --check".to_string() })
                            .build()],
                    },
                    VesselRequirement {
                        name: "review".to_string(),
                        stance: Default::default(),
                        depends_on: vec!["implement".to_string()],
                        repository_refs: None,
                        crew: vec![
                            CrewSpec::builder()
                                .role("build".to_string())
                                .source(CrewSource::Tool { command: "cargo check".to_string() })
                                .build(),
                            CrewSpec::builder()
                                .role("test".to_string())
                                .source(CrewSource::Tool { command: "cargo test".to_string() })
                                .labels(BTreeMap::from([
                                    ("service".to_string(), "api".to_string()),
                                    ("team".to_string(), "platform".to_string()),
                                    (CONVOY_LABEL.to_string(), "wrong-convoy".to_string()),
                                    (VESSEL_LABEL.to_string(), "wrong-task".to_string()),
                                    (VESSEL_REF_LABEL.to_string(), "wrong-workspace".to_string()),
                                    (ROLE_LABEL.to_string(), "wrong-role".to_string()),
                                    (VESSEL_ORDINAL_LABEL.to_string(), "999".to_string()),
                                    (CREW_ORDINAL_LABEL.to_string(), "999".to_string()),
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
        .create(&labeled_meta(name, [(VESSEL_REF_LABEL.to_string(), workspace_name.to_string())]), &EnvironmentSpec {
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
            &labeled_meta(name, [(VESSEL_REF_LABEL.to_string(), workspace_name.to_string())]),
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
            &labeled_meta(name, [(VESSEL_REF_LABEL.to_string(), workspace_name.to_string())])
                .with_lifecycle_authority(LifecycleAuthority::Adopted),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: GIT_REF.to_string(),
                path: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
                repo_ref: RepositorySpec::remote(REPO_URL).expect("repository URL").key(),
                host_ref: HOST_REF.to_string(),
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
                repo_ref: RepositorySpec::remote(REPO_URL).expect("repository URL").key(),
                host_ref: HOST_REF.to_string(),
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
            branch_provenance: Default::default(),
            integration: Default::default(),
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
                repo_ref: RepositorySpec::remote(REPO_URL).expect("repository URL").key(),
                host_ref: HOST_REF.to_string(),
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
            branch_provenance: Default::default(),
            integration: Default::default(),
            message: None,
        })
        .await
        .expect("checkout status update should succeed");
}

async fn create_labeled_terminal(backend: &ResourceBackend, namespace: &str, name: &str, workspace_name: &str) {
    backend
        .clone()
        .using::<TerminalSession>(namespace)
        .create(&labeled_meta(name, [(VESSEL_REF_LABEL.to_string(), workspace_name.to_string())]), &TerminalSessionSpec {
            env_ref: host_direct_env_name(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: format!("/Users/alice/dev/flotilla-repos/{workspace_name}"),
            pool: "cleat".to_string(),
        })
        .await
        .expect("terminal create should succeed");
}
