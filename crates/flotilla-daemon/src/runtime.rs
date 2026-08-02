Warning: truncated output (original token count: 84023)
Total output lines: 7130

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, Weak},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    BranchPreservationReason, CheckoutReconciler, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneReconciler, CloneRuntime,
    DockerEnvironmentRuntime, DockerProvisioning, DockerProvisioningError, EnvironmentReconciler, ForgeDefaultBranchResolver,
    HopChainContext, PreparedCheckout, PresentationPolicyRegistry, PresentationReconciler, ProviderPresentationRuntime,
    RepositoryReconciler, TerminalRuntime, TerminalRuntimeState, TerminalSessionReconciler, VesselPlacementProjector, VesselReconciler,
};
use flotilla_core::{
    agent_adapter::{AgentLaunchRequest, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    checkout_integration::{checkout_path_from_status_and_spec, inspect_checkout_integration},
    config::ConfigStore,
    in_process::InProcessDaemon,
    measure_available_space,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    placement_policy::reconcile_registered_policy,
    providers::{
        discovery::{run_provisioned_host_detectors, EnvironmentBag},
        environment::{CreateOpts, EnvironmentHandle, EnvironmentToolAssetKind, EnvironmentVariableUpdate},
        registry::ProviderRegistry,
        terminal::{ScreenActivity, TerminalPool},
        vcs::{CloneInspection, CloneProvisioner, GitCloneProvisioner},
        ChannelLabel, CommandRunner,
    },
};
use flotilla_protocol::{EnvironmentId, HostSummary, ImageId, NodeId, Rows, TerminalStatus};
use flotilla_resources::{
    canonicalize_repo_url, clone_key, controller::ControllerLoop, descriptive_repo_slug, Checkout, CheckoutBranchProvenance,
    CheckoutIntegrationStatus, Clone, CloneSpec, ConditionValue, Convoy, ConvoyReconciler, ConvoyTeardownRuntime, CrewSource, CrewSpec,
    Demand, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, Environment, EnvironmentSpec, EnvironmentWaitReason, ForgeIdentity,
    Host, HostCondition, HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus,
    InputDefinition, InputMeta, PlacementPolicySpec, Presentation, Project, Regard, ReplicationClass, Repository, ResourceBackend,
    ResourceError, ResourceObject, Stance, TerminalSession, TerminalSessionSource, Vessel, VesselRequirement, WorkflowTemplate,
    WorkflowTemplateSpec, AGENT_ADAPTERS_CAPABILITY, CREDENTIAL_REFS_ENV, CREDENTIAL_REF_SESSION_TAG, HELD_CREDENTIALS_CAPABILITY,
    MANAGED_BY_LABEL, REGISTERED_RESOURCE_KINDS,
};
use serde_json::json;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, error, info, warn};

use crate::{
    agent_material::{AgentMaterialPrepareError, AgentMaterialRegistry},
    credential::CredentialStore,
    environment_tools::EnvironmentToolProvisioner,
    resource_limits::file_descriptor_pressure_condition,
    resource_manifest::ResourceManifestReconciler,
    sleep_inhibitor,
    supervisor::{supervise, ControllerSupervision, RestartBudgetExhausted},
    Aggregator, AggregatorResolvers,
};

/// Cadence of the liveness marker (see `spawn_liveness_watchdog_task`). Long
/// enough to be quiet in a healthy log, short enough to bound how much time a
/// wedge can hide in.
const LIVENESS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(60);
const MANIFEST_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_DOCKER_IMAGE: &str = "ubuntu:24.04";
const DEFAULT_REPO_DIR_SUFFIX: &str = "dev/flotilla-repos";
const BUILTIN_MANAGED_BY_VALUE: &str = "builtin";

struct DaemonConvoyTeardownRuntime {
    daemon: Arc<InProcessDaemon>,
    reclaim_refusals: StdMutex<HashMap<String, ReclaimRefusal>>,
}

struct ReclaimRefusal {
    error: String,
    attempts: u64,
}

impl DaemonConvoyTeardownRuntime {
    fn new(daemon: Arc<InProcessDaemon>) -> Self {
        Self { daemon, reclaim_refusals: StdMutex::new(HashMap::new()) }
    }
}

#[async_trait]
impl ConvoyTeardownRuntime for DaemonConvoyTeardownRuntime {
    async fn no_change_request_outstanding(
        &self,
        convoy: &ResourceObject<Convoy>,
        _checkouts: &[ResourceObject<Checkout>],
    ) -> Result<bool, String> {
        self.daemon.convoy_change_requests_settled(&convoy.metadata.namespace, &convoy.metadata.name).await
    }

    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        let result = self.daemon.verify_convoy_teardown_gate(&convoy.metadata.namespace, &convoy.metadata.name, false).await;
        let key = format!("{}/{}", convoy.metadata.namespace, convoy.metadata.name);
        match &result {
            Err(error) => {
                let attempts = {
                    let mut refusals = self.reclaim_refusals.lock().expect("reclaim refusal lock poisoned");
                    match refusals.get_mut(&key) {
                        Some(refusal) if refusal.error == *error => {
                            refusal.attempts += 1;
                            refusal.attempts
                        }
                        _ => {
                            refusals.insert(key, ReclaimRefusal { error: error.clone(), attempts: 1 });
                            1
                        }
                    }
                };
                if attempts == 1 {
                    warn!(
                        namespace = %convoy.metadata.namespace,
                        convoy = %convoy.metadata.name,
                        attempts,
                        %error,
                        "automatic convoy reclaim refused"
                    );
                }
            }
            Ok(()) => {
                if let Some(refusal) = self.reclaim_refusals.lock().expect("reclaim refusal lock poisoned").remove(&key) {
                    if refusal.attempts > 1 {
                        info!(
                            namespace = %convoy.metadata.namespace,
                            convoy = %convoy.metadata.name,
                            refused_attempts = refusal.attempts,
                            "automatic convoy reclaim recovered after repeated refusal"
                        );
                    }
                }
            }
        }
        result
    }
}

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
    /// Set by `shutdown` so `Drop` can tell an intended stop from a runtime
    /// that vanished while the daemon was meant to keep working.
    stop_expected: bool,
}

#[derive(Debug, Clone)]
struct DaemonHealthIdentity {
    generation: Option<String>,
    version: String,
    started_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeHealth {
    failures: Arc<StdMutex<BTreeMap<String, HostCondition>>>,
    restart_history_dir: Option<Arc<PathBuf>>,
}

impl RuntimeHealth {
    fn report_capability_regression(&self, condition: HostCondition) {
        self.failures.lock().expect("runtime health lock poisoned").insert(condition.condition_type.clone(), condition);
    }

    fn with_restart_history_dir(mut self, path: PathBuf) -> Self {
        self.restart_history_dir = Some(Arc::new(path));
        self
    }

    fn report_restart_budget_exhausted(&self, exhausted: RestartBudgetExhausted) {
        let condition_type = format!("Controller/{}", exhausted.controller);
        let condition = HostCondition::builder()
            .condition_type(condition_type.clone())
            .value(ConditionValue::False)
            .reason("RestartBudgetExhausted")
            .message(format!(
                "{} controller stopped after {} consecutive failures: {}",
                exhausted.controller, exhausted.attempts, exhausted.error
            ))
            .observed_at(Utc::now())
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(condition_type, condition);
    }

    fn report_projection_parity(&self, condition: Option<HostCondition>) {
        const CONDITION_TYPE: &str = "ProjectionParity";
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match condition {
            Some(condition) => {
                failures.insert(CONDITION_TYPE.to_string(), condition);
            }
            None => {
                failures.remove(CONDITION_TYPE);
            }
        }
    }

    async fn conditions(&self) -> Vec<HostCondition> {
        let mut conditions = self.failures.lock().expect("runtime health lock poisoned").values().cloned().collect::<Vec<_>>();
        if let Some(state_dir) = self.restart_history_dir.clone() {
            let frequency =
                tokio::task::spawn_blocking(move || crate::restart_history::recent_abnormal_restarts(state_dir.as_path(), Utc::now()))
                    .await
                    .map_err(|error| format!("restart history task failed: {error}"))
                    .and_then(|result| result);
            let condition = match frequency {
                Ok(frequency) if frequency.count > 0 => Some(
                    HostCondition::builder()
                        .condition_type("Daemon/AbnormalRestarts")
                        .value(ConditionValue::False)
                        .reason("AbnormalExitFrequency")
                        .message(format!(
                            "daemon restarted {}× after abnormal exits in {}m",
                            frequency.count,
                            frequency.window.as_secs() / 60
                        ))
                        .observed_at(Utc::now())
                        .build(),
                ),
                Ok(_) => None,
                Err(error) => Some(
                    HostCondition::builder()
                        .condition_type("Daemon/RestartTracking")
                        .value(ConditionValue::False)
                        .reason("RestartHistoryUnavailable")
                        .message(error)
                        .observed_at(Utc::now())
                        .build(),
                ),
            };
            conditions.extend(condition);
        }
        conditions
    }
}

struct AgentAdapterCapabilityAssessment {
    baseline: BTreeSet<String>,
    regression: Option<HostCondition>,
}

fn assess_agent_adapter_capabilities(
    previous: Option<&HostStatus>,
    current: &BTreeSet<String>,
    health: &DaemonHealthIdentity,
) -> AgentAdapterCapabilityAssessment {
    let Some(previous) = previous else {
        return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
    };
    let baseline = match &previous.agent_adapter_baseline {
        Some(baseline) => baseline.clone(),
        None => match previous.agent_adapters() {
            Ok(adapters) => adapters,
            Err(error) => {
                warn!(%error, "cannot compare agent adapter capabilities with previous daemon generation");
                return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
            }
        },
    };
    let same_daemon = previous.daemon_generation == health.generation && previous.daemon_started_at == Some(health.started_at);
    if same_daemon {
        return AgentAdapterCapabilityAssessment { baseline, regression: None };
    }
    let missing = baseline.difference(current).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
    }

    warn!(
        previous_generation = ?previous.daemon_generation,
        current_generation = ?health.generation,
        baseline_adapters = ?baseline,
        current_adapters = ?current,
        missing_adapters = ?missing,
        "host capabilities regressed across daemon restart"
    );
    let regression = Some(
        HostCondition::builder()
            .condition_type("CapabilityRegression")
            .value(ConditionValue::False)
            .reason("AgentAdaptersMissing")
            .message(format!("agent adapters from the last non-regressed daemon generation are missing: {}", missing.join(", ")))
            .observed_at(Utc::now())
            .build(),
    );
    AgentAdapterCapabilityAssessment { baseline, regression }
}

#[cfg(test)]
fn test_health_identity() -> DaemonHealthIdentity {
    DaemonHealthIdentity {
        generation: Some("test-generation".to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: Utc::now(),
    }
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
        let manifests = config.load_daemon_config()?.manifests;

        let local_registry = probe_local_provider_registry(&daemon, &config).await?;
        let profile = build_local_profile(&daemon, &local_registry)?;
        let credential_store = Arc::new(CredentialStore::new(
            daemon.resource_backend(),
            &options.namespace,
            Arc::clone(&daemon.discovery_runtime().env),
            daemon.local_environment_bag().ok_or_else(|| "local environment bag unavailable".to_string())?,
            daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?,
            config.state_dir().as_path().to_path_buf(),
        ));
        let agent_material = Arc::new(AgentMaterialRegistry::new(
            daemon.resource_backend(),
            &options.namespace,
            Arc::clone(&daemon.discovery_runtime().env),
        ));
        let health = DaemonHealthIdentity {
            generation: daemon
                .observed_resource_backend()
                .using::<Checkout>(&options.namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .generation,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Utc::now(),
        };
        daemon.set_local_placement_capabilities(&profile.available_agent_adapters, &profile.available_pools).await;
        let runtime_health = RuntimeHealth::default().with_restart_history_dir(config.state_dir().as_path().to_path_buf());
        flotilla_resources::quarantine_undecodable_stored_objects(&daemon.resource_backend(), &options.namespace)
            .await
            .map_err(|error| format!("scan stored resources for decode quarantine: {error}"))?;
        register_startup_resources(&daemon, &options.namespace, &profile).await?;
        flotilla_resources::PreparedSnapshotGarbageCollector::new(daemon.resource_backend(), &options.namespace)
            .recover_pending_claims()
            .await
            .map_err(|error| format!("recover prepared convoy admissions: {error}"))?;
        let active_environments = daemon
            .resource_backend()
            .using::<Environment>(&options.namespace)
            .list()
            .await
            .map_err(|error| format!("list environments for material lease recovery: {error}"))?
            .items
            .into_iter()
            .map(|environment| environment.metadata.name);
        agent_material.recover(active_environments).await?;
        apply_host_heartbeat_with_credentials(&daemon, &options.namespace, &profile, Some(&credential_store), &health, &runtime_health)
            .await?;
        if let Err(error) = daemon.reconcile_adopted_checkouts(&options.namespace).await {
            warn!(%error, "failed to restore adopted checkout observations during startup; periodic reconciliation will retry");
        }

        let mut tasks = vec![
            spawn_heartbeat_task_with_credentials(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                Arc::new(Some(Arc::clone(&credential_store))),
                health,
                runtime_health.clone(),
                options.heartbeat_interval,
            ),
            spawn_replica_refresh_task(Arc::clone(&daemon), options.heartbeat_interval),
            spawn_adopted_checkout_reconciliation_task(Arc::clone(&daemon), options.namespace.clone(), options.controller_resync_interval),
            spawn_projection_parity_task(
                daemon.resource_backend(),
                options.namespace.clone(),
                aggregator_projection_state.clone(),
                runtime_health.clone(),
                options.heartbeat_interval,
            ),
            spawn_sleep_inhibitor_task(
                daemon.resource_backend(),
                options.namespace.clone(),
                profile.host_id.clone(),
                options.controller_supervision.clone(),
                runtime_health.clone(),
            ),
            spawn_aggregator_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                aggregator_projection_state,
                options.controller_supervision.clone(),
                runtime_health.clone(),
            ),
        ];
        if let Some(manifests) = manifests {
            tasks.push(spawn_manifest_reconciler_task(
                daemon.resource_backend(),
                options.namespace.clone(),
                manifests.dir,
                MANIFEST_RECONCILE_INTERVAL,
                options.controller_supervision.clone(),
                runtime_health.clone(),
            ));
        }

        if options.start_controllers {
            let local_repo_root = daemon.tracked_repo_paths().await.into_iter().next().map(ExecutionEnvironmentPath::new);
            let state = Arc::new(
                ControllerRuntimeState::new(
                    daemon,
                    config,
                    local_registry,
                    daemon_socket_path.map(DaemonHostPath::new),
                    profile.host_id.clone(),
                    local_repo_root,
                    profile.host_direct_environment_name(),
                )
                .with_credential_store(credential_store)
                .with_agent_material(agent_material),
            );
            tasks.extend(spawn_controller_loops(
                state,
                &options.namespace,
                options.controller_resync_interval,
                options.controller_supervision.clone(),
                runtime_health,
            ));
        }

        // +1 for the watchdog's own handle, pushed below, so this count matches
        // `self.tasks.len()` at drop time.
        let supervisory_tasks = tasks.len() + 1;
        tasks.push(spawn_liveness_watchdog_task(supervisory_tasks, LIVENESS_WATCHDOG_INTERVAL));

        Ok(Self { tasks, stop_expected: false })
    }
}

fn spawn_manifest_reconciler_task(
    backend: ResourceBackend,
    namespace: String,
    root: PathBuf,
    interval: Duration,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        supervise_controller("manifest", supervision, runtime_health, move || {
            let reconciler = ResourceManifestReconciler::new(backend.clone(), namespace.clone(), root.clone());
            async move { reconciler.run(interval).await }
        })
        .await;
    })
}

impl DaemonRuntime {
    /// Stop the supervisory tasks deliberately. Every routine daemon exit —
    /// SIGTERM, SIGINT, idle timeout, explicit shutdown — should call this, so
    /// that `Drop`'s ERROR path stays reserved for a runtime that disappeared
    /// while the daemon was still meant to be working.
    pub fn shutdown(mut self) {
        self.stop_expected = true;
        info!(tasks = self.tasks.len(), "daemon runtime stopping; aborting supervisory tasks");
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        // Dropping this value aborts every supervisory task — heartbeat, replica
        // refresh, aggregator, controllers. A daemon whose runtime is dropped
        // while its accept loop keeps running looks alive (process up, socket
        // accepting) but serves nothing, and until this log existed it left no
        // trace whatsoever (flotilla#1111). An expected stop goes through
        // `shutdown` and logs at INFO there; reaching here without that flag is
        // the case worth shouting about.
        if self.stop_expected {
            debug!(tasks = self.tasks.len(), "daemon runtime dropped after expected shutdown");
        } else {
            error!(
                tasks = self.tasks.len(),
                "daemon runtime dropped unexpectedly; aborting supervisory tasks — the daemon can no longer do background work"
            );
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalProvisioningProfile {
    host_id: String,
    display_name: String,
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
    local_host_ref: String,
    local_repo_root: Option<ExecutionEnvironmentPath>,
    host_direct_environment_name: String,
    environment_tools: EnvironmentToolProvisioner,
    credential_store: Option<Arc<CredentialStore>>,
    agent_material: Option<Arc<AgentMaterialRegistry>>,
    provisioned_environments: Mutex<HashMap<String, ActiveProvisionedEnvironment>>,
    clone_flights: Arc<CloneFlights>,
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
        let environment_tools = EnvironmentToolProvisioner::for_local_host(&daemon, &config, daemon_socket_path.clone());
        Self {
            daemon,
            config,
            local_registry,
            local_host_ref,
            local_repo_root,
            host_direct_environment_name,
            environment_tools,
            credential_store: None,
            agent_material: None,
            provisioned_environments: Mutex::new(HashMap::new()),
            clone_flights: Arc::new(CloneFlights::default()),
        }
    }

    fn with_credential_store(mut self, credential_store: Arc<CredentialStore>) -> Self {
        self.credential_store = Some(credential_store);
        self
    }

    fn with_agent_material(mut self, agent_material: Arc<AgentMaterialRegistry>) -> Self {
        self.agent_material = Some(agent_material);
        self
    }

    #[cfg(test)]
    fn with_environment_tools(mut self, environment_tools: EnvironmentToolProvisioner) -> Self {
        self.environment_tools = environment_tools;
        self
    }
}

struct ActiveProvisionedEnvironment {
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
    let docker_pool = "cleat".to_string();
    let docker_available =
        local_registry.environment_providers.contains_key("docker") && local_registry.terminal_pools.contains_key(&docker_pool);
    let available_agent_adapters = local_registry.agent_adapters.ids().map(ToString::to_string).collect();

    Ok(LocalProvisioningProfile {
        host_id,
        display_name: daemon.host_name().to_string(),
        repo_default_dir,
        host_direct_pool,
        docker_pool,
        available_pools,
        available_agent_adapters,
        docker_available,
    })
}

async fn register_startup_resources(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
) -> Result<(), String> {
    let backend = daemon.resource_backend();
    ensure_host_exists(&backend, namespace, &profile.host_id, &profile.display_name).await?;
    ensure_host_direct_environment_exists(&backend, namespace, profile).await?;
    discover_local_clones(daemon, &backend, namespace, profile).await?;
    ensure_default_policies(&backend, namespace, profile).await?;
    reconcile_builtin_workflow_templates(&backend, namespace).await?;
    daemon.materialize_tracked_repo_projects().await?;
    Ok(())
}

fn builtin_workflow_templates() -> Vec<(&'static str, WorkflowTemplateSpec)> {
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
        ("implement-review", flotilla_resources::implement_review_workflow_spec()),
        ("interactive-single", flotilla_resources::interactive_single_workflow_spec()),
        ("single-agent-contained", flotilla_resources::single_agent_contained_workflow_spec()),
        ("single-agent-shepherd", flotilla_resources::single_agent_shepherd_workflow_spec()),
        ("single-agent-trusted", flotilla_resources::single_agent_trusted_workflow_spec()),
    ]
}

fn mark_builtin_managed(mut meta: InputMeta) -> InputMeta {
    meta.labels.insert(MANAGED_BY_LABEL.to_string(), BUILTIN_MANAGED_BY_VALUE.to_string());
    meta
}

/// Reconciles code-owned manifests as the builtin special case of the ruled
/// manifest loop in https://github.com/flotilla-org/flotilla/issues/1192.
async fn reconcile_builtin_workflow_templates(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    for (name, spec) in builtin_workflow_templates() {
        match templates.get(name).await {
            Ok(existing) => {
                let spec_diverged = existing.spec != spec;
                let managed_by_builtin =
                    existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == BUILTIN_MANAGED_BY_VALUE);
                if !spec_diverged && managed_by_builtin {
                    continue;
                }
                if !spec_diverged {
                    templates
                        .update(&mark_builtin_managed(InputMeta::from(&existing.metadata)), &existing.metadata.resource_version, &spec)
                        .await
                        .map_err(|err| format!("reconcile builtin workflow template {name}: {err}"))?;
                    continue;
                }
                templates
                    .update(&mark_builtin_managed(InputMeta::from(&existing.metadata)), &existing.metadata.resource_version, &spec)
                    .await
                    .map_err(|err| format!("reconcile builtin workflow template {name}: {err}"))?;
                warn!(template = %name, "stored spec diverged from code builtin; overwriting");
            }
            Err(ResourceError::NotFound { .. }) => {
                templates
                    .create(&mark_builtin_managed(empty_meta(name)), &spec)
                    .await
                    .map_err(|err| format!("seed builtin workflow template {name}: {err}"))?;
            }
            Err(err) => return Err(format!("check workflow template {name}: {err}")),
        }
    }
    Ok(())
}

async fn ensure_host_exists(backend: &ResourceBackend, namespace: &str, host_name: &str, display_name: &str) -> Result<(), String> {
    let hosts = backend.clone().using::<Host>(namespace);
    match hosts.get(host_name).await {
        Ok(existing) if existing.spec.display_name == display_name => return Ok(()),
        Ok(existing) => {
            return hosts
                .update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &HostSpec {
                    display_name: display_name.to_string(),
                })
                .await
                .map(|_| ())
                .map_err(|err| err.to_string())
        }
        Err(ResourceError::NotFound { .. }) => {}
        Err(err) => return Err(format!("check host {host_name}: {err}")),
    }
    hosts
        .create(&empty_meta(host_name), &HostSpec { display_name: display_name.to_string() })
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
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
    let host_direct_name = profile.host_direct_policy_name();
    reconcile_registered_policy(
        backend,
        namespace,
        &host_direct_name,
        &PlacementPolicySpec::builder()
            .pool(profile.host_direct_pool.clone())
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: profile.host_id.clone(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build(),
    )
    .await?;

    if profile.docker_available {
        let docker_name = profile.docker_policy_name();
        reconcile_registered_policy(
            backend,
            namespace,
            &docker_name,
            &PlacementPolicySpec::builder()
                .pool(profile.docker_pool.clone())
                .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                    host_ref: profile.host_id.clone(),
                    image: DEFAULT_DOCKER_IMAGE.to_string(),
                    pull_policy: Default::default(),
                    agent_adapters: BTreeSet::new(),
                    default_cwd: Some("/workspace".to_string()),
                    env: BTreeMap::new(),
                    checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
                })
                .build(),
        )
        .await?;
    }

    Ok(())
}

async fn supervise_controller<F, Fut>(name: &'static str, supervision: ControllerSupervision, runtime_health: RuntimeHealth, make_run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), ResourceError>>,
{
    if let Err(exhausted) = supervise(name, supervision, make_run).await {
        runtime_health.report_restart_budget_exhausted(exhausted);
    }
}

fn spawn_sleep_inhibitor_task(
    backend: ResourceBackend,
    namespace: String,
    host_id: String,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        supervise_controller("sleep_inhibitor", supervision, runtime_health, move || {
            let convoys = backend.clone().including_replicas::<Convoy>(&namespace);
            let vessels = backend.clone().using::<Vessel>(&namespace);
            let hosts = backend.clone().using::<Host>(&namespace);
            let host_id = host_id.clone();
            async move { sleep_inhibitor::run(convoys, vessels, hosts, host_id).await }
        })
        .await;
    })
}

#[cfg(test)]
fn spawn_heartbeat_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    health: DaemonHealthIdentity,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_heartbeat_task_with_credentials(daemon, namespace, profile, Arc::new(None), health, RuntimeHealth::default(), interval)
}

fn spawn_heartbeat_task_with_credentials(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    profile: LocalProvisioningProfile,
    credential_store: Arc<Option<Arc<CredentialStore>>>,
    health: DaemonHealthIdentity,
    runtime_health: RuntimeHealth,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let profile = profile.clone();
        let credential_store = Arc::clone(&credential_store);
        let health = health.clone();
        let runtime_health = runtime_health.clone();
        async move {
            if let Err(err) =
                apply_host_heartbeat_with_credentials(&daemon, &namespace, &profile, credential_store.as_deref(), &health, &runtime_health)
                    .await
            {
                warn!(%err, "failed to publish host heartbeat");
            }
        }
    })
}

#[cfg(test)]
async fn apply_host_heartbeat(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    health: &DaemonHealthIdentity,
) -> Result<(), String> {
    apply_host_heartbeat_with_credentials(daemon, namespace, profile, None, health, &RuntimeHealth::default()).await
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

fn spawn_projection_parity_task(
    backend: ResourceBackend,
    namespace: String,
    projection: AggregatorProjectionState,
    runtime_health: RuntimeHealth,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let backend = backend.clone();
        let namespace = namespace.clone();
        let projection = projection.clone();
        let runtime_health = runtime_health.clone();
        async move {
            match projection_parity_condition(&backend, &namespace, &projection).await {
                Ok(condition) => runtime_health.report_projection_parity(condition),
                Err(error) => warn!(%error, "failed to evaluate aggregator projection parity"),
            }
        }
    })
}

async fn projection_parity_condition(
    backend: &ResourceBackend,
    namespace: &str,
    projection: &AggregatorProjectionState,
) -> Result<Option<HostCondition>, String> {
    let stored = backend.using::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?;
    let expected = stored
        .items
        .into_iter()
        .filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()))
        .map(|convoy| convoy.metadata.name)
        .collect::<BTreeSet<_>>();
    let projected = match projection.local_result_set().await.rows {
        Rows::Convoys { rows, .. } => rows.into_iter().map(|row| row.resource.name).collect::<BTreeSet<_>>(),
        rows => return Err(format!("local convoy projection returned unexpected rows: {rows:?}")),
    };
    let missing = expected.difference(&projected).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(None);
    }
    let message = format!(
        "durable store has {} live convoys but the local aggregator projection has {}; missing: {}",
        expected.len(),
        projected.len(),
        missing.join(", ")
    );
    error!(
        stored = expected.len(),
        projected = projected.len(),
        missing = ?missing,
        "aggregator projection parity check failed"
    );
    Ok(Some(
        HostCondition::builder()
            .condition_type("ProjectionParity")
            .value(ConditionValue::False)
            .reason("LocalRowsMissing")
            .message(message)
            .observed_at(Utc::now())
            .build(),
    ))
}

#[derive(Clone, Copy)]
enum PeriodicTaskStart {
    Immediate,
    AfterInterval,
}

/// Emits one liveness line per interval so a daemon that stops scheduling is
/// visible in the log by the *absence* of a known-cadence marker, rather than by
/// the absence of incidental chatter. Added after flotilla#1111, where the only
/// evidence of a wedged daemon was that unrelated debug lines stopped.
fn spawn_liveness_watchdog_task(tasks: usize, interval: Duration) -> JoinHandle<()> {
    let started = tokio::time::Instant::now();
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || async move {
        info!(uptime_secs = started.elapsed().as_secs(), supervisory_tasks = tasks, "daemon alive");
    })
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

async fn apply_host_heartbeat_with_credentials(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    credential_store: Option<&CredentialStore>,
    health: &DaemonHealthIdentity,
    runtime_health: &RuntimeHealth,
) -> Result<(), String> {
    ensure_host_exists(&daemon.resource_backend(), namespace, &profile.host_id, &profile.display_name).await?;
    let backend = daemon.resource_backend();
    let hosts = backend.using::<Host>(namespace);
    let host = hosts.get(&profile.host_id).await.map_err(|err| err.to_string())?;
    let adapter_assessment = assess_agent_adapter_capabilities(host.status.as_ref(), &profile.available_agent_adapters, health);
    if let Some(condition) = adapter_assessment.regression {
        runtime_health.report_capability_regression(condition);
    }
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
    let held_credentials = match credential_store {
        Some(store) => store.held_credentials().await?,
        None => BTreeSet::new(),
    };
    let repo_default_dir = PathBuf::from(&profile.repo_default_dir);
    let disk_free_bytes = tokio::task::spawn_blocking(move || measure_available_space(&repo_default_dir))
        .await
        .map_err(|error| format!("measure available disk space: {error}"))?;
    let mut conditions = runtime_health.conditions().await;
    conditions.extend(file_descriptor_pressure_condition());
    if let Some(condition) = resource_decode_quarantine_condition(resource_store.as_ref()) {
        conditions.push(condition);
    }
    if let Some(condition) = resource_field_ownership_condition(resource_store.as_ref()) {
        conditions.push(condition);
    }
    if let Some(condition) = resource_replication_content_condition(daemon, namespace).await? {
        conditions.push(condition);
    }
    let status = HostStatus {
        capabilities: host_capabilities(&summary, profile, &held_credentials),
        agent_adapter_baseline: Some(adapter_assessment.baseline),
        heartbeat_at: Some(Utc::now()),
        ready: conditions.is_empty(),
        resource_store,
        daemon_generation: health.generation.clone(),
        daemon_version: Some(health.version.clone()),
        daemon_started_at: Some(health.started_at),
        disk_free_bytes,
        conditions,
        sleep_inhibition: host.status.as_ref().map(|status| status.sleep_inhibition.clone()).unwrap_or_default(),
    };
    hosts.update_status(&profile.host_id, &host.metadata.resource_version, &status).await.map_err(|err| err.to_string())?;
    daemon.refresh_connected_peer_host_heartbeats().await;
    Ok(())
}

async fn resource_replication_content_condition(daemon: &Arc<InProcessDaemon>, namespace: &str) -> Result<Option<HostCondition>, String> {
    let connected_peers = daemon.connected_peer_node_ids().await;
    if connected_peers.is_empty() {
        return Ok(None);
    }

    let backend = daemon.resource_backend();
    let mut peers_without_cursors = Vec::new();
    for peer in connected_peers {
        let mut cursor_count = 0;
        for kind in REGISTERED_RESOURCE_KINDS {
            if kind.replication_class == ReplicationClass::None {
                continue;
            }
            if flotilla_resources::replica_cursor_for_resource_kind(&backend, namespace, kind.kind, &peer)
                .await
                .map_err(|error| error.to_string())?
                .is_some()
            {
                cursor_count += 1;
            }
        }
        if cursor_count == 0 {
            peers_without_cursors.push(peer);
        }
    }
    if peers_without_cursors.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        HostCondition::builder()
            .condition_type("ResourceReplication")
            .value(ConditionValue::False)
            .reason("ReplicaCursorsMissing")
            .message(format!(
                "connected peer{} {} {} zero replica cursors; resource replication has not bootstrapped",
                if peers_without_cursors.len() == 1 { "" } else { "s" },
                peers_without_cursors.iter().map(NodeId::as_str).collect::<Vec<_>>().join(", "),
                if peers_without_cursors.len() == 1 { "has" } else { "have" },
            ))
            .observed_at(Utc::now())
            .build(),
    ))
}

fn resource_decode_quarantine_condition(diagnostics: Option<&flotilla_resources::ResourceStoreDiagnostics>) -> Option<HostCondition> {
    let diagnostics = diagnostics?;
    let object_quarantines = &diagnostics.decode_quarantines;
    let event_quarantines = &diagnostics.event_decode_quarantines;
    if object_quarantines.is_empty() && event_quarantines.is_empty() {
        return None;
    }
    let identities = object_quarantines
        .iter()
        .map(|quarantine| format!("{}/{}: {}", quarantine.kind, quarantine.name, quarantine.error))
        .chain(
            event_quarantines
                .iter()
                .map(|quarantine| format!("{}/{}@{}: {}", quarantine.kind, quarantine.name, quarantine.event_version, quarantine.error)),
        )
        .collect::<Vec<_>>()
        .join("; ");
    let (reason, message) = if event_quarantines.is_empty() {
        (
            "StoredObjectDecodeFailed",
            format!(
                "{} stored resource object{} quarantined after typed decode failure{}: {identities}",
                object_quarantines.len(),
                if object_quarantines.len() == 1 { "" } else { "s" },
                if object_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    } else if object_quarantines.is_empty() {
        (
            "StoredEventDecodeFailed",
            format!(
                "{} stored resource event{} quarantined after typed decode failure{}: {identities}",
                event_quarantines.len(),
                if event_quarantines.len() == 1 { "" } else { "s" },
                if event_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    } else {
        (
            "StoredResourceDecodeFailed",
            format!(
                "{} stored resource object{} and {} event{} quarantined after typed decode failures: {identities}",
                object_quarantines.len(),
                if object_quarantines.len() == 1 { "" } else { "s" },
                event_quarantines.len(),
                if event_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    };
    Some(
        HostCondition::builder()
            .condition_type("ResourceStore/DecodeQuarantine")
            .value(ConditionValue::False)
            .reason(reason)
            .message(message)
            .observed_at(Utc::now())
            .build(),
    )
}

fn resource_field_ownership_condition(diagnostics: Option<&flotilla_resources::ResourceStoreDiagnostics>) -> Option<HostCondition> {
    let violations = &diagnostics?.field_ownership_violations;
    if violations.is_empty() {
        return None;
    }
    let details = violations
        .iter()
        .map(|violation| {
            format!(
                "{}/{}/{} {:?} attempted {}={} ({})",
                violation.kind,
                violation.namespace,
                violation.name,
                violation.writer.role,
                violation.field,
                violation.attempted_value,
                violation.rule
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(
        HostCondition::builder()
            .condition_type("ResourceStore/FieldOwnership")
            .value(ConditionValue::False)
            .reason("FieldOwnershipViolation")
            .message(format!(
                "{} field ownership violation{} recorded: {details}",
                violations.len(),
                if violations.len() == 1 { "" } else { "s" }
            ))
            .observed_at(Utc::now())
            .build(),
    )
}

fn host_capabilities(
    _summary: &HostSummary,
    profile: &LocalProvisioningProfile,
    held_credentials: &BTreeSet<String>,
) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
        (HELD_CREDENTIALS_CAPABILITY.to_string(), json!(held_credentials)),
        ("docker".to_string(), json!(profile.docker_available)),
        ("terminal_pools".to_string(), json!(profile.available_pools)),
    ])
}

fn spawn_controller_loops(
    state: Arc<ControllerRuntimeState>,
    namespace: &str,
    controller_resync_interval: Duration,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
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
            let namespace_string = namespace_string.clone();
            let local_host_ref = state.local_host_ref.clone();
            let supervision = supervision.clone();
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("vessel_placement", supervision, runtime_health, move || {
                    let projector = VesselPlacementProjector::new(backend.clone(), namespace_string.clone(), local_host_ref.clone());
                    async move { projector.run().await }
                })
                .await;
            }
        }),
        tokio::spawn({
            let backend = backend.clone();
            let observed_backend = observed_backend.clone();
            let namespace_string = namespace_string.clone();
            let forge_default_branch_resolver = forge_default_branch_resolver.clone();
            let supervision = supervision.clone();
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("repository", supervision, runtime_health, move || {
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
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("environment", supervision, runtime_health, move || {
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
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("clone", supervision, runtime_health, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone()…54023 tokens truncated…()));
        let daemon = in_memory_daemon(Vec::new(), config).await;
        let local_host_id = daemon.local_host_id().expect("local host id").to_string();
        let peer_node = flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new("feta-node"), "feta");
        daemon
            .publish_peer_summary(
                HostSummary::builder()
                    .environment_id(EnvironmentId::host(flotilla_protocol::qualified_path::HostId::new("feta-host")))
                    .host_name(flotilla_protocol::HostName::new("feta"))
                    .node(peer_node.clone())
                    .system(flotilla_protocol::SystemInfo::default())
                    .providers(Vec::new())
                    .build(),
            )
            .await;
        daemon.publish_peer_connection_status(&peer_node, flotilla_protocol::PeerConnectionState::Connected).await;

        let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
        let peer = hosts.get("feta-host").await.expect("peer host should be materialized");
        let stale_heartbeat = Utc::now() - chrono::Duration::seconds(61);
        hosts
            .update_status(&peer.metadata.name, &peer.metadata.resource_version, &HostStatus {
                capabilities: BTreeMap::new(),
                heartbeat_at: Some(stale_heartbeat),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            })
            .await
            .expect("seed stale peer heartbeat");

        let heartbeat = spawn_heartbeat_task(
            Arc::clone(&daemon),
            NAMESPACE.to_string(),
            manual_profile(&local_host_id, false),
            test_health_identity(),
            Duration::from_millis(20),
        );
        wait_until(|| {
            let hosts = hosts.clone();
            async move {
                hosts
                    .get("feta-host")
                    .await
                    .ok()
                    .and_then(|host| host.status)
                    .is_some_and(|status| status.heartbeat_at.is_some_and(|heartbeat| heartbeat > stale_heartbeat))
            }
        })
        .await;

        assert_eq!(
            daemon.peer_connection_status(&peer_node.node_id).await,
            flotilla_protocol::PeerConnectionState::Connected,
            "the peer transport should remain connected"
        );
        let mut status = hosts.get("feta-host").await.expect("peer host").status.expect("peer status");
        status.apply_heartbeat_readiness(Utc::now());
        assert!(status.ready, "a connected peer should remain ready for placement");

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
    async fn policy_registration_preserves_operator_fields_and_corrects_owned_drift() {
        let temp = TempDir::new().expect("tempdir");
        let database_path = temp.path().join("resources.sqlite");
        let profile = manual_profile("host-test", false);
        {
            let backend =
                ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("initial sqlite resource backend should open"));
            ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("initial policy registration should succeed");

            let policies = backend.using::<PlacementPolicy>(NAMESPACE);
            let registered = policies.get(&profile.host_direct_policy_name()).await.expect("registered host-direct policy");
            policies
                .update(
                    &InputMeta::from(&registered.metadata),
                    &registered.metadata.resource_version,
                    &PlacementPolicySpec::builder()
                        .pool("operator-edited-pool".to_string())
                        .priority(10)
                        .host_direct(HostDirectPlacementPolicySpec {
                            host_ref: "operator-edited-host".to_string(),
                            checkout: HostDirectPlacementPolicyCheckout::Worktree,
                        })
                        .build(),
                )
                .await
                .expect("operator policy apply should succeed");
        }

        let backend = ResourceBackend::Sqlite(SqliteBackend::open(&database_path).expect("restarted sqlite resource backend should open"));
        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("policy re-registration should succeed");

        let reconciled = backend
            .using::<PlacementPolicy>(NAMESPACE)
            .get(&profile.host_direct_policy_name())
            .await
            .expect("reconciled host-direct policy");
        assert_eq!(reconciled.spec.priority, 10, "operator-owned priority must survive re-registration");
        assert_eq!(reconciled.spec.pool, profile.host_direct_pool, "registration must assert the discovered terminal pool");
        assert_eq!(
            reconciled.spec.host_direct,
            Some(HostDirectPlacementPolicySpec {
                host_ref: profile.host_id.clone(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            }),
            "registration must assert its host and checkout strategy"
        );
        assert!(reconciled.spec.docker_per_vessel.is_none());

        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("steady-state policy registration should succeed");
        let steady =
            backend.using::<PlacementPolicy>(NAMESPACE).get(&profile.host_direct_policy_name()).await.expect("steady host-direct policy");
        assert_eq!(steady.spec.priority, 10);
        assert_eq!(
            steady.metadata.resource_version, reconciled.metadata.resource_version,
            "steady registration must not rewrite the policy"
        );
    }

    #[tokio::test]
    async fn docker_policy_registration_preserves_runtime_configuration_and_corrects_owned_drift() {
        let backend = ResourceBackend::InMemory(Default::default());
        let profile = manual_profile("host-test", true);
        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("initial policy registration should succeed");

        let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
        let registered = policies.get(&profile.docker_policy_name()).await.expect("registered docker policy");
        policies
            .update(
                &InputMeta::from(&registered.metadata),
                &registered.metadata.resource_version,
                &PlacementPolicySpec::builder()
                    .pool("operator-edited-pool".to_string())
                    .priority(20)
                    .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
                        host_ref: "operator-edited-host".to_string(),
                        image: "operator/image:latest".to_string(),
                        pull_policy: flotilla_resources::DockerImagePullPolicy::Never,
                        agent_adapters: BTreeSet::from(["codex".to_string()]),
                        default_cwd: Some("/operator-workspace".to_string()),
                        env: BTreeMap::from([("OPERATOR_CONFIG".to_string(), "true".to_string())]),
                        checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/operator-clone".to_string() },
                    })
                    .build(),
            )
            .await
            .expect("operator policy apply should succeed");

        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("policy re-registration should succeed");

        let reconciled = policies.get(&profile.docker_policy_name()).await.expect("reconciled docker policy");
        assert_eq!(reconciled.spec.priority, 20);
        assert_eq!(reconciled.spec.pool, profile.docker_pool);
        assert!(reconciled.spec.host_direct.is_none());
        assert_eq!(
            reconciled.spec.docker_per_vessel,
            Some(DockerPerVesselPlacementPolicySpec {
                host_ref: profile.host_id,
                image: "operator/image:latest".to_string(),
                pull_policy: flotilla_resources::DockerImagePullPolicy::Never,
                agent_adapters: BTreeSet::from(["codex".to_string()]),
                default_cwd: Some("/operator-workspace".to_string()),
                env: BTreeMap::from([("OPERATOR_CONFIG".to_string(), "true".to_string())]),
                checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
            })
        );
    }

    #[tokio::test]
    async fn policy_registration_leaves_manifest_managed_collision_untouched() {
        let backend = ResourceBackend::InMemory(Default::default());
        let profile = manual_profile("host-test", false);
        let policies = backend.clone().using::<PlacementPolicy>(NAMESPACE);
        let manifest_spec = PlacementPolicySpec::builder()
            .pool("manifest-pool".to_string())
            .priority(25)
            .host_direct(HostDirectPlacementPolicySpec {
                host_ref: "manifest-host".to_string(),
                checkout: HostDirectPlacementPolicyCheckout::Worktree,
            })
            .build();
        let manifest = policies
            .create(
                &empty_meta_with_labels(
                    &profile.host_direct_policy_name(),
                    BTreeMap::from([(MANAGED_BY_LABEL.to_string(), "manifest".to_string())]),
                ),
                &manifest_spec,
            )
            .await
            .expect("manifest-managed policy should exist");

        ensure_default_policies(&backend, NAMESPACE, &profile).await.expect("registration should tolerate managed collision");

        let unchanged = policies.get(&profile.host_direct_policy_name()).await.expect("manifest-managed policy should remain");
        assert_eq!(unchanged.spec, manifest_spec);
        assert_eq!(unchanged.metadata.resource_version, manifest.metadata.resource_version, "registration must not rewrite managed policy");
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
        run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, repo, CompletionAction::Retain).await;
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
        run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, repo, CompletionAction::Retain).await;
    }

    #[tokio::test]
    async fn passing_teardown_gate_removes_the_managed_worktree_path() {
        let temp = TempDir::new().expect("tempdir");
        let repo_default_dir = temp.path().join("flotilla-repos");
        let git_repo =
            TestGitRepo::init(temp.path().join("repo")).with_initial_commit().with_origin("git@github.com:flotilla-org/flotilla.git");
        let repo = git_repo.path().to_path_buf();
        let config = Arc::new(ConfigStore::with_base(temp.path().join("config")));
        config.save_repo(&ExecutionEnvironmentPath::new(&repo));
        let daemon = daemon_with_backend_and_runner(
            vec![repo.clone()],
            Arc::clone(&config),
            ResourceBackend::InMemory(Default::default()),
            Arc::new(MergedPrProcessRunner::new(884)),
        )
        .await;

        run_stage4a_flow_reaches_running_and_completes_convoy(daemon, config, repo_default_dir, repo, CompletionAction::Delete).await;
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
    async fn codex_interactive_prompt_is_observed_as_needing_input() {
        let temp = TempDir::new().expect("tempdir");
        let config_path = temp.path().join("config");
        std::fs::create_dir_all(&config_path).expect("config dir");
        std::fs::write(config_path.join("daemon.toml"), "machine_id = \"dinghy-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_path));
        let (daemon, pool) = crew_daemon(Arc::clone(&config)).await;
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
        let session_name = "terminal-demo-work-coder";
        pool.add_sessions(vec![flotilla_core::providers::terminal::TerminalSession {
            session_name: session_name.to_string(),
            status: TerminalStatus::Running,
            command: Some("codex".to_string()),
            working_directory: Some(ExecutionEnvironmentPath::new("/workspace")),
            screen_activity: Some(ScreenActivity::Stable),
        }])
        .await;
        pool.set_captured_screen(
            session_name,
            "Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, quit\n\nPress enter to continue",
        )
        .await;
        let spec = flotilla_resources::TerminalSessionSpec {
            env_ref: profile.host_direct_environment_name(),
            role: "coder".to_string(),
            source: TerminalSessionSource::Agent {
                selector: Selector { capability: "coding".to_string() },
                brief: flotilla_resources::TerminalBrief {
                    path: ".flotilla/briefs/coder.md".to_string(),
                    content: "Implement the issue.".to_string(),
                    copies: Vec::new(),
                },
                context: flotilla_resources::TerminalCrewContext {
                    namespace: NAMESPACE.to_string(),
                    convoy: "demo".to_string(),
                    vessel_ref: "demo-work".to_string(),
                },
                message: None,
            },
            cwd: "/workspace".to_string(),
            pool: "fake-terminals".to_string(),
        };

        let attention = runtime.observe_attention(session_name, &spec).await.expect("observe prompt").expect("attention observation");
        assert_eq!(attention.state, TerminalAttentionState::NeedsInput);

        pool.set_captured_screen(session_name, "› Ask Codex to do something\n\ngpt-5.6-sol high · /workspace").await;
        let attention =
            runtime.observe_attention(session_name, &spec).await.expect("observe normal composer").expect("attention observation");
        assert_eq!(attention.state, TerminalAttentionState::Idle);
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
    async fn crew_provisioning_recovers_lost_session_after_in_process_daemon_restart_and_runs_handoffs() {
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
        apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat");
        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            local_registry,
            None,
            profile.host_id.clone(),
            Some(ExecutionEnvironmentPath::new(&repo)),
            profile.host_direct_environment_name(),
        ));
        let controller_handles = spawn_controller_loops(
            Arc::clone(&state),
            NAMESPACE,
            Duration::from_millis(20),
            ControllerSupervision::default(),
            RuntimeHealth::default(),
        );

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
                                    brief_template: None,
                                })
                                .build(),
                            CrewSpec::builder()
                                .role("reviewer".to_string())
                                .source(CrewSource::Agent {
                                    selector: Selector { capability: "review".to_string() },
                                    prompt: Some("Review the coder's work.".to_string()),
                                    brief_template: None,
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
        assert!(coder_launch.env_vars.iter().any(|(key, value)| key == "CARGO_INCREMENTAL" && value == "0"));
        let watcher_launch = ensured.iter().find(|launch| launch.session_name.ends_with("-watcher")).expect("watcher launch");
        assert!(watcher_launch.env_vars.iter().any(|(key, value)| key == "CARGO_INCREMENTAL" && value == "0"));
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
        // The reviewer is latent above and has no terminal session yet, so the
        // convoy status has to agree rather than report a crew member that was
        // never launched as working.
        assert_eq!(initial_status.crew_work["implement"]["reviewer"].phase, flotilla_resources::CrewWorkPhase::Pending);
        assert_eq!(initial_status.crew_work["implement"]["reviewer"].started_at, None);
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

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
        }
        pool.remove_session(&coder.metadata.name).await;
        let listed_before_restart = convoys.list().await.expect("convoys before daemon restart");
        let mut convoy_watch =
            convoys.watch(flotilla_resources::WatchStart::resuming_from(&listed_before_restart)).await.expect("watch convoy recovery");
        let (daemon, pool) = crew_daemon_with_backend(Arc::clone(&config), backend.clone()).await;
        let local_registry = probe_local_provider_registry(&daemon, &config).await.expect("crew provider registry after restart");
        let profile = build_local_profile(&daemon, &local_registry).expect("local profile after restart");
        let surviving_sessions = terminals
            .list()
            .await
            .expect("persisted terminal resources")
            .items
            .into_iter()
            .filter(|session| session.metadata.name != coder.metadata.name)
            .map(|session| {
                flotilla_core::providers::terminal::TerminalSession::builder()
                    .session_name(session.metadata.name)
                    .status(TerminalStatus::Running)
                    .command("persisted process".to_string())
                    .working_directory(ExecutionEnvironmentPath::new(session.spec.cwd))
                    .build()
            })
            .collect();
        pool.add_sessions(surviving_sessions).await;
        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            local_registry,
            None,
            profile.host_id.clone(),
            Some(ExecutionEnvironmentPath::new(&repo)),
            profile.host_direct_environment_name(),
        ));
        let controller_handles = spawn_controller_loops(
            Arc::clone(&state),
            NAMESPACE,
            Duration::from_millis(20),
            ControllerSupervision::default(),
            RuntimeHealth::default(),
        );
        wait_until(|| {
            let terminals = terminals.clone();
            let name = coder.metadata.name.clone();
            let old_id = coder_id.clone();
            async move {
                terminals.get(&name).await.ok().and_then(|session| session.status).is_some_and(|status| {
                    status.phase == TerminalSessionPhase::Running && status.crew.is_some_and(|crew| crew.id != old_id)
                })
            }
        })
        .await;
        wait_until(|| {
            let convoys = convoys.clone();
            async move {
                convoys.get("crew-convoy").await.ok().and_then(|convoy| convoy.status).is_some_and(|status| {
                    status.phase == ConvoyPhase::Active
                        && status.work["implement"].phase == WorkPhase::Running
                        && status.crew_work["implement"]["coder"].phase == flotilla_resources::CrewWorkPhase::Working
                })
            }
        })
        .await;
        let mut saw_interrupted_work = false;
        let mut saw_interrupted_convoy = false;
        while !(saw_interrupted_work && saw_interrupted_convoy) {
            let event = tokio::time::timeout(Duration::from_secs(1), convoy_watch.next())
                .await
                .expect("interruption should be durably observable")
                .expect("convoy watch should remain open")
                .expect("convoy watch event");
            let convoy = match event {
                flotilla_resources::WatchEvent::Added(convoy) | flotilla_resources::WatchEvent::Modified(convoy) => convoy,
                flotilla_resources::WatchEvent::Deleted(_) => continue,
            };
            let Some(status) = convoy.status else { continue };
            saw_interrupted_work |= status.work.get("implement").is_some_and(|work| work.phase == WorkPhase::Interrupted);
            saw_interrupted_convoy |= status.phase == ConvoyPhase::Interrupted;
        }
        let revived_coder = terminals.get(&coder.metadata.name).await.expect("revived coder");
        let revived_coder_id = revived_coder.status.as_ref().and_then(|status| status.crew.as_ref()).expect("revived identity").id.clone();
        let reviewer = terminals
            .list()
            .await
            .expect("terminal list after restart")
            .items
            .into_iter()
            .find(|session| session.spec.role == "reviewer")
            .expect("reviewer session after restart");
        let reviewer_id = reviewer.status.as_ref().and_then(|status| status.crew.as_ref()).expect("revived reviewer identity").id.clone();

        let attach = daemon.resolve_attach_command_internal("crew-convoy/implement/coder").await.expect("attach coder");
        let [flotilla_protocol::ResolvedAttachAction::Command(args)] = attach.plan.0.as_slice() else {
            panic!("expected one local attach command, got {:?}", attach.plan);
        };
        assert!(flotilla_protocol::arg::flatten(args, 0).contains("attach terminal-crew-convoy-implement-coder"));

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
        let checkouts = backend.clone().using::<ResourceCheckout>(NAMESPACE);
        let checkout = checkouts.get("adopted-checkout-crew-convoy").await.expect("adopted checkout");
        let mut integration = checkout.status.expect("checkout status").integration;
        integration.landed = flotilla_resources::IntegrationCondition::builder()
            .value(flotilla_resources::ConditionValue::True)
            .details(vec!["no change request exists for branch".to_string()])
            .observed_at(Utc::now().to_rfc3339())
            .build();
        flotilla_resources::apply_status_patch(
            &checkouts,
            &checkout.metadata.name,
            &flotilla_resources::CheckoutStatusPatch::UpdateIntegration { integration: Box::new(integration) },
        )
        .await
        .expect("record observed absence of a change request");
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
                    .is_some_and(|status| status.phase == ConvoyPhase::Landed)
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
                            .source(CrewSource::Agent {
                                selector: Selector { capability: "architect".to_string() },
                                prompt: None,
                                brief_template: None,
                            })
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
        apply_host_heartbeat(&daemon, NAMESPACE, &profile, &test_health_identity()).await.expect("host heartbeat should succeed");

        let state = Arc::new(ControllerRuntimeState::new(
            Arc::clone(&daemon),
            Arc::clone(&config),
            passthrough_registry(),
            None,
            profile.host_id.clone(),
            Some(ExecutionEnvironmentPath::new(&repo)),
            profile.host_direct_environment_name(),
        ));
        let controller_handles = spawn_controller_loops(
            Arc::clone(&state),
            NAMESPACE,
            Duration::from_millis(25),
            ControllerSupervision::default(),
            RuntimeHealth::default(),
        );

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

        daemon.reconcile_adopted_checkouts(NAMESPACE).await.expect("adopted checkout integration observation should succeed");
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
                        if status.phase == ConvoyPhase::Landed
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
