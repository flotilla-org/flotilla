mod common;

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use common::meta;
use flotilla_controllers::reconcilers::{
    AppliedPresentation, ApplyPresentationError, HopChainContext, PresentationPlan, PresentationPolicyRegistry, PresentationReconciler,
    PresentationRuntime, ProviderPresentationRuntime,
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
use flotilla_protocol::arg::Arg;
use flotilla_resources::{
    controller::Reconciler, Environment, EnvironmentSpec, EnvironmentStatus, EnvironmentStatusPatch, Host, HostDirectEnvironmentSpec,
    HostSpec, HostStatus, HostStatusPatch, Presentation, PresentationSpec, PresentationStatus, PresentationStatusPatch, ResourceBackend,
    StatusPatch, TerminalSession, TerminalSessionSpec, TerminalSessionStatus, TerminalSessionStatusPatch, CONVOY_LABEL,
    PROCESS_ORDINAL_LABEL, TASK_ORDINAL_LABEL,
};

const NAMESPACE: &str = "flotilla";
const HOST_REF: &str = "01HXYZ";

#[derive(Default)]
struct FakePresentationRuntime {
    apply_calls: Mutex<Vec<PresentationPlan>>,
    tear_down_calls: Mutex<Vec<(String, String)>>,
    apply_results: Mutex<VecDeque<Result<AppliedPresentation, ApplyPresentationError>>>,
}

impl FakePresentationRuntime {
    fn with_results(results: Vec<Result<AppliedPresentation, ApplyPresentationError>>) -> Self {
        Self { apply_results: Mutex::new(results.into()), ..Default::default() }
    }
}

#[async_trait]
impl PresentationRuntime for FakePresentationRuntime {
    async fn apply(&self, plan: &PresentationPlan) -> Result<AppliedPresentation, ApplyPresentationError> {
        self.apply_calls.lock().expect("apply calls lock").push(plan.clone());
        if let Some(result) = self.apply_results.lock().expect("apply results lock").pop_front() {
            return result;
        }
        Ok(AppliedPresentation {
            presentation_manager: "fake-manager".to_string(),
            workspace_ref: format!("workspace-{}", self.apply_calls.lock().expect("apply calls lock").len()),
            spec_hash: plan.spec_hash.clone(),
        })
    }

    async fn tear_down(&self, manager: &str, workspace_ref: &str) -> Result<(), String> {
        self.tear_down_calls.lock().expect("tear down calls lock").push((manager.to_string(), workspace_ref.to_string()));
        Ok(())
    }
}

#[derive(Default)]
struct FakeTerminalPool;

#[async_trait]
impl TerminalPool for FakeTerminalPool {
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
    ) -> Result<Vec<Arg>, String> {
        Ok(vec![Arg::Literal(format!("attach {session_name}"))])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingPresentationManager {
    created: Mutex<Vec<WorkspaceAttachRequest>>,
    deleted: Mutex<Vec<String>>,
    fail_create: Mutex<Option<String>>,
}

impl RecordingPresentationManager {
    fn fail_create_with(&self, message: &str) {
        *self.fail_create.lock().expect("fail create lock") = Some(message.to_string());
    }
}

#[async_trait]
impl PresentationManager for RecordingPresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        Ok(Vec::new())
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        if let Some(message) = self.fail_create.lock().expect("fail create lock").take() {
            return Err(message);
        }
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

#[tokio::test]
async fn no_sessions_and_no_observed_workspace_is_in_sync() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    let presentation = create_presentation(&backend, "presentation-a", "default").await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    let deps = reconciler.fetch_dependencies(&presentation).await.expect("deps should load");
    let outcome = reconciler.reconcile(&presentation, &deps, Utc::now());

    assert!(matches!(deps, flotilla_controllers::reconcilers::PresentationDeps::InSync));
    assert!(outcome.patch.is_none());
    assert!(runtime.apply_calls.lock().expect("apply calls lock").is_empty());
    assert!(runtime.tear_down_calls.lock().expect("tear down calls lock").is_empty());
}

#[tokio::test]
async fn first_apply_marks_presentation_active() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_host_direct_env(&backend, "env-a").await;
    create_running_terminal(
        &backend,
        "term-a",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let presentation = create_presentation(&backend, "presentation-a", "default").await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    let deps = reconciler.fetch_dependencies(&presentation).await.expect("deps should load");
    let outcome = reconciler.reconcile(&presentation, &deps, Utc::now());

    let plan = runtime.apply_calls.lock().expect("apply calls lock").clone();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].processes.len(), 1);
    assert_eq!(plan[0].processes[0].attach_command, "attach term-a");
    assert!(matches!(
        outcome.patch,
        Some(PresentationStatusPatch::MarkActive { ref presentation_manager, ref workspace_ref, .. })
            if presentation_manager == "fake-manager" && workspace_ref == "workspace-1"
    ));
}

#[tokio::test]
async fn unchanged_world_is_a_no_op() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_host_direct_env(&backend, "env-a").await;
    create_running_terminal(
        &backend,
        "term-a",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());
    let created = create_presentation(&backend, "presentation-a", "default").await;
    let first_deps = reconciler.fetch_dependencies(&created).await.expect("deps should load");
    let first_outcome = reconciler.reconcile(&created, &first_deps, Utc::now());
    let first_patch = first_outcome.patch.expect("first reconcile should produce a patch");
    let updated = update_presentation_status(&backend, &created, first_patch).await;

    runtime.apply_calls.lock().expect("apply calls lock").clear();

    let deps = reconciler.fetch_dependencies(&updated).await.expect("deps should load");
    let outcome = reconciler.reconcile(&updated, &deps, Utc::now());

    assert!(matches!(deps, flotilla_controllers::reconcilers::PresentationDeps::InSync));
    assert!(outcome.patch.is_none());
    assert!(runtime.apply_calls.lock().expect("apply calls lock").is_empty());
}

#[tokio::test]
async fn sorted_session_determinism_uses_task_and_process_ordinals() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_host_direct_env(&backend, "env-a").await;
    create_running_terminal(
        &backend,
        "term-b",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "001".to_string()),
        ]),
    )
    .await;
    create_running_terminal(
        &backend,
        "term-a",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let presentation = create_presentation(&backend, "presentation-a", "default").await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    reconciler.fetch_dependencies(&presentation).await.expect("deps should load");

    let apply_calls = runtime.apply_calls.lock().expect("apply calls lock");
    assert_eq!(apply_calls[0].processes.iter().map(|process| process.attach_command.as_str()).collect::<Vec<_>>(), vec![
        "attach term-a",
        "attach term-b"
    ]);
}

#[tokio::test]
async fn empty_sessions_trigger_teardown() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    let presentation = create_presentation_with_status(&backend, "presentation-a", "default", PresentationStatus {
        observed_presentation_manager: Some("fake-manager".to_string()),
        observed_workspace_ref: Some("workspace-a".to_string()),
        observed_spec_hash: Some("hash".to_string()),
        ..Default::default()
    })
    .await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    let deps = reconciler.fetch_dependencies(&presentation).await.expect("deps should load");
    let outcome = reconciler.reconcile(&presentation, &deps, Utc::now());

    assert!(matches!(deps, flotilla_controllers::reconcilers::PresentationDeps::TornDown { message: None }));
    assert_eq!(runtime.tear_down_calls.lock().expect("tear down calls lock").as_slice(), &[(
        "fake-manager".to_string(),
        "workspace-a".to_string()
    )]);
    assert!(matches!(outcome.patch, Some(PresentationStatusPatch::MarkTornDown { .. })));
}

#[tokio::test]
async fn retry_from_clean_slate_clears_previous_workspace_before_retry() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_host_direct_env(&backend, "env-a").await;
    create_running_terminal(
        &backend,
        "term-a",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let runtime = Arc::new(FakePresentationRuntime::with_results(vec![
        Err(ApplyPresentationError::RetryFromCleanSlate("create failed".to_string())),
        Ok(AppliedPresentation {
            presentation_manager: "fake-manager".to_string(),
            workspace_ref: "workspace-retry".to_string(),
            spec_hash: "will-be-replaced".to_string(),
        }),
    ]));
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());
    let created = create_presentation_with_status(&backend, "presentation-a", "default", PresentationStatus {
        observed_presentation_manager: Some("old-manager".to_string()),
        observed_workspace_ref: Some("workspace-old".to_string()),
        observed_spec_hash: Some("old-hash".to_string()),
        ..Default::default()
    })
    .await;

    let first_deps = reconciler.fetch_dependencies(&created).await.expect("deps should load");
    let first_outcome = reconciler.reconcile(&created, &first_deps, Utc::now());
    let first_patch = first_outcome.patch.expect("first reconcile should patch");
    let updated = update_presentation_status(&backend, &created, first_patch).await;

    let second_deps = reconciler.fetch_dependencies(&updated).await.expect("deps should load");
    let second_outcome = reconciler.reconcile(&updated, &second_deps, Utc::now());

    let apply_calls = runtime.apply_calls.lock().expect("apply calls lock");
    assert_eq!(apply_calls.len(), 2);
    assert_eq!(
        apply_calls[0].previous,
        Some(flotilla_controllers::reconcilers::PreviousWorkspace {
            presentation_manager: "old-manager".to_string(),
            workspace_ref: "workspace-old".to_string(),
        })
    );
    assert!(apply_calls[1].previous.is_none());
    assert!(matches!(
        second_outcome.patch,
        Some(PresentationStatusPatch::MarkActive { ref workspace_ref, .. }) if workspace_ref == "workspace-retry"
    ));
}

#[tokio::test]
async fn unknown_policy_fails_without_runtime_invocation() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_host_direct_env(&backend, "env-a").await;
    create_running_terminal(
        &backend,
        "term-a",
        "env-a",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let presentation = create_presentation(&backend, "presentation-a", "missing-policy").await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    let deps = reconciler.fetch_dependencies(&presentation).await.expect("deps should load");
    let outcome = reconciler.reconcile(&presentation, &deps, Utc::now());

    assert!(matches!(
        deps,
        flotilla_controllers::reconcilers::PresentationDeps::UnknownPolicy(ref name) if name == "missing-policy"
    ));
    assert!(runtime.apply_calls.lock().expect("apply calls lock").is_empty());
    assert!(matches!(
        outcome.patch,
        Some(PresentationStatusPatch::MarkFailed { ref message })
            if message == "unknown presentation policy 'missing-policy'"
    ));
}

#[tokio::test]
async fn finalizer_tears_down_recorded_workspace() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    let presentation = create_presentation_with_status(&backend, "presentation-a", "default", PresentationStatus {
        observed_presentation_manager: Some("fake-manager".to_string()),
        observed_workspace_ref: Some("workspace-a".to_string()),
        ..Default::default()
    })
    .await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    reconciler.run_finalizer(&presentation).await.expect("finalizer should succeed");

    assert_eq!(runtime.tear_down_calls.lock().expect("tear down calls lock").as_slice(), &[(
        "fake-manager".to_string(),
        "workspace-a".to_string()
    )]);
}

#[tokio::test]
async fn working_directory_fallback_is_separate_from_session_cwd() {
    let backend = ResourceBackend::InMemory(Default::default());
    create_ready_host(&backend, HOST_REF).await;
    create_ready_docker_env(&backend, "docker-env").await;
    create_running_terminal(
        &backend,
        "term-a",
        "docker-env",
        BTreeMap::from([
            (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
            (TASK_ORDINAL_LABEL.to_string(), "000".to_string()),
            (PROCESS_ORDINAL_LABEL.to_string(), "000".to_string()),
        ]),
    )
    .await;
    let presentation = create_presentation(&backend, "presentation-a", "default").await;
    let runtime = Arc::new(FakePresentationRuntime::default());
    let reconciler = reconciler(Arc::clone(&runtime), backend.clone());

    reconciler.fetch_dependencies(&presentation).await.expect("deps should load");

    let apply_calls = runtime.apply_calls.lock().expect("apply calls lock");
    let plan = &apply_calls[0];
    assert_eq!(plan.presentation_local_cwd.as_path(), dirs::home_dir().expect("home dir").as_path());
    assert_ne!(plan.presentation_local_cwd.as_path(), PathBuf::from("/workspace/repo").as_path());
    assert!(plan.processes[0].attach_command.contains("/workspace/repo"));
    assert!(plan.processes[0].attach_command.contains("container-docker-env"));
}

#[tokio::test]
async fn provider_runtime_replaces_across_managers() {
    let policies = Arc::new(PresentationPolicyRegistry::with_defaults());
    let mut registry = ProviderRegistry::new();
    let old_manager = Arc::new(RecordingPresentationManager::default());
    let new_manager = Arc::new(RecordingPresentationManager::default());
    registry.presentation_managers.insert(
        "new".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "new", "New", "", "", ""),
        Arc::clone(&new_manager) as Arc<dyn PresentationManager>,
    );
    registry.presentation_managers.insert(
        "old".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "old", "Old", "", "", ""),
        Arc::clone(&old_manager) as Arc<dyn PresentationManager>,
    );
    registry.presentation_managers.prefer_by_implementation("new");
    let runtime = ProviderPresentationRuntime::new(Arc::new(registry), Arc::clone(&policies));

    let result = runtime
        .apply(&PresentationPlan {
            policy: "default".to_string(),
            name: "convoy-a".to_string(),
            processes: vec![resolved_process("main", "attach term-a")],
            presentation_local_cwd: flotilla_core::path_context::ExecutionEnvironmentPath::new("/tmp"),
            previous: Some(flotilla_controllers::reconcilers::PreviousWorkspace {
                presentation_manager: "old".to_string(),
                workspace_ref: "workspace-old".to_string(),
            }),
            spec_hash: "hash".to_string(),
        })
        .await
        .expect("apply should succeed");

    assert_eq!(result.presentation_manager, "new");
    assert_eq!(old_manager.deleted.lock().expect("deleted lock").as_slice(), &["workspace-old".to_string()]);
    assert_eq!(new_manager.created.lock().expect("created lock").len(), 1);
}

#[tokio::test]
async fn provider_runtime_returns_retry_from_clean_slate_after_delete_then_create_failure() {
    let policies = Arc::new(PresentationPolicyRegistry::with_defaults());
    let mut registry = ProviderRegistry::new();
    let old_manager = Arc::new(RecordingPresentationManager::default());
    let new_manager = Arc::new(RecordingPresentationManager::default());
    new_manager.fail_create_with("boom");
    registry.presentation_managers.insert(
        "new".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "new", "New", "", "", ""),
        Arc::clone(&new_manager) as Arc<dyn PresentationManager>,
    );
    registry.presentation_managers.insert(
        "old".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::WorkspaceManager, "old", "Old", "", "", ""),
        Arc::clone(&old_manager) as Arc<dyn PresentationManager>,
    );
    registry.presentation_managers.prefer_by_implementation("new");
    let runtime = ProviderPresentationRuntime::new(Arc::new(registry), Arc::clone(&policies));

    let result = runtime
        .apply(&PresentationPlan {
            policy: "default".to_string(),
            name: "convoy-a".to_string(),
            processes: vec![resolved_process("main", "attach term-a")],
            presentation_local_cwd: flotilla_core::path_context::ExecutionEnvironmentPath::new("/tmp"),
            previous: Some(flotilla_controllers::reconcilers::PreviousWorkspace {
                presentation_manager: "old".to_string(),
                workspace_ref: "workspace-old".to_string(),
            }),
            spec_hash: "hash".to_string(),
        })
        .await;

    assert!(matches!(result, Err(ApplyPresentationError::RetryFromCleanSlate(ref message)) if message == "boom"));
    assert_eq!(old_manager.deleted.lock().expect("deleted lock").as_slice(), &["workspace-old".to_string()]);
}

fn reconciler(runtime: Arc<FakePresentationRuntime>, backend: ResourceBackend) -> PresentationReconciler<FakePresentationRuntime> {
    let registry = Arc::new(registry_with_terminal_pool());
    PresentationReconciler::new(
        runtime,
        backend,
        NAMESPACE,
        HopChainContext::new(HOST_REF, HostName::new("local"), temp_config_base(), move |_env_ref| Ok(Arc::clone(&registry))),
        Arc::new(PresentationPolicyRegistry::with_defaults()),
    )
}

fn registry_with_terminal_pool() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.terminal_pools.insert(
        "fake".to_string(),
        ProviderDescriptor::labeled_simple(ProviderCategory::TerminalPool, "fake", "Fake Pool", "", "", ""),
        Arc::new(FakeTerminalPool),
    );
    registry
}

fn temp_config_base() -> DaemonHostPath {
    let path = std::env::temp_dir().join("flotilla-presentation-reconciler-tests");
    std::fs::create_dir_all(&path).expect("temp config base should exist");
    DaemonHostPath::new(path)
}

async fn create_ready_host(backend: &ResourceBackend, name: &str) {
    let hosts = backend.clone().using::<Host>(NAMESPACE);
    let created = hosts.create(&meta(name), &HostSpec {}).await.expect("host create should succeed");
    let mut status = HostStatus::default();
    HostStatusPatch::Heartbeat { capabilities: BTreeMap::new(), heartbeat_at: Utc::now(), ready: true }.apply(&mut status);
    hosts.update_status(name, &created.metadata.resource_version, &status).await.expect("host status update should succeed");
}

async fn create_ready_host_direct_env(backend: &ResourceBackend, name: &str) {
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    let created = environments
        .create(&meta(name), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: HOST_REF.to_string(),
                repo_default_dir: "/Users/alice/dev/flotilla".to_string(),
            }),
            docker: None,
        })
        .await
        .expect("env create should succeed");
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: None }.apply(&mut status);
    environments.update_status(name, &created.metadata.resource_version, &status).await.expect("env status update should succeed");
}

async fn create_ready_docker_env(backend: &ResourceBackend, name: &str) {
    let environments = backend.clone().using::<Environment>(NAMESPACE);
    let created = environments
        .create(&meta(name), &EnvironmentSpec {
            host_direct: None,
            docker: Some(flotilla_resources::DockerEnvironmentSpec {
                host_ref: HOST_REF.to_string(),
                image: "ubuntu:24.04".to_string(),
                mounts: Vec::new(),
                env: BTreeMap::new(),
            }),
        })
        .await
        .expect("env create should succeed");
    let mut status = EnvironmentStatus::default();
    EnvironmentStatusPatch::MarkReady { docker_container_id: Some("container-docker-env".to_string()) }.apply(&mut status);
    environments.update_status(name, &created.metadata.resource_version, &status).await.expect("env status update should succeed");
}

async fn create_running_terminal(backend: &ResourceBackend, name: &str, env_ref: &str, labels: BTreeMap<String, String>) {
    let sessions = backend.clone().using::<TerminalSession>(NAMESPACE);
    let created = sessions
        .create(&common::controller_meta().name(name).labels(labels).call(), &TerminalSessionSpec {
            env_ref: env_ref.to_string(),
            role: "main".to_string(),
            command: "bash".to_string(),
            cwd: "/workspace/repo".to_string(),
            pool: "fake".to_string(),
        })
        .await
        .expect("session create should succeed");
    let mut status = TerminalSessionStatus::default();
    TerminalSessionStatusPatch::MarkRunning { session_id: name.to_string(), pid: Some(42), started_at: Utc::now() }.apply(&mut status);
    sessions.update_status(name, &created.metadata.resource_version, &status).await.expect("session status update should succeed");
}

async fn create_presentation(backend: &ResourceBackend, name: &str, policy_ref: &str) -> flotilla_resources::ResourceObject<Presentation> {
    create_presentation_with_status(backend, name, policy_ref, PresentationStatus::default()).await
}

async fn create_presentation_with_status(
    backend: &ResourceBackend,
    name: &str,
    policy_ref: &str,
    status: PresentationStatus,
) -> flotilla_resources::ResourceObject<Presentation> {
    let presentations = backend.clone().using::<Presentation>(NAMESPACE);
    let created = presentations
        .create(&meta(name), &PresentationSpec {
            convoy_ref: "convoy-a".to_string(),
            presentation_policy_ref: policy_ref.to_string(),
            name: "convoy-a".to_string(),
            process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]),
        })
        .await
        .expect("presentation create should succeed");
    if status != PresentationStatus::default() {
        presentations
            .update_status(name, &created.metadata.resource_version, &status)
            .await
            .expect("presentation status update should succeed");
    }
    presentations.get(name).await.expect("presentation get should succeed")
}

async fn update_presentation_status(
    backend: &ResourceBackend,
    object: &flotilla_resources::ResourceObject<Presentation>,
    patch: PresentationStatusPatch,
) -> flotilla_resources::ResourceObject<Presentation> {
    let presentations = backend.clone().using::<Presentation>(NAMESPACE);
    let mut status = object.status.clone().unwrap_or_default();
    patch.apply(&mut status);
    presentations
        .update_status(&object.metadata.name, &object.metadata.resource_version, &status)
        .await
        .expect("presentation status update should succeed");
    presentations.get(&object.metadata.name).await.expect("presentation get should succeed")
}

fn resolved_process(role: &str, attach_command: &str) -> flotilla_controllers::reconcilers::ResolvedProcess {
    flotilla_controllers::reconcilers::ResolvedProcess {
        role: role.to_string(),
        labels: BTreeMap::new(),
        attach_command: attach_command.to_string(),
    }
}
