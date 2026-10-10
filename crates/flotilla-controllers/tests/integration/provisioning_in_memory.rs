use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{
    controller_meta, create_convoy_with_single_task, create_host_direct_policy, create_ready_clone, create_ready_host_direct_environment,
    create_workspace, ControllerLoopHarness,
};
use flotilla_controllers::reconcilers::{
    checkout::CheckoutPrepared, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneReconciler,
    CloneRuntime, DockerEnvironmentRuntime, DockerProvisioning, EnvironmentReconciler, PreparedCheckout, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_protocol::ConfiguredResourceLimits;
use flotilla_resources::{
    canonicalize_repo_url, clone_key, Checkout, CheckoutBranchProvenance, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone,
    ClonePhase, CloneSpec, CloneStatus, Convoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec,
    DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, Host,
    HostDirectEnvironmentSpec, HostSpec, HostStatus, Repository, RepositorySpec, ResourceError, ResourceObject, TerminalSession,
    TerminalSessionPhase, Vessel, VesselPhase, VesselRequirement, VESSEL_REF_LABEL,
};
use flotilla_store::{
    controller::{ControllerLoop, ReconcileOutcome, Reconciler},
    ResourceBackend,
};

use crate::common;

const NAMESPACE: &str = "flotilla";

#[derive(Default)]
struct FakeDockerRuntime {
    destroyed: Mutex<Vec<String>>,
}

#[async_trait]
impl DockerEnvironmentRuntime for FakeDockerRuntime {
    async fn provision(&self, name: &str, spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        Ok(DockerProvisioning {
            configured_limits: Some(ConfiguredResourceLimits { cpus: Some(4), build_jobs: Some(4), linker_threads: Some(4) }),
            container_id: format!("container-{name}"),
            image_ref: spec.image.clone(),
            local_image_id: "sha256:test-image".to_string(),
            registry_digest: None,
        })
    }

    async fn destroy(&self, _environment_ref: &str, container_id: &str) -> Result<(), String> {
        self.destroyed.lock().expect("destroyed lock").push(container_id.to_string());
        Ok(())
    }
}

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

#[derive(Default)]
struct FakeCheckoutRuntime;

#[async_trait]
impl CheckoutRuntime for FakeCheckoutRuntime {
    async fn create_worktree(
        &self,
        _clone_path: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        Ok(PreparedCheckout { commit: Some("44982740".to_string()), branch_provenance: CheckoutBranchProvenance::CreatedForConvoy })
    }

    async fn create_fresh_clone(
        &self,
        _repo_url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        Ok(PreparedCheckout { commit: Some("44982740".to_string()), branch_provenance: CheckoutBranchProvenance::PreExisting })
    }

    async fn inspect_integration(
        &self,
        _checkout: &flotilla_resources::ResourceObject<Checkout>,
        _convoy: Option<&flotilla_resources::ResourceObject<Convoy>>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        Ok(flotilla_resources::CheckoutIntegrationStatus::default())
    }

    async fn remove_checkout(&self, _removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

#[derive(Default)]
struct CountingCheckoutRuntime {
    attempts: AtomicUsize,
}

#[async_trait]
impl CheckoutRuntime for CountingCheckoutRuntime {
    async fn create_worktree(
        &self,
        _clone_path: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(PreparedCheckout { commit: Some("44982740".to_string()), branch_provenance: CheckoutBranchProvenance::CreatedForConvoy })
    }

    async fn create_fresh_clone(
        &self,
        _repo_url: &str,
        _branch: &str,
        _base_ref: Option<&str>,
        _target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(PreparedCheckout { commit: Some("44982740".to_string()), branch_provenance: CheckoutBranchProvenance::PreExisting })
    }

    async fn inspect_integration(
        &self,
        _checkout: &ResourceObject<Checkout>,
        _convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        Ok(flotilla_resources::CheckoutIntegrationStatus::default())
    }

    async fn remove_checkout(&self, _removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

struct DropFirstCheckoutCompletion {
    inner: CheckoutReconciler<CountingCheckoutRuntime>,
    dropped: AtomicBool,
}

impl Reconciler for DropFirstCheckoutCompletion {
    type Resource = Checkout;
    type Prepared = CheckoutPrepared;

    async fn prepare(&self, obj: &ResourceObject<Checkout>) -> Result<Self::Prepared, ResourceError> {
        self.inner.prepare(obj).await
    }

    fn reconcile(&self, obj: &ResourceObject<Checkout>, deps: &Self::Prepared, now: chrono::DateTime<Utc>) -> ReconcileOutcome<Checkout> {
        let mut outcome = self.inner.reconcile(obj, deps, now);
        if !self.dropped.swap(true, Ordering::SeqCst) {
            outcome.patch = None;
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Checkout>) -> Result<(), ResourceError> {
        self.inner.run_finalizer(obj).await
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        self.inner.finalizer_name()
    }
}

#[derive(Default)]
struct FakeTerminalRuntime;

#[async_trait]
impl TerminalRuntime for FakeTerminalRuntime {
    async fn ensure_session(
        &self,
        name: &str,
        _spec: &flotilla_resources::TerminalSessionSpec,
        _tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        Ok(TerminalRuntimeState {
            configured_limits: Some(ConfiguredResourceLimits { cpus: None, build_jobs: Some(4), linker_threads: Some(4) }),
            session_id: format!("session-{name}"),
            pid: Some(42),
            started_at: Utc::now(),
            crew: None,
            launch_command: "bash".to_string(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str, _spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn controller_loops_drive_host_direct_workspace_to_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, "01HXYZ").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-a", "01HXYZ", "cleat").await;
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
    create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "policy-a", repository_url).await;

    let harness = full_controller_harness(backend.clone());

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
    assert_eq!(status.configured_limits, Some(ConfiguredResourceLimits { cpus: None, build_jobs: Some(4), linker_threads: Some(4) }));
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
async fn controller_materializes_a_missing_repository_for_a_multi_repository_convoy() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, "01HXYZ").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-multi", "01HXYZ", "cleat").await;

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
        "policy-multi",
        "https://github.com/flotilla-org/flotilla",
    )
    .await;

    let harness = full_controller_harness(backend.clone());
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
    let clones = backend.clone().using::<Clone>(NAMESPACE).list().await.expect("clones should list");
    assert_eq!(clones.items.len(), 2);
    assert!(clones.items.iter().all(|clone| clone.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Ready)));

    harness.shutdown().await;
}

#[tokio::test]
async fn clone_controller_marks_clone_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    let repository_spec = flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    flotilla_store::ensure_repository(
        &backend.clone().using::<flotilla_resources::Repository>(NAMESPACE),
        &repository_spec.key(),
        &repository_spec,
    )
    .await
    .expect("repository create should succeed");

    let clones = backend.clone().using::<Clone>(NAMESPACE);
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ"));
    clones
        .create(
            &controller_meta().name(&clone_name).call(),
            &CloneSpec {
                repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-01HXYZ".to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
        )
        .await
        .expect("clone create should succeed");

    let harness = clone_harness(backend.clone());
    harness
        .wait_until(Duration::from_secs(1), || {
            let clones = clones.clone();
            let clone_name = clone_name.clone();
            async move {
                matches!(
                    clones.get(&clone_name).await.ok().and_then(|clone| clone.status).map(|status| status.phase),
                    Some(ClonePhase::Ready)
                )
            }
        })
        .await;

    let clone = clones.get(&clone_name).await.expect("clone get should succeed");
    let status = clone.status.expect("clone status should be present");
    assert_eq!(status.phase, flotilla_resources::ClonePhase::Ready);
    assert_eq!(status.default_branch.as_deref(), Some("main"));

    harness.shutdown().await;
}

#[tokio::test]
async fn new_convoy_checkout_demand_redrives_a_clone_failed_on_old_auth() {
    let backend = ResourceBackend::InMemory(Default::default());
    let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    flotilla_store::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_spec.key(), &repository_spec)
        .await
        .expect("repository create should succeed");
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ"));
    let clones = backend.clone().using::<Clone>(NAMESPACE);
    let failed_clone = clones
        .create(
            &controller_meta().name(&clone_name).call(),
            &CloneSpec {
                repo_ref: repository_spec.key(),
                url: "git@github.com:flotilla-org/flotilla.git".to_string(),
                env_ref: "host-direct-01HXYZ".to_string(),
                path: "/Users/alice/dev/flotilla".to_string(),
            },
        )
        .await
        .expect("clone create should succeed");
    clones
        .update_status(
            &clone_name,
            &failed_clone.metadata.resource_version,
            &CloneStatus {
                phase: ClonePhase::Failed,
                default_branch: None,
                message: Some("authentication failed: repository access denied".to_string()),
                failed_at: Some(Utc::now() - chrono::Duration::hours(15)),
                failure_policy: None,
                retry: None,
            },
        )
        .await
        .expect("legacy clone failure should apply");

    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    checkouts
        .create(
            &controller_meta().name("checkout-new-demand").call(),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: repository_spec.key(),
                env_ref: "host-direct-01HXYZ".to_string(),
                r#ref: "fix/clone-failed-latch".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/Users/alice/dev/flotilla.fix-clone-failed-latch".to_string(),
                clone_ref: clone_name.clone(),
            }),
        )
        .await
        .expect("checkout create should succeed");

    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: clones.clone(),
            secondaries: vec![],
            reconciler: CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.clone().using(NAMESPACE)),
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );
    harness.spawn(
        ControllerLoop {
            primary: checkouts.clone(),
            secondaries: vec![],
            reconciler: CheckoutReconciler::new(Arc::new(FakeCheckoutRuntime), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(3), || {
            let clones = clones.clone();
            let checkouts = checkouts.clone();
            let clone_name = clone_name.clone();
            async move {
                matches!(
                    clones.get(&clone_name).await.ok().and_then(|clone| clone.status),
                    Some(status) if status.phase == ClonePhase::Ready
                ) && matches!(
                    checkouts.get("checkout-new-demand").await.ok().and_then(|checkout| checkout.status),
                    Some(status) if status.phase == CheckoutPhase::Ready
                )
            }
        })
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn environment_controller_marks_docker_environment_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    environments
        .create(
            &controller_meta().name("docker-env").call(),
            &EnvironmentSpec {
                host_direct: None,
                docker: Some(DockerEnvironmentSpec {
                    image_composition: None,
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "01HXYZ".to_string(),
                    image: "ghcr.io/flotilla/dev:latest".to_string(),
                    declared_agent_adapters: Default::default(),
                    required_agent_adapters: Default::default(),
                    pull_policy: Default::default(),
                    mounts: vec![EnvironmentMount {
                        source_path: "/tmp/src".to_string(),
                        target_path: "/workspace".to_string(),
                        mode: EnvironmentMountMode::Rw,
                    }],
                    env: Default::default(),
                }),
            },
        )
        .await
        .expect("environment create should succeed");

    let harness = environment_harness(backend.clone());
    harness
        .wait_until(Duration::from_secs(1), || {
            let environments = environments.clone();
            async move {
                matches!(
                    environments.get("docker-env").await.ok().and_then(|environment| environment.status),
                    Some(status)
                        if status.phase == EnvironmentPhase::Ready
                            && status.docker_container_id.as_deref() == Some("container-docker-env")
                            && status.image_ref.as_deref() == Some("ghcr.io/flotilla/dev:latest")
                            && status.local_image_id.as_deref() == Some("sha256:test-image")
                )
            }
        })
        .await;

    // #2650: provisioning records the limits actually applied, rather than
    // recomputing them from potentially changed host configuration.
    assert_eq!(
        environments.get("docker-env").await.expect("ready environment").status.expect("environment status").configured_limits,
        Some(ConfiguredResourceLimits { cpus: Some(4), build_jobs: Some(4), linker_threads: Some(4) })
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn checkout_controller_marks_worktree_checkout_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_clone(
        &backend,
        NAMESPACE,
        "clone-a",
        "git@github.com:flotilla-org/flotilla.git",
        "host-direct-01HXYZ",
        "/Users/alice/dev/flotilla",
    )
    .await;
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    checkouts
        .create(
            &controller_meta().name("checkout-a").call(),
            &flotilla_resources::CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
                env_ref: "host-direct-01HXYZ".to_string(),
                r#ref: "feat/convoy-resource".to_string(),
                base_ref: None,
                target_path: "/Users/alice/dev/flotilla.feat-123".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");

    let harness = checkout_harness(backend.clone());
    harness
        .wait_until(Duration::from_secs(1), || {
            let checkouts = checkouts.clone();
            async move {
                matches!(
                    checkouts.get("checkout-a").await.ok().and_then(|checkout| checkout.status),
                    Some(status)
                        if status.phase == CheckoutPhase::Ready
                            && status.path.as_deref() == Some("/Users/alice/dev/flotilla.feat-123")
                            && status.commit.as_deref() == Some("44982740")
                )
            }
        })
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn checkout_controller_requeues_when_first_completion_is_dropped() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_clone(
        &backend,
        NAMESPACE,
        "clone-a",
        "git@github.com:flotilla-org/flotilla.git",
        "host-direct-01HXYZ",
        "/Users/alice/dev/flotilla",
    )
    .await;
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    checkouts
        .create(
            &controller_meta().name("checkout-redrive").call(),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
                env_ref: "host-direct-01HXYZ".to_string(),
                r#ref: "feat/actuation-recovery".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/Users/alice/dev/flotilla.feat-actuation-recovery".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");

    let runtime = Arc::new(CountingCheckoutRuntime::default());
    let reconciler = DropFirstCheckoutCompletion {
        inner: CheckoutReconciler::new(Arc::clone(&runtime), backend.clone(), NAMESPACE),
        dropped: AtomicBool::new(false),
    };
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: checkouts.clone(),
            secondaries: vec![],
            reconciler,
            // The point retry must recover without waiting for the ordinary
            // production resync or restarting the controller.
            resync_interval: Duration::from_secs(60),
            backend,
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(3), || {
            let checkouts = checkouts.clone();
            async move {
                matches!(
                    checkouts.get("checkout-redrive").await.ok().and_then(|checkout| checkout.status),
                    Some(status) if status.phase == CheckoutPhase::Ready
                )
            }
        })
        .await;
    assert!(runtime.attempts.load(Ordering::SeqCst) >= 2, "dropped checkout completion should be re-driven");

    harness.shutdown().await;
}

#[tokio::test]
async fn terminal_session_controller_marks_session_running() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
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

    let harness = terminal_harness(backend.clone());
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
async fn vessel_controller_finalizer_deletes_vessel_owned_children_but_preserves_checkout() {
    let backend = ResourceBackend::InMemory(Default::default());
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

    backend
        .clone()
        .using::<Environment>(NAMESPACE)
        .create(
            &controller_meta()
                .name("env-workspace-delete")
                .labels([(VESSEL_REF_LABEL.to_string(), "workspace-delete".to_string())].into_iter().collect())
                .call(),
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec {
                    host_ref: "01HXYZ".to_string(),
                    repo_default_dir: "/Users/alice/dev/flotilla-repos".to_string(),
                }),
                docker: None,
            },
        )
        .await
        .expect("environment create should succeed");
    backend
        .clone()
        .using::<Checkout>(NAMESPACE)
        .create(
            &controller_meta()
                .name("checkout-workspace-delete")
                .labels([(VESSEL_REF_LABEL.to_string(), "workspace-delete".to_string())].into_iter().collect())
                .call(),
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
                env_ref: "host-direct-01HXYZ".to_string(),
                r#ref: "feat/task-provisioning".to_string(),
                base_ref: None,
                target_path: "/Users/alice/dev/flotilla-repos/workspace-delete".to_string(),
                clone_ref: "clone-a".to_string(),
            }),
        )
        .await
        .expect("checkout create should succeed");
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
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    let checkouts = backend.clone().using::<Checkout>(NAMESPACE);
    let terminals = backend.clone().using::<TerminalSession>(NAMESPACE);
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: workspaces.clone(),
            secondaries: VesselReconciler::secondary_watches(),
            reconciler: VesselReconciler::new(backend.clone(), NAMESPACE),
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
            let environments = environments.clone();
            let checkouts = checkouts.clone();
            let terminals = terminals.clone();
            async move {
                matches!(workspaces.get("workspace-delete").await, Err(ResourceError::NotFound { .. }))
                    && matches!(environments.get("env-workspace-delete").await, Err(ResourceError::NotFound { .. }))
                    && checkouts.get("checkout-workspace-delete").await.is_ok()
                    && matches!(terminals.get("terminal-workspace-delete-coder").await, Err(ResourceError::NotFound { .. }))
            }
        })
        .await;

    harness.shutdown().await;
}

fn environment_harness(backend: ResourceBackend) -> ControllerLoopHarness {
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Environment>(NAMESPACE),
            secondaries: vec![],
            reconciler: EnvironmentReconciler::new(Arc::new(FakeDockerRuntime::default()), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_millis(50),
            backend,
        }
        .run(),
    );
    harness
}

fn clone_harness(backend: ResourceBackend) -> ControllerLoopHarness {
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Clone>(NAMESPACE),
            secondaries: vec![],
            reconciler: CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.clone().using(NAMESPACE)),
            resync_interval: Duration::from_millis(50),
            backend,
        }
        .run(),
    );
    harness
}

fn checkout_harness(backend: ResourceBackend) -> ControllerLoopHarness {
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Checkout>(NAMESPACE),
            secondaries: vec![],
            reconciler: CheckoutReconciler::new(Arc::new(FakeCheckoutRuntime), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_millis(50),
            backend,
        }
        .run(),
    );
    harness
}

fn terminal_harness(backend: ResourceBackend) -> ControllerLoopHarness {
    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<TerminalSession>(NAMESPACE),
            secondaries: vec![],
            reconciler: TerminalSessionReconciler::new(Arc::new(FakeTerminalRuntime), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_millis(50),
            backend,
        }
        .run(),
    );
    harness
}

fn full_controller_harness(backend: ResourceBackend) -> ControllerLoopHarness {
    let mut harness = environment_harness(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Clone>(NAMESPACE),
            secondaries: vec![],
            reconciler: CloneReconciler::new(Arc::new(FakeCloneRuntime), backend.clone().using(NAMESPACE)),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Checkout>(NAMESPACE),
            secondaries: vec![],
            reconciler: CheckoutReconciler::new(Arc::new(FakeCheckoutRuntime), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<TerminalSession>(NAMESPACE),
            secondaries: vec![],
            reconciler: TerminalSessionReconciler::new(Arc::new(FakeTerminalRuntime), backend.clone(), NAMESPACE),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );
    harness.spawn(
        ControllerLoop {
            primary: backend.clone().using::<Vessel>(NAMESPACE),
            secondaries: VesselReconciler::secondary_watches(),
            reconciler: VesselReconciler::new(backend.clone(), NAMESPACE),
            // This must progress through Checkout Ready events rather than a
            // periodic Vessel relist; production resync is also deliberately slow.
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
