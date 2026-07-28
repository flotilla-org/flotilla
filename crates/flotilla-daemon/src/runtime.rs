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
    DockerEnvironmentRuntime, DockerProvisioning, EnvironmentReconciler, ForgeDefaultBranchResolver, HopChainContext, PreparedCheckout,
    PresentationPolicyRegistry, PresentationReconciler, ProviderPresentationRuntime, RepositoryReconciler, TerminalRuntime,
    TerminalRuntimeState, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_core::{
    agent_adapter::{AgentLaunchRequest, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    checkout_integration::{checkout_path_from_status_and_spec, inspect_checkout_integration},
    config::ConfigStore,
    in_process::InProcessDaemon,
    measure_available_space,
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{run_provisioned_host_detectors, EnvironmentBag},
        environment::{CreateOpts, EnvironmentHandle},
        registry::ProviderRegistry,
        terminal::{ScreenActivity, TerminalPool},
        vcs::{CloneInspection, CloneProvisioner, GitCloneProvisioner},
        ChannelLabel, CommandRunner,
    },
};
use flotilla_protocol::{EnvironmentId, HostSummary, ImageId, Rows, TerminalStatus};
use flotilla_resources::{
    canonicalize_repo_url, clone_key, controller::ControllerLoop, descriptive_repo_slug, Checkout, CheckoutBranchProvenance,
    CheckoutIntegrationStatus, Clone, CloneSpec, ConditionValue, Convoy, ConvoyReconciler, ConvoyTeardownRuntime, CrewSource, CrewSpec,
    Demand, DockerCheckoutStrategy, DockerPerVesselPlacementPolicySpec, Environment, EnvironmentSpec, ForgeIdentity, Host, HostCondition,
    HostDirectEnvironmentSpec, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, HostSpec, HostStatus, InputDefinition,
    InputMeta, PlacementPolicy, PlacementPolicySpec, Presentation, Project, Regard, Repository, ResourceBackend, ResourceError,
    ResourceObject, Stance, TerminalSession, TerminalSessionSource, Vessel, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
    AGENT_ADAPTERS_CAPABILITY, CREDENTIAL_REFS_ENV, CREDENTIAL_REF_SESSION_TAG, HELD_CREDENTIALS_CAPABILITY, MANAGED_BY_LABEL,
};
use serde_json::json;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, error, info, warn};

use crate::{
    credential::CredentialStore,
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
}

impl RuntimeHealth {
    fn report_capability_regression(&self, condition: HostCondition) {
        self.failures.lock().expect("runtime health lock poisoned").insert(condition.condition_type.clone(), condition);
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

    fn conditions(&self) -> Vec<HostCondition> {
        self.failures.lock().expect("runtime health lock poisoned").values().cloned().collect()
    }
}

fn agent_adapter_regression_condition(
    previous: Option<&HostStatus>,
    current: &BTreeSet<String>,
    health: &DaemonHealthIdentity,
) -> Option<HostCondition> {
    let previous = previous?;
    let same_daemon = previous.daemon_generation == health.generation && previous.daemon_started_at == Some(health.started_at);
    if same_daemon {
        return None;
    }
    let previous_adapters = match previous.agent_adapters() {
        Ok(adapters) => adapters,
        Err(error) => {
            warn!(%error, "cannot compare agent adapter capabilities with previous daemon generation");
            return None;
        }
    };
    let missing = previous_adapters.difference(current).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        return None;
    }

    warn!(
        previous_generation = ?previous.daemon_generation,
        current_generation = ?health.generation,
        previous_adapters = ?previous_adapters,
        current_adapters = ?current,
        missing_adapters = ?missing,
        "host capabilities regressed across daemon restart"
    );
    Some(
        HostCondition::builder()
            .condition_type("CapabilityRegression")
            .value(ConditionValue::False)
            .reason("AgentAdaptersMissing")
            .message(format!("agent adapters advertised by the previous daemon generation are missing: {}", missing.join(", ")))
            .observed_at(Utc::now())
            .build(),
    )
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
        let runtime_health = RuntimeHealth::default();
        register_startup_resources(&daemon, &options.namespace, &profile).await?;
        flotilla_resources::PreparedSnapshotGarbageCollector::new(daemon.resource_backend(), &options.namespace)
            .recover_pending_claims()
            .await
            .map_err(|error| format!("recover prepared convoy admissions: {error}"))?;
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
                .with_credential_store(credential_store),
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
    daemon_socket_path: Option<DaemonHostPath>,
    local_host_ref: String,
    local_repo_root: Option<ExecutionEnvironmentPath>,
    host_direct_environment_name: String,
    credential_store: Option<Arc<CredentialStore>>,
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
        Self {
            daemon,
            config,
            local_registry,
            daemon_socket_path,
            local_host_ref,
            local_repo_root,
            host_direct_environment_name,
            credential_store: None,
            provisioned_environments: Mutex::new(HashMap::new()),
            clone_flights: Arc::new(CloneFlights::default()),
        }
    }

    fn with_credential_store(mut self, credential_store: Arc<CredentialStore>) -> Self {
        self.credential_store = Some(credential_store);
        self
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
        display_name: daemon.host_name().to_string(),
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
                            pull_policy: Default::default(),
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
            let convoys = backend.clone().using::<Convoy>(&namespace);
            let hosts = backend.clone().using::<Host>(&namespace);
            let host_id = host_id.clone();
            async move { sleep_inhibitor::run(convoys, hosts, host_id).await }
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
    if let Some(condition) = agent_adapter_regression_condition(host.status.as_ref(), &profile.available_agent_adapters, health) {
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
    let conditions = runtime_health.conditions();
    let status = HostStatus {
        capabilities: host_capabilities(&summary, profile, &held_credentials),
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
                    let namespace_string = namespace_string.clone();
                    let runner = state.daemon.local_command_runner().expect("local runner should exist");
                    let flights = Arc::clone(&state.clone_flights);
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Clone>(&namespace_string),
                            secondaries: vec![],
                            reconciler: CloneReconciler::new(
                                Arc::new(CloneControllerRuntime { runner, flights }),
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
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("checkout", supervision, runtime_health, move || {
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
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("terminal_session", supervision, runtime_health, move || {
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
            let config_dir = state.config.base_path().as_path().to_path_buf();
            let supervision = supervision.clone();
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("vessel", supervision, runtime_health, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let config_dir = config_dir.clone();
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Vessel>(&namespace_string),
                            secondaries: VesselReconciler::secondary_watches(),
                            reconciler: VesselReconciler::new_with_config_dir(backend.clone(), &namespace_string, config_dir),
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
                supervise_controller("presentation", supervision, runtime_health, move || {
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
            let daemon = Arc::clone(&state.daemon);
            let supervision = supervision.clone();
            let runtime_health = runtime_health.clone();
            async move {
                supervise_controller("convoy", supervision, runtime_health, move || {
                    let backend = backend.clone();
                    let namespace_string = namespace_string.clone();
                    let daemon = Arc::clone(&daemon);
                    async move {
                        ControllerLoop {
                            primary: backend.clone().using::<Convoy>(&namespace_string),
                            secondaries: ConvoyReconciler::secondary_watches(),
                            reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>(&namespace_string))
                                .with_vessels(backend.clone().using::<Vessel>(&namespace_string))
                                .with_terminal_sessions(backend.clone().using::<TerminalSession>(&namespace_string))
                                .with_presentations(backend.clone().using::<Presentation>(&namespace_string))
                                .with_checkouts(backend.clone().using::<Checkout>(&namespace_string))
                                .with_teardown_runtime(Arc::new(DaemonConvoyTeardownRuntime::new(daemon)))
                                .with_prepared_snapshot_gc(flotilla_resources::PreparedSnapshotGarbageCollector::new(
                                    backend.clone(),
                                    &namespace_string,
                                )),
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
    runtime_health: RuntimeHealth,
) -> JoinHandle<()> {
    let durable = daemon.resource_backend();
    let observed = daemon.observed_resource_backend();
    tokio::spawn(async move {
        supervise_controller("aggregator", supervision, runtime_health, move || {
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
                            .durable_convoys(durable.including_replicas::<Convoy>(&namespace))
                            .durable_demands(durable.clone().using::<Demand>(&namespace))
                            .durable_environments(durable.clone().using::<Environment>(&namespace))
                            .durable_presentations(durable.using::<Presentation>(&namespace))
                            .durable_sessions(durable.including_replicas::<flotilla_resources::TerminalSession>(&namespace))
                            .durable_projects(durable.including_replicas::<Project>(&namespace))
                            .durable_repositories(durable.using::<Repository>(&namespace))
                            .durable_regards(durable.using::<Regard>(&namespace))
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
    async fn provision(&self, name: &str, spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        let credential_refs = credential_refs_from_environment(spec)?;
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

        let docker_config_dir = match &self.state.credential_store {
            Some(store) => store.prepare_registry_pull(name, &credential_refs, &spec.image).await?.map(DaemonHostPath::new),
            None if credential_refs.is_empty() => None,
            None => return Err("host-local credential store unavailable".to_string()),
        };
        let image = ImageId::new(spec.image.clone());
        let env_id = EnvironmentId::new(name.to_string());
        let handle = match provider
            .create(env_id.clone(), &image, CreateOpts {
                tokens: Vec::new(),
                daemon_socket_path,
                working_directory: None,
                image_pull_policy: spec.pull_policy.into(),
                provisioned_mounts: spec.mounts.iter().map(flotilla_controllers::actuators::provisioned_mount).collect(),
                docker_config_dir,
            })
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(store) = &self.state.credential_store {
                    if let Err(cleanup_error) = store.forget_environment(name).await {
                        return Err(format!("{error}; additionally failed to remove credential cache: {cleanup_error}"));
                    }
                }
                return Err(error);
            }
        };
        let image_ref = handle.image().as_str().to_string();
        let image_digest = match handle.image_digest() {
            Some(digest) => digest.to_string(),
            None => {
                return Err(discard_failed_environment(
                    &handle,
                    self.state.credential_store.as_deref(),
                    name,
                    format!("docker environment provider did not report an image digest for {name}"),
                )
                .await)
            }
        };

        let container_id = handle.container_name().map(ToString::to_string).unwrap_or_else(|| format!("flotilla-env-{}", env_id));
        if let Some(store) = &self.state.credential_store {
            if let Err(error) = store.prepare(name, &credential_refs, handle.runner()).await {
                return Err(discard_failed_environment(&handle, Some(store), name, error).await);
            }
        } else if !credential_refs.is_empty() {
            return Err(discard_failed_environment(&handle, None, name, "host-local credential store unavailable".to_string()).await);
        }
        let (bag, registry) = match probe_provisioned_environment(&self.state, &env_id, &handle).await {
            Ok(probed) => probed,
            Err(error) => {
                return Err(discard_failed_environment(&handle, self.state.credential_store.as_deref(), name, error).await);
            }
        };
        if let Err(error) = verify_declared_agent_adapters(spec, &registry) {
            return Err(discard_failed_environment(&handle, self.state.credential_store.as_deref(), name, error).await);
        }
        if let Err(error) = self
            .state
            .daemon
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(registry))
            .map_err(|err| format!("failed to register provisioned environment {env_id}: {err}"))
        {
            return Err(discard_failed_environment(&handle, self.state.credential_store.as_deref(), name, error).await);
        }
        self.state.provisioned_environments.lock().await.insert(container_id.clone(), ActiveProvisionedEnvironment { env_id, handle });
        Ok(DockerProvisioning { container_id, image_ref, image_digest })
    }

    async fn destroy(&self, container_id: &str) -> Result<(), String> {
        let active = self.state.provisioned_environments.lock().await.remove(container_id);
        let Some(active) = active else {
            return Ok(());
        };
        active.handle.destroy().await?;
        let _ = self.state.daemon.remove_provisioned_environment(&active.env_id);
        if let Some(store) = &self.state.credential_store {
            store.forget_environment(active.env_id.as_str()).await?;
        }
        Ok(())
    }
}

fn verify_declared_agent_adapters(spec: &flotilla_resources::DockerEnvironmentSpec, registry: &ProviderRegistry) -> Result<(), String> {
    let discovered = registry.agent_adapters.ids().collect::<BTreeSet<_>>();
    let missing =
        spec.declared_agent_adapters.iter().map(String::as_str).filter(|adapter| !discovered.contains(adapter)).collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "image `{}` declares agent adapter{} {}, but interior discovery did not find {}",
        spec.image,
        if missing.len() == 1 { "" } else { "s" },
        missing.iter().map(|adapter| format!("`{adapter}`")).collect::<Vec<_>>().join(", "),
        if missing.len() == 1 { "it" } else { "them" },
    ))
}

fn credential_refs_from_environment(spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<BTreeSet<String>, String> {
    spec.env
        .get(CREDENTIAL_REFS_ENV)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid credential references: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

async fn discard_failed_environment(
    handle: &EnvironmentHandle,
    credential_store: Option<&CredentialStore>,
    environment_ref: &str,
    error: String,
) -> String {
    let mut cleanup_errors = Vec::new();
    if let Err(cleanup_error) = handle.destroy().await {
        cleanup_errors.push(format!("destroy rejected environment: {cleanup_error}"));
    }
    if let Some(store) = credential_store {
        if let Err(cleanup_error) = store.forget_environment(environment_ref).await {
            cleanup_errors.push(format!("remove credential cache: {cleanup_error}"));
        }
    }
    if cleanup_errors.is_empty() {
        error
    } else {
        format!("{error}; additionally failed to {}", cleanup_errors.join("; "))
    }
}

async fn probe_provisioned_environment(
    state: &ControllerRuntimeState,
    env_id: &EnvironmentId,
    handle: &EnvironmentHandle,
) -> Result<(EnvironmentBag, Arc<ProviderRegistry>), String> {
    let env_vars = handle.env_vars().await?;
    let discovery = state.daemon.discovery_runtime();
    let bag = run_provisioned_host_detectors(&discovery.host_detectors, &*handle.runner(), &env_vars).await;
    let probe_root = ExecutionEnvironmentPath::new("/workspace");
    let config = ConfigStore::with_base(state.config.base_path().as_path().join(format!("env-discovery/{env_id}")));
    let registry = discovery.factories.probe_all(&bag, &config, &probe_root, handle.runner()).await;
    Ok((bag, Arc::new(registry)))
}

#[derive(Default)]
struct CloneFlights {
    by_target: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl CloneFlights {
    fn for_target(&self, target_path: &str) -> Arc<Mutex<()>> {
        let mut by_target = self.by_target.lock().expect("clone flights lock poisoned");
        by_target.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = by_target.get(target_path).and_then(Weak::upgrade) {
            return flight;
        }
        let flight = Arc::new(Mutex::new(()));
        by_target.insert(target_path.to_string(), Arc::downgrade(&flight));
        flight
    }
}

struct CloneControllerRuntime {
    runner: Arc<dyn CommandRunner>,
    flights: Arc<CloneFlights>,
}

#[async_trait]
impl CloneRuntime for CloneControllerRuntime {
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        let flight = self.flights.for_target(target_path);
        let _flight_guard = flight.lock().await;
        if let Some(inspection) = recover_existing_clone(Arc::clone(&self.runner), repo_url, target_path).await? {
            return Ok(inspection.default_branch);
        }

        let staging_path = clone_staging_path(target_path);
        remove_checkout_path(&*self.runner, &staging_path).await?;
        let provisioner = GitCloneProvisioner::new(Arc::clone(&self.runner));
        let staging = ExecutionEnvironmentPath::new(&staging_path);
        let prepare = async {
            provisioner.clone_repo(repo_url, &staging).await?;
            provisioner.inspect_clone(&staging).await
        }
        .await;
        let inspection = match prepare {
            Ok(inspection) => inspection,
            Err(error) => return Err(cleanup_failed_checkout(&*self.runner, &staging_path, error).await),
        };
        if let Err(error) = tokio::fs::rename(&staging_path, target_path).await {
            let error = cleanup_failed_checkout(&*self.runner, &staging_path, format!("publish clone: {error}")).await;
            return match recover_existing_clone(Arc::clone(&self.runner), repo_url, target_path).await {
                Ok(Some(inspection)) => Ok(inspection.default_branch),
                Ok(None) => Err(error),
                Err(adoption_error) => Err(format!("{error}; additionally failed to adopt clone at target: {adoption_error}")),
            };
        }
        Ok(inspection.default_branch)
    }

    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String> {
        let provisioner = GitCloneProvisioner::new(Arc::clone(&self.runner));
        let inspection = provisioner.inspect_clone(&ExecutionEnvironmentPath::new(target_path)).await?;
        Ok(inspection.default_branch)
    }
}

async fn recover_existing_clone(
    runner: Arc<dyn CommandRunner>,
    repo_url: &str,
    target_path: &str,
) -> Result<Option<CloneInspection>, String> {
    if !runner.path_exists(Path::new(target_path)).await? {
        return Ok(None);
    }

    verify_clone_origin(&*runner, repo_url, target_path, "clone target").await?;
    GitCloneProvisioner::new(runner).inspect_clone(&ExecutionEnvironmentPath::new(target_path)).await.map(Some)
}

async fn verify_clone_origin(runner: &dyn CommandRunner, repo_url: &str, target_path: &str, target_label: &str) -> Result<(), String> {
    let origin = runner
        .run("git", &["-C", target_path, "remote", "get-url", "origin"], Path::new("/"), &ChannelLabel::Noop)
        .await
        .map_err(|error| format!("{target_label} {target_path} already exists but is not a reusable clone: {error}"))?;
    let origin = origin.trim();
    let same_origin = origin == repo_url
        || canonicalize_repo_url(origin)
            .ok()
            .zip(canonicalize_repo_url(repo_url).ok())
            .is_some_and(|(origin, expected)| origin == expected);
    if !same_origin {
        return Err(format!("{target_label} {target_path} already exists with origin {origin}, expected {repo_url}"));
    }

    Ok(())
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
        if let Some(prepared) = recover_existing_worktree(&*runner, clone_path, branch, target_path).await? {
            return Ok(prepared);
        }

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
        if let Some(prepared) = recover_existing_fresh_clone(&*runner, repo_url, branch, target_path).await? {
            return Ok(prepared);
        }
        let staging_path = clone_staging_path(target_path);
        remove_checkout_path(&*runner, &staging_path).await?;
        let clone_ref = base_ref.unwrap_or(branch);
        let prepare = async {
            if clone_ref == "HEAD" {
                runner.run("git", &["clone", repo_url, &staging_path], Path::new("/"), &ChannelLabel::Noop).await?;
            } else {
                runner.run("git", &["clone", "--branch", clone_ref, repo_url, &staging_path], Path::new("/"), &ChannelLabel::Noop).await?;
            }
            if clone_ref != branch {
                let remote_ref = format!("refs/remotes/origin/{branch}");
                let remote_exists = runner
                    .run("git", &["-C", &staging_path, "show-ref", "--verify", "--quiet", &remote_ref], Path::new("/"), &ChannelLabel::Noop)
                    .await
                    .is_ok();
                if remote_exists {
                    runner
                        .run(
                            "git",
                            &["-C", &staging_path, "switch", "-c", branch, "--track", &format!("origin/{branch}")],
                            Path::new("/"),
                            &ChannelLabel::Noop,
                        )
                        .await?;
                } else {
                    runner.run("git", &["-C", &staging_path, "switch", "-c", branch], Path::new("/"), &ChannelLabel::Noop).await?;
                }
            }
            resolve_head_commit(&*runner, &staging_path).await
        }
        .await;
        let commit = match prepare {
            Ok(commit) => commit,
            Err(error) => {
                return Err(cleanup_failed_checkout(&*runner, &staging_path, error).await);
            }
        };
        if let Err(error) = tokio::fs::rename(&staging_path, target_path).await {
            return Err(cleanup_failed_checkout(&*runner, &staging_path, format!("publish fresh clone: {error}")).await);
        }
        Ok(PreparedCheckout { commit, branch_provenance: CheckoutBranchProvenance::PreExisting })
    }

    async fn inspect_integration(&self, checkout: &ResourceObject<Checkout>) -> Result<CheckoutIntegrationStatus, String> {
        Ok(inspect_checkout_integration(&*self.local_runner()?, Path::new(self.checkout_path(checkout)?), &checkout.spec).await)
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        let runner = self.local_runner()?;
        match removal {
            CheckoutRemoval::FreshClone { target_path } => {
                let target_path = utf8_path(target_path)?;
                remove_checkout_path(&*runner, target_path).await?;
                remove_checkout_path(&*runner, &clone_staging_path(target_path)).await?;
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
                remove_checkout_path(&*runner, target_path).await?;
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

async fn recover_existing_worktree(
    runner: &dyn CommandRunner,
    clone_path: &str,
    branch: &str,
    target_path: &str,
) -> Result<Option<PreparedCheckout>, String> {
    if !runner.path_exists(Path::new(target_path)).await? {
        return Ok(None);
    }

    let target_common_dir = runner
        .run("git", &["-C", target_path, "rev-parse", "--path-format=absolute", "--git-common-dir"], Path::new("/"), &ChannelLabel::Noop)
        .await
        .map_err(|error| format!("checkout target {target_path} already exists but is not a reusable git worktree: {error}"))?;
    let clone_common_dir = runner
        .run("git", &["-C", clone_path, "rev-parse", "--path-format=absolute", "--git-common-dir"], Path::new("/"), &ChannelLabel::Noop)
        .await?;
    if target_common_dir.trim() != clone_common_dir.trim() {
        return Err(format!("checkout target {target_path} already exists but belongs to a different git repository"));
    }

    let current_branch =
        runner.run("git", &["-C", target_path, "symbolic-ref", "--quiet", "--short", "HEAD"], Path::new("/"), &ChannelLabel::Noop).await;
    if current_branch.as_deref().map(str::trim) != Ok(branch) {
        let target_commit = resolve_head_commit(runner, target_path).await?;
        let expected_commit = runner.run("git", &["-C", clone_path, "rev-parse", branch], Path::new("/"), &ChannelLabel::Noop).await?;
        if target_commit.as_deref() != Some(expected_commit.trim()) {
            return Err(format!("checkout target {target_path} already exists at a different ref than {branch}"));
        }
    }

    let branch_provenance = if runner
        .run(
            "git",
            &["-C", clone_path, "show-ref", "--verify", "--quiet", &bootstrap_branch_ref(branch)],
            Path::new("/"),
            &ChannelLabel::Noop,
        )
        .await
        .is_ok()
    {
        CheckoutBranchProvenance::CreatedForConvoy
    } else {
        CheckoutBranchProvenance::PreExisting
    };
    Ok(Some(PreparedCheckout { commit: resolve_head_commit(runner, target_path).await?, branch_provenance }))
}

async fn recover_existing_fresh_clone(
    runner: &dyn CommandRunner,
    repo_url: &str,
    branch: &str,
    target_path: &str,
) -> Result<Option<PreparedCheckout>, String> {
    if !runner.path_exists(Path::new(target_path)).await? {
        return Ok(None);
    }

    verify_clone_origin(runner, repo_url, target_path, "checkout target").await?;

    if branch != "HEAD" {
        let current_branch = runner
            .run("git", &["-C", target_path, "symbolic-ref", "--quiet", "--short", "HEAD"], Path::new("/"), &ChannelLabel::Noop)
            .await
            .map_err(|error| format!("checkout target {target_path} already exists but its branch cannot be resolved: {error}"))?;
        if current_branch.trim() != branch {
            return Err(format!("checkout target {target_path} already exists on branch {}, expected {branch}", current_branch.trim()));
        }
    }

    Ok(Some(PreparedCheckout {
        commit: resolve_head_commit(runner, target_path).await?,
        branch_provenance: CheckoutBranchProvenance::PreExisting,
    }))
}

fn bootstrap_branch_ref(branch: &str) -> String {
    format!("refs/flotilla/bootstrap/{branch}")
}

async fn delete_ref(runner: &dyn CommandRunner, clone_path: &str, reference: &str) -> Result<(), String> {
    runner.run("git", &["-C", clone_path, "update-ref", "-d", reference], Path::new("/"), &ChannelLabel::Noop).await?;
    Ok(())
}

async fn remove_checkout_path(runner: &dyn CommandRunner, target_path: &str) -> Result<(), String> {
    runner.run("rm", &["-rf", target_path], Path::new("/"), &ChannelLabel::Noop).await?;
    for predicate in ["-e", "-L"] {
        let remaining = runner.run_output("test", &[predicate, target_path], Path::new("/"), &ChannelLabel::Noop).await?;
        if remaining.success {
            return Err(format!("checkout cleanup reported success but path remains: {target_path}"));
        }
    }
    Ok(())
}

async fn cleanup_failed_checkout(runner: &dyn CommandRunner, target_path: &str, error: String) -> String {
    match remove_checkout_path(runner, target_path).await {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; additionally failed to remove partial checkout: {cleanup_error}"),
    }
}

fn clone_staging_path(target_path: &str) -> String {
    format!("{target_path}.flotilla-clone-partial")
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
        let credential_refs =
            tags.iter().filter(|tag| tag.key == CREDENTIAL_REF_SESSION_TAG).map(|tag| tag.value.clone()).collect::<BTreeSet<_>>();
        let pool_tags = tags.iter().filter(|tag| tag.key != CREDENTIAL_REF_SESSION_TAG).cloned().collect::<Vec<_>>();
        let credential_env = match &self.state.credential_store {
            Some(store) => {
                let runner = self.runner_for_env(&spec.env_ref)?;
                store.prepare(&spec.env_ref, &credential_refs, runner).await?
            }
            None if credential_refs.is_empty() => Vec::new(),
            None => return Err("host-local credential store unavailable".to_string()),
        };
        let (command, mut env, crew, initial_message) = match &spec.source {
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
        env.extend(credential_env);
        env.push(("CARGO_INCREMENTAL".to_string(), "0".to_string()));

        if matches!(spec.source, TerminalSessionSource::Agent { .. })
            && pool.list_sessions().await?.iter().any(|session| session.session_name == name)
        {
            pool.kill_session(name).await?;
        }
        pool.ensure_session(name, &command, &cwd, &env, &pool_tags).await?;
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
        if activity == ScreenActivity::Stable {
            if let TerminalSessionSource::Agent { selector, .. } = &spec.source {
                let requirement = CapabilityTable::seeded().resolve(&selector.capability)?.clone();
                let registry = self.registry_for_env(&spec.env_ref)?;
                let adapter = registry
                    .agent_adapters
                    .get(&requirement.adapter)
                    .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;
                match pool.capture_screen(session_id).await {
                    Ok(Some(screen))
                        if adapter.classify_screen_attention(&screen) == Some(flotilla_resources::TerminalAttentionState::NeedsInput) =>
                    {
                        return Ok(Some(flotilla_resources::TerminalAttention {
                            state: flotilla_resources::TerminalAttentionState::NeedsInput,
                            as_of: Utc::now(),
                            source: flotilla_resources::TerminalAttentionSource::Screen,
                        }));
                    }
                    Ok(_) => {}
                    Err(error) => tracing::debug!(%session_id, %error, "could not capture terminal screen for attention observation"),
                }
            }
        }
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
    fn runner_for_env(&self, env_ref: &str) -> Result<Arc<dyn CommandRunner>, String> {
        if env_ref == self.state.host_direct_environment_name {
            return self.state.daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string());
        }
        self.state
            .daemon
            .command_runner_for_environment(&EnvironmentId::new(env_ref.to_string()))
            .ok_or_else(|| format!("command runner unavailable for environment {env_ref}"))
    }

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
    use std::{
        collections::HashMap,
        fs,
        process::Command as ProcessCommand,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
    };

    use flotilla_core::{
        config::ConfigStore,
        daemon::DaemonHandle,
        in_process::DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE,
        providers::{
            discovery::{
                test_support::{
                    fake_discovery_with_provider_set, git_process_discovery, DiscoveryMockRunner, FakeDiscoveryProviders, FakeTerminalPool,
                    MergedPrProcessRunner,
                },
                EnvironmentAssertion, EnvironmentBag,
            },
            environment::{EnvironmentHandle, EnvironmentProvider, ProvisionedEnvironment, ProvisionedMount},
            ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner,
        },
    };
    use flotilla_protocol::{Command, CommandAction, CommandValue, CrewCommandContext, DaemonEvent, ImageId, ImageSource};
    use flotilla_resources::{
        Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec,
        CheckoutStatus as ResourceCheckoutStatus, ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, CrewSource, CrewSpec, LifecycleAuthority,
        ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PlacementPolicy, RepositorySpec, Selector, SqliteBackend,
        TerminalAttentionState, TerminalSession, TerminalSessionPhase, VesselRequirement, WorkPhase, WorkflowTemplate,
        WorkflowTemplateSpec,
    };
    use futures::StreamExt;
    use tempfile::TempDir;
    use tokio::sync::Notify;

    use super::{test_git_repo::TestGitRepo, *};

    #[derive(Clone, Copy)]
    enum CompletionAction {
        Retain,
        Delete,
    }

    #[derive(Clone)]
    struct LogCaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log output lock should be healthy").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct NoPrProcessRunner;

    #[async_trait]
    impl CommandRunner for NoPrProcessRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            if cmd == "gh" {
                Ok("[]".to_string())
            } else {
                ProcessCommandRunner.run(cmd, args, cwd, label).await
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            if cmd == "gh" {
                Ok(CommandOutput { stdout: "[]".to_string(), stderr: String::new(), success: true })
            } else {
                ProcessCommandRunner.run_output(cmd, args, cwd, label).await
            }
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
    }

    struct FailFirstCloneProcessRunner {
        failed: AtomicBool,
    }

    #[async_trait]
    impl CommandRunner for FailFirstCloneProcessRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            if cmd == "git" && args.first() == Some(&"clone") && !self.failed.swap(true, Ordering::SeqCst) {
                let destination = args.last().expect("git clone should have a destination");
                fs::create_dir_all(destination).expect("failed clone should create its partial destination");
                fs::write(Path::new(destination).join("partial"), "incomplete clone").expect("failed clone should leave partial content");
                Err("simulated interrupted clone".to_string())
            } else {
                ProcessCommandRunner.run(cmd, args, cwd, label).await
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
    }

    struct BlockingCloneProcessRunner {
        clone_attempts: AtomicUsize,
        clone_started: Notify,
        release_clone: Notify,
    }

    #[async_trait]
    impl CommandRunner for BlockingCloneProcessRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            if cmd == "git" && args.first() == Some(&"clone") {
                let attempt = self.clone_attempts.fetch_add(1, Ordering::SeqCst);
                self.clone_started.notify_one();
                if attempt == 0 {
                    self.release_clone.notified().await;
                }
            }
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
    }

    struct TestInteriorEnvironment {
        id: EnvironmentId,
        image: ImageId,
        runner: Arc<dyn CommandRunner>,
        env_vars: HashMap<String, String>,
        destroyed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ProvisionedEnvironment for TestInteriorEnvironment {
        fn id(&self) -> &EnvironmentId {
            &self.id
        }

        fn image(&self) -> &ImageId {
            &self.image
        }

        fn image_digest(&self) -> Option<&str> {
            Some("sha256:test-interior")
        }

        fn container_name(&self) -> Option<&str> {
            Some("test-interior")
        }

        fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
            Vec::new()
        }

        async fn status(&self) -> Result<flotilla_protocol::EnvironmentStatus, String> {
            Ok(flotilla_protocol::EnvironmentStatus::Running)
        }

        async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
            Ok(self.env_vars.clone())
        }

        fn runner(&self) -> Arc<dyn CommandRunner> {
            Arc::clone(&self.runner)
        }

        async fn destroy(&self) -> Result<(), String> {
            self.destroyed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn clone_runtime_single_flights_concurrent_requests_for_the_same_target() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("clone");
        let runner = Arc::new(BlockingCloneProcessRunner {
            clone_attempts: AtomicUsize::new(0),
            clone_started: Notify::new(),
            release_clone: Notify::new(),
        });
        let flights = Arc::new(CloneFlights::default());
        let first_runtime = Arc::new(CloneControllerRuntime { runner: runner.clone(), flights: Arc::clone(&flights) });
        let second_runtime = Arc::new(CloneControllerRuntime { runner: runner.clone(), flights });
        let repo_url = source.path().to_str().expect("utf-8 source path").to_string();
        let target_path = target.to_str().expect("utf-8 target path").to_string();

        let first = tokio::spawn({
            let runtime = Arc::clone(&first_runtime);
            let repo_url = repo_url.clone();
            let target_path = target_path.clone();
            async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
        });
        runner.clone_started.notified().await;
        let second = tokio::spawn({
            let runtime = Arc::clone(&second_runtime);
            async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        runner.release_clone.notify_one();

        let first = first.await.expect("first clone task should join");
        let second = second.await.expect("second clone task should join");

        assert_eq!(first.expect("first clone should succeed").as_deref(), Some("main"));
        assert_eq!(second.expect("second clone should succeed").as_deref(), Some("main"));
        assert_eq!(runner.clone_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn clone_runtime_adopts_a_matching_clone_that_wins_an_external_flight() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("clone");
        let runner = Arc::new(BlockingCloneProcessRunner {
            clone_attempts: AtomicUsize::new(0),
            clone_started: Notify::new(),
            release_clone: Notify::new(),
        });
        let first_runtime = Arc::new(CloneControllerRuntime { runner: runner.clone(), flights: Arc::new(CloneFlights::default()) });
        let external_runtime = Arc::new(CloneControllerRuntime { runner: runner.clone(), flights: Arc::new(CloneFlights::default()) });
        let repo_url = source.path().to_str().expect("utf-8 source path").to_string();
        let target_path = target.to_str().expect("utf-8 target path").to_string();

        let first = tokio::spawn({
            let runtime = Arc::clone(&first_runtime);
            let repo_url = repo_url.clone();
            let target_path = target_path.clone();
            async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
        });
        runner.clone_started.notified().await;
        let external = tokio::spawn({
            let runtime = Arc::clone(&external_runtime);
            async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
        });
        let external = external.await.expect("external clone task should join");
        runner.release_clone.notify_one();
        let first = first.await.expect("first clone task should join");

        assert_eq!(external.expect("external clone should succeed").as_deref(), Some("main"));
        assert_eq!(first.expect("losing flight should adopt the external clone").as_deref(), Some("main"));
        assert_eq!(runner.clone_attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn clone_runtime_retries_after_an_interrupted_clone() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("clone");
        let runtime = CloneControllerRuntime {
            runner: Arc::new(FailFirstCloneProcessRunner { failed: AtomicBool::new(false) }),
            flights: Arc::new(CloneFlights::default()),
        };
        let repo_url = source.path().to_str().expect("utf-8 source path");
        let target_path = target.to_str().expect("utf-8 target path");

        runtime.clone_and_inspect(repo_url, target_path).await.expect_err("interrupted clone should fail its first actuation");
        assert!(!target.exists(), "failed clone debris should be removed");

        let default_branch = runtime.clone_and_inspect(repo_url, target_path).await.expect("redrive should replace the interrupted clone");

        assert_eq!(default_branch.as_deref(), Some("main"));
    }

    struct TestInteriorEnvironmentProvider {
        handle: Mutex<Option<EnvironmentHandle>>,
    }

    #[async_trait]
    impl EnvironmentProvider for TestInteriorEnvironmentProvider {
        async fn ensure_image(&self, spec: &flotilla_protocol::EnvironmentSpec, _repo_root: &Path) -> Result<ImageId, String> {
            match &spec.image {
                ImageSource::Registry(image) => Ok(ImageId::new(image.clone())),
                ImageSource::Dockerfile { .. } => Err("test provider expects a registry image".to_string()),
            }
        }

        async fn create(&self, _id: EnvironmentId, _image: &ImageId, _opts: CreateOpts) -> Result<EnvironmentHandle, String> {
            self.handle.lock().await.take().ok_or_else(|| "test environment already created".to_string())
        }

        async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
            Ok(Vec::new())
        }
    }

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
    async fn checkout_runtime_recovers_a_worktree_after_its_completion_is_lost() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let target = temp.path().join("workspace/flotilla");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let first = runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("first worktree actuation should succeed");
        let recovered = runtime
            .create_worktree(
                clone.path().to_str().expect("utf-8 clone path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("redrive should recover the worktree already created on disk");

        assert_eq!(recovered, first);
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
    async fn checkout_runtime_removes_unregistered_worktree_path_with_embedded_repository() {
        let temp = TempDir::new().expect("tempdir");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let target = temp.path().join("checkout-root/convoy-a/flotilla.feature-cleanup");
        TestGitRepo::init(target.join("embedded")).with_initial_commit();
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };
        let removal = CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: "feature/cleanup".to_string(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        };

        assert_eq!(
            runtime.remove_checkout(&removal).await.expect("stale worktree path should be removed"),
            CheckoutRemovalOutcome::Removed
        );

        assert!(!target.exists(), "successful cleanup must not strand an embedded repository");
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
    async fn checkout_runtime_recovers_a_fresh_clone_after_its_completion_is_lost() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("fresh-clone");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let first = runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("first clone actuation should succeed");
        let recovered = runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("redrive should recover the clone already created on disk");

        assert_eq!(recovered, first);
    }

    #[tokio::test]
    async fn checkout_runtime_retries_after_an_interrupted_fresh_clone() {
        let temp = TempDir::new().expect("tempdir");
        let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
        let target = temp.path().join("fresh-clone");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(FailFirstCloneProcessRunner { failed: AtomicBool::new(false) }) };

        runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect_err("interrupted clone should fail its first actuation");
        assert!(!Path::new(&clone_staging_path(target.to_str().expect("utf-8 target path"))).exists());

        runtime
            .create_fresh_clone(
                source.path().to_str().expect("utf-8 source path"),
                "feature/redrive",
                Some("main"),
                target.to_str().expect("utf-8 target path"),
            )
            .await
            .expect("redrive should replace the interrupted clone");

        let branch = ProcessCommand::new("git")
            .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
            .output()
            .expect("git should inspect the retried clone");
        assert!(branch.status.success());
        assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/redrive");
    }

    #[tokio::test]
    async fn checkout_runtime_removes_an_interrupted_fresh_clone() {
        let temp = TempDir::new().expect("tempdir");
        let target = temp.path().join("fresh-clone");
        let target = target.to_str().expect("utf-8 target path");
        let staging_path = clone_staging_path(target);
        fs::create_dir_all(&staging_path).expect("create partial clone directory");
        fs::write(Path::new(&staging_path).join("partial"), "incomplete clone").expect("write partial clone content");
        let runtime = CheckoutControllerRuntime { runner: Arc::new(ProcessCommandRunner) };

        let outcome = runtime
            .remove_checkout(&CheckoutRemoval::FreshClone { target_path: target.to_string() })
            .await
            .expect("fresh clone removal should clean staging");

        assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
        assert!(!Path::new(&staging_path).exists());
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
    async fn provisioned_environment_discovers_and_registers_interior_agent_adapters() {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"interior-discovery-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
        discovery.host_detectors = flotilla_core::providers::discovery::detectors::default_host_detectors();
        let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
        let state = ControllerRuntimeState::new(
            Arc::clone(&daemon),
            config,
            passthrough_registry(),
            None,
            "host-test".to_string(),
            None,
            "host-direct-host-test".to_string(),
        );
        let env_id = EnvironmentId::new("contained-work");
        let runner: Arc<dyn CommandRunner> = Arc::new(
            DiscoveryMockRunner::builder()
                .on_run("codex", &["--version"], Ok("codex-cli 1.2.3".to_string()))
                .on_run("claude", &["--version"], Ok("1.0.0 (Claude Code)".to_string()))
                .build(),
        );
        let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
            id: env_id.clone(),
            image: ImageId::new("contained-image"),
            runner,
            env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
            destroyed: Arc::new(AtomicBool::new(false)),
        });

        let (bag, registry) = probe_provisioned_environment(&state, &env_id, &handle).await.expect("interior discovery should succeed");

        assert!(bag.find_binary("codex").is_some());
        assert!(bag.find_binary("claude").is_some());
        assert_eq!(bag.find_env_var("HOME"), Some("/home/crew"));
        assert!(registry.agent_adapters.get("codex").is_some());
        assert!(registry.agent_adapters.get("claude-code").is_some());

        daemon
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(Arc::clone(&registry)))
            .expect("provisioned environment should register");
        let registered = daemon.environment_registry_for_environment(&env_id).expect("interior registry should be available");
        assert!(registered.agent_adapters.get("codex").is_some());
        assert!(registered.agent_adapters.get("claude-code").is_some());

        let spec = flotilla_resources::DockerEnvironmentSpec {
            host_ref: "host-test".to_string(),
            image: "contained-image".to_string(),
            declared_agent_adapters: BTreeSet::from(["codex".to_string(), "missing-adapter".to_string()]),
            pull_policy: Default::default(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
        };
        let error = verify_declared_agent_adapters(&spec, &registry).expect_err("missing declared adapter should fail");
        assert_eq!(error, "image `contained-image` declares agent adapter `missing-adapter`, but interior discovery did not find it");
    }

    #[tokio::test]
    async fn provisioned_environment_is_destroyed_when_declared_adapter_is_missing() {
        let temp = TempDir::new().expect("tempdir");
        let config_base = temp.path().join("config");
        fs::create_dir_all(&config_base).expect("config directory");
        fs::write(config_base.join("daemon.toml"), "machine_id = \"interior-rejection-test\"\n").expect("daemon config");
        let config = Arc::new(ConfigStore::with_base(config_base));
        let mut discovery = fake_discovery_with_provider_set(FakeDiscoveryProviders::new());
        discovery.host_detectors = flotilla_core::providers::discovery::detectors::default_host_detectors();
        let daemon = InProcessDaemon::new(Vec::new(), Arc::clone(&config), discovery, flotilla_protocol::HostName::new("dinghy")).await;
        let destroyed = Arc::new(AtomicBool::new(false));
        let handle: EnvironmentHandle = Arc::new(TestInteriorEnvironment {
            id: EnvironmentId::new("contained-rejected"),
            image: ImageId::new("contained-image"),
            runner: Arc::new(DiscoveryMockRunner::builder().build()),
            env_vars: HashMap::from([("HOME".to_string(), "/home/crew".to_string())]),
            destroyed: Arc::clone(&destroyed),
        });
        let mut local_registry = ProviderRegistry::new();
        local_registry.environment_providers.insert(
            "docker",
            flotilla_core::providers::discovery::ProviderDescriptor::named(
                flotilla_core::providers::discovery::ProviderCategory::EnvironmentProvider,
                "docker",
            ),
            Arc::new(TestInteriorEnvironmentProvider { handle: Mutex::new(Some(handle)) }),
        );
        let state = Arc::new(ControllerRuntimeState::new(
            daemon,
            config,
            Arc::new(local_registry),
            Some(DaemonHostPath::new("/tmp/flotilla.sock")),
            "host-test".to_string(),
            None,
            "host-direct-host-test".to_string(),
        ));
        let spec = flotilla_resources::DockerEnvironmentSpec {
            host_ref: "host-test".to_string(),
            image: "contained-image".to_string(),
            declared_agent_adapters: BTreeSet::from(["codex".to_string()]),
            pull_policy: Default::default(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
        };

        let error = DockerControllerRuntime { state }
            .provision("contained-rejected", &spec)
            .await
            .expect_err("missing declared adapter should reject the environment");

        assert_eq!(error, "image `contained-image` declares agent adapter `codex`, but interior discovery did not find it");
        assert!(destroyed.load(Ordering::SeqCst), "rejected environment should be destroyed");
    }

    #[tokio::test]
    async fn startup_seeding_reconciles_existing_builtin_template_definition() {
        let backend = ResourceBackend::InMemory(Default::default());
        let templates = backend.clone().using::<WorkflowTemplate>(NAMESPACE);
        let mut stale = flotilla_resources::single_agent_contained_workflow_spec();
        stale.vessels[0].stance = Stance::Trusted;
        templates
            .create(
                &empty_meta_with_labels(
                    "single-agent-contained",
                    BTreeMap::from([("example.com/preserved".to_string(), "true".to_string())]),
                ),
                &stale,
            )
            .await
            .expect("stale template create should succeed");

        let log_output = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let writer = LogCaptureWriter(Arc::clone(&log_output));
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("startup reconciliation should succeed");
        }

        let reconciled = templates.get("single-agent-contained").await.expect("template should remain");
        assert_eq!(reconciled.spec, flotilla_resources::single_agent_contained_workflow_spec());
        assert_eq!(reconciled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));
        assert_eq!(reconciled.metadata.labels.get("example.com/preserved").map(String::as_str), Some("true"));
        let logs = String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
        assert!(logs.contains("stored spec diverged from code builtin; overwriting"), "missing overwrite warning: {logs}");
        assert!(logs.contains("single-agent-contained"), "warning should name the template: {logs}");

        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("restart reconciliation should succeed");
        let unchanged = templates.get("single-agent-contained").await.expect("template should remain");
        assert_eq!(unchanged.metadata.resource_version, reconciled.metadata.resource_version);
    }

    #[tokio::test]
    async fn deleted_code_owned_builtin_is_reconciled_back() {
        let backend = ResourceBackend::InMemory(Default::default());
        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("initial builtin reconciliation should succeed");

        let deleted = flotilla_resources::delete_resource_kind(&backend, NAMESPACE, "workflowtemplates", "single-agent-contained")
            .await
            .expect("raw delete should remove builtin");
        assert_eq!(deleted.value["metadata"]["name"], "single-agent-contained");
        assert!(matches!(
            backend.using::<WorkflowTemplate>(NAMESPACE).get("single-agent-contained").await,
            Err(ResourceError::NotFound { .. })
        ));

        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("level-triggered reconciliation should recreate builtin");
        let recreated =
            backend.using::<WorkflowTemplate>(NAMESPACE).get("single-agent-contained").await.expect("builtin should be recreated");
        assert_eq!(recreated.spec, flotilla_resources::single_agent_contained_workflow_spec());
        assert_eq!(recreated.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));
    }

    #[tokio::test]
    async fn startup_seeding_labels_matching_unlabelled_builtin_template_once() {
        const NAMESPACE: &str = "test";
        let backend = ResourceBackend::InMemory(Default::default());
        let templates = backend.clone().using::<WorkflowTemplate>(NAMESPACE);
        templates
            .create(&empty_meta("single-agent-contained"), &flotilla_resources::single_agent_contained_workflow_spec())
            .await
            .expect("matching template create should succeed");
        let existing = templates.get("single-agent-contained").await.expect("template should exist");

        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("startup reconciliation should succeed");

        let labelled = templates.get("single-agent-contained").await.expect("template should remain");
        assert_ne!(labelled.metadata.resource_version, existing.metadata.resource_version);
        assert_eq!(labelled.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(BUILTIN_MANAGED_BY_VALUE));

        reconcile_builtin_workflow_templates(&backend, NAMESPACE).await.expect("restart reconciliation should succeed");

        let unchanged = templates.get("single-agent-contained").await.expect("template should remain");
        assert_eq!(unchanged.metadata.resource_version, labelled.metadata.resource_version);
    }

    fn manual_profile(host_id: &str, docker_available: bool) -> LocalProvisioningProfile {
        LocalProvisioningProfile {
            host_id: host_id.to_string(),
            display_name: "kiwi".to_string(),
            repo_default_dir: "/Users/tester/dev/flotilla-repos".to_string(),
            host_direct_pool: "passthrough".to_string(),
            docker_pool: "passthrough".to_string(),
            available_pools: vec!["passthrough".to_string()],
            available_agent_adapters: BTreeSet::new(),
            docker_available,
        }
    }

    #[tokio::test]
    async fn daemon_restart_publishes_and_logs_agent_adapter_regression() {
        let temp = TempDir::new().expect("tempdir");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
        let daemon = in_memory_daemon(Vec::new(), config).await;
        let host_id = daemon.local_host_id().expect("local host id").to_string();
        let mut first_profile = manual_profile(&host_id, false);
        first_profile.available_agent_adapters = BTreeSet::from(["claude-code".to_string(), "codex".to_string()]);
        ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration");
        apply_host_heartbeat_with_credentials(
            &daemon,
            NAMESPACE,
            &first_profile,
            None,
            &DaemonHealthIdentity {
                generation: Some("generation-1".to_string()),
                version: "1.0.0".to_string(),
                started_at: Utc::now() - chrono::Duration::minutes(1),
            },
            &RuntimeHealth::default(),
        )
        .await
        .expect("first generation heartbeat");

        let mut restarted_profile = first_profile.clone();
        restarted_profile.available_agent_adapters.remove("claude-code");
        let runtime_health = RuntimeHealth::default();
        let log_output = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let writer = LogCaptureWriter(Arc::clone(&log_output));
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            apply_host_heartbeat_with_credentials(
                &daemon,
                NAMESPACE,
                &restarted_profile,
                None,
                &DaemonHealthIdentity {
                    generation: Some("generation-2".to_string()),
                    version: "1.0.0".to_string(),
                    started_at: Utc::now(),
                },
                &runtime_health,
            )
            .await
            .expect("restarted generation heartbeat");
        }

        let status = daemon
            .resource_backend()
            .using::<Host>(NAMESPACE)
            .get(&host_id)
            .await
            .expect("host after restart")
            .status
            .expect("host status after restart");
        assert!(!status.ready, "capability regression should degrade the host");
        assert_eq!(status.conditions.len(), 1);
        assert_eq!(status.conditions[0].condition_type, "CapabilityRegression");
        assert_eq!(status.conditions[0].reason, "AgentAdaptersMissing");
        assert!(status.conditions[0].message.contains("claude-code"));

        let logs = String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be utf-8");
        assert!(logs.contains("host capabilities regressed across daemon restart"), "missing capability regression warning: {logs}");
        assert!(logs.contains("claude-code"), "warning should name the missing adapter: {logs}");
    }

    #[tokio::test]
    async fn projection_parity_reports_and_clears_missing_local_convoys() {
        let backend = ResourceBackend::InMemory(Default::default());
        backend
            .using::<Convoy>(NAMESPACE)
            .create(&empty_meta("convoy-a"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
            .await
            .expect("create durable convoy");
        let projection = AggregatorProjectionState::new();

        let degraded = projection_parity_condition(&backend, NAMESPACE, &projection)
            .await
            .expect("evaluate parity")
            .expect("missing projection should degrade the host");
        assert_eq!(degraded.condition_type, "ProjectionParity");
        assert_eq!(degraded.reason, "LocalRowsMissing");
        assert!(degraded.message.contains("convoy-a"));

        let resource = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", NAMESPACE, "convoy-a");
        projection.write().await.local_rows.insert(
            resource.clone(),
            flotilla_protocol::ConvoyRow::builder()
                .resource(resource)
                .name("convoy-a")
                .workflow_ref("workflow")
                .phase(flotilla_protocol::ConvoyPhase::Pending)
                .build(),
        );
        assert!(
            projection_parity_condition(&backend, NAMESPACE, &projection).await.expect("evaluate restored parity").is_none(),
            "restored parity should clear the degraded condition"
        );
    }

    #[tokio::test]
    async fn restart_budget_exhaustion_is_recorded_in_runtime_health() {
        let runtime_health = RuntimeHealth::default();
        supervise_controller(
            "checkout",
            ControllerSupervision {
                max_consecutive_failures: 1,
                initial_backoff: Duration::ZERO,
                max_backoff: Duration::ZERO,
                success_reset_after: Duration::from_secs(60),
            },
            runtime_health.clone(),
            || async { Err(ResourceError::other("root-owned debris")) },
        )
        .await;

        let conditions = runtime_health.conditions();
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].condition_type, "Controller/checkout");
        assert_eq!(conditions[0].reason, "RestartBudgetExhausted");
        assert!(conditions[0].message.contains("root-owned debris"));
    }

    async fn daemon_with_backend(tracked_repos: Vec<PathBuf>, config: Arc<ConfigStore>, backend: ResourceBackend) -> Arc<InProcessDaemon> {
        daemon_with_backend_and_runner(tracked_repos, config, backend, Arc::new(NoPrProcessRunner)).await
    }

    async fn daemon_with_backend_and_runner(
        tracked_repos: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        backend: ResourceBackend,
        runner: Arc<dyn CommandRunner>,
    ) -> Arc<InProcessDaemon> {
        let mut discovery = git_process_discovery(false);
        discovery.runner = runner;
        let daemon = InProcessDaemon::new_with_resource_backend(
            tracked_repos,
            config,
            discovery,
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
        crew_daemon_with_backend(config, ResourceBackend::InMemory(Default::default())).await
    }

    async fn crew_daemon_with_backend(config: Arc<ConfigStore>, backend: ResourceBackend) -> (Arc<InProcessDaemon>, Arc<FakeTerminalPool>) {
        let pool = Arc::new(FakeTerminalPool::new());
        let discovery = fake_discovery_with_provider_set(
            FakeDiscoveryProviders::new()
                .with_terminal_pool(Arc::clone(&pool) as Arc<dyn flotilla_core::providers::terminal::TerminalPool>),
        );
        let daemon =
            InProcessDaemon::new_with_resource_backend(Vec::new(), config, discovery, flotilla_protocol::HostName::new("dinghy"), backend)
                .await;
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
        let codex_home = config.base_path().join("codex-home");
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
                    .with(EnvironmentAssertion::env_var("CODEX_HOME", codex_home.to_string()))
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
        completion_action: CompletionAction,
    ) {
        std::fs::create_dir_all(&repo_default_dir).expect("repo default dir");
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
                dispatching_principal_ref: Default::default(),
                inputs: BTreeMap::new(),
                placement_policy: Some(format!("host-direct-{host_id}")),
                repositories: vec![ConvoyRepositorySpec {
                    url: "https://github.com/flotilla-org/flotilla.git".to_string(),
                    repo_ref: repository_key,
                    source_ref: "main".to_string(),
                    target_ref: "main".to_string(),
                    workspace_slug: repository_spec.leaf_slug(),
                    subpaths: Vec::new(),
                }],
                r#ref: Some("main".to_string()),
                project_ref: None,
                adopted_checkout_refs: BTreeMap::new(),
                issues: Vec::new(),
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

        let teardown_checkout_path = if matches!(completion_action, CompletionAction::Delete) {
            let checkouts = backend.clone().using::<ResourceCheckout>(NAMESPACE);
            let checkout = checkouts
                .list()
                .await
                .expect("checkout list should succeed")
                .items
                .into_iter()
                .find(|checkout| checkout.metadata.labels.get(flotilla_resources::CONVOY_LABEL).is_some_and(|value| value == "convoy-a"))
                .expect("convoy checkout should exist");
            let checkout_path = checkout.status.expect("checkout should be ready").path.expect("checkout should have a path");
            for args in [
                ["update-ref", "refs/remotes/origin/main", "HEAD"].as_slice(),
                ["branch", "--set-upstream-to", "origin/main", "main"].as_slice(),
            ] {
                let status = ProcessCommand::new("git").arg("-C").arg(&checkout_path).args(args).status().expect("prepare pushed state");
                assert!(status.success());
            }
            Some(checkout_path)
        } else {
            None
        };

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
                        if status.phase == ConvoyPhase::Landed
                            && matches!(status.work.get("implement"), Some(task) if task.phase == WorkPhase::Complete)
                )
            }
        })
        .await;

        if let Some(checkout_path) = teardown_checkout_path {
            wait_until(|| {
                let convoys = convoys.clone();
                let workspaces = workspaces.clone();
                let checkout_path = checkout_path.clone();
                async move {
                    convoys
                        .get("convoy-a")
                        .await
                        .ok()
                        .and_then(|convoy| convoy.status)
                        .is_some_and(|status| status.phase == ConvoyPhase::Landed)
                        && workspaces.list().await.is_ok_and(|list| list.items.is_empty())
                        && !Path::new(&checkout_path).exists()
                }
            })
            .await;
        }

        for handle in controller_handles {
            handle.abort();
            let _ = handle.await;
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

        ensure_host_exists(&daemon.resource_backend(), NAMESPACE, &host_id, "kiwi").await.expect("host registration should succeed");
        let hosts = daemon.resource_backend().using::<Host>(NAMESPACE);
        flotilla_resources::apply_status_patch(&hosts, &host_id, &flotilla_resources::HostStatusPatch::SleepInhibition {
            health: flotilla_protocol::SleepInhibitionHealth::Failed { consecutive_failures: 3, message: "polkit denied".to_string() },
        })
        .await
        .expect("seed sleep inhibition health");
        let heartbeat =
            spawn_heartbeat_task(Arc::clone(&daemon), NAMESPACE.to_string(), profile, test_health_identity(), Duration::from_millis(20));

        wait_until(|| {
            let hosts = hosts.clone();
            let host_id = host_id.clone();
            async move { hosts.get(&host_id).await.ok().and_then(|host| host.status).is_some_and(|status| status.ready) }
        })
        .await;
        let status = hosts.get(&host_id).await.expect("get host").status.expect("host status");
        assert!(status.ready, "heartbeat should mark host ready");
        assert_eq!(status.agent_adapters().expect("valid agent adapter capability"), BTreeSet::new());
        assert_eq!(status.capabilities.get("docker"), Some(&json!(false)));
        assert_eq!(status.capabilities.get("terminal_pools"), Some(&json!(["passthrough"])));
        assert_eq!(status.daemon_generation.as_deref(), Some("test-generation"));
        assert_eq!(status.daemon_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert!(status.daemon_started_at.is_some());
        assert!(status.disk_free_bytes.is_some());
        assert!(matches!(status.sleep_inhibition, flotilla_protocol::SleepInhibitionHealth::Failed { consecutive_failures: 3, .. }));
        assert!(
            status.resource_store.expect("heartbeat should publish resource store diagnostics").event_log_within_retention(),
            "heartbeat should report a bounded resource event log"
        );

        heartbeat.abort();
        let _ = heartbeat.await;
    }

    #[tokio::test]
    async fn heartbeat_task_advances_connected_peer_host_status_after_restart() {
        let temp = TempDir::new().expect("tempdir");
        let config = Arc::new(ConfigStore::with_base(temp.path()));
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
            &flotilla_resources::CheckoutStatusPatch::UpdateIntegration { integration },
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
