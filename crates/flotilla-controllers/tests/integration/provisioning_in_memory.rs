use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{
    controller_meta, create_convoy_with_single_task, create_host_direct_kind, create_ready_host_direct_environment, create_workspace,
    ControllerLoopHarness,
};
use flotilla_crew::reconcilers::terminal_session::{TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler};
use flotilla_crew::reconcilers::vessel::VesselReconciler;
use flotilla_protocol::ConfiguredResourceLimits;
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::{engine::checkouts::CheckoutVesselWake, reconcilers::CloneRuntime};
use flotilla_provisioning_testkit::{
    clones::{MadeCheckouts, RunningClones},
    EnvironmentFixture,
};
use flotilla_resources::checkout_records::{CheckoutHolder, CheckoutRecords as _};
use flotilla_resources::clone_records::CloneRecords;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::{
    canonicalize_repo_url, CheckoutSpec, CheckoutWorktreeSpec, CloneSpec, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus,
    CrewSource, CrewSpec, Host, HostSpec, HostStatus, InputMeta, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec,
    Repository, RepositorySpec, ResourceError, TerminalSession, TerminalSessionPhase, TerminalSessionTarget, Vessel, VesselPhase,
    VesselRequirement, VESSEL_REF_LABEL,
};
use flotilla_store::{
    controller::{ControllerLoop, Reconciler},
    ResourceBackend,
};

use crate::common;

const NAMESPACE: &str = "flotilla";

#[derive(Default)]
struct FakeCloneRuntime;

#[async_trait]
impl CloneRuntime for FakeCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

/// Refuses the first clone as the forge refused a stale credential, then clones as `FakeCloneRuntime`.
#[derive(Default)]
struct RefusingFirstCloneRuntime {
    refused: AtomicBool,
}

#[async_trait]
impl CloneRuntime for RefusingFirstCloneRuntime {
    async fn clone_and_inspect(&self, _repo_url: &str, _target_path: &str) -> Result<Option<String>, String> {
        if self.refused.swap(true, Ordering::SeqCst) {
            Ok(Some("main".to_string()))
        } else {
            Err("authentication failed: repository access denied".to_string())
        }
    }

    async fn inspect_existing(&self, _target_path: &str) -> Result<Option<String>, String> {
        Ok(Some("main".to_string()))
    }
}

#[derive(Default)]
struct FakeTerminalRuntime;

#[async_trait]
impl TerminalRuntime for FakeTerminalRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        _spec: TerminalSessionTarget<'_>,
        _tags: &[flotilla_terminal::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        Ok(TerminalRuntimeState {
            configured_limits: Some(ConfiguredResourceLimits {
                cpus: None,
                cpu_shares: None,
                build_jobs: Some(4),
                linker_threads: Some(4),
            }),
            session_id: format!("session-{name}"),
            pid: Some(42),
            started_at: Utc::now(),
            crew: None,
            launch_command: "bash".to_string(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: TerminalSessionTarget<'_>) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn controller_loops_drive_host_direct_workspace_to_ready() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-a", "01HXYZ", "cleat").await;
    let repository_url = "git@github.com:flotilla-org/flotilla.git";
    create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", repository_url, "feat/task-provisioning").await;
    let repository_spec = RepositorySpec::remote(canonicalize_repo_url(repository_url).expect("repository URL should canonicalize"))
        .expect("repository spec");
    backend
        .clone()
        .using::<Repository>(NAMESPACE)
        .delete(&repository_spec.key().to_string())
        .await
        .expect("destination should start without the convoy repository");
    create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "kind-a", repository_url).await;

    engine.checkouts.ensure_convoy("convoy-a");
    let harness = full_controller_harness(backend.clone(), &engine);

    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            let workspaces = workspaces.clone();
            async move {
                matches!(
                    workspaces.get("workspace-a").await.ok().and_then(|workspace| workspace.status).map(|status| status.phase),
                    Some(VesselPhase::Ready)
                )
            }
        })
        .await;

    let workspace = workspaces.get("workspace-a").await.expect("workspace get should succeed");
    let ready_resource_version = workspace.metadata.resource_version.clone();
    let status = workspace.status.expect("workspace status should be present");
    // #2650: host-direct configured limits survive launch into Vessel status;
    // no container quota is invented for a host-direct vessel.
    assert_eq!(
        status.configured_limits,
        Some(ConfiguredResourceLimits { cpus: None, cpu_shares: None, build_jobs: Some(4), linker_threads: Some(4) })
    );
    assert_eq!(status.phase, VesselPhase::Ready);
    assert_eq!(status.environment_ref.as_deref(), Some("host-direct-01HXYZ"));
    assert_eq!(status.checkout_refs.values().next().map(String::as_str), Some("checkout-convoy-a"));
    assert_eq!(status.terminal_session_refs, vec!["terminal-workspace-a-coder".to_string()]);
    backend
        .clone()
        .using::<Repository>(NAMESPACE)
        .get(&repository_spec.key().to_string())
        .await
        .expect("missing repository should be materialized");

    tokio::time::sleep(Duration::from_millis(200)).await;
    let steady = workspaces.get("workspace-a").await.expect("ready workspace should remain readable");
    assert_eq!(
        steady.metadata.resource_version, ready_resource_version,
        "steady-state reconciliation must not create new resource versions"
    );
    assert_eq!(steady.status.expect("steady workspace status").ready_at, status.ready_at);

    harness.shutdown().await;
}

#[tokio::test]
async fn a_project_gaining_a_member_leaves_a_running_convoys_checkouts_unchanged() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-a", "01HXYZ", "cleat").await;
    let repository_url = "https://github.com/flotilla-org/flotilla";
    let convoy = create_convoy_with_single_task(&backend, NAMESPACE, "convoy-a", "implement", repository_url, "fix/members").await;
    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let mut spec = convoy.spec.clone();
    spec.project_ref = Some("suite".to_string());
    convoys.update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &spec).await.expect("bind project");
    let admitted = convoy.spec.repositories.clone();
    let projects = backend.clone().using::<Project>(NAMESPACE);
    let member = |repo| {
        ProjectRepositorySpec::builder().repo(repo).roles([ProjectRepositoryRole::Code].into()).default_branch("main".into()).build()
    };
    projects
        .create(
            &controller_meta().name("suite").call(),
            &ProjectSpec::builder().display_name("Suite".into()).repositories(vec![member(admitted[0].repo_ref.clone())]).build(),
        )
        .await
        .expect("project create");
    create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "kind-a", repository_url).await;

    engine.checkouts.ensure_convoy("convoy-a");
    let harness = full_controller_harness(backend.clone(), &engine);
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            let vessels = vessels.clone();
            async move {
                matches!(
                    vessels.get("workspace-a").await.ok().and_then(|vessel| vessel.status).map(|status| status.phase),
                    Some(VesselPhase::Ready)
                )
            }
        })
        .await;
    let checkout_launches =
        || engine.checkouts.checkouts.here().expect("checkouts").into_iter().map(|checkout| checkout.name).collect::<Vec<_>>();
    let before = checkout_launches();
    assert_eq!(before, ["checkout-convoy-a#1"]);

    let added = RepositorySpec::remote("https://github.com/flotilla-org/binnacle").expect("added repository");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &added.key(), &added)
        .await
        .expect("added repository");
    let project = projects.get("suite").await.expect("project");
    let mut grown = project.spec.clone();
    grown.repositories.push(member(added.key()));
    projects.update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &grown).await.expect("project gains a member");

    let vessel = vessels.get("workspace-a").await.expect("vessel");
    let reconciler = VesselReconciler::new(backend.clone(), NAMESPACE).with_lifecycle_reads(engine.reads.clone());
    reconciler.reconcile(&vessel, &reconciler.prepare(&vessel).await.expect("vessel dependencies"), Utc::now());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(checkout_launches(), before, "no checkout is requested for the new member");
    assert_eq!(convoys.get("convoy-a").await.expect("convoy").spec.repositories, admitted);

    harness.shutdown().await;
}

#[tokio::test]
async fn controller_materializes_a_missing_repository_for_a_multi_repository_convoy() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_ready_host(&backend, "01HXYZ").await;
    let engine = RunningClones::start_with_checkouts("01HXYZ", Arc::new(FakeCloneRuntime), Arc::new(MadeCheckouts::default()));
    let environments = &engine.environments;
    create_ready_host_direct_environment(environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    create_host_direct_kind(&backend, NAMESPACE, "kind-multi", "01HXYZ", "cleat").await;

    let known = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("known repository");
    let missing = RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("missing repository");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &known.key(), &known)
        .await
        .expect("known repository should create");

    let convoys = backend.clone().using::<Convoy>(NAMESPACE);
    let convoy = convoys
        .create(
            &controller_meta().name("convoy-multi").call(),
            &ConvoySpec {
                continuation: None,
                subjects: Vec::new(),
                role: String::new(),
                generation: 1,
                workflow_ref: "wf".to_string(),
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: None,
                repositories: vec![
                    ConvoyRepositorySpec::builder()
                        .url("https://github.com/flotilla-org/flotilla".to_string())
                        .repo_ref(known.key())
                        .source_ref("main".to_string())
                        .target_ref("main".to_string())
                        .workspace_slug("flotilla".to_string())
                        .subpaths(Vec::new())
                        .build(),
                    ConvoyRepositorySpec::builder()
                        .url("https://github.com/flotilla-org/cleat".to_string())
                        .repo_ref(missing.key())
                        .source_ref("main".to_string())
                        .target_ref("main".to_string())
                        .workspace_slug("cleat".to_string())
                        .subpaths(Vec::new())
                        .build(),
                ],
                r#ref: Some("fix/multi".to_string()),
                project_ref: Some("flotilla-suite".to_string()),
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
                promises: Vec::new(),
            },
        )
        .await
        .expect("convoy should create");
    convoys
        .update_status(
            "convoy-multi",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![VesselRequirement {
                        needs: Default::default(),
                        name: "implement".to_string(),
                        depends_on: Vec::new(),
                        repository_refs: None,
                        credential_refs: Default::default(),
                        credential_scopes: Default::default(),
                        credential_permissions: Default::default(),
                        crew: vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "cargo test".to_string() })
                            .build()],
                    }],
                }),
                ..Default::default()
            },
        )
        .await
        .expect("convoy status should update");
    create_workspace(
        &backend,
        NAMESPACE,
        "workspace-multi",
        "convoy-multi",
        "implement",
        "kind-multi",
        "https://github.com/flotilla-org/flotilla",
    )
    .await;

    engine.checkouts.ensure_convoy("convoy-multi");
    let harness = full_controller_harness(backend.clone(), &engine);
    let vessels = backend.clone().using::<Vessel>(NAMESPACE);
    harness
        .wait_until(Duration::from_secs(3), || {
            let vessels = vessels.clone();
            async move {
                matches!(
                    vessels.get("workspace-multi").await.ok().and_then(|vessel| vessel.status).map(|status| status.phase),
                    Some(VesselPhase::Ready)
                )
            }
        })
        .await;

    let repositories = backend.clone().using::<Repository>(NAMESPACE);
    repositories.get(&known.key().to_string()).await.expect("known repository should remain registered");
    repositories.get(&missing.key().to_string()).await.expect("missing repository should be materialized");
    let clones = engine.clones.here().expect("clones should read");
    assert_eq!(clones.len(), 2);
    assert!(clones.iter().all(|clone| clone.is_ready()), "{clones:?}");

    harness.shutdown().await;
}

#[tokio::test]
async fn clone_controller_marks_clone_ready() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let engine = RunningClones::start("01HXYZ", Arc::new(FakeCloneRuntime));
    create_ready_host_direct_environment(&engine.environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    let launch = engine
        .clones
        .request(
            &repository_spec,
            &CloneSpec {
                repo_ref: repository_spec.key(),
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-01HXYZ".to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
            false,
        )
        .expect("clone request should succeed");

    let harness = ControllerLoopHarness::new(backend);
    harness
        .wait_until(Duration::from_secs(1), || {
            let ready = engine.clones.get(&launch, None).ok().flatten().is_some_and(|clone| clone.is_ready());
            async move { ready }
        })
        .await;

    let clone = engine.clones.get(&launch, None).expect("clone read should succeed").expect("clone held");
    assert_eq!(clone.reading, LifecycleReading::Ready { identity: "/Users/alice/dev/flotilla".to_string() });
    assert_eq!(clone.default_branch.as_deref(), Some("main"));

    harness.shutdown().await;
}

// Behaviour (#3108): a clone the forge refused gives up; a new checkout's demand for it requests
// its next launch, which clones again, and the checkout made from that launch becomes ready.
#[tokio::test]
async fn new_convoy_checkout_demand_redrives_a_clone_failed_on_old_auth() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let engine =
        RunningClones::start_with_checkouts("01HXYZ", Arc::new(RefusingFirstCloneRuntime::default()), Arc::new(MadeCheckouts::default()));
    create_ready_host_direct_environment(&engine.environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_spec.key(), &repository_spec)
        .await
        .expect("repository create should succeed");
    let clone_spec = CloneSpec {
        repo_ref: repository_spec.key(),
        url: "git@github.com:flotilla-org/flotilla.git".to_string(),
        env_ref: "host-direct-01HXYZ".to_string(),
        path: "/Users/alice/dev/flotilla".to_string(),
    };
    let refused = engine.clones.request(&repository_spec, &clone_spec, false).expect("clone request should succeed");
    let harness = ControllerLoopHarness::new(backend.clone());
    harness
        .wait_until(Duration::from_secs(3), || {
            let gave_up = matches!(
                engine.clones.get(&refused, None).ok().flatten().map(|clone| clone.reading),
                Some(LifecycleReading::GaveUp { cause }) if cause.contains("authentication failed")
            );
            async move { gave_up }
        })
        .await;

    let launch = engine.clones.request(&repository_spec, &clone_spec, false).expect("new demand should request the next launch");
    assert_ne!(launch, refused, "an ended launch is never revived");
    engine.checkouts.open_convoy("convoy-new-demand");
    let checkout = engine.checkouts.request(
        "checkout-new-demand",
        &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: repository_spec.key(),
            env_ref: "host-direct-01HXYZ".to_string(),
            r#ref: "fix/clone-failed-latch".to_string(),
            base_ref: Some("main".to_string()),
            target_path: "/Users/alice/dev/flotilla.fix-clone-failed-latch".to_string(),
            clone_ref: launch.clone(),
        }),
        Some(&CheckoutHolder::Convoy("convoy-new-demand".to_string())),
    );

    harness
        .wait_until(Duration::from_secs(3), || {
            let clone_ready = engine.clones.get(&launch, None).ok().flatten().is_some_and(|clone| clone.is_ready());
            let checkout_ready = engine.checkouts.checkouts.get(&checkout, None).ok().flatten().is_some_and(|checkout| checkout.is_ready());
            async move { clone_ready && checkout_ready }
        })
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn terminal_session_controller_marks_session_running() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let environments = EnvironmentFixture::default();
    create_ready_host_direct_environment(&environments, "01HXYZ", "/Users/alice/dev/flotilla-repos");
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    sessions
        .create(
            &controller_meta().name("term-a").call(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "host-direct-01HXYZ".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("session create should succeed");

    let harness = terminal_harness(backend.clone(), environments.reads.clone());
    harness
        .wait_until(Duration::from_secs(1), || {
            let sessions = sessions.clone();
            async move {
                matches!(
                    sessions.get("term-a").await.ok().and_then(|session| session.status),
                    Some(status)
                        if status.phase == TerminalSessionPhase::Running
                            && status.session_id.as_deref() == Some("session-term-a")
                            && status.pid == Some(42)
                )
            }
        })
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn vessel_controller_finalizer_deletes_vessel_owned_children_but_preserves_the_convoys_checkout() {
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    create_workspace(
        &backend,
        NAMESPACE,
        "workspace-delete",
        "convoy-delete",
        "implement",
        "policy-delete",
        "git@github.com:flotilla-org/flotilla.git",
    )
    .await;

    // The engine holds the convoy's checkout, realised.
    let engine = flotilla_provisioning_testkit::CheckoutFixture::default();
    engine.open_convoy("convoy-delete");
    let checkout = engine.put_ready(
        "checkout-convoy-delete",
        &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
            env_ref: "host-direct-01HXYZ".to_string(),
            r#ref: "feat/task-provisioning".to_string(),
            base_ref: None,
            target_path: "/Users/alice/dev/flotilla-repos/workspace-delete".to_string(),
            clone_ref: "clone-a".to_string(),
        }),
        Some(&CheckoutHolder::Convoy("convoy-delete".to_string())),
    );
    backend
        .clone()
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &controller_meta()
                .name("terminal-workspace-delete-coder")
                .labels([(VESSEL_REF_LABEL.to_string(), "workspace-delete".to_string())].into_iter().collect())
                .call(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "host-direct-01HXYZ".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/Users/alice/dev/flotilla-repos/workspace-delete".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal create should succeed");

    let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
    let terminals = backend.clone().using::<TerminalSession>(NAMESPACE);
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: workspaces.clone(),
            secondaries: VesselReconciler::secondary_watches(),
            reconciler: VesselReconciler::new(backend.clone(), NAMESPACE).with_lifecycle_reads(engine.reads.clone()),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(1), || {
            let workspaces = workspaces.clone();
            async move {
                matches!(
                    workspaces.get("workspace-delete").await,
                    Ok(workspace) if workspace.metadata.finalizers == vec!["flotilla.work/vessel-workspace-teardown".to_string()]
                )
            }
        })
        .await;

    workspaces.delete("workspace-delete").await.expect("workspace delete should succeed");

    harness
        .wait_until(Duration::from_secs(1), || {
            let workspaces = workspaces.clone();
            let terminals = terminals.clone();
            async move {
                matches!(workspaces.get("workspace-delete").await, Err(ResourceError::NotFound { .. }))
                    && matches!(terminals.get("terminal-workspace-delete-coder").await, Err(ResourceError::NotFound { .. }))
            }
        })
        .await;
    assert!(engine.record(&checkout).is_ready(), "the vessel's finalizer leaves the convoy's checkout to the convoy");

    harness.shutdown().await;
}

fn terminal_harness(backend: ResourceBackend, reads: Arc<dyn LifecycleReads>) -> ControllerLoopHarness {
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<TerminalSession>(NAMESPACE),
            secondaries: vec![],
            reconciler: TerminalSessionReconciler::new(Arc::new(FakeTerminalRuntime), backend.clone(), NAMESPACE)
                .with_lifecycle_reads(reads),
            resync_interval: Duration::from_millis(50),
            backend,
        }
        .run(),
    );
    harness
}

/// The old-store controllers over the engine's environments, clones and checkouts, which
/// `engine` serves; the engine's Clone and Checkout controllers run beside them, and a checkout's
/// change wakes its convoy's vessels.
fn full_controller_harness(backend: ResourceBackend, engine: &RunningClones) -> ControllerLoopHarness {
    let reads = engine.reads.clone();
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<TerminalSession>(NAMESPACE),
            secondaries: vec![],
            reconciler: TerminalSessionReconciler::new(Arc::new(FakeTerminalRuntime), backend.clone(), NAMESPACE)
                .with_lifecycle_reads(Arc::clone(&reads)),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );
    let mut secondaries = VesselReconciler::secondary_watches();
    secondaries.push(Box::new(CheckoutVesselWake::new(Arc::clone(&engine.service))));
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Vessel>(NAMESPACE),
            secondaries,
            reconciler: VesselReconciler::new(backend.clone(), NAMESPACE).with_lifecycle_reads(reads),
            // This must progress through checkout wakes rather than a periodic Vessel relist;
            // production resync is also deliberately slow.
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );
    harness
}

async fn create_ready_host(backend: &ResourceBackend, name: &str) {
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let created = hosts.create(&controller_meta().name(name).call(), &HostSpec::default()).await.expect("host create should succeed");
    hosts
        .update_status(
            name,
            &created.metadata.resource_version,
            &HostStatus {
                capabilities: Default::default(),
                heartbeat_at: Some(Utc::now()),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            },
        )
        .await
        .expect("host status update should succeed");
}
