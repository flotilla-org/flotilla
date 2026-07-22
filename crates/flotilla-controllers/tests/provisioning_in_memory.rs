mod common;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use common::{
    controller_meta, create_convoy_with_single_task, create_host_direct_policy, create_ready_clone, create_ready_host_direct_environment,
    create_workspace, ControllerLoopHarness,
};
use flotilla_controllers::reconcilers::{
    CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneReconciler, CloneRuntime, DockerEnvironmentRuntime,
    EnvironmentReconciler, HopChainContext, PreparedCheckout, PresentationPolicyRegistry, PresentationReconciler,
    ProviderPresentationRuntime, TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_core::{
    path_context::DaemonHostPath,
    providers::{
        discovery::{ProviderCategory, ProviderDescriptor},
        presentation::PresentationManager,
        registry::ProviderRegistry,
        terminal::{TerminalEnvVars, TerminalPool, TerminalSession as PoolTerminalSession},
        types::{Workspace, WorkspaceAttachRequest},
    },
    HostName,
};
use flotilla_resources::{
    clone_key, controller::ControllerLoop, Checkout, CheckoutBranchProvenance, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone,
    ClonePhase, CloneSpec, DockerEnvironmentSpec, Environment, EnvironmentMount, EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec,
    Host, HostDirectEnvironmentSpec, HostSpec, HostStatus, Presentation, PresentationPhase, PresentationSpec, ResourceBackend,
    ResourceError, StatusPatch, TerminalSession, TerminalSessionPhase, Vessel, VesselPhase, CONVOY_LABEL, CREW_ORDINAL_LABEL,
    VESSEL_ORDINAL_LABEL, VESSEL_REF_LABEL,
};

const NAMESPACE: &str = "flotilla";

#[derive(Default)]
struct FakeDockerRuntime {
    destroyed: Mutex<Vec<String>>,
}

#[async_trait]
impl DockerEnvironmentRuntime for FakeDockerRuntime {
    async fn provision(&self, name: &str, _spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<String, String> {
        Ok(format!("container-{name}"))
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
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
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        Ok(flotilla_resources::CheckoutIntegrationStatus::default())
    }

    async fn remove_checkout(&self, _removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        Ok(CheckoutRemovalOutcome::Removed)
    }
}

#[derive(Default)]
struct FakeTerminalRuntime;

#[async_trait]
impl TerminalRuntime for FakeTerminalRuntime {
    async fn ensure_session(&self, name: &str, _spec: &flotilla_resources::TerminalSessionSpec) -> Result<TerminalRuntimeState, String> {
        Ok(TerminalRuntimeState {
            session_id: format!("session-{name}"),
            pid: Some(42),
            started_at: Utc::now(),
            crew: None,
            launch_command: "bash".to_string(),
            delivered_message_id: None,
        })
    }

    async fn kill_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct FakePresentationManager {
    created: Mutex<Vec<WorkspaceAttachRequest>>,
    deleted: Mutex<Vec<String>>,
}

#[async_trait]
impl PresentationManager for FakePresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        Ok(Vec::new())
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        self.created.lock().expect("created lock").push(config.clone());
        Ok((format!("workspace:{}", self.created.lock().expect("created lock").len()), Workspace {
            name: config.name.clone(),
            correlation_keys: Vec::new(),
            attachable_set_id: None,
        }))
    }

    async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
        Ok(())
    }

    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
        self.deleted.lock().expect("deleted lock").push(ws_ref.to_string());
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        String::new()
    }
}

struct FakePresentationTerminalPool;

#[async_trait]
impl TerminalPool for FakePresentationTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<PoolTerminalSession>, String> {
        Ok(Vec::new())
    }

    async fn ensure_session(
        &self,
        _session_name: &str,
        _command: &str,
        _cwd: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        session_name: &str,
        _command: &str,
        _cwd: &flotilla_core::path_context::ExecutionEnvironmentPath,
        _env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        Ok(vec![flotilla_protocol::arg::Arg::Literal(format!("attach {session_name}"))])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn controller_loops_drive_host_direct_workspace_to_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, "01HXYZ").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    create_host_direct_policy(&backend, NAMESPACE, "policy-a", "01HXYZ", "cleat").await;
    create_convoy_with_single_task(
        &backend,
        NAMESPACE,
        "convoy-a",
        "implement",
        "git@github.com:flotilla-org/flotilla.git",
        "feat/task-provisioning",
    )
    .await;
    create_workspace(&backend, NAMESPACE, "workspace-a", "convoy-a", "implement", "policy-a", "git@github.com:flotilla-org/flotilla.git")
        .await;

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
    assert_eq!(status.phase, VesselPhase::Ready);
    assert_eq!(status.environment_ref.as_deref(), Some("host-direct-01HXYZ"));
    assert_eq!(status.checkout_refs.values().next().map(String::as_str), Some("checkout-convoy-a"));
    assert_eq!(status.terminal_session_refs, vec!["terminal-workspace-a-coder".to_string()]);

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
async fn clone_controller_marks_clone_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    let repository_spec = flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
    flotilla_resources::ensure_repository(
        &backend.clone().using::<flotilla_resources::Repository>(NAMESPACE),
        &repository_spec.key(),
        &repository_spec,
    )
    .await
    .expect("repository create should succeed");

    let clones = backend.clone().using::<Clone>(NAMESPACE);
    let clone_name = format!("clone-{}", clone_key("https://github.com/flotilla-org/flotilla", "host-direct-01HXYZ"));
    clones
        .create(&controller_meta().name(&clone_name).call(), &CloneSpec {
            repo_ref: flotilla_resources::RepositoryKey(flotilla_resources::repo_key("https://github.com/flotilla-org/flotilla")),
            url: "git@github.com:flotilla-org/flotilla.git".to_string(),
            env_ref: "host-direct-01HXYZ".to_string(),
            path: "/Users/alice/dev/flotilla".to_string(),
        })
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
async fn environment_controller_marks_docker_environment_ready() {
    let backend = ResourceBackend::InMemory(Default::default());
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    environments
        .create(&controller_meta().name("docker-env").call(), &EnvironmentSpec {
            host_direct: None,
            docker: Some(DockerEnvironmentSpec {
                host_ref: "01HXYZ".to_string(),
                image: "ghcr.io/flotilla/dev:latest".to_string(),
                mounts: vec![EnvironmentMount {
                    source_path: "/tmp/src".to_string(),
                    target_path: "/workspace".to_string(),
                    mode: EnvironmentMountMode::Rw,
                }],
                env: Default::default(),
            }),
        })
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
                )
            }
        })
        .await;

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
async fn terminal_session_controller_marks_session_running() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    sessions
        .create(&controller_meta().name("term-a").call(), &flotilla_resources::TerminalSessionSpec {
            env_ref: "host-direct-01HXYZ".to_string(),
            role: "coder".to_string(),
            source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/workspace".to_string(),
            pool: "cleat".to_string(),
        })
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
async fn presentation_controller_marks_presentation_active_for_live_convoy_session() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, "01HXYZ").await;
    create_ready_host_direct_environment(&backend, NAMESPACE, "01HXYZ", "/Users/alice/dev/flotilla-repos").await;
    create_convoy_with_single_task(
        &backend,
        NAMESPACE,
        "convoy-a",
        "implement",
        "git@github.com:flotilla-org/flotilla.git",
        "feat/presentation",
    )
    .await;

    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let session = sessions
        .create(
            &controller_meta()
                .name("term-a")
                .labels(
                    [
                        (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                        (VESSEL_ORDINAL_LABEL.to_string(), "000".to_string()),
                        (CREW_ORDINAL_LABEL.to_string(), "000".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                )
                .call(),
            &flotilla_resources::TerminalSessionSpec {
                env_ref: "host-direct-01HXYZ".to_string(),
                role: "coder".to_string(),
                source: flotilla_resources::TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/Users/alice/dev/flotilla-repos/convoy-a".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("session create should succeed");
    sessions
        .update_status("term-a", &session.metadata.resource_version, &{
            let mut status = flotilla_resources::TerminalSessionStatus::default();
            flotilla_resources::TerminalSessionStatusPatch::MarkRunning {
                session_id: "term-a".to_string(),
                pid: Some(42),
                started_at: Utc::now(),
                crew: None,
                launch_command: "cargo test".to_string(),
                delivered_message_id: None,
            }
            .apply(&mut status);
            status
        })
        .await
        .expect("session status update should succeed");

    let presentations = backend.clone().using::<Presentation>(NAMESPACE);
    presentations
        .create(&controller_meta().name("presentation-a").call(), &PresentationSpec {
            convoy_ref: "convoy-a".to_string(),
            presentation_policy_ref: "default".to_string(),
            name: "convoy-a".to_string(),
            process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]),
        })
        .await
        .expect("presentation create should succeed");

    let mut registry = ProviderRegistry::new();
    let manager = Arc::new(FakePresentationManager::default());
    registry.presentation_managers.insert(
        "fake".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "fake", "Fake", "", "", ""),
        Arc::clone(&manager) as Arc<dyn PresentationManager>,
    );
    registry.terminal_pools.insert(
        "cleat".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::TerminalPool, "cleat", "Cleat", "", "", ""),
        Arc::new(FakePresentationTerminalPool),
    );
    let registry = Arc::new(registry);
    let policies = Arc::new(PresentationPolicyRegistry::with_defaults());

    let mut harness = ControllerLoopHarness::new(backend.clone());
    harness.spawn(
        ControllerLoop {
            primary: presentations.clone(),
            secondaries: PresentationReconciler::<ProviderPresentationRuntime>::secondary_watches(),
            reconciler: PresentationReconciler::new(
                Arc::new(ProviderPresentationRuntime::new(Arc::clone(&registry), Arc::clone(&policies))),
                backend.clone(),
                NAMESPACE,
                HopChainContext::new(
                    "01HXYZ",
                    HostName::new("local"),
                    {
                        let path = std::env::temp_dir().join("flotilla-presentation-provisioning-in-memory");
                        std::fs::create_dir_all(&path).expect("temp config dir should exist");
                        DaemonHostPath::new(path)
                    },
                    move |_env_ref| Ok(Arc::clone(&registry)),
                ),
                policies,
            ),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );

    harness
        .wait_until(Duration::from_secs(1), || {
            let presentations = presentations.clone();
            async move {
                matches!(
                    presentations.get("presentation-a").await.ok().and_then(|presentation| presentation.status),
                    Some(status)
                        if status.phase == PresentationPhase::Active
                            && status.observed_presentation_manager.as_deref() == Some("fake")
                            && status.observed_workspace_ref.as_deref() == Some("workspace:1")
                )
            }
        })
        .await;

    assert_eq!(manager.created.lock().expect("created lock").len(), 1);

    harness.shutdown().await;
}

#[tokio::test]
async fn vessel_controller_finalizer_deletes_labeled_children_on_delete() {
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
                    && matches!(checkouts.get("checkout-workspace-delete").await, Err(ResourceError::NotFound { .. }))
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
            reconciler: EnvironmentReconciler::new(Arc::new(FakeDockerRuntime::default())),
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
    let created = hosts.create(&controller_meta().name(name).call(), &HostSpec {}).await.expect("host create should succeed");
    hosts
        .update_status(name, &created.metadata.resource_version, &HostStatus {
            capabilities: Default::default(),
            heartbeat_at: Some(Utc::now()),
            ready: true,
            resource_store: None,
        })
        .await
        .expect("host status update should succeed");
}
