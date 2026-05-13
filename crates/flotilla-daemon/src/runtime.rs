use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    CheckoutReconciler, CheckoutRuntime, CloneReconciler, CloneRuntime, DockerEnvironmentRuntime, EnvironmentReconciler, HopChainContext,
    PresentationPolicyRegistry, PresentationReconciler, ProviderPresentationRuntime, TaskWorkspaceReconciler, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler,
};
use flotilla_core::{
    config::ConfigStore,
    in_process::InProcessDaemon,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{EnvironmentAssertion, EnvironmentBag},
        environment::{CreateOpts, EnvironmentHandle, ProvisionedMount},
        registry::ProviderRegistry,
        vcs::{CloneProvisioner, GitCloneProvisioner},
        ChannelLabel, CommandRunner,
    },
};
use flotilla_protocol::{EnvironmentId, EnvironmentSpec as RuntimeEnvironmentSpec, HostSummary, ImageId, ImageSource};
use flotilla_resources::{
    canonicalize_repo_url, clone_key, controller::ControllerLoop, descriptive_repo_slug, repo_key, Clone, CloneSpec, Convoy,
    ConvoyReconciler, DockerCheckoutStrategy, DockerPerTaskPlacementPolicySpec, Environment, EnvironmentSpec, Host,
    HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, InputMeta,
    PlacementPolicy, PlacementPolicySpec, Presentation, ResourceBackend, ResourceError, ResourceObject, TaskWorkspace, WorkflowTemplate,
};
use serde_json::json;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{error, warn};

use crate::ConvoyProjection;

const DEFAULT_DOCKER_IMAGE: &str = "ubuntu:24.04";
const DEFAULT_REPO_DIR_SUFFIX: &str = "dev/flotilla-repos";

#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    pub namespace: String,
    pub heartbeat_interval: Duration,
    pub controller_resync_interval: Duration,
    pub start_controllers: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            namespace: flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE.to_string(),
            heartbeat_interval: Duration::from_secs(30),
            controller_resync_interval: Duration::from_secs(60),
            start_controllers: true,
        }
    }
}

pub struct DaemonRuntime {
    tasks: Vec<JoinHandle<()>>,
}

impl DaemonRuntime {
    pub async fn start(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        daemon_socket_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        Self::start_with_options(daemon, config, daemon_socket_path, RuntimeOptions::default()).await
    }

    pub async fn start_with_options(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        daemon_socket_path: Option<PathBuf>,
        options: RuntimeOptions,
    ) -> Result<Self, String> {
        if let Some(path) = daemon_socket_path.as_ref() {
            daemon.set_daemon_socket_path(path.clone()).await;
        }
        daemon.set_provisioning_namespace(options.namespace.clone()).await;

        let local_registry = probe_local_provider_registry(&daemon, &config).await?;
        let profile = build_local_profile(&daemon, &local_registry)?;
        register_startup_resources(&daemon, &options.namespace, &profile).await?;
        apply_host_heartbeat(&daemon, &options.namespace, &profile).await?;

        let mut tasks =
            vec![spawn_heartbeat_task(Arc::clone(&daemon), options.namespace.clone(), profile.clone(), options.heartbeat_interval)];

        if options.start_controllers {
            let local_repo_root = daemon.tracked_repo_paths().await.into_iter().next().map(ExecutionEnvironmentPath::new);
            let state = Arc::new(ControllerRuntimeState::new(
                daemon,
                config,
                local_registry,
                daemon_socket_path.map(DaemonHostPath::new),
                profile.host_id.clone(),
                local_repo_root,
                profile.host_direct_environment_name(),
            ));
            tasks.extend(spawn_controller_loops(state, &options.namespace, options.controller_resync_interval));
        }

        Ok(Self { tasks })
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalProvisioningProfile {
    host_id: String,
    repo_default_dir: String,
    host_direct_pool: String,
    docker_pool: String,
    available_pools: Vec<String>,
    docker_available: bool,
}

impl LocalProvisioningProfile {
    fn host_direct_environment_name(&self) -> String {
        format!("host-direct-{}", self.host_id)
    }

    fn host_direct_policy_name(&self) -> String {
        format!("host-direct-{}", self.host_id)
    }

    fn docker_policy_name(&self) -> String {
        format!("docker-on-{}", self.host_id)
    }
}

struct ControllerRuntimeState {
    daemon: Arc<InProcessDaemon>,
    config: Arc<ConfigStore>,
    local_registry: Arc<ProviderRegistry>,
    daemon_socket_path: Option<DaemonHostPath>,
    local_host_ref: String,
    local_repo_root: Option<ExecutionEnvironmentPath>,
    host_direct_environment_name: String,
    provisioned_environments: Mutex<HashMap<String, ActiveProvisionedEnvironment>>,
    active_sessions: Mutex<HashMap<String, ActiveSession>>,
}

impl ControllerRuntimeState {
    fn new(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        local_registry: Arc<ProviderRegistry>,
        daemon_socket_path: Option<DaemonHostPath>,
        local_host_ref: String,
        local_repo_root: Option<ExecutionEnvironmentPath>,
        host_direct_environment_name: String,
    ) -> Self {
        Self {
            daemon,
            config,
            local_registry,
            daemon_socket_path,
            local_host_ref,
            local_repo_root,
            host_direct_environment_name,
            provisioned_environments: Mutex::new(HashMap::new()),
            active_sessions: Mutex::new(HashMap::new()),
        }
    }
}

struct ActiveProvisionedEnvironment {
    env_id: EnvironmentId,
    handle: EnvironmentHandle,
}

struct ActiveSession {
    env_ref: String,
    pool: String,
}

async fn probe_local_provider_registry(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Result<Arc<ProviderRegistry>, String> {
    let local_bag = daemon.local_environment_bag().ok_or_else(|| "local environment bag unavailable".to_string())?;
    let runner = daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
    let probe_root = daemon
        .tracked_repo_paths()
        .await
        .into_iter()
        .next()
        .map(ExecutionEnvironmentPath::new)
        .unwrap_or_else(|| ExecutionEnvironmentPath::new("/"));
    Ok(Arc::new(daemon.discovery_runtime().factories.probe_all(&local_bag, config, &probe_root, runner).await))
}

fn build_local_profile(daemon: &Arc<InProcessDaemon>, local_registry: &ProviderRegistry) -> Result<LocalProvisioningProfile, String> {
    let host_id = daemon.local_host_id().ok_or_else(|| "local host id unavailable".to_string())?.to_string();
    let repo_default_dir = daemon
        .local_environment_bag()
        .and_then(|bag| bag.find_env_var("HOME").map(|home| format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}")))
        .or_else(|| daemon.discovery_runtime().env.get("HOME").map(|home| format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}")))
        .unwrap_or_else(|| "/tmp/flotilla-repos".to_string());

    let mut available_pools: Vec<_> = local_registry.terminal_pools.iter().map(|(desc, _)| desc.implementation.clone()).collect();
    available_pools.sort();
    available_pools.dedup();

    let host_direct_pool = local_registry.terminal_pools.preferred_name().unwrap_or("passthrough").to_string();
    let docker_pool =
        if local_registry.terminal_pools.contains_key("passthrough") { "passthrough".to_string() } else { host_direct_pool.clone() };

    Ok(LocalProvisioningProfile {
        host_id,
        repo_default_dir,
        host_direct_pool,
        docker_pool,
        available_pools,
        docker_available: local_registry.environment_providers.contains_key("docker"),
    })
}

async fn register_startup_resources(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let backend = daemon.resource_backend();
    ensure_host_exists(&backend, namespace, &profile.host_id).await?;
    ensure_host_direct_environment_exists(&backend, namespace, profile).await?;
    discover_local_clones(daemon, &backend, namespace, profile).await?;
    ensure_default_policies(&backend, namespace, profile).await?;
    Ok(())
}

async fn ensure_host_exists(backend: &ResourceBackend, namespace: &str, host_name: &str) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace);
    if hosts.get(host_name).await.is_ok() {
        return Ok(());
    }
    hosts.create(&empty_meta(host_name), &HostSpec {}).await.map(|_| ()).map_err(|err| err.to_string())
}

async fn ensure_host_direct_environment_exists(
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let name = profile.host_direct_environment_name();
    let environments = backend.clone().using::<Environment>(namespace);
    if environments.get(&name).await.is_ok() {
        return Ok(());
    }

    environments
        .create(&empty_meta(&name), &EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec {
                host_ref: profile.host_id.clone(),
                repo_default_dir: profile.repo_default_dir.clone(),
            }),
            docker: None,
        })
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

async fn discover_local_clones(
    daemon: &Arc<InProcessDaemon>,
    backend: &ResourceBackend,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let runner = daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
    let clones = backend.clone().using::<Clone>(namespace);
    let host_direct_env_ref = profile.host_direct_environment_name();

    for repo_path in daemon.tracked_repo_paths().await {
        let repo_path_str = match repo_path.to_str() {
            Some(path) => path,
            None => {
                warn!(path = %repo_path.display(), "skipping clone discovery for non-utf8 repo path");
                continue;
            }
        };

        let transport_url =
            match runner.run("git", &["-C", repo_path_str, "remote", "get-url", "origin"], Path::new("/"), &ChannelLabel::Noop).await {
                Ok(url) => url.trim().to_string(),
                Err(err) => {
                    warn!(path = %repo_path.display(), %err, "skipping clone discovery because origin remote is unavailable");
                    continue;
                }
            };

        let canonical_url = match canonicalize_repo_url(&transport_url) {
            Ok(url) => url,
            Err(err) => {
                warn!(path = %repo_path.display(), %err, "skipping clone discovery because canonical url resolution failed");
                continue;
            }
        };

        let repo_key_value = repo_key(&canonical_url);
        let name = format!("clone-{}", clone_key(&canonical_url, &host_direct_env_ref));
        let expected_spec =
            CloneSpec { url: transport_url.clone(), env_ref: host_direct_env_ref.clone(), path: repo_path.display().to_string() };
        let expected_labels = BTreeMap::from([
            ("flotilla.work/discovered".to_string(), "true".to_string()),
            ("flotilla.work/repo-key".to_string(), repo_key_value),
            ("flotilla.work/env".to_string(), host_direct_env_ref.clone()),
            ("flotilla.work/repo".to_string(), descriptive_repo_slug(&canonical_url)),
        ]);

        match clones.get(&name).await {
            Ok(existing) => {
                if existing.metadata.deletion_timestamp.is_some() {
                    continue;
                }
                let existing_canonical = match canonicalize_repo_url(&existing.spec.url) {
                    Ok(url) => url,
                    Err(err) => {
                        warn!(clone = %name, %err, "leaving discovered clone untouched because existing canonical url is invalid");
                        continue;
                    }
                };
                if existing_canonical != canonical_url || existing.spec.env_ref != host_direct_env_ref {
                    warn!(clone = %name, "leaving discovered clone untouched because the existing resource does not match the expected repo/env tuple");
                    continue;
                }

                let merged_labels = merged_labels(&existing.metadata.labels, &expected_labels);
                if existing.spec != expected_spec || existing.metadata.labels != merged_labels {
                    clones
                        .update(&meta_from_existing(&existing, merged_labels), &existing.metadata.resource_version, &expected_spec)
                        .await
                        .map_err(|err| err.to_string())?;
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                clones.create(&empty_meta_with_labels(&name, expected_labels), &expected_spec).await.map_err(|err| err.to_string())?;
            }
            Err(err) => return Err(err.to_string()),
        }
    }

    Ok(())
}

async fn ensure_default_policies(backend: &ResourceBackend, namespace: &str, profile: &LocalProvisioningProfile) -> Result<(), String> {
    let policies = backend.clone().using::<PlacementPolicy>(namespace);

    let host_direct_name = profile.host_direct_policy_name();
    if matches!(policies.get(&host_direct_name).await, Err(ResourceError::NotFound { .. })) {
        policies
            .create(
                &empty_meta(&host_direct_name),
                &PlacementPolicySpec::builder()
                    .pool(profile.host_direct_pool.clone())
                    .host_direct(HostDirectPlacementPolicySpec {
                        host_ref: profile.host_id.clone(),
                        checkout: HostDirectPlacementPolicyCheckout::Worktree,
                    })
                    .build(),
            )
            .await
            .map_err(|err| err.to_string())?;
    }

    if profile.docker_available {
        let docker_name = profile.docker_policy_name();
        if matches!(policies.get(&docker_name).await, Err(ResourceError::NotFound { .. })) {
            policies
                .create(
                    &empty_meta(&docker_name),
                    &PlacementPolicySpec::builder()
                        .pool(profile.docker_pool.clone())
                        .docker_per_task(DockerPerTaskPlacementPolicySpec {
                            host_ref: profile.host_id.clone(),
                            image: DEFAULT_DOCKER_IMAGE.to_string(),
                            default_cwd: Some("/workspace".to_string()),
                            env: BTreeMap::new(),
                            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
                        })
                        .build(),
                )
                .await
                .map_err(|err| err.to_string())?;
        }
    }

    Ok(())
}

fn spawn_heartbeat_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(err) = apply_host_heartbeat(&daemon, &namespace, &profile).await {
                warn!(%err, "failed to publish host heartbeat");
            }
        }
    })
}

async fn apply_host_heartbeat(daemon: &Arc<InProcessDaemon>, namespace: &str, profile: &LocalProvisioningProfile) -> Result<(), String> {
    ensure_host_exists(&daemon.resource_backend(), namespace, &profile.host_id).await?;
    let hosts = daemon.resource_backend().using::<Host>(namespace);
    let host = hosts.get(&profile.host_id).await.map_err(|err| err.to_string())?;
    let summary = daemon.local_host_summary().await;
    let status = HostStatus { capabilities: host_capabilities(&summary, profile), heartbeat_at: Some(Utc::now()), ready: true };
    hosts.update_status(&profile.host_id, &host.metadata.resource_version, &status).await.map(|_| ()).map_err(|err| err.to_string())
}

fn host_capabilities(_summary: &HostSummary, profile: &LocalProvisioningProfile) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        ("docker".to_string(), json!(profile.docker_available)),
        ("terminal_pools".to_string(), json!(profile.available_pools)),
    ])
}

fn spawn_controller_loops(
    state: Arc<ControllerRuntimeState>,
    namespace: &str,
    controller_resync_interval: Duration,
) -> Vec<JoinHandle<()>> {
    let backend = state.daemon.resource_backend();
    let namespace_string = namespace.to_string();
    vec![
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<Environment>(&namespace_string),
                    secondaries: vec![],
                    reconciler: EnvironmentReconciler::new(Arc::new(DockerControllerRuntime { state: Arc::clone(&state) })),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "environment", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<Clone>(&namespace_string),
                    secondaries: vec![],
                    reconciler: CloneReconciler::new(Arc::new(CloneControllerRuntime {
                        runner: state.daemon.local_command_runner().expect("local runner should exist"),
                    })),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "clone", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<flotilla_resources::Checkout>(&namespace_string),
                    secondaries: vec![],
                    reconciler: CheckoutReconciler::new(
                        Arc::new(CheckoutControllerRuntime {
                            runner: state.daemon.local_command_runner().expect("local runner should exist"),
                        }),
                        backend.clone(),
                        &namespace_string,
                    ),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "checkout", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<flotilla_resources::TerminalSession>(&namespace_string),
                    secondaries: vec![],
                    reconciler: TerminalSessionReconciler::new(
                        Arc::new(TerminalControllerRuntime { state: Arc::clone(&state) }),
                        backend.clone(),
                        &namespace_string,
                    ),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "terminal_session", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<TaskWorkspace>(&namespace_string),
                    secondaries: TaskWorkspaceReconciler::secondary_watches(),
                    reconciler: TaskWorkspaceReconciler::new(backend.clone(), &namespace_string),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "task_workspace", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            async move {
                let policies = Arc::new(PresentationPolicyRegistry::with_defaults());
                let runtime = Arc::new(ProviderPresentationRuntime::new(Arc::clone(&state.local_registry), Arc::clone(&policies)));
                let mut hop_chain = HopChainContext::new(
                    state.local_host_ref.clone(),
                    state.daemon.host_name().clone(),
                    state.config.base_path().clone(),
                    {
                        let state = Arc::clone(&state);
                        move |env_ref| {
                            if env_ref == state.host_direct_environment_name {
                                return Ok(Arc::clone(&state.local_registry));
                            }
                            state
                                .daemon
                                .environment_registry_for_environment(&EnvironmentId::new(env_ref.to_string()))
                                .ok_or_else(|| format!("provider registry unavailable for environment {env_ref}"))
                        }
                    },
                );
                if let Some(repo_root) = state.local_repo_root.clone() {
                    hop_chain = hop_chain.with_repo_root(repo_root);
                }

                if let Err(err) = (ControllerLoop {
                    primary: backend.clone().using::<Presentation>(&namespace_string),
                    secondaries: PresentationReconciler::<ProviderPresentationRuntime>::secondary_watches(),
                    reconciler: PresentationReconciler::new(runtime, backend.clone(), &namespace_string, hop_chain, policies),
                    resync_interval: controller_resync_interval,
                    backend: backend.clone(),
                })
                .run()
                .await
                {
                    error!(controller = "presentation", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let namespace_string = namespace_string.clone();
            let backend_for_reconciler = backend.clone();
            async move {
                if let Err(err) = (ControllerLoop {
                    primary: backend_for_reconciler.clone().using::<Convoy>(&namespace_string),
                    secondaries: ConvoyReconciler::secondary_watches(),
                    reconciler: ConvoyReconciler::new(backend_for_reconciler.clone().using::<WorkflowTemplate>(&namespace_string))
                        .with_task_workspaces(backend_for_reconciler.clone().using::<TaskWorkspace>(&namespace_string))
                        .with_presentations(backend_for_reconciler.clone().using::<Presentation>(&namespace_string)),
                    resync_interval: controller_resync_interval,
                    backend: backend_for_reconciler,
                })
                .run()
                .await
                {
                    error!(controller = "convoy", %err, "controller loop exited");
                }
            }
        }),
        tokio::spawn({
            let namespace_string = namespace_string.clone();
            let event_tx = state.daemon.event_sender();
            async move {
                ConvoyProjection::new(event_tx)
                    .run(backend.clone().using::<Convoy>(&namespace_string), backend.using::<Presentation>(&namespace_string))
                    .await;
            }
        }),
    ]
}

struct DockerControllerRuntime {
    state: Arc<ControllerRuntimeState>,
}

#[async_trait]
impl DockerEnvironmentRuntime for DockerControllerRuntime {
    async fn provision(&self, name: &str, spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<String, String> {
        let daemon_socket_path = self
            .state
            .daemon_socket_path
            .clone()
            .ok_or_else(|| "daemon socket path unavailable for docker environment provisioning".to_string())?;
        let (_, provider) = self
            .state
            .local_registry
            .environment_providers
            .get("docker")
            .or_else(|| self.state.local_registry.environment_providers.preferred_with_desc())
            .ok_or_else(|| "docker environment provider unavailable".to_string())?;

        let runtime_spec = RuntimeEnvironmentSpec { image: ImageSource::Registry(spec.image.clone()), token_env_vars: Vec::new() };
        let image = provider.ensure_image(&runtime_spec, Path::new("/")).await?;
        let env_id = EnvironmentId::new(name.to_string());
        let handle = provider
            .create(env_id.clone(), &ImageId::new(image.as_str().to_string()), CreateOpts {
                tokens: Vec::new(),
                daemon_socket_path,
                working_directory: None,
                provisioned_mounts: spec
                    .mounts
                    .iter()
                    .map(|mount| ProvisionedMount::new(mount.source_path.clone(), mount.target_path.clone()))
                    .collect(),
            })
            .await?;

        let container_id = handle.container_name().map(ToString::to_string).unwrap_or_else(|| format!("flotilla-env-{}", env_id));
        let (bag, registry) = probe_provisioned_environment(&self.state, &env_id, &handle).await?;
        self.state
            .daemon
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(registry))
            .map_err(|err| format!("failed to register provisioned environment {env_id}: {err}"))?;
        self.state.provisioned_environments.lock().await.insert(container_id.clone(), ActiveProvisionedEnvironment { env_id, handle });
        Ok(container_id)
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        let active = self.state.provisioned_environments.lock().await.remove(container_id);
        let Some(active) = active else {
            return Ok(());
        };
        active.handle.destroy().await?;
        let _ = self.state.daemon.remove_provisioned_environment(&active.env_id);
        Ok(())
    }
}

async fn probe_provisioned_environment(
    state: &ControllerRuntimeState,
    env_id: &EnvironmentId,
    handle: &EnvironmentHandle,
) -> Result<(EnvironmentBag, Arc<ProviderRegistry>), String> {
    let mut bag = EnvironmentBag::new();
    for (key, value) in handle.env_vars().await? {
        bag = bag.with(EnvironmentAssertion::env_var(key, value));
    }
    let probe_root = ExecutionEnvironmentPath::new("/workspace");
    let config = ConfigStore::with_base(state.config.base_path().as_path().join(format!("env-discovery/{env_id}")));
    let registry = state.daemon.discovery_runtime().factories.probe_all(&bag, &config, &probe_root, handle.runner()).await;
    Ok((bag, Arc::new(registry)))
}

struct CloneControllerRuntime {
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl CloneRuntime for CloneControllerRuntime {
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        let provisioner = GitCloneProvisioner::new(Arc::clone(&self.runner));
        let target_path = ExecutionEnvironmentPath::new(target_path);
        provisioner.clone_repo(repo_url, &target_path).await?;
        let inspection = provisioner.inspect_clone(&target_path).await?;
        Ok(inspection.default_branch)
    }

    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String> {
        let provisioner = GitCloneProvisioner::new(Arc::clone(&self.runner));
        let inspection = provisioner.inspect_clone(&ExecutionEnvironmentPath::new(target_path)).await?;
        Ok(inspection.default_branch)
    }
}

struct CheckoutControllerRuntime {
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl CheckoutRuntime for CheckoutControllerRuntime {
    async fn create_worktree(&self, clone_path: &str, branch: &str, target_path: &str) -> Result<Option<String>, String> {
        let clone_path = utf8_path(clone_path)?;
        let target_path = utf8_path(target_path)?;

        let local_ref = format!("refs/heads/{branch}");
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local_exists = self
            .runner
            .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &local_ref], Path::new("/"), &ChannelLabel::Noop)
            .await
            .is_ok();
        let remote_exists = self
            .runner
            .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &remote_ref], Path::new("/"), &ChannelLabel::Noop)
            .await
            .is_ok();

        if local_exists {
            self.runner
                .run("git", &["-C", clone_path, "worktree", "add", target_path, branch], Path::new("/"), &ChannelLabel::Noop)
                .await?;
        } else if remote_exists {
            let _ = self.runner.run("git", &["-C", clone_path, "fetch", "origin", branch], Path::new("/"), &ChannelLabel::Noop).await;
            self.runner
                .run(
                    "git",
                    &["-C", clone_path, "worktree", "add", "-b", branch, target_path, &format!("origin/{branch}")],
                    Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
        } else {
            self.runner
                .run("git", &["-C", clone_path, "worktree", "add", target_path, branch], Path::new("/"), &ChannelLabel::Noop)
                .await?;
        }

        resolve_head_commit(&*self.runner, target_path).await
    }

    async fn create_fresh_clone(&self, repo_url: &str, branch: &str, target_path: &str) -> Result<Option<String>, String> {
        let target_path = utf8_path(target_path)?;
        self.runner.run("git", &["clone", "--branch", branch, repo_url, target_path], Path::new("/"), &ChannelLabel::Noop).await?;
        resolve_head_commit(&*self.runner, target_path).await
    }

    async fn remove_checkout(&self, target_path: &str) -> Result<(), String> {
        self.runner.run("rm", &["-rf", utf8_path(target_path)?], Path::new("/"), &ChannelLabel::Noop).await?;
        Ok(())
    }
}

async fn resolve_head_commit(runner: &dyn CommandRunner, path: &str) -> Result<Option<String>, String> {
    let commit = runner.run("git", &["-C", path, "rev-parse", "HEAD"], Path::new("/"), &ChannelLabel::Noop).await?;
    Ok(Some(commit.trim().to_string()))
}

struct TerminalControllerRuntime {
    state: Arc<ControllerRuntimeState>,
}

#[async_trait]
impl TerminalRuntime for TerminalControllerRuntime {
    async fn ensure_session(&self, name: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<TerminalRuntimeState, String> {
        let registry = self.registry_for_env(&spec.env_ref)?;
        let pool = registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))?;

        pool.ensure_session(name, &spec.command, &ExecutionEnvironmentPath::new(&spec.cwd), &Vec::new()).await?;
        self.state
            .active_sessions
            .lock()
            .await
            .insert(name.to_string(), ActiveSession { env_ref: spec.env_ref.clone(), pool: spec.pool.clone() });
        Ok(TerminalRuntimeState { session_id: name.to_string(), pid: None, started_at: Utc::now() })
    }

    async fn kill_session(&self, session_id: &str) -> Result<(), String> {
        let Some(active) = self.state.active_sessions.lock().await.remove(session_id) else {
            return Ok(());
        };
        let registry = self.registry_for_env(&active.env_ref)?;
        let pool = registry
            .terminal_pools
            .get(&active.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", active.pool, active.env_ref))?;
        pool.kill_session(session_id).await
    }
}

impl TerminalControllerRuntime {
    fn registry_for_env(&self, env_ref: &str) -> Result<Arc<ProviderRegistry>, String> {
        if env_ref == self.state.host_direct_environment_name {
            return Ok(Arc::clone(&self.state.local_registry));
        }
        self.state
            .daemon
            .environment_registry_for_environment(&EnvironmentId::new(env_ref.to_string()))
            .ok_or_else(|| format!("provider registry unavailable for environment {env_ref}"))
    }
}

fn utf8_path(path: &str) -> Result<&str, String> {
    if Path::new(path).to_str().is_some() {
        Ok(path)
    } else {
        Err(format!("path is not valid utf-8: {path}"))
    }
}

fn empty_meta(name: &str) -> InputMeta {
    empty_meta_with_labels(name, BTreeMap::new())
}

fn empty_meta_with_labels(name: &str, labels: BTreeMap<String, String>) -> InputMeta {
    InputMeta::builder().name(name.to_string()).labels(labels).build()
}

fn meta_from_existing<T: flotilla_resources::Resource>(existing: &ResourceObject<T>, labels: BTreeMap<String, String>) -> InputMeta {
    InputMeta::builder()
        .name(existing.metadata.name.clone())
        .labels(labels)
        .annotations(existing.metadata.annotations.clone())
        .owner_references(existing.metadata.owner_references.clone())
        .finalizers(existing.metadata.finalizers.clone())
        .maybe_deletion_timestamp(existing.metadata.deletion_timestamp)
        .build()
}

fn merged_labels(existing: &BTreeMap<String, String>, expected: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut merged = existing.clone();
    for (key, value) in expected {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

#[cfg(test)]
mod test_git_repo;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_core::{
        config::ConfigStore,
        daemon::DaemonHandle,
        in_process::DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE,
        providers::discovery::{test_support::git_process_discovery, EnvironmentAssertion, EnvironmentBag},
    };
    use flotilla_protocol::{Command, CommandAction};
    use flotilla_resources::{
        ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, PlacementPolicy, ProcessDefinition, ProcessSource, TaskDefinition, TaskPhase,
        TypedResolver, WorkflowTemplate, WorkflowTemplateSpec,
    };
    use tempfile::TempDir;

    use super::{test_git_repo::TestGitRepo, *};

    fn passthrough_registry() -> Arc<ProviderRegistry> {
        use flotilla_core::providers::{
            discovery::{ProviderCategory, ProviderDescriptor},
            registry::ProviderRegistry,
            terminal::passthrough::PassthroughTerminalPool,
        };

        let mut registry = ProviderRegistry::new();
        registry.terminal_pools.insert(
            "passthrough",
            ProviderDescriptor::named(ProviderCategory::TerminalPool, "passthrough"),
            Arc::new(PassthroughTerminalPool),
        );
        Arc::new(registry)
    }

    fn manual_profile(host_id: &str, docker_available: bool) -> LocalProvisioningProfile {
        LocalProvisioningProfile {
            host_id: host_id.to_string(),
            repo_default_dir: "/Users/tester/dev/flotilla-repos".to_string(),
            host_direct_pool: "passthrough".to_string(),
            docker_pool: "passthrough".to_string(),
            available_pools: vec!["passthrough".to_string()],
            docker_available,
        }
    }

    async fn in_memory_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
        let daemon = InProcessDaemon::new_with_resource_backend(
            tracked_repos,
            config,
            git_process_discovery(false),
            flotilla_protocol::HostName::new("test-host"),
            ResourceBackend::InMemory(Default::default()),
        )
        .await;
        daemon
            .replace_local_environment_bag_for_test(
                EnvironmentBag::new()
                    .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                    .with(EnvironmentAssertion::binary("git", "/usr/bin/git")),
            )
            .expect("local environment bag should be replaceable in tests");
        daemon
    }

    async fn wait_for_host_status(hosts: &TypedResolver<Host>, name: &str) -> HostStatus {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let host = hosts.get(name).await.expect("host get should succeed");
            if let Some(status) = host.status {
                return status;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for host status");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn wait_until<F, Fut>(mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if condition().await {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for condition");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn heartbeat_task_updates_host_status_without_socket_server() {
        let temp = TempDir::new().expect("tempdir");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = in_memory_daemon(Vec::new(), Arc::clone(&config)).await;
        let host_id = daemon.local_host_id().expect("local host id").to_string();
        let profile = manual_profile(&host_id, false);

        ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id).await.expect("host registration should succeed");
        let heartbeat = spawn_heartbeat_task(Arc::clone(&daemon), NAMESPACE.to_string(), profile, Duration::from_millis(20));
        let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);

        let status = wait_for_host_status(&hosts, &host_id).await;
        assert!(status.ready, "heartbeat should mark host ready");
        assert_eq!(status.capabilities.get("docker"), Some(&json!(false)));
        assert_eq!(status.capabilities.get("terminal_pools"), Some(&json!(["passthrough"])));

        heartbeat.abort();
        let _ = heartbeat.await;
    }

    #[tokio::test]
    async fn startup_registration_is_idempotent_and_discovers_existing_clone() {
        let temp = TempDir::new().expect("tempdir");
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();

        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
        let host_id = daemon.local_host_id().expect("local host id").to_string();
        let profile = manual_profile(&host_id, false);

        register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("first startup registration should succeed");
        register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("second startup registration should succeed");

        let backend = daemon.resource_backend();
        let hosts = backend.clone().using::<Host>(NAMESPACE);
        let environments = backend.clone().using::<Environment>(NAMESPACE);
        let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
        let clones = backend.using::<Clone>(NAMESPACE);

        assert!(hosts.get(&host_id).await.is_ok(), "host resource should exist");
        assert!(environments.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct environment should exist");
        assert!(policies.get(&format!("host-direct-{host_id}")).await.is_ok(), "host-direct policy should exist");

        let clone_name = format!(
            "clone-{}",
            clone_key(
                &canonicalize_repo_url("https://github.com/flotilla-org/flotilla.git").expect("canonical url"),
                &format!("host-direct-{host_id}")
            )
        );
        let clone = clones.get(&clone_name).await.expect("discovered clone should exist");
        assert_eq!(clone.spec.url, "git@github.com:flotilla-org/flotilla.git");
        assert_eq!(clone.metadata.labels.get("flotilla.work/discovered").map(String::as_str), Some("true"));
    }

    #[tokio::test]
    async fn startup_registration_skips_repos_without_origin_and_gates_docker_policy() {
        let temp = TempDir::new().expect("tempdir");
        let git_repo = TestGitRepo::init(temp.path().join("repo-no-origin"));
        let repo = git_repo.path().to_path_buf();

        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
        let host_id = daemon.local_host_id().expect("local host id").to_string();

        register_startup_resources(&daemon, NAMESPACE, &manual_profile(&host_id, false))
            .await
            .expect("startup registration should succeed");

        let backend = daemon.resource_backend();
        let clones = backend.clone().using::<Clone>(NAMESPACE);
        let policies = backend.using::<PlacementPolicy>(NAMESPACE);
        assert!(clones.list().await.expect("clone list").items.is_empty(), "repo without origin should not create a discovered clone");
        assert!(
            policies.get(&format!("docker-on-{host_id}")).await.is_err(),
            "docker policy should be absent when docker capability is false"
        );

        let temp2 = TempDir::new().expect("tempdir");
        let config2 = Arc::new(ConfigStore::with_base(temp2.path().join("config")));
        let daemon2 = in_memory_daemon(Vec::new(), Arc::clone(&config2)).await;
        let host_id2 = daemon2.local_host_id().expect("local host id").to_string();
        register_startup_resources(&daemon2, NAMESPACE, &manual_profile(&host_id2, true))
            .await
            .expect("startup registration with docker capability should succeed");
        let policies2 = daemon2.resource_backend().using::<PlacementPolicy>(NAMESPACE);
        assert!(
            policies2.get(&format!("docker-on-{host_id2}")).await.is_ok(),
            "docker policy should be created when docker capability is true"
        );
    }

    #[tokio::test]
    async fn in_memory_stage4a_flow_reaches_running_and_completes_convoy() {
        let temp = TempDir::new().expect("tempdir");
        let repo_default_dir = temp.path().join("flotilla-repos");
        std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();
        let commit = git_repo.head();

        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
        let host_id = daemon.local_host_id().expect("local host id").to_string();
        let profile =
            LocalProvisioningProfile { repo_default_dir: repo_default_dir.display().to_string(), ..manual_profile(&host_id, false) };
        let backend = daemon.resource_backend();

        register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("startup registration should succeed");
        apply_host_heartbeat(&daemon, NAMESPACE, &profile).await.expect("host heartbeat should succeed");

        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            passthrough_registry(),
            None,
            profile.host_id.clone(),
            Some(ExecutionEnvironmentPath::new(&repo)),
            profile.host_direct_environment_name(),
        ));
        let controller_handles = spawn_controller_loops(Arc::clone(&state), NAMESPACE, Duration::from_millis(25));

        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("wf-a"),
                &WorkflowTemplateSpec::builder()
                    .inputs(Vec::new())
                    .tasks(vec![TaskDefinition::builder()
                        .name("implement".to_string())
                        .processes(vec![ProcessDefinition::builder()
                            .role("coder".to_string())
                            .source(ProcessSource::Tool { command: "bash -lc 'echo stage4a'".to_string() })
                            .build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("workflow template create should succeed");
        backend
            .clone()
            .using::<Convoy>(NAMESPACE)
            .create(&empty_meta("convoy-a"), &ConvoySpec {
                workflow_ref: "wf-a".to_string(),
                inputs: BTreeMap::new(),
                placement_policy: Some(format!("host-direct-{host_id}")),
                repository: Some(ConvoyRepositorySpec { url: "https://github.com/flotilla-org/flotilla.git".to_string() }),
                r#ref: Some(commit),
            })
            .await
            .expect("convoy create should succeed");

        let convoys = backend.clone().using::<Convoy>(NAMESPACE);
        let run_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if matches!(
                convoys.get("convoy-a").await.ok().and_then(|convoy| convoy.status).as_ref(),
                Some(status)
                    if status.phase == ConvoyPhase::Active
                        && matches!(status.tasks.get("implement"), Some(task) if task.phase == TaskPhase::Running)
            ) {
                break;
            }
            if tokio::time::Instant::now() >= run_deadline {
                let convoy = convoys.get("convoy-a").await.expect("convoy should exist");
                let workspace = backend.clone().using::<TaskWorkspace>(NAMESPACE).list().await.expect("workspace list should succeed");
                panic!("convoy did not reach running state: convoy={convoy:?} task_workspaces={workspace:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let host = backend.clone().using::<Host>(NAMESPACE).get(&host_id).await.expect("host should exist after startup");
        assert!(host.status.is_some(), "startup heartbeat should publish host status");

        daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyTaskComplete {
                    convoy: "convoy-a".to_string(),
                    task: "implement".to_string(),
                    message: Some("done".to_string()),
                },
            })
            .await
            .expect("convoy completion command should succeed");

        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                matches!(
                    convoys.get("convoy-a").await.ok().and_then(|convoy| convoy.status).as_ref(),
                    Some(status)
                        if status.phase == ConvoyPhase::Completed
                            && matches!(status.tasks.get("implement"), Some(task) if task.phase == TaskPhase::Completed)
                )
            }
        })
        .await;

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}
