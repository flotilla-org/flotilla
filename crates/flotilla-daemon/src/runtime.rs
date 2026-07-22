use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    BranchPreservationReason, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneReconciler, CloneRuntime,
    DockerEnvironmentRuntime, EnvironmentReconciler, ForgeDefaultBranchResolver, HopChainContext, PreparedCheckout,
    PresentationPolicyRegistry, PresentationReconciler, ProviderPresentationRuntime, RepositoryReconciler, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_core::{
    agent_adapter::{AgentLaunchRequest, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    checkout_integration::{checkout_branch_from_spec, checkout_path_from_status_and_spec, inspect_checkout_integration},
    config::ConfigStore,
    in_process::InProcessDaemon,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{EnvironmentAssertion, EnvironmentBag},
        environment::{CreateOpts, EnvironmentHandle, ProvisionedMount},
        registry::ProviderRegistry,
        terminal::{ScreenActivity, TerminalPool},
        vcs::{CloneProvisioner, GitCloneProvisioner},
        ChannelLabel, CommandRunner,
    },
};
use flotilla_protocol::{EnvironmentId, EnvironmentSpec as RuntimeEnvironmentSpec, HostSummary, ImageId, ImageSource, TerminalStatus};
use flotilla_resources::{
    clone_key, controller::ControllerLoop, descriptive_repo_slug, Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, Clone,
    CloneSpec, Convoy, ConvoyReconciler, CrewSource, CrewSpec, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, Environment,
    EnvironmentSpec, ForgeIdentity, Host, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec,
    HostSpec, HostStatus, InputDefinition, InputMeta, PlacementPolicy, PlacementPolicySpec, Presentation, Project, Repository,
    ResourceBackend, ResourceError, ResourceObject, Stance, TerminalSessionSource, Vessel, VesselRequirement, WorkflowTemplate,
    WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY,
};
use serde_json::json;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::warn;

use crate::{
    supervisor::{supervise, ControllerSupervision},
    Aggregator, AggregatorResolvers,
};

const DEFAULT_DOCKER_IMAGE: &str = "ubuntu:24.04";
const DEFAULT_REPO_DIR_SUFFIX: &str = "dev/flotilla-repos";

#[derive(Debug, Clone, bon::Builder)]
pub struct RuntimeOptions {
    pub namespace: String,
    pub heartbeat_interval: Duration,
    pub controller_resync_interval: Duration,
    pub controller_supervision: ControllerSupervision,
    pub start_controllers: bool,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            namespace: flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE.to_string(),
            heartbeat_interval: Duration::from_secs(30),
            controller_resync_interval: Duration::from_secs(60),
            controller_supervision: ControllerSupervision::default(),
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
        let aggregator_projection_state = daemon.aggregator_projection_state().await;

        let local_registry = probe_local_provider_registry(&daemon, &config).await?;
        let profile = build_local_profile(&daemon, &local_registry)?;
        daemon.set_local_placement_capabilities(&profile.available_agent_adapters, &profile.available_pools).await;
        register_startup_resources(&daemon, &options.namespace, &profile).await?;
        apply_host_heartbeat(&daemon, &options.namespace, &profile).await?;
        if let Err(error) = daemon.reconcile_adopted_checkouts(&options.namespace).await {
            warn!(%error, "failed to restore adopted checkout observations during startup; periodic reconciliation will retry");
        }

        let mut tasks = vec![
            spawn_heartbeat_task(Arc::clone(&daemon), options.namespace.clone(), profile.clone(), options.heartbeat_interval),
            spawn_replica_refresh_task(Arc::clone(&daemon), options.heartbeat_interval),
            spawn_adopted_checkout_reconciliation_task(Arc::clone(&daemon), options.namespace.clone(), options.controller_resync_interval),
            spawn_aggregator_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                aggregator_projection_state,
                options.controller_supervision.clone(),
            ),
        ];

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
            tasks.extend(spawn_controller_loops(
                state,
                &options.namespace,
                options.controller_resync_interval,
                options.controller_supervision.clone(),
            ));
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
    available_agent_adapters: BTreeSet<String>,
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
}

struct GhForgeDefaultBranchResolver {
    runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl ForgeDefaultBranchResolver for GhForgeDefaultBranchResolver {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String> {
        if forge.service_url.trim_end_matches('/') != "https://github.com" {
            return Ok(None);
        }
        let endpoint = format!("repos/{}", forge.repository);
        let output = self.runner.run("gh", &["api", &endpoint, "--jq", ".default_branch"], Path::new("/"), &ChannelLabel::Noop).await?;
        let branch = output.trim();
        Ok((!branch.is_empty()).then(|| branch.to_string()))
    }
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
        }
    }
}

struct ActiveProvisionedEnvironment {
    env_id: EnvironmentId,
    handle: EnvironmentHandle,
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
    let available_agent_adapters = local_registry.agent_adapters.ids().map(ToString::to_string).collect();

    Ok(LocalProvisioningProfile {
        host_id,
        repo_default_dir,
        host_direct_pool,
        docker_pool,
        available_pools,
        available_agent_adapters,
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
    ensure_default_workflow_templates(&backend, namespace).await?;
    daemon.materialize_tracked_repo_projects().await?;
    Ok(())
}

fn default_workflow_templates() -> Vec<(&'static str, WorkflowTemplateSpec)> {
    vec![
        (
            "scratch",
            WorkflowTemplateSpec::builder()
                .inputs(vec![InputDefinition { name: "topic".to_string(), description: Some("Short label for this convoy".into()) }])
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .stance(Stance::Trusted)
                    .crew(vec![CrewSpec::builder()
                        .role("shell".to_string())
                        .source(CrewSource::Tool {
                            command: r#"bash -c 'echo "Convoy {{workflow.name}} ({{inputs.topic}})"; exec bash'"#.to_string(),
                        })
                        .build()])
                    .build()])
                .build(),
        ),
        ("single-agent-contained", flotilla_resources::single_agent_contained_workflow_spec()),
    ]
}

async fn ensure_default_workflow_templates(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    for (name, spec) in default_workflow_templates() {
        match templates.get(name).await {
            Ok(_) => continue,
            Err(ResourceError::NotFound { .. }) => {}
            Err(err) => return Err(format!("check workflow template {name}: {err}")),
        }
        templates.create(&empty_meta(name), &spec).await.map(|_| ()).map_err(|err| format!("seed workflow template {name}: {err}"))?;
    }
    Ok(())
}

async fn ensure_host_exists(backend: &ResourceBackend, namespace: &str, host_name: &str) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace);
    match hosts.get(host_name).await {
        Ok(_) => return Ok(()),
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check host {host_name}: {err}")),
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
    match environments.get(&name).await {
        Ok(_) => return Ok(()),
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check environment {name}: {err}")),
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
    let clones = backend.clone().using::<Clone>(namespace);
    let host_direct_env_ref = profile.host_direct_environment_name();

    for repo_path in daemon.tracked_repo_paths().await {
        let inspection = match daemon.inspect_repository_path(&repo_path, None).await {
            Ok(inspection) => inspection,
            Err(err) => {
                warn!(path = %repo_path.display(), %err, "skipping clone discovery because repository identity resolution failed");
                continue;
            }
        };
        let Some(transport_url) = inspection.transport_url else {
            continue;
        };
        let canonical_url = match inspection.spec.identity() {
            flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
            flotilla_resources::RepositoryIdentity::Local { .. } => continue,
        };
        let repository_spec = inspection.spec;
        let repository_key = repository_spec.key();
        flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, &repository_spec)
            .await
            .map_err(|error| error.to_string())?;
        let repo_key_value = repository_key.to_string();
        let name = format!("clone-{}", clone_key(&canonical_url, &host_direct_env_ref));
        let expected_spec = CloneSpec {
            repo_ref: repository_key.clone(),
            url: transport_url.clone(),
            env_ref: host_direct_env_ref.clone(),
            path: repo_path.display().to_string(),
        };
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
                if existing.spec.repo_ref != repository_key || existing.spec.env_ref != host_direct_env_ref {
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
                        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                            host_ref: profile.host_id.clone(),
                            image: DEFAULT_DOCKER_IMAGE.to_string(),
                            agent_adapters: BTreeSet::new(),
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
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let profile = profile.clone();
        async move {
            if let Err(err) = apply_host_heartbeat(&daemon, &namespace, &profile).await {
                warn!(%err, "failed to publish host heartbeat");
            }
        }
    })
}

fn spawn_replica_refresh_task(daemon: Arc<InProcessDaemon>, interval: Duration) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        async move {
            if let Err(err) = daemon.refresh_fleet_replicas_once().await {
                warn!(%err, "failed to refresh fleet replicas");
            }
        }
    })
}

fn spawn_adopted_checkout_reconciliation_task(daemon: Arc<InProcessDaemon>, namespace: String, interval: Duration) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = daemon.reconcile_adopted_checkouts(&namespace).await {
                warn!(%error, "failed to reconcile adopted checkout observations");
            }
        }
    })
}

#[derive(Clone, Copy)]
enum PeriodicTaskStart {
    Immediate,
    AfterInterval,
}

fn spawn_periodic_task<Operation, OperationFuture>(interval: Duration, start: PeriodicTaskStart, mut operation: Operation) -> JoinHandle<()>
where
    Operation: FnMut() -> OperationFuture + Send + 'static,
    OperationFuture: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let start = match start {
            PeriodicTaskStart::Immediate => tokio::time::Instant::now(),
            PeriodicTaskStart::AfterInterval => tokio::time::Instant::now() + interval,
        };
        let mut ticker = tokio::time::interval_at(start, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            operation().await;
        }
    })
}

async fn apply_host_heartbeat(daemon: &Arc<InProcessDaemon>, namespace: &str, profile: &LocalProvisioningProfile) -> Result<(), String> {
    ensure_host_exists(&daemon.resource_backend(), namespace, &profile.host_id).await?;
    let backend = daemon.resource_backend();
    let hosts = backend.using::<Host>(namespace);
    let host = hosts.get(&profile.host_id).await.map_err(|err| err.to_string())?;
    let summary = daemon.local_host_summary().await;
    let resource_store = backend.diagnostics().await.map_err(|err| err.to_string())?;
    if let Some(diagnostics) = resource_store.as_ref().filter(|diagnostics| !diagnostics.warnings.is_empty()) {
        warn!(
            event_count = diagnostics.event_count,
            object_count = diagnostics.object_count,
            resource_stream_count = diagnostics.resource_stream_count,
            max_retained_events = diagnostics.max_retained_events,
            warnings = ?diagnostics.warnings,
            "resource event log tripwire triggered",
        );
    }
    let status =
        HostStatus { capabilities: host_capabilities(&summary, profile), heartbeat_at: Some(Utc::now()), ready: true, resource_store };
    hosts.update_status(&profile.host_id, &host.metadata.resource_version, &status).await.map(|_| ()).map_err(|err| err.to_string())
}

fn host_capabilities(_summary: &HostSummary, profile: &LocalProvisioningProfile) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
        ("docker".to_string(), json!(profile.docker_available)),
        ("terminal_pools".to_string(), json!(profile.available_pools)),
    ])
}

fn spawn_controller_loops(
    state: Arc<ControllerRuntimeState>,
    namespace: &str,
    controller_resync_interval: Duration,
    supervision: ControllerSupervision,
) -> Vec<JoinHandle<()>> {
    let backend = state.daemon.resource_backend();
    let observed_backend = state.daemon.observed_resource_backend();
    let forge_default_branch_resolver = state
        .daemon
        .local_command_runner()
        .map(|runner| Arc::new(GhForgeDefaultBranchResolver { runner }) as Arc<dyn ForgeDefaultBranchResolver>);
    let namespace_string = namespace.to_string();
    vec![
        tokio::spawn({
            let backend = backend.clone();
            let observed_backend = observed_backend.clone();
            let namespace_string = namespace_string.clone();
            let forge_default_branch_resolver = forge_default_branch_resolver.clone();
            let supervision = supervision.clone();
            async move {
                supervise("repository", supervision, move || {
                    let backend = backend.clone();
                    let observed_backend = observed_backend.clone();
                    let namespace_string = namespace_string.clone();
                    let forge_default_branch_resolver = forge_default_branch_resolver.clone();
                    async move {
                        let mut reconciler = RepositoryReconciler::new(backend.clone(), observed_backend.clone(), &namespace_string);
                        if let Some(resolver) = forge_default_branch_resolver {
                            reconciler = reconciler.with_forge_default_branch_resolver(resolver);
                        }
                        ControllerLoop {
                            primary: backend.clone().using::<Repository>(&namespace_string),
                            secondaries: RepositoryReconciler::secondary_watches(observed_backend.clone(), &namespace_string),
                            reconciler,
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            let supervision = supervision.clone();
            async move {
                supervise("environment", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let state = Arc::clone(&state);
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Environment>(&namespace_string),
                            secondaries: vec![],
                            reconciler: EnvironmentReconciler::new(Arc::new(DockerControllerRuntime { state })),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            let supervision = supervision.clone();
            async move {
                supervise("clone", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let runner = state.daemon.local_command_runner().expect("local runner should exist");
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Clone>(&namespace_string),
                            secondaries: vec![],
                            reconciler: CloneReconciler::new(
                                Arc::new(CloneControllerRuntime { runner }),
                                backend.clone().using::<Repository>(&namespace_string),
                            ),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            let supervision = supervision.clone();
            async move {
                supervise("checkout", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let state = Arc::clone(&state);
                    async move {
                        let runner = state.daemon.local_command_runner().expect("local runner should exist");
                        ControllerLoop {
                            primary: backend.clone().using::<flotilla_resources::Checkout>(&namespace_string),
                            secondaries: vec![],
                            reconciler: CheckoutReconciler::new(
                                Arc::new(CheckoutControllerRuntime { runner }),
                                backend.clone(),
                                &namespace_string,
                            ),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            let supervision = supervision.clone();
            async move {
                supervise("terminal_session", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let state = Arc::clone(&state);
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<flotilla_resources::TerminalSession>(&namespace_string),
                            secondaries: vec![],
                            reconciler: TerminalSessionReconciler::new(
                                Arc::new(TerminalControllerRuntime { state }),
                                backend.clone(),
                                &namespace_string,
                            ),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let supervision = supervision.clone();
            async move {
                supervise("vessel", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Vessel>(&namespace_string),
                            secondaries: VesselReconciler::secondary_watches(),
                            reconciler: VesselReconciler::new(backend.clone(), &namespace_string),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let state = Arc::clone(&state);
            let supervision = supervision.clone();
            async move {
                supervise("presentation", supervision, move || {
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

                        ControllerLoop {
                            primary: backend.clone().using::<Presentation>(&namespace_string),
                            secondaries: PresentationReconciler::<ProviderPresentationRuntime>::secondary_watches(),
                            reconciler: PresentationReconciler::new(runtime, backend.clone(), &namespace_string, hop_chain, policies),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let namespace_string = namespace_string.clone();
            let supervision = supervision.clone();
            async move {
                supervise("convoy", supervision, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Convoy>(&namespace_string),
                            secondaries: ConvoyReconciler::secondary_watches(),
                            reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>(&namespace_string))
                                .with_vessels(backend.clone().using::<Vessel>(&namespace_string))
                                .with_presentations(backend.clone().using::<Presentation>(&namespace_string))
                                .with_checkouts(backend.clone().using::<Checkout>(&namespace_string)),
                            resync_interval: controller_resync_interval,
                            backend,
                        }
                        .run()
                        .await
                    }
                })
                .await;
            }
        }),
    ]
}

fn spawn_aggregator_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    state: AggregatorProjectionState,
    supervision: ControllerSupervision,
) -> JoinHandle<()> {
    let durable = daemon.resource_backend();
    let observed = daemon.observed_resource_backend();
    tokio::spawn(async move {
        supervise("aggregator", supervision, move || {
            let daemon = Arc::clone(&daemon);
            let durable = durable.clone();
            let observed = observed.clone();
            let namespace = namespace.clone();
            let state = state.clone();
            async move {
                let mut aggregator = Aggregator::new(state, daemon.host_name().clone(), daemon.event_sender())
                    .with_attach_resolver(Arc::clone(&daemon))
                    .with_change_request_resolver(Arc::clone(&daemon))
                    .with_issue_resolver(Arc::clone(&daemon));
                aggregator.apply_replica_cache(daemon.cached_fleet_replica_snapshots().await).await;
                aggregator
                    .run(
                        AggregatorResolvers::builder()
                            .durable_convoys(durable.clone().using::<Convoy>(&namespace))
                            .durable_environments(durable.clone().using::<Environment>(&namespace))
                            .durable_presentations(durable.using::<Presentation>(&namespace))
                            .durable_sessions(durable.using::<flotilla_resources::TerminalSession>(&namespace))
                            .durable_projects(durable.using::<Project>(&namespace))
                            .durable_repositories(durable.using::<Repository>(&namespace))
                            .observed_convoys(observed.clone().using::<Convoy>(&namespace))
                            .observed_presentations(observed.using::<Presentation>(&namespace))
                            .observed_sessions(observed.using::<flotilla_resources::TerminalSession>(&namespace))
                            .observed_checkouts(observed.using::<Checkout>(&namespace))
                            .build(),
                        daemon.subscribe_fleet_replicas(),
                    )
                    .await
            }
        })
        .await;
    })
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

impl CheckoutControllerRuntime {
    fn local_runner(&self) -> Result<Arc<dyn CommandRunner>, String> {
        Ok(Arc::clone(&self.runner))
    }

    fn checkout_path<'a>(&self, checkout: &'a ResourceObject<Checkout>) -> Result<&'a str, String> {
        checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec)
            .ok_or_else(|| format!("checkout {} has no resolved path", checkout.metadata.name))
    }

    fn checkout_branch<'a>(&self, checkout: &'a ResourceObject<Checkout>) -> &'a str {
        checkout_branch_from_spec(&checkout.spec)
    }
}

#[async_trait]
impl CheckoutRuntime for CheckoutControllerRuntime {
    async fn create_worktree(
        &self,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        let runner = self.local_runner()?;
        let clone_path = utf8_path(clone_path)?;
        let target_path = utf8_path(target_path)?;

        let local_ref = format!("refs/heads/{branch}");
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local_exists = runner
            .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &local_ref], Path::new("/"), &ChannelLabel::Noop)
            .await
            .is_ok();
        if !local_exists
            && runner.run("git", &["-C", clone_path, "remote", "get-url", "origin"], Path::new("/"), &ChannelLabel::Noop).await.is_ok()
        {
            let remote_head = format!("refs/heads/{branch}");
            let advertised = runner
                .run("git", &["-C", clone_path, "ls-remote", "--heads", "origin", &remote_head], Path::new("/"), &ChannelLabel::Noop)
                .await
                .map_err(|error| format!("inspect remote convoy branch {branch}: {error}"))?;
            if !advertised.trim().is_empty() {
                let refspec = format!("{remote_head}:refs/remotes/origin/{branch}");
                runner
                    .run("git", &["-C", clone_path, "fetch", "origin", &refspec], Path::new("/"), &ChannelLabel::Noop)
                    .await
                    .map_err(|error| format!("fetch convoy branch {branch}: {error}"))?;
            }
        }
        let remote_exists = runner
            .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &remote_ref], Path::new("/"), &ChannelLabel::Noop)
            .await
            .is_ok();
        let branch_provenance = if !local_exists && !remote_exists && base_ref.is_some() {
            CheckoutBranchProvenance::CreatedForConvoy
        } else {
            CheckoutBranchProvenance::PreExisting
        };

        if local_exists {
            // Multiple vessels can intentionally share the convoy branch. `--force`
            // overrides Git's protection against attaching it to another worktree.
            runner
                .run("git", &["-C", clone_path, "worktree", "add", "--force", target_path, branch], Path::new("/"), &ChannelLabel::Noop)
                .await?;
        } else if remote_exists {
            runner
                .run(
                    "git",
                    &["-C", clone_path, "worktree", "add", "-b", branch, "--track", target_path, &format!("origin/{branch}")],
                    Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
        } else if let Some(base_ref) = base_ref {
            let local_base_ref = format!("refs/heads/{base_ref}");
            let remote_base_ref = format!("refs/remotes/origin/{base_ref}");
            let resolved_base_ref = if runner
                .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &local_base_ref], Path::new("/"), &ChannelLabel::Noop)
                .await
                .is_ok()
            {
                base_ref.to_string()
            } else if runner
                .run("git", &["-C", clone_path, "show-ref", "--verify", "--quiet", &remote_base_ref], Path::new("/"), &ChannelLabel::Noop)
                .await
                .is_ok()
            {
                format!("origin/{base_ref}")
            } else {
                base_ref.to_string()
            };
            runner
                .run(
                    "git",
                    &["-C", clone_path, "worktree", "add", "-b", branch, target_path, &resolved_base_ref],
                    Path::new("/"),
                    &ChannelLabel::Noop,
                )
                .await?;
        } else {
            runner
                .run("git", &["-C", clone_path, "worktree", "add", "--detach", target_path, branch], Path::new("/"), &ChannelLabel::Noop)
                .await?;
        }

        let commit = resolve_head_commit(&*runner, target_path).await?;
        if branch_provenance == CheckoutBranchProvenance::CreatedForConvoy {
            // Ownership belongs to the branch, not one checkout: sibling vessels
            // can share it and may finalize in either order.
            let commit = commit.as_deref().ok_or_else(|| format!("resolve bootstrap commit for {branch}"))?;
            runner
                .run("git", &["-C", clone_path, "update-ref", &bootstrap_branch_ref(branch), commit], Path::new("/"), &ChannelLabel::Noop)
                .await?;
        }
        Ok(PreparedCheckout { commit, branch_provenance })
    }

    async fn create_fresh_clone(
        &self,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        let runner = self.local_runner()?;
        let target_path = utf8_path(target_path)?;
        let clone_ref = base_ref.unwrap_or(branch);
        if clone_ref == "HEAD" {
            runner.run("git", &["clone", repo_url, target_path], Path::new("/"), &ChannelLabel::Noop).await?;
        } else {
            runner.run("git", &["clone", "--branch", clone_ref, repo_url, target_path], Path::new("/"), &ChannelLabel::Noop).await?;
        }
        if clone_ref != branch {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            let remote_exists = runner
                .run("git", &["-C", target_path, "show-ref", "--verify", "--quiet", &remote_ref], Path::new("/"), &ChannelLabel::Noop)
                .await
                .is_ok();
            if remote_exists {
                runner
                    .run(
                        "git",
                        &["-C", target_path, "switch", "-c", branch, "--track", &format!("origin/{branch}")],
                        Path::new("/"),
                        &ChannelLabel::Noop,
                    )
                    .await?;
            } else {
                runner.run("git", &["-C", target_path, "switch", "-c", branch], Path::new("/"), &ChannelLabel::Noop).await?;
            }
        }
        Ok(PreparedCheckout {
            commit: resolve_head_commit(&*runner, target_path).await?,
            branch_provenance: CheckoutBranchProvenance::PreExisting,
        })
    }

    async fn inspect_integration(&self, checkout: &ResourceObject<Checkout>) -> Result<CheckoutIntegrationStatus, String> {
        Ok(inspect_checkout_integration(&*self.local_runner()?, Path::new(self.checkout_path(checkout)?), self.checkout_branch(checkout))
            .await)
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        let runner = self.local_runner()?;
        match removal {
            CheckoutRemoval::FreshClone { target_path } => {
                runner.run("rm", &["-rf", utf8_path(target_path)?], Path::new("/"), &ChannelLabel::Noop).await?;
                Ok(CheckoutRemovalOutcome::Removed)
            }
            CheckoutRemoval::Worktree { clone_path, branch, target_path } => {
                let clone_path = utf8_path(clone_path)?;
                let target_path = utf8_path(target_path)?;
                let remove = runner
                    .run_output(
                        "git",
                        &["-C", clone_path, "worktree", "remove", "--force", target_path],
                        Path::new("/"),
                        &ChannelLabel::Noop,
                    )
                    .await?;
                if !remove.success && !remove.stderr.contains("is not a working tree") {
                    return Err(remove.stderr);
                }
                remove_empty_checkout_parents(clone_path, target_path).await?;

                let branch_ref = format!("refs/heads/{branch}");
                let bootstrap_ref = bootstrap_branch_ref(branch);
                let head = runner
                    .run_output("git", &["-C", clone_path, "rev-parse", "--verify", &branch_ref], Path::new("/"), &ChannelLabel::Noop)
                    .await?;
                if !head.success {
                    delete_ref(&*runner, clone_path, &bootstrap_ref).await?;
                    return Ok(CheckoutRemovalOutcome::Removed);
                }
                let bootstrap = runner
                    .run_output("git", &["-C", clone_path, "rev-parse", "--verify", &bootstrap_ref], Path::new("/"), &ChannelLabel::Noop)
                    .await?;
                if !bootstrap.success {
                    return Ok(CheckoutRemovalOutcome::PreservedBranch {
                        branch: branch.clone(),
                        reason: BranchPreservationReason::NotCreatedForConvoy,
                    });
                }
                if head.stdout.trim() != bootstrap.stdout.trim() {
                    delete_ref(&*runner, clone_path, &bootstrap_ref).await?;
                    return Ok(CheckoutRemovalOutcome::PreservedBranch {
                        branch: branch.clone(),
                        reason: BranchPreservationReason::CommitsPastBase,
                    });
                }

                let worktrees =
                    runner.run("git", &["-C", clone_path, "worktree", "list", "--porcelain"], Path::new("/"), &ChannelLabel::Noop).await?;
                if worktrees.lines().any(|line| line == format!("branch {branch_ref}")) {
                    return Ok(CheckoutRemovalOutcome::PreservedBranch {
                        branch: branch.clone(),
                        reason: BranchPreservationReason::CheckedOutElsewhere,
                    });
                }

                runner
                    .run("git", &["-C", clone_path, "branch", "--delete", "--force", branch], Path::new("/"), &ChannelLabel::Noop)
                    .await?;
                delete_ref(&*runner, clone_path, &bootstrap_ref).await?;
                Ok(CheckoutRemovalOutcome::Removed)
            }
        }
    }
}

fn bootstrap_branch_ref(branch: &str) -> String {
    format!("refs/flotilla/bootstrap/{branch}")
}

async fn delete_ref(runner: &dyn CommandRunner, clone_path: &str, reference: &str) -> Result<(), String> {
    runner.run("git", &["-C", clone_path, "update-ref", "-d", reference], Path::new("/"), &ChannelLabel::Noop).await?;
    Ok(())
}

async fn remove_empty_checkout_parents(clone_path: &str, target_path: &str) -> Result<(), String> {
    let Some(checkout_root) = Path::new(clone_path).parent() else {
        return Ok(());
    };
    let Some(mut parent) = Path::new(target_path).parent() else {
        return Ok(());
    };
    while parent != checkout_root && parent.starts_with(checkout_root) {
        match tokio::fs::remove_dir(parent).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) => return Err(format!("remove empty checkout parent {}: {error}", parent.display())),
        }
        let Some(next) = parent.parent() else {
            break;
        };
        parent = next;
    }
    Ok(())
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
    async fn ensure_session(
        &self,
        name: &str,
        spec: &flotilla_resources::TerminalSessionSpec,
        tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        let registry = self.registry_for_env(&spec.env_ref)?;
        let pool = registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))?;

        let cwd = ExecutionEnvironmentPath::new(&spec.cwd);
        let (command, env, crew, initial_message) = match &spec.source {
            TerminalSessionSource::Tool { command } => (command.clone(), Vec::new(), None, None),
            TerminalSessionSource::Agent { selector, brief, context, message } => {
                let requirement = CapabilityTable::seeded().resolve(&selector.capability)?.clone();
                let adapter = registry
                    .agent_adapters
                    .get(&requirement.adapter)
                    .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;
                adapter.prepare(&cwd, brief).await?;
                for copy_root in &brief.copies {
                    let copy_root = ExecutionEnvironmentPath::new(copy_root);
                    if copy_root != cwd {
                        adapter.prepare(&copy_root, brief).await?;
                    }
                }
                let plan = adapter.launch(&AgentLaunchRequest {
                    role: spec.role.clone(),
                    model: requirement.model.clone(),
                    brief: brief.clone(),
                })?;
                let crew_id = uuid::Uuid::new_v4().to_string();
                let crew = flotilla_resources::CrewSessionStatus::builder()
                    .id(crew_id.clone())
                    .adapter(requirement.adapter)
                    .maybe_model(requirement.model)
                    .stance(plan.stance)
                    .build();
                let mut env = plan.env;
                env.extend([
                    ("FLOTILLA_CREW_ID".to_string(), crew_id),
                    ("FLOTILLA_CONVOY".to_string(), context.convoy.clone()),
                    ("FLOTILLA_VESSEL".to_string(), context.vessel_ref.clone()),
                    ("FLOTILLA_CREW_ROLE".to_string(), spec.role.clone()),
                    ("FLOTILLA_NAMESPACE".to_string(), context.namespace.clone()),
                    ("FLOTILLA_TERMINAL_SESSION".to_string(), name.to_string()),
                ]);
                (plan.command, env, Some(crew), message.clone())
            }
        };

        if matches!(spec.source, TerminalSessionSource::Agent { .. })
            && pool.list_sessions().await?.iter().any(|session| session.session_name == name)
        {
            pool.kill_session(name).await?;
        }
        pool.ensure_session(name, &command, &cwd, &env, tags).await?;
        let delivered_message_id = initial_message.as_ref().map(|message| message.id.clone());
        if let Some(message) = initial_message {
            if let Err(err) = pool.deliver(name, &message.text, true).await {
                let _ = pool.kill_session(name).await;
                return Err(format!("deliver initial crew message: {err}"));
            }
        }
        Ok(TerminalRuntimeState::builder()
            .session_id(name.to_string())
            .maybe_pid(None)
            .started_at(Utc::now())
            .maybe_crew(crew)
            .launch_command(command)
            .maybe_delivered_message_id(delivered_message_id)
            .build())
    }

    async fn session_is_running(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<bool, String> {
        let pool = self.pool_for_spec(spec)?;
        if !pool.tracks_session_liveness() {
            return Ok(true);
        }
        let running = pool.list_sessions().await?.iter().any(|session| session.session_name == session_id);
        Ok(running)
    }

    async fn observe_attention(
        &self,
        session_id: &str,
        spec: &flotilla_resources::TerminalSessionSpec,
    ) -> Result<Option<flotilla_resources::TerminalAttention>, String> {
        let pool = self.pool_for_spec(spec)?;
        let Some(session) = pool.list_sessions().await?.into_iter().find(|session| session.session_name == session_id) else {
            return Ok(None);
        };
        let Some(activity) = session.screen_activity else { return Ok(None) };
        let state = match activity {
            ScreenActivity::Active => flotilla_resources::TerminalAttentionState::Working,
            ScreenActivity::Stable => flotilla_resources::TerminalAttentionState::Idle,
        };
        Ok(Some(flotilla_resources::TerminalAttention {
            state,
            as_of: Utc::now(),
            source: flotilla_resources::TerminalAttentionSource::Screen,
        }))
    }

    async fn deliver_message(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec, message: &str) -> Result<(), String> {
        self.pool_for_spec(spec)?.deliver(session_id, message, true).await
    }

    async fn kill_session(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        let pool = self.pool_for_spec(spec)?;
        if pool.tracks_session_liveness() {
            match pool.list_sessions().await {
                Ok(sessions) => {
                    let Some(session) = sessions.iter().find(|session| session.session_name == session_id) else {
                        return Ok(());
                    };
                    if session.status == TerminalStatus::Running {
                        if let TerminalSessionSource::Agent { context, .. } = &spec.source {
                            warn!(%session_id, convoy = %context.convoy, vessel = %context.vessel_ref, "convoy teardown is terminating an attached terminal session");
                        } else {
                            warn!(%session_id, "convoy teardown is terminating an attached terminal session");
                        }
                    }
                }
                Err(error) => warn!(%session_id, %error, "could not inspect terminal session before teardown; attempting kill"),
            }
        }
        pool.kill_session(session_id).await
    }

    async fn cleanup_session_artifacts(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        let TerminalSessionSource::Agent { selector, brief, .. } = &spec.source else {
            return Ok(());
        };
        let registry = self.registry_for_env(&spec.env_ref)?;
        let requirement = CapabilityTable::seeded().resolve(&selector.capability)?.clone();
        let adapter = registry
            .agent_adapters
            .get(&requirement.adapter)
            .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;

        let mut roots = BTreeSet::from([spec.cwd.clone()]);
        roots.extend(brief.copies.iter().cloned());
        for root in roots {
            adapter.cleanup(&ExecutionEnvironmentPath::new(root), brief).await?;
        }
        Ok(())
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

    fn pool_for_spec(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<Arc<dyn TerminalPool>, String> {
        let registry = self.registry_for_env(&spec.env_ref)?;
        registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))
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
    use std::{fs, process::Command as ProcessCommand, sync::Arc};

    use flotilla_core::{
        config::ConfigStore,
        daemon::DaemonHandle,
        in_process::DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE,
        providers::{
            discovery::{
                test_support::{fake_discovery_with_provider_set, git_process_discovery, FakeDiscoveryProviders, FakeTerminalPool},
                EnvironmentAssertion, EnvironmentBag,
            },
            ProcessCommandRunner,
        },
    };
    use flotilla_protocol::{Command, CommandAction, CommandValue, CrewCommandContext, DaemonEvent};
    use flotilla_resources::{
        Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec,
        CheckoutStatus as ResourceCheckoutStatus, ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, CrewSource, CrewSpec, LifecycleAuthority,
        ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PlacementPolicy, RepositorySpec, Selector, SqliteBackend, TerminalSession,
        TerminalSessionPhase, TypedResolver, VesselRequirement, WorkPhase, WorkflowTemplate, WorkflowTemplateSpec,
    };
    use tempfile::TempDir;

    use super::{test_git_repo::TestGitRepo, *};

    #[tokio::test]
    async fn checkout_runtime_creates_convoy_branch_from_snapshotted_base() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "feature/multi-repo",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");

        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert!(branch.status.success());
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/multi-repo");
    }

    #[tokio::test]
    async fn checkout_runtime_removes_zero_commit_worktree_without_git_or_directory_debris() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let convoy_dir = temp.path().join("checkout-root/convoy-a");
        let target = convoy_dir.join("flotilla.feature-cleanup");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let prepared = runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "feature/cleanup",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");
        assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);
        assert!(prepared.commit.is_some(), "worktree should resolve its initial commit");
        let removal = CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: "feature/cleanup".to_string(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        };
        assert_eq!(runtime.remove_checkout(&removal).await.expect("worktree should be removed"), CheckoutRemovalOutcome::Removed);

        let worktrees = ProcessCommand::new("git")
            .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "worktree", "list", "--porcelain"])
            .output()
            .expect("git should list worktrees");
        assert!(worktrees.status.success());
        assert!(!String::from_utf8(worktrees.stdout).expect("utf-8 worktree list").contains(target.to_str().expect("utf-8 target path")));
        assert!(!convoy_dir.exists(), "empty convoy directory should be removed");

        let branch = ProcessCommand::new("git")
            .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/feature/cleanup"])
            .status()
            .expect("git should inspect the branch");
        assert!(!branch.success(), "zero-commit convoy branch should be deleted");
    }

    #[tokio::test]
    async fn checkout_runtime_preserves_and_reports_branch_with_commits() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let convoy_dir = temp.path().join("checkout-root/convoy-a");
        let target = convoy_dir.join("feature-work/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let prepared = runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "feature/work",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");
        assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);
        assert!(prepared.commit.is_some(), "worktree should resolve its initial commit");
        fs::write(target.join("work.txt"), "real work\n").expect("work file should be written");
        assert!(ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "add", "work.txt"])
            .status()
            .expect("git add should run")
            .success());
        assert!(ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "commit", "-m", "real work"])
            .status()
            .expect("git commit should run")
            .success());

        let removal = CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: "feature/work".to_string(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        };
        assert_eq!(runtime.remove_checkout(&removal).await.expect("worktree should be removed"), CheckoutRemovalOutcome::PreservedBranch {
            branch: "feature/work".to_string(),
            reason: BranchPreservationReason::CommitsPastBase,
        });
        assert!(!convoy_dir.exists(), "empty convoy directory should be removed");

        let branch = ProcessCommand::new("git")
            .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/feature/work"])
            .status()
            .expect("git should inspect the branch");
        assert!(branch.success(), "convoy branch with commits should be preserved");
        let marker = ProcessCommand::new("git")
            .args([
                "-C",
                clone.path().to_str().expect("utf-8 clone path"),
                "show-ref",
                "--verify",
                "--quiet",
                &bootstrap_branch_ref("feature/work"),
            ])
            .status()
            .expect("git should inspect the ownership marker");
        assert!(!marker.success(), "ownership marker should be removed after preserving committed work");
    }

    #[tokio::test]
    async fn checkout_runtime_removes_a_shared_bootstrap_branch_in_either_teardown_order() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        for reverse_teardown in [false, true] {
            let case = if reverse_teardown { "reverse" } else { "forward" };
            let branch = format!("feature/shared-{case}");
            let workspace = temp.path().join(format!("workspace-{case}"));
            let targets = [workspace.join("first"), workspace.join("second")];
            for (index, target) in targets.iter().enumerate() {
                let prepared = runtime
                    .create_worktree(
                        clone.path().to_str().expect("utf-8 clone path"),
                        &branch,
                        Some("main"),
                        target.to_str().expect("utf-8 target path"),
                    )
                    .await
                    .expect("worktree should create");
                let expected = if index == 0 { CheckoutBranchProvenance::CreatedForConvoy } else { CheckoutBranchProvenance::PreExisting };
                assert_eq!(prepared.branch_provenance, expected, "only the creating checkout should record convoy provenance");
            }

            let removals = targets.each_ref().map(|target| CheckoutRemoval::Worktree {
                clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
                branch: branch.clone(),
                target_path: target.to_str().expect("utf-8 target path").to_string(),
            });
            let order = if reverse_teardown { [1, 0] } else { [0, 1] };
            assert_eq!(
                runtime.remove_checkout(&removals[order[0]]).await.expect("first worktree should be removed"),
                CheckoutRemovalOutcome::PreservedBranch { branch: branch.clone(), reason: BranchPreservationReason::CheckedOutElsewhere }
            );
            assert_eq!(
                runtime.remove_checkout(&removals[order[1]]).await.expect("last worktree should be removed"),
                CheckoutRemovalOutcome::Removed
            );
            assert!(!workspace.exists(), "empty workspace directory should be removed");

            for reference in [format!("refs/heads/{branch}"), bootstrap_branch_ref(&branch)] {
                let reference = ProcessCommand::new("git")
                    .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", &reference])
                    .status()
                    .expect("git should inspect the reference");
                assert!(!reference.success(), "zero-commit branch and ownership marker should be deleted");
            }
        }
    }

    #[tokio::test]
    async fn checkout_runtime_does_not_contact_origin_for_an_existing_local_branch() {
        let temp = TempDir::new().expect("tempdir");
        let missing_origin = temp.path().join("missing-origin.git");
        let clone = TestGitRepo::init(temp.path().join("clone"))
            .with_initial_commit()
            .with_origin(missing_origin.to_str().expect("utf-8 origin path"));
        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let prepared = runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "main",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("local branch should not require its origin");
        assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::PreExisting);

        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert!(branch.status.success());
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "main");

        let removal = CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: "main".to_string(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        };
        assert_eq!(runtime.remove_checkout(&removal).await.expect("worktree should be removed"), CheckoutRemovalOutcome::PreservedBranch {
            branch: "main".to_string(),
            reason: BranchPreservationReason::NotCreatedForConvoy,
        });
        let branch = ProcessCommand::new("git")
            .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/main"])
            .status()
            .expect("git should inspect the branch");
        assert!(branch.success(), "pre-existing local branch should be preserved");
    }

    #[tokio::test]
    async fn checkout_runtime_resolves_a_remote_only_snapshotted_base() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let source_path = source.path().to_str().expect("utf-8 source path");
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "switch", "-c", "stable"])
            .status()
            .expect("git switch should run")
            .success());
        fs::write(source.path().join("stable.txt"), "stable base\n").expect("write stable file");
        assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "stable.txt"]).status().expect("git add should run").success());
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "commit", "-m", "stable commit"])
            .status()
            .expect("git commit should run")
            .success());
        assert!(ProcessCommand::new("git").args(["-C", source_path, "switch", "main"]).status().expect("git switch should run").success());
        let clone_path = temp.path().join("clone");
        assert!(ProcessCommand::new("git")
            .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
            .status()
            .expect("git clone should run")
            .success());
        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_worktree(
                clone_path.to_str().expect("utf-8 clone path"),
                "feature/remote-base",
                Some("stable"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");

        assert_eq!(fs::read_to_string(target.join("stable.txt")).expect("stable file should exist"), "stable base\n");
        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/remote-base");
    }

    #[tokio::test]
    async fn checkout_runtime_attaches_an_existing_remote_convoy_branch() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let source_path = source.path().to_str().expect("utf-8 source path");
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "switch", "-c", "feature/existing"])
            .status()
            .expect("git switch should run")
            .success());
        fs::write(source.path().join("feature.txt"), "existing branch\n").expect("write feature file");
        assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "feature.txt"]).status().expect("git add should run").success());
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "commit", "-m", "feature commit"])
            .status()
            .expect("git commit should run")
            .success());
        assert!(ProcessCommand::new("git").args(["-C", source_path, "switch", "main"]).status().expect("git switch should run").success());
        let clone_path = temp.path().join("clone");
        assert!(ProcessCommand::new("git")
            .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
            .status()
            .expect("git clone should run")
            .success());
        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_worktree(
                clone_path.to_str().expect("utf-8 clone path"),
                "feature/existing",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");

        assert_eq!(fs::read_to_string(target.join("feature.txt")).expect("feature file should exist"), "existing branch\n");
        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/existing");
    }

    #[tokio::test]
    async fn checkout_runtime_fetches_a_convoy_branch_created_after_the_clone() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let source_path = source.path().to_str().expect("utf-8 source path");
        let clone_path = temp.path().join("clone");
        assert!(ProcessCommand::new("git")
            .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
            .status()
            .expect("git clone should run")
            .success());

        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "switch", "-c", "feature/created-later"])
            .status()
            .expect("git switch should run")
            .success());
        fs::write(source.path().join("created-later.txt"), "remote branch\n").expect("write feature file");
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "add", "created-later.txt"])
            .status()
            .expect("git add should run")
            .success());
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "commit", "-m", "later branch commit"])
            .status()
            .expect("git commit should run")
            .success());

        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };
        runtime
            .create_worktree(
                clone_path.to_str().expect("utf-8 clone path"),
                "feature/created-later",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("worktree should create");

        assert_eq!(fs::read_to_string(target.join("created-later.txt")).expect("feature file should exist"), "remote branch\n");
        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/created-later");
    }

    #[tokio::test]
    async fn fresh_clone_checkout_creates_convoy_branch_from_snapshotted_base() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("fresh-clone");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/multi-repo",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("fresh clone should create");

        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert!(branch.status.success());
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/multi-repo");
    }

    #[tokio::test]
    async fn fresh_clone_checkout_treats_head_as_the_remote_default() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("fresh-clone");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/from-head",
                Some("HEAD"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("fresh clone should create");

        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should run");
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/from-head");
    }

    #[tokio::test]
    async fn fresh_clone_checkout_preserves_an_existing_convoy_branch() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let source_path = source.path().to_str().expect("utf-8 source path");
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "switch", "-c", "feature/existing"])
            .status()
            .expect("git switch should run")
            .success());
        fs::write(source.path().join("feature.txt"), "existing branch\n").expect("write feature file");
        assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "feature.txt"]).status().expect("git add should run").success());
        assert!(ProcessCommand::new("git")
            .args(["-C", source_path, "commit", "-m", "feature commit"])
            .status()
            .expect("git commit should run")
            .success());
        let target = temp.path().join("fresh-clone");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        runtime
            .create_fresh_clone(source_path, "feature/existing", Some("main"), target.to_str().expect("utf-8 target path"))
            .await
            .expect("fresh clone should create");

        assert_eq!(fs::read_to_string(target.join("feature.txt")).expect("feature file should be checked out"), "existing branch\n");
    }

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

    #[tokio::test]
    async fn startup_seeding_preserves_existing_contained_template_definition() {
        let backend = ResourceBackend::InMemory(Default::default());
        let templates = backend.clone().using::<WorkflowTemplate>(NAMESPACE);
        let custom = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("custom".to_string())
                .stance(Stance::Contained)
                .crew(vec![CrewSpec::builder()
                    .role("maintainer".to_string())
                    .source(CrewSource::Agent {
                        selector: Selector { capability: "custom-code".to_string() },
                        prompt: Some("Keep this definition".to_string()),
                    })
                    .build()])
                .build()])
            .build();
        templates.create(&empty_meta("single-agent-contained"), &custom).await.expect("custom template create should succeed");

        ensure_default_workflow_templates(&backend, NAMESPACE).await.expect("startup seeding should succeed");
        ensure_default_workflow_templates(&backend, NAMESPACE).await.expect("restart seeding should succeed");

        let preserved = templates.get("single-agent-contained").await.expect("template should remain");
        assert_eq!(preserved.spec, custom);
    }

    fn manual_profile(host_id: &str, docker_available: bool) -> LocalProvisioningProfile {
        LocalProvisioningProfile {
            host_id: host_id.to_string(),
            repo_default_dir: "/Users/tester/dev/flotilla-repos".to_string(),
            host_direct_pool: "passthrough".to_string(),
            docker_pool: "passthrough".to_string(),
            available_pools: vec!["passthrough".to_string()],
            available_agent_adapters: BTreeSet::new(),
            docker_available,
        }
    }

    async fn daemon_with_backend(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>, backend: ResourceBackend) -> Arc<InProcessDaemon> {
        let daemon = InProcessDaemon::new_with_resource_backend(
            tracked_repos,
            config,
            git_process_discovery(false),
            flotilla_protocol::HostName::new("test-host"),
            backend,
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

    async fn in_memory_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
        daemon_with_backend(tracked_repos, config, ResourceBackend::InMemory(Default::default())).await
    }

    async fn sqlite_daemon(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>) -> Arc<InProcessDaemon> {
        std::fs::create_dir_all(config.state_dir()).expect("state dir");
        let backend = ResourceBackend::Sqlite(SqliteBackend::open(config.state_dir().join("resources.sqlite")).expect("sqlite backend"));
        daemon_with_backend(tracked_repos, config, backend).await
    }

    async fn crew_daemon(config: Arc<ConfigStore>) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
        let pool = Arc::new(FakeTerminalPool::new());
        let discovery = fake_discovery_with_provider_set(
            FakeDiscoveryProviders::new()
                .with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_core::providers::terminal::TerminalPool>),
        );
        let daemon = InProcessDaemon::new(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy")).await;
        daemon
            .replace_local_environment_bag_for_test(
                EnvironmentBag::new()
                    .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                    .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
                    .with(EnvironmentAssertion::binary("codex", "/tools/codex"))
                    .with(EnvironmentAssertion::binary("claude", "/tools/claude")),
            )
            .expect("crew environment bag");
        (daemon, pool)
    }

    async fn crew_daemon_with_process_runner(config: Arc<ConfigStore>) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
        let pool = Arc::new(FakeTerminalPool::new());
        let mut discovery = fake_discovery_with_provider_set(
            FakeDiscoveryProviders::new()
                .with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_core::providers::terminal::TerminalPool>),
        );
        discovery.runner = Arc::new(ProcessCommandRunner);
        let daemon = InProcessDaemon::new(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy")).await;
        daemon
            .replace_local_environment_bag_for_test(
                EnvironmentBag::new()
                    .with(EnvironmentAssertion::env_var("HOME", "/Users/tester"))
                    .with(EnvironmentAssertion::binary("git", "/usr/bin/git"))
                    .with(EnvironmentAssertion::binary("codex", "/tools/codex"))
                    .with(EnvironmentAssertion::binary("claude", "/tools/claude")),
            )
            .expect("crew environment bag");
        (daemon, pool)
    }

    async fn run_stage4a_flow_reaches_running_and_completes_convoy(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        repo_default_dir: PathBuf,
        repo: PathBuf,
    ) {
        std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
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
        let controller_handles =
            spawn_controller_loops(Arc::clone(&state), NAMESPACE, Duration::from_millis(25), ControllerSupervision::default());

        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("wf-a"),
                &WorkflowTemplateSpec::builder()
                    .inputs(Vec::new())
                    .vessels(vec![VesselRequirement::builder()
                        .name("implement".to_string())
                        .crew(vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "bash -lc 'echo stage4a'".to_string() })
                            .build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("workflow template create should succeed");
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("repository spec");
        let repository_key = repository_spec.key();
        flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(NAMESPACE), &repository_key, &repository_spec)
            .await
            .expect("repository create should succeed");
        backend
            .clone()
            .using::<Convoy>(NAMESPACE)
            .create(&empty_meta("convoy-a"), &ConvoySpec {
                workflow_ref: "wf-a".to_string(),
                inputs: BTreeMap::new(),
                placement_policy: Some(format!("host-direct-{host_id}")),
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla.git".to_string(),
                    repo_ref: repository_key,
                    base_ref: "main".to_string(),
                    workspace_slug: repository_spec.leaf_slug(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("main".to_string()),
                project_ref: None,
                adopted_checkout_refs: BTreeMap::new(),
                issue: None,
                instruction: None,
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
                        && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
            ) {
                break;
            }
            if tokio::time::Instant::now() >= run_deadline {
                let convoy = convoys.get("convoy-a").await.expect("convoy should exist");
                let workspace = backend.clone().using::<Vessel>(NAMESPACE).list().await.expect("workspace list should succeed");
                panic!("convoy did not reach running state: convoy={convoy:?} vessels={workspace:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let host = backend.clone().using::<Host>(NAMESPACE).get(&host_id).await.expect("host should exist after startup");
        assert!(host.status.is_some(), "startup heartbeat should publish host status");

        let workspaces = backend.clone().using::<Vessel>(NAMESPACE);
        let sqlite_path = config.state_dir().as_path().join("resources.sqlite");
        let idle_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut previous_idle_sample = None;
        loop {
            let workspace = workspaces.get("convoy-a-implement").await.expect("steady workspace should remain");
            let sample = (
                workspace.metadata.resource_version,
                workspace.status.expect("steady workspace status").ready_at,
                sqlite_path.exists().then(|| sqlite_max_event_rowid(&sqlite_path)),
            );
            if previous_idle_sample.as_ref() == Some(&sample) {
                break;
            }
            assert!(tokio::time::Instant::now() < idle_deadline, "resource store did not reach an idle fixed point");
            previous_idle_sample = Some(sample);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-a".to_string(),
                    work: "implement".to_string(),
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
                            && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
                )
            }
        })
        .await;

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
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

    fn sqlite_max_event_rowid(path: &Path) -> u64 {
        let connection = rusqlite::Connection::open(path).expect("open SQLite store for idle inspection");
        connection
            .query_row("SELECT COALESCE(MAX(rowid), 0) FROM resource_events", [], |row| row.get(0))
            .expect("read maximum resource event rowid")
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

    async fn wait_for_command_result(rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>, command_id: u64) -> CommandValue {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => break result,
                    Ok(_) => {}
                    Err(err) => panic!("unexpected event error: {err}"),
                }
            }
        })
        .await
        .expect("timed out waiting for command result")
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
        assert_eq!(status.agent_adapters().expect("valid agent adapter capability"), BTreeSet::new());
        assert_eq!(status.capabilities.get("docker"), Some(&json!(false)));
        assert_eq!(status.capabilities.get("terminal_pools"), Some(&json!(["passthrough"])));
        assert!(
            status.resource_store.expect("heartbeat should publish resource store diagnostics").event_log_within_retention(),
            "heartbeat should report a bounded resource event log"
        );

        heartbeat.abort();
        let _ = heartbeat.await;
    }

    #[tokio::test(start_paused = true)]
    async fn adopted_checkout_reconciliation_task_runs_after_interval() {
        let temp = TempDir::new().expect("tempdir");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = in_memory_daemon(Vec::new(), config).await;
        let durable = daemon.resource_backend().using::<ResourceCheckout>(NAMESPACE);
        let created = durable
            .create(
                &InputMeta::builder()
                    .name("adopted-checkout-periodic".to_string())
                    .build()
                    .with_lifecycle_authority(LifecycleAuthority::Adopted),
                &ResourceCheckoutSpec::Observed(
                    ResourceObservedCheckoutSpec::builder()
                        .r#ref("feature/periodic".to_string())
                        .path("/work/periodic".to_string())
                        .repo_ref(flotilla_resources::RepositoryKey("widgets-api".to_string()))
                        .host_ref("host-01".to_string())
                        .is_main(false)
                        .build(),
                ),
            )
            .await
            .expect("durable adopted checkout should be created");
        durable
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).path("/work/periodic".to_string()).build(),
            )
            .await
            .expect("durable checkout status should be stored");
        let interval = Duration::from_secs(60);
        let reconciliation = spawn_adopted_checkout_reconciliation_task(Arc::clone(&daemon), NAMESPACE.to_string(), interval);
        tokio::task::yield_now().await;
        let observed = daemon.observed_resource_backend().using::<ResourceCheckout>(NAMESPACE);
        assert!(
            matches!(observed.get("adopted-checkout-periodic").await, Err(ResourceError::NotFound { .. })),
            "the periodic task should wait for its first interval"
        );

        tokio::time::advance(interval).await;
        tokio::task::yield_now().await;

        observed.get("adopted-checkout-periodic").await.expect("periodic reconciliation should restore the observed checkout");
        reconciliation.abort();
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
                &flotilla_resources::canonicalize_repo_url("https://github.com/flotilla-org/flotilla.git").expect("canonical url"),
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
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();
        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = in_memory_daemon(vec![repo.clone()], Arc::clone(&config)).await;
        run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, repo).await;
    }

    #[tokio::test]
    async fn sqlite_stage4a_flow_reaches_running_and_completes_convoy() {
        let temp = TempDir::new().expect("tempdir");
        let repo_default_dir = temp.path().join("flotilla-repos");
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();
        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = sqlite_daemon(vec![repo.clone()], Arc::clone(&config)).await;
        run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, repo).await;
    }

    #[tokio::test]
    async fn starting_agent_replaces_a_pool_session_left_by_a_previous_daemon_runtime() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join("config");
        std::fs::create_dir_all(&config_path).expect("config dir");
        std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
        let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
        let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            local_registry,
            None,
            profile.host_id.clone(),
            None,
            profile.host_direct_environment_name(),
        ));
        let session_name = "terminal-demo-implement-coder";
        pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession {
            session_name: session_name.to_string(),
            status: flotilla_protocol::TerminalStatus::Running,
            command: Some("old codex process with stale crew identity".to_string()),
            working_directory: Some(ExecutionEnvironmentPath::new("/repo")),
            screen_activity: None,
        }])
        .await;
        let runtime = TerminalControllerRuntime { state };
        let session_cwd = temp.path().join("session-cwd");
        std::fs::create_dir_all(&session_cwd).expect("session cwd");
        let durable_checkout = temp.path().join("durable-checkout");
        std::fs::create_dir_all(&durable_checkout).expect("durable checkout dir");
        let spec = flotilla_resources::TerminalSessionSpec {
            env_ref: profile.host_direct_environment_name(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Agent {
                selector: Selector { capability: "coding".to_string() },
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".to_string(),
                    content: "Implement the issue.".to_string(),
                    copies: vec![durable_checkout.display().to_string()],
                },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: NAMESPACE.to_string(),
                    convoy: "demo".to_string(),
                    vessel_ref: "demo-implement".to_string(),
                },
                message: None,
            },
            cwd: session_cwd.display().to_string(),
            pool: "fake-terminals".to_string(),
        };

        let launched = runtime.ensure_session(session_name, &spec, &[]).await.expect("replace stale session");

        assert_eq!(pool.killed.lock().await.as_slice(), &[session_name.to_string()]);
        assert_eq!(pool.ensured.lock().await.len(), 1, "the fresh agent command must actually be launched");
        assert!(launched.crew.is_some(), "the replacement gets a fresh crew identity");
        assert_eq!(
            std::fs::read_to_string(durable_checkout.join(".flotilla/briefs/coder.md")).expect("durable brief copy"),
            "Implement the issue."
        );

        runtime.cleanup_session_artifacts(&spec).await.expect("cleanup generated briefs");
        assert!(!session_cwd.join(".flotilla/briefs/coder.md").exists(), "session brief should be removed");
        assert!(!durable_checkout.join(".flotilla/briefs/coder.md").exists(), "durable brief copy should be removed");
        assert!(!durable_checkout.join(".flotilla/briefs").exists(), "empty durable brief directory should be removed");
    }

    #[tokio::test]
    async fn terminal_teardown_kills_a_persisted_session_after_runtime_restart() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join("config");
        std::fs::create_dir_all(&config_path).expect("config dir");
        std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
        let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
        let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
        let runtime = TerminalControllerRuntime {
            state: Arc::new(ControllerRuntimeState::new(
                Arc::clone(&daemon),
                config,
                local_registry,
                None,
                profile.host_id.clone(),
                None,
                profile.host_direct_environment_name(),
            )),
        };
        let session_name = "terminal-demo-implement-coder";
        let spec = flotilla_resources::TerminalSessionSpec {
            env_ref: profile.host_direct_environment_name(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/repo".to_string(),
            pool: "fake-terminals".to_string(),
        };
        pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession::builder()
            .session_name(session_name.to_string())
            .status(TerminalStatus::Running)
            .command("codex".to_string())
            .working_directory(ExecutionEnvironmentPath::new("/repo"))
            .build()])
            .await;

        runtime.kill_session(session_name, &spec).await.expect("teardown should resolve the persisted session pool");

        assert_eq!(pool.killed.lock().await.as_slice(), &[session_name.to_string()]);
    }

    #[tokio::test]
    async fn terminal_teardown_skips_a_session_absent_from_the_pool() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join("config");
        std::fs::create_dir_all(&config_path).expect("config dir");
        std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let (daemon, pool) = crew_daemon_with_process_runner(Arc::clone(&config)).await;
        let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
        let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
        let runtime = TerminalControllerRuntime {
            state: Arc::new(ControllerRuntimeState::new(
                Arc::clone(&daemon),
                config,
                local_registry,
                None,
                profile.host_id.clone(),
                None,
                profile.host_direct_environment_name(),
            )),
        };
        let spec = flotilla_resources::TerminalSessionSpec {
            env_ref: profile.host_direct_environment_name(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
            cwd: "/repo".to_string(),
            pool: "fake-terminals".to_string(),
        };

        runtime.kill_session("terminal-demo-implement-coder", &spec).await.expect("missing sessions should be idempotent");

        assert!(pool.killed.lock().await.is_empty(), "teardown must not invoke the pool for an already-gone session");
    }

    #[tokio::test]
    async fn crew_provisioning_runs_coder_reviewer_handoffs_and_rejects_unknown_capabilities() {
        let temp = TempDir::new().expect("tempdir");
        let repo = TestGitRepo::init(temp.path().join("repo"))
            .with_initial_commit()
            .with_origin("git@github.com:flotilla-org/flotilla.git")
            .path()
            .to_path_buf();
        let config_path = temp.path().join("config");
        std::fs::create_dir_all(&config_path).expect("config dir");
        std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let (daemon, pool) = crew_daemon(Arc::clone(&config)).await;
        let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry");
        assert!(local_registry.agent_adapters.get("codex").is_some());
        assert!(local_registry.agent_adapters.get("claude-code").is_some());
        let profile = build_local_profile(&daemon, &local_registry).expect("local profile");
        let backend = daemon.resource_backend();

        register_startup_resources(&daemon, NAMESPACE, &profile).await.expect("startup resources");
        apply_host_heartbeat(&daemon, NAMESPACE, &profile).await.expect("host heartbeat");
        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            local_registry,
            None,
            profile.host_id.clone(),
            Some(ExecutionEnvironmentPath::new(&repo)),
            profile.host_direct_environment_name(),
        ));
        let controller_handles =
            spawn_controller_loops(Arc::clone(&state), NAMESPACE, Duration::from_millis(20), ControllerSupervision::default());

        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("crew-workflow"),
                &WorkflowTemplateSpec::builder()
                    .inputs(Vec::new())
                    .vessels(vec![VesselRequirement::builder()
                        .name("implement".to_string())
                        .crew(vec![
                            CrewSpec::builder()
                                .role("coder".to_string())
                                .source(CrewSource::Agent {
                                    selector: Selector { capability: "coding".to_string() },
                                    prompt: Some(
                                        "Implement issue 668 without leaking this full brief into the launch command.".to_string(),
                                    ),
                                })
                                .build(),
                            CrewSpec::builder()
                                .role("reviewer".to_string())
                                .source(CrewSource::Agent {
                                    selector: Selector { capability: "review".to_string() },
                                    prompt: Some("Review the coder's work.".to_string()),
                                })
                                .build(),
                            CrewSpec::builder()
                                .role("watcher".to_string())
                                .source(CrewSource::Tool { command: "cargo test --watch".to_string() })
                                .build(),
                        ])
                        .build()])
                    .build(),
            )
            .await
            .expect("crew workflow");

        let mut rx = daemon.subscribe();
        let create_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "crew-convoy".to_string(),
                    workflow_ref: "crew-workflow".to_string(),
                    inputs: Vec::new(),
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".to_string()),
                    r#ref: Some("main".to_string()),
                    project_ref: None,
                    placement_policy: Some(profile.host_direct_policy_name()),
                    adopted_checkout: Some(Box::new(repo.clone())),
                },
            })
            .await
            .expect("create crew convoy");
        assert_eq!(wait_for_command_result(&mut rx, create_id).await, CommandValue::ConvoyCreated { name: "crew-convoy".to_string() });

        let convoys = backend.clone().using::<Convoy>(NAMESPACE);
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                matches!(
                    convoys.get("crew-convoy").await.ok().and_then(|convoy| convoy.status).as_ref(),
                    Some(status)
                        if status.phase == ConvoyPhase::Active
                            && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
                )
            }
        })
        .await;

        let terminals = backend.clone().using::<TerminalSession>(NAMESPACE);
        let coder = terminals
            .list()
            .await
            .expect("terminal list")
            .items
            .into_iter()
            .find(|session| session.spec.role == "coder")
            .expect("coder session");
        let coder_id = coder.status.as_ref().and_then(|status| status.crew.as_ref()).expect("coder identity").id.clone();
        assert_eq!(coder.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.adapter.as_str()), Some("codex"));
        assert!(terminals.list().await.expect("terminal list").items.iter().any(|session| session.spec.role == "watcher"));
        assert!(terminals.list().await.expect("terminal list").items.iter().all(|session| session.spec.role != "reviewer"));
        let ensured = pool.ensured.lock().await;
        let coder_launch = ensured.iter().find(|launch| launch.session_name.ends_with("-coder")).expect("coder launch");
        assert!(coder_launch.command.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(!coder_launch.command.contains("without leaking this full brief"));
        assert!(coder_launch.env_vars.iter().any(|(key, value)| key == "FLOTILLA_CREW_ID" && value == &coder_id));
        drop(ensured);

        let crew_context = CrewCommandContext { crew_id: Some(coder_id.clone()), ..Default::default() };
        let crew_list = daemon
            .execute_query(
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryCrewList { context: crew_context.clone() },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("crew list");
        let CommandValue::CrewList(crew_list) = crew_list else { panic!("expected crew list") };
        assert_eq!(crew_list.members.iter().map(|member| (member.role.as_str(), member.state.as_str())).collect::<Vec<_>>(), vec![
            ("coder", "active"),
            ("reviewer", "latent"),
            ("watcher", "active")
        ]);
        let initial_status = convoys.get("crew-convoy").await.expect("crew convoy").status.expect("convoy status");
        assert_eq!(initial_status.crew_work["implement"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
        assert_eq!(initial_status.crew_work["implement"]["reviewer"].phase, flotilla_resources::CrewWorkPhase::Working);
        assert!(!initial_status.crew_work["implement"].contains_key("watcher"));

        let mut rx = daemon.subscribe();
        let coder_complete_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewComplete { context: crew_context.clone(), message: Some("implementation ready".to_string()) },
            })
            .await
            .expect("coder complete");
        assert_eq!(wait_for_command_result(&mut rx, coder_complete_id).await, CommandValue::Ok);

        let mut rx = daemon.subscribe();
        let handoff_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewHandoff {
                    context: crew_context.clone(),
                    target: "reviewer".to_string(),
                    message: "Review commit abc123".to_string(),
                },
            })
            .await
            .expect("handoff reviewer");
        assert_eq!(wait_for_command_result(&mut rx, handoff_id).await, CommandValue::Ok);
        wait_until(|| {
            let terminals = terminals.clone();
            async move {
                terminals
                    .list()
                    .await
                    .ok()
                    .and_then(|list| list.items.into_iter().find(|session| session.spec.role == "reviewer"))
                    .and_then(|session| session.status)
                    .is_some_and(|status| status.phase == TerminalSessionPhase::Running)
            }
        })
        .await;
        let reviewer = terminals
            .list()
            .await
            .expect("terminal list")
            .items
            .into_iter()
            .find(|session| session.spec.role == "reviewer")
            .expect("reviewer session");
        let reviewer_id = reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).expect("reviewer identity").id.clone();
        assert_eq!(reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.adapter.as_str()), Some("claude-code"));
        let delivered = pool.delivered.lock().await;
        assert!(delivered.iter().any(|(session, text, submit)| {
            session.ends_with("-reviewer") && text == "handoff from coder@implement\n\nReview commit abc123" && *submit
        }));
        drop(delivered);

        let mut rx = daemon.subscribe();
        let reviewer_complete_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some(reviewer_id.clone()), ..Default::default() },
                    message: Some("initial review complete".to_string()),
                },
            })
            .await
            .expect("reviewer complete");
        assert_eq!(wait_for_command_result(&mut rx, reviewer_complete_id).await, CommandValue::Ok);
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                convoys
                    .get("crew-convoy")
                    .await
                    .ok()
                    .and_then(|convoy| convoy.status)
                    .is_some_and(|status| status.phase == ConvoyPhase::Completed)
            }
        })
        .await;

        let mut rx = daemon.subscribe();
        let hand_back_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewHandoff {
                    context: CrewCommandContext { crew_id: Some(reviewer_id.clone()), ..Default::default() },
                    target: "coder".to_string(),
                    message: "Address the review findings".to_string(),
                },
            })
            .await
            .expect("hand back to coder");
        assert_eq!(wait_for_command_result(&mut rx, hand_back_id).await, CommandValue::Ok);
        let delivered = pool.delivered.lock().await;
        assert!(delivered.iter().any(|(session, text, submit)| {
            session.ends_with("-coder") && text == "handoff from reviewer@implement\n\nAddress the review findings" && *submit
        }));
        drop(delivered);
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                convoys.get("crew-convoy").await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
                    status.phase == ConvoyPhase::Active && status.work.get("implement").is_some_and(|work| work.phase == WorkPhase::Running)
                })
            }
        })
        .await;
        let reopened = convoys.get("crew-convoy").await.expect("reopened convoy").status.expect("reopened status");
        assert_eq!(reopened.crew_work["implement"]["coder"].phase, flotilla_resources::CrewWorkPhase::Working);
        assert_eq!(reopened.crew_work["implement"]["reviewer"].phase, flotilla_resources::CrewWorkPhase::HandedBack);

        pool.remove_session(&coder.metadata.name).await;
        wait_until(|| {
            let terminals = terminals.clone();
            let name = coder.metadata.name.clone();
            async move {
                terminals
                    .get(&name)
                    .await
                    .ok()
                    .and_then(|session| session.status)
                    .is_some_and(|status| status.phase == TerminalSessionPhase::Stopped)
            }
        })
        .await;
        assert!(matches!(
            convoys.get("crew-convoy").await.expect("crew convoy").status.and_then(|status| status.work.get("implement").cloned()),
            Some(task) if task.phase == WorkPhase::Running
        ));
        let stopped_list = daemon
            .execute_query(
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::QueryCrewList {
                        context: CrewCommandContext { crew_id: Some(reviewer_id.clone()), ..Default::default() },
                    },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("stopped crew list");
        let CommandValue::CrewList(stopped_list) = stopped_list else { panic!("expected stopped crew list") };
        assert_eq!(stopped_list.members.iter().find(|member| member.role == "coder").map(|member| member.state.as_str()), Some("stopped"));
        let mut rx = daemon.subscribe();
        let revive_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewHandoff {
                    context: CrewCommandContext { crew_id: Some(reviewer_id.clone()), ..Default::default() },
                    target: "coder".to_string(),
                    message: "Resume after review".to_string(),
                },
            })
            .await
            .expect("revive coder");
        assert_eq!(wait_for_command_result(&mut rx, revive_id).await, CommandValue::Ok);
        wait_until(|| {
            let terminals = terminals.clone();
            let name = coder.metadata.name.clone();
            let old_id = coder_id.clone();
            async move {
                terminals
                    .get(&name)
                    .await
                    .ok()
                    .and_then(|session| session.status)
                    .and_then(|status| status.crew)
                    .is_some_and(|crew| crew.id != old_id)
            }
        })
        .await;
        let revived_coder = terminals.get(&coder.metadata.name).await.expect("revived coder");
        let revived_coder_id = revived_coder.status.as_ref().and_then(|status| status.crew.as_ref()).expect("revived identity").id.clone();

        let attach = daemon.resolve_attach_command_internal("crew-convoy/implement/coder").await.expect("attach coder");
        assert!(attach.command.contains("attach terminal-crew-convoy-implement-coder"));

        let mut rx = daemon.subscribe();
        let coder_recomplete_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some(revived_coder_id.clone()), ..Default::default() },
                    message: Some("review findings addressed".to_string()),
                },
            })
            .await
            .expect("coder re-complete");
        assert_eq!(wait_for_command_result(&mut rx, coder_recomplete_id).await, CommandValue::Ok);

        let mut rx = daemon.subscribe();
        let return_to_reviewer_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewHandoff {
                    context: CrewCommandContext { crew_id: Some(revived_coder_id), ..Default::default() },
                    target: "reviewer".to_string(),
                    message: "Please verify the fixes".to_string(),
                },
            })
            .await
            .expect("return to reviewer");
        assert_eq!(wait_for_command_result(&mut rx, return_to_reviewer_id).await, CommandValue::Ok);

        let mut rx = daemon.subscribe();
        let final_review_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CrewComplete {
                    context: CrewCommandContext { crew_id: Some(reviewer_id), ..Default::default() },
                    message: Some("changes accepted".to_string()),
                },
            })
            .await
            .expect("final reviewer completion");
        assert_eq!(wait_for_command_result(&mut rx, final_review_id).await, CommandValue::Ok);
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                convoys
                    .get("crew-convoy")
                    .await
                    .ok()
                    .and_then(|convoy| convoy.status)
                    .is_some_and(|status| status.phase == ConvoyPhase::Completed)
            }
        })
        .await;
        let completed = convoys.get("crew-convoy").await.expect("completed convoy").status.expect("completed status");
        assert_eq!(completed.work["implement"].phase, WorkPhase::Complete);
        assert!(completed.crew_work["implement"].values().all(|state| state.phase == flotilla_resources::CrewWorkPhase::Done));

        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("unknown-capability"),
                &WorkflowTemplateSpec::builder()
                    .inputs(Vec::new())
                    .vessels(vec![VesselRequirement::builder()
                        .name("implement".to_string())
                        .crew(vec![CrewSpec::builder()
                            .role("architect".to_string())
                            .source(CrewSource::Agent { selector: Selector { capability: "architect".to_string() }, prompt: None })
                            .build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("unknown capability workflow");
        let mut rx = daemon.subscribe();
        let create_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "unknown-convoy".to_string(),
                    workflow_ref: "unknown-capability".to_string(),
                    inputs: Vec::new(),
                    repository_url: Some("https://github.com/flotilla-org/flotilla.git".to_string()),
                    r#ref: Some("main".to_string()),
                    project_ref: None,
                    placement_policy: Some(profile.host_direct_policy_name()),
                    adopted_checkout: Some(Box::new(repo)),
                },
            })
            .await
            .expect("create unknown convoy");
        assert_eq!(wait_for_command_result(&mut rx, create_id).await, CommandValue::Error {
            message: "unknown agent capability `architect`".to_string()
        });
        assert!(convoys.get("unknown-convoy").await.is_err(), "rejected convoy should not be persisted");

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    #[tokio::test]
    async fn sqlite_adopted_checkout_flow_reaches_running_and_preserves_checkout_on_complete() {
        let temp = TempDir::new().expect("tempdir");
        let repo_default_dir = temp.path().join("flotilla-repos");
        std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();
        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = sqlite_daemon(vec![repo.clone()], Arc::clone(&config)).await;
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
        let controller_handles =
            spawn_controller_loops(Arc::clone(&state), NAMESPACE, Duration::from_millis(25), ControllerSupervision::default());

        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(
                &empty_meta("wf-a"),
                &WorkflowTemplateSpec::builder()
                    .inputs(Vec::new())
                    .vessels(vec![VesselRequirement::builder()
                        .name("implement".to_string())
                        .crew(vec![CrewSpec::builder()
                            .role("coder".to_string())
                            .source(CrewSource::Tool { command: "bash -lc 'echo adopted-stage4a'".to_string() })
                            .build()])
                        .build()])
                    .build(),
            )
            .await
            .expect("workflow template create should succeed");

        let mut rx = daemon.subscribe();
        let create_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyCreate {
                    name: "convoy-adopted".to_string(),
                    workflow_ref: "wf-a".to_string(),
                    inputs: Vec::new(),
                    repository_url: None,
                    r#ref: None,
                    project_ref: None,
                    placement_policy: Some(format!("host-direct-{host_id}")),
                    adopted_checkout: Some(Box::new(repo.clone())),
                },
            })
            .await
            .expect("convoy create command should start");
        assert_eq!(wait_for_command_result(&mut rx, create_id).await, CommandValue::ConvoyCreated { name: "convoy-adopted".to_string() });

        let convoys = backend.clone().using::<Convoy>(NAMESPACE);
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                matches!(
                    convoys.get("convoy-adopted").await.ok().and_then(|convoy| convoy.status).as_ref(),
                    Some(status)
                        if status.phase == ConvoyPhase::Active
                            && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Running)
                )
            }
        })
        .await;

        let complete_id = daemon
            .execute(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::ConvoyWorkForceComplete {
                    convoy: "convoy-adopted".to_string(),
                    work: "implement".to_string(),
                    message: Some("done".to_string()),
                },
            })
            .await
            .expect("convoy completion command should start");
        assert_eq!(wait_for_command_result(&mut rx, complete_id).await, CommandValue::Ok);

        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                matches!(
                    convoys.get("convoy-adopted").await.ok().and_then(|convoy| convoy.status).as_ref(),
                    Some(status)
                        if status.phase == ConvoyPhase::Completed
                            && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
                )
            }
        })
        .await;

        let checkout = backend
            .clone()
            .using::<ResourceCheckout>(NAMESPACE)
            .get("adopted-checkout-convoy-adopted")
            .await
            .expect("adopted checkout should remain after completion");
        assert_eq!(checkout.metadata.lifecycle_authority().expect("authority should parse"), Some(LifecycleAuthority::Adopted));
        assert!(backend.clone().using::<ResourceCheckout>(NAMESPACE).get("checkout-convoy-adopted-implement").await.is_err());

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}
