//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    arg::{flatten, Arg},
    qualified_path::QualifiedPath,
    result_set::{ConvoyRow, ResultSet, Rows},
    AttachBinding, Command, CorrelationKey, CrewCommandContext, CrewListMember, CrewListResponse, DaemonEvent, DeltaEntry, EnvironmentId,
    FleetListResponse, FleetListRow, FleetReplicaSnapshot, FleetReplicaStatus, FleetStaleness, HostListResponse, HostName,
    HostProvidersResponse, HostStatusResponse, HostSummary, NodeId, NodeInfo, PeerConnectionState, ProviderData, ProviderInfo, QueryCursor,
    RepoDelta, RepoDetailResponse, RepoInfo, RepoProvidersResponse, RepoSnapshot, RepoSummary, RepoWorkResponse, ResolvedPaneCommand,
    StatusResponse, StreamKey, SystemInfo, ToolInventory, TopologyResponse, TopologyRoute,
};
use flotilla_resources::{
    apply_status_patch as apply_resource_status_patch, apply_status_patch_checked as apply_resource_status_patch_checked,
    external_patches as convoy_external_patches, normalize_project_spec, terminal_session_attach_target, Checkout as ResourceCheckout,
    CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus,
    Convoy as ResourceConvoy, ConvoyRepositorySpec, ConvoySpec, ConvoyStatusPatch, CrewSource, Environment as ResourceEnvironment,
    InMemoryBackend, InputMeta, InputValue, LifecycleAuthority, ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PlacementPolicy,
    Project, ProjectRepositorySpec, ProjectSpec, Repository, RepositoryKey, RepositorySpec, Resource, ResourceBackend, ResourceError,
    ResourceObject, TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalSession as ResourceTerminalSession,
    TerminalSessionIdentity, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionStatusPatch,
    Vessel, WorkflowTemplate, WorkflowTemplateSpec, CONVOY_LABEL, REPO_KEY_LABEL, REPO_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    aggregator_projection::AggregatorProjectionState,
    config::{ConfigStore, RemoteHostConfig, StaticEnvironmentConfig},
    convert::snapshot_to_proto,
    daemon::DaemonHandle,
    environment_manager::EnvironmentManager,
    executor,
    executor::checkout::{checkout_matches_scope, CheckoutResolutionScope},
    hop_chain::remote::ssh_resolver_from_config,
    host_identity::{
        resolve_local_environment_state_dir, resolve_local_host_id, resolve_local_node_id, resolve_or_create_environment_id,
        resolve_or_create_remote_environment_id, resolve_or_create_remote_host_id,
    },
    host_registry::HostCounts,
    model::{provider_names_from_registry, repo_name, RepoModel},
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        discovery::{
            discover_providers_with_host_scoped, run_host_detectors, DiscoveryResult, DiscoveryRuntime, EnvironmentAssertion,
            EnvironmentBag,
        },
        ssh_runner::SshCommandRunner,
        ChannelLabel, CommandRunner,
    },
    refresh::RefreshSnapshot,
    repo_state::{RepoRootState, RepoState, SnapshotBuildContext},
    repository_inspection::{GitRepositoryInspector, RepositoryInspection, RepositoryInspector},
    step::{
        run_step_plan_with_remote_executor, RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, StepOutcome, StepResolver,
    },
};

fn host_environment_ids_in_provider_data(providers: &ProviderData) -> Vec<EnvironmentId> {
    let mut environment_ids = HashSet::new();
    for checkout in providers.checkouts.values() {
        if let Some(environment_id) = checkout.environment_id.as_ref().filter(|environment_id| environment_id.is_host()) {
            environment_ids.insert(environment_id.clone());
        }
    }
    let mut environment_ids: Vec<_> = environment_ids.into_iter().collect();
    environment_ids.sort();
    environment_ids
}

fn host_name_for_provider_environment(providers: &ProviderData, environment_id: &EnvironmentId) -> Option<HostName> {
    let mut candidates: Vec<_> = providers
        .checkouts
        .iter()
        .filter_map(|(path, checkout)| {
            checkout
                .environment_id
                .as_ref()
                .filter(|checkout_environment_id| *checkout_environment_id == environment_id)
                .and_then(|_| checkout.host_name.clone().or_else(|| path.host_name().cloned()))
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates.into_iter().next()
}

fn static_ssh_environment_id(config_key: &str) -> EnvironmentId {
    let mut encoded = String::with_capacity(config_key.len() * 2);
    for byte in config_key.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    let suffix = if encoded.is_empty() { "empty".to_string() } else { encoded };
    // Remote direct environments do not have a persisted remote identity yet.
    // Use a deterministic temporary id encoded directly from the daemon.toml
    // entry key bytes so distinct legal config keys remain injective in this tranche.
    EnvironmentId::new(format!("static-ssh-{suffix}"))
}

#[derive(Default)]
struct StaticEnvVars {
    vars: HashMap<String, String>,
}

impl StaticEnvVars {
    fn from_bag(bag: &EnvironmentBag) -> Self {
        let mut vars = HashMap::new();
        for assertion in bag.assertions() {
            if let crate::providers::discovery::EnvironmentAssertion::EnvVarSet { key, value } = assertion {
                vars.insert(key.clone(), value.clone());
            }
        }
        Self { vars }
    }
}

impl crate::providers::discovery::EnvVars for StaticEnvVars {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
}

async fn load_env_vars(runner: &dyn CommandRunner, cwd: &Path) -> HashMap<String, String> {
    let Ok(output) = runner.run("env", &[], cwd, &ChannelLabel::Noop).await else {
        return HashMap::new();
    };

    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn merge_host_counts(counts: &mut HashMap<EnvironmentId, HostCounts>, other: HashMap<EnvironmentId, HostCounts>) {
    for (environment_id, delta) in other {
        let entry = counts.entry(environment_id).or_default();
        entry.repo_count += delta.repo_count;
        entry.work_item_count += delta.work_item_count;
    }
}

const STATIC_SSH_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

async fn register_static_ssh_direct_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config_key: &str,
    environment: &StaticEnvironmentConfig,
) -> Result<(), String> {
    let fallback_env_id = static_ssh_environment_id(config_key);
    let runner = Arc::new(SshCommandRunner::new(environment.hostname.clone(), true, Arc::clone(&discovery.runner)));
    tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, runner.run("true", &[], Path::new("/"), &ChannelLabel::Noop))
        .await
        .map_err(|_| format!("ssh preflight timed out for {}", environment.hostname))?
        .map_err(|err| format!("ssh preflight failed for {}: {err}", environment.hostname))?;
    let remote_env_vars =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, load_env_vars(&*runner, Path::new("/"))).await.unwrap_or_default();
    let remote_env = StaticEnvVars { vars: remote_env_vars };
    let env_id = resolve_or_create_remote_environment_id(&*runner, &remote_env, fallback_env_id).await?;
    let host_id = resolve_or_create_remote_host_id(&*runner, &remote_env).await?;
    let mut env_bag =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, run_host_detectors(&discovery.host_detectors, &*runner, &remote_env))
            .await
            .map_err(|_| format!("host detector execution timed out for {}", environment.hostname))?;
    if let Some(display_name) = environment.display_name.as_ref() {
        env_bag = env_bag.with(EnvironmentAssertion::env_var("DISPLAY_NAME", display_name));
    }
    environment_manager.register_direct_environment(env_id, runner, env_bag, host_id)
}

async fn register_static_ssh_direct_environments(
    config: &ConfigStore,
    discovery: &DiscoveryRuntime,
    environment_manager: &EnvironmentManager,
) {
    let daemon_config = match config.load_daemon_config() {
        Ok(config) => config,
        Err(err) => {
            warn!(%err, "failed to load daemon config for static SSH environments; continuing with local startup only");
            return;
        }
    };

    for (config_key, environment) in &daemon_config.environments {
        if let Err(err) = register_static_ssh_direct_environment(environment_manager, discovery, config_key, environment).await {
            warn!(
                environment = %config_key,
                hostname = %environment.hostname,
                %err,
                "failed to register static SSH direct environment; continuing startup"
            );
        }
    }
}

fn fallback_repo_identity(path: &Path) -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.to_string_lossy().into_owned() }
}

fn empty_repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() }
}

/// An attach resolution: the command the CLI should exec, plus the
/// structured binding it stamps onto its enclosing PM pane (#708).
#[derive(Debug, Clone)]
pub struct ResolvedAttach {
    pub command: String,
    pub binding: Option<AttachBinding>,
}

fn attach_reference_keys(session_name: &str, labels: &BTreeMap<String, String>) -> Vec<String> {
    let mut refs = vec![session_name.to_string()];

    let convoy = labels.get(CONVOY_LABEL);
    let task = labels.get(VESSEL_LABEL);
    let role = labels.get(ROLE_LABEL);
    let vessel = labels.get(VESSEL_REF_LABEL);

    if let Some(convoy) = convoy {
        refs.push(convoy.clone());
    }
    if let Some(vessel) = vessel {
        refs.push(vessel.clone());
    }
    if let (Some(convoy), Some(task)) = (convoy, task) {
        refs.push(format!("{convoy}/{task}"));
    }
    if let (Some(convoy), Some(task), Some(role)) = (convoy, task, role) {
        refs.push(format!("{convoy}/{task}/{role}"));
    }
    if let (Some(vessel), Some(role)) = (vessel, role) {
        refs.push(format!("{vessel}/{role}"));
    }
    if let Some(role) = role {
        refs.push(role.clone());
    }

    refs.sort();
    refs.dedup();
    refs
}

fn attach_reference_label(session_name: &str, labels: &BTreeMap<String, String>) -> String {
    match (labels.get(CONVOY_LABEL), labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) {
        (Some(convoy), Some(task), Some(role)) => format!("{convoy}/{task}/{role} ({session_name})"),
        (Some(convoy), Some(task), None) => format!("{convoy}/{task} ({session_name})"),
        (Some(convoy), None, Some(role)) => format!("{convoy}/{role} ({session_name})"),
        (Some(convoy), None, None) => format!("{convoy} ({session_name})"),
        _ => session_name.to_string(),
    }
}

fn fleet_row_attach_reference_keys(row: &FleetListRow) -> Vec<String> {
    let mut refs = vec![row.convoy.clone(), row.vessel.clone(), row.crew.clone()];
    if let Some(session) = &row.session {
        refs.push(session.clone());
    }
    if row.crew != "-" {
        refs.push(format!("{}/{}", row.convoy, row.crew));
        if let Some((_task, role)) = row.crew.rsplit_once('/') {
            refs.push(role.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn fleet_row_attach_reference_label(row: &FleetListRow) -> String {
    if row.crew == "-" {
        format!("{} ({})", row.convoy, row.host)
    } else {
        format!("{}/{} ({})", row.convoy, row.crew, row.host)
    }
}

enum AttachTarget {
    Local(Box<flotilla_resources::ResourceObject<ResourceTerminalSession>>),
    Replica { row: Box<FleetListRow> },
}

impl AttachTarget {
    async fn resolve(&self, daemon: &InProcessDaemon, reference: &str, transient: bool) -> Result<ResolvedAttach, String> {
        match self {
            Self::Local(session) => {
                let (command, host) = daemon.attach_command_for_session(reference, session, transient).await?;
                let labels = &session.metadata.labels;
                let binding = AttachBinding::builder()
                    .host(host)
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .maybe_convoy(labels.get(CONVOY_LABEL).cloned())
                    .maybe_vessel(labels.get(VESSEL_LABEL).cloned())
                    .role(labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone()))
                    .build();
                Ok(ResolvedAttach { command, binding: Some(binding) })
            }
            Self::Replica { row } => {
                let command = daemon.recursive_attach_command_for_remote(&row.host, reference, transient).await?;
                // Replica rows carry crew as "vessel/role" (or a bare role)
                // and the owning host's namespace + session name, so
                // cross-host panes stamp the full join key.
                let (vessel, role) = match row.crew.split_once('/') {
                    Some((vessel, role)) => (Some(vessel.to_owned()), Some(role.to_owned())),
                    None => (None, Some(row.crew.clone()).filter(|role| !role.is_empty() && role != "-")),
                };
                let binding = AttachBinding::builder()
                    .host(row.host.clone())
                    .namespace(row.namespace.clone())
                    .maybe_session(row.session.clone())
                    .maybe_convoy(Some(row.convoy.clone()).filter(|convoy| convoy != "-"))
                    .maybe_vessel(vessel)
                    .maybe_role(role)
                    .build();
                Ok(ResolvedAttach { command, binding: Some(binding) })
            }
        }
    }
}

struct AttachCandidate {
    label: String,
    references: Vec<String>,
    host: HostName,
    target: AttachTarget,
}

struct AttachCandidateIndex {
    candidates: Vec<AttachCandidate>,
    exact: HashMap<String, Vec<usize>>,
}

impl AttachCandidateIndex {
    fn new(candidates: Vec<AttachCandidate>) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            for reference in &candidate.references {
                exact.entry(reference.clone()).or_default().push(index);
            }
        }
        Self { candidates, exact }
    }

    async fn resolve(
        &self,
        daemon: &InProcessDaemon,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
    ) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }

        let mut matches = self.exact.get(reference).cloned().unwrap_or_else(|| {
            self.candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.references.iter().any(|candidate_reference| candidate_reference.starts_with(reference)))
                .map(|(index, _)| index)
                .collect()
        });
        if let Some(host) = host {
            matches.retain(|index| &self.candidates[*index].host == host);
        }
        match matches.as_slice() {
            [] => match host {
                Some(host) => Err(format!("no attach target matching '{reference}' on host '{host}'")),
                None => Err(format!("no attach target matching '{reference}'")),
            },
            [index] => self.candidates[*index].target.resolve(daemon, reference, transient).await,
            _ => {
                let mut labels: Vec<_> = matches.iter().map(|index| self.candidates[*index].label.clone()).collect();
                labels.sort();
                labels.dedup();
                Err(format!("attach reference '{reference}' is ambiguous: {}", labels.join(", ")))
            }
        }
    }
}

fn session_status_label(phase: Option<ResourceTerminalSessionPhase>) -> String {
    match phase {
        Some(ResourceTerminalSessionPhase::Starting) | None => "starting".to_string(),
        Some(ResourceTerminalSessionPhase::Running) => "running".to_string(),
        Some(ResourceTerminalSessionPhase::Stopped) => "stopped".to_string(),
        Some(ResourceTerminalSessionPhase::Failed) => "failed".to_string(),
    }
}

fn convoy_state_label(row: &ConvoyRow) -> String {
    match row.message.as_deref().filter(|message| !message.trim().is_empty()) {
        Some(message) => format!("{}: {message}", row.phase),
        None => row.phase.to_string(),
    }
}

fn append_crewless_convoy_rows(
    rows: &mut Vec<FleetListRow>,
    target_namespace: &str,
    result_sets: &[ResultSet],
    host: &HostName,
    staleness: FleetStaleness,
) {
    let mut convoys_with_crew: HashSet<String> = rows.iter().map(|row| row.convoy.clone()).collect();
    for result_set in result_sets {
        let Rows::Convoys(convoys) = &result_set.rows else { continue };
        for row in convoys {
            if row.resource.namespace != target_namespace {
                continue;
            }
            if !convoys_with_crew.insert(row.name.clone()) {
                continue;
            }
            rows.push(
                FleetListRow::builder()
                    .convoy(row.name.clone())
                    .vessel("-")
                    .crew("-")
                    .crew_state(convoy_state_label(row))
                    .host(host.clone())
                    .namespace(target_namespace)
                    .staleness(staleness.clone())
                    .build(),
            );
        }
    }
}

fn resource_environment_host_ref(environment: &flotilla_resources::ResourceObject<ResourceEnvironment>) -> Option<&str> {
    environment
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
}

fn ssh_destination(remote: &RemoteHostConfig) -> String {
    match remote.user.as_deref() {
        Some(user) if !user.is_empty() => format!("{user}@{}", remote.hostname),
        _ => remote.hostname.clone(),
    }
}

fn fleet_replica_ssh_args(remote: &RemoteHostConfig, multiplex: bool) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()),
        "-o".to_string(),
        "ConnectionAttempts=1".to_string(),
    ];
    if multiplex {
        args.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPath=/tmp/flotilla-ssh-%C".to_string(),
            "-o".to_string(),
            "ControlPersist=60".to_string(),
        ]);
    }
    args.push(ssh_destination(remote));
    let snapshot_command = vec![
        Arg::Literal("cd".to_string()),
        Arg::Quoted("/".to_string()),
        Arg::Literal("&&".to_string()),
        Arg::Literal("exec".to_string()),
        Arg::Literal("flotilla".to_string()),
        Arg::Literal("--socket".to_string()),
        Arg::Quoted(remote.daemon_socket.clone()),
        Arg::Literal("--json".to_string()),
        Arg::Quoted("replica-snapshot".to_string()),
    ];
    let login_wrapper = vec![
        Arg::Literal("${SHELL:-/bin/sh}".to_string()),
        Arg::Literal("-l".to_string()),
        Arg::Literal("-c".to_string()),
        Arg::NestedCommand(snapshot_command),
    ];
    args.push(flatten(&login_wrapper, 0));
    args
}

fn replica_staleness(entry: &FleetReplicaCacheEntry, now: DateTime<Utc>) -> FleetStaleness {
    if let Some(message) = &entry.last_error {
        return FleetStaleness::Unreachable { last_sync: entry.last_sync, message: message.clone() };
    }
    let Some(last_sync) = entry.last_sync else {
        return FleetStaleness::Unreachable { last_sync: None, message: "replica has never synced".to_string() };
    };
    if now.signed_duration_since(last_sync).num_seconds() > FLEET_REPLICA_FRESH_SECS {
        FleetStaleness::Stale { last_sync }
    } else {
        FleetStaleness::Fresh { last_sync }
    }
}

fn parse_and_validate_workflow_template_yaml(yaml: &str) -> Result<WorkflowTemplateSpec, String> {
    let spec: WorkflowTemplateSpec = serde_yml::from_str(yaml).map_err(|err| format!("invalid workflow template YAML: {err}"))?;
    flotilla_resources::validate(&spec).map_err(|errors| {
        let joined = errors.iter().map(|e| format!("{e}")).collect::<Vec<_>>().join("; ");
        format!("workflow template validation failed: {joined}")
    })?;
    Ok(spec)
}

fn parse_project_yaml(yaml: &str) -> Result<ProjectSpec, String> {
    serde_yml::from_str(yaml).map_err(|err| format!("invalid project YAML: {err}"))
}

fn adopted_checkout_name(convoy_name: &str) -> String {
    format!("adopted-checkout-{convoy_name}")
}

#[derive(bon::Builder)]
struct AdoptedCheckoutRequest<'a> {
    namespace: &'a str,
    convoy_name: &'a str,
    checkout_path: &'a Path,
    repository_spec: &'a RepositorySpec,
    repository_url: &'a str,
    git_ref: &'a str,
    host_ref: &'a str,
}

async fn create_adopted_checkout_resource(
    backend: &ResourceBackend,
    request: AdoptedCheckoutRequest<'_>,
) -> Result<(String, String, String), String> {
    let AdoptedCheckoutRequest { namespace, convoy_name, checkout_path, repository_spec, repository_url, git_ref, host_ref } = request;
    let path = std::fs::canonicalize(checkout_path)
        .map_err(|err| format!("adopted checkout path {} cannot be resolved: {err}", checkout_path.display()))?;
    let path_str = path.to_string_lossy().to_string();
    let checkout_ref = adopted_checkout_name(convoy_name);
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), &repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let meta = InputMeta::builder()
        .name(checkout_ref.clone())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Adopted);
    let spec = ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
        r#ref: git_ref.to_string(),
        path: path_str.clone(),
        repo_ref: repository_key,
        host_ref: host_ref.to_string(),
        is_main: matches!(git_ref, "main" | "master" | "trunk"),
    });

    let checkout = match checkouts.create(&meta, &spec).await {
        Ok(checkout) => checkout,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(&checkout_ref).await.map_err(|err| err.to_string())?;
            if existing.metadata.lifecycle_authority().map_err(|err| err.to_string())? != Some(LifecycleAuthority::Adopted) {
                return Err(format!("checkout {checkout_ref} already exists but is not adopted"));
            }
            if existing.spec != spec {
                return Err(format!("checkout {checkout_ref} already exists with different adopted checkout details"));
            }
            existing
        }
        Err(err) => return Err(err.to_string()),
    };
    checkouts
        .update_status(&checkout_ref, &checkout.metadata.resource_version, &ResourceCheckoutStatus {
            phase: ResourceCheckoutPhase::Ready,
            path: Some(path_str),
            commit: None,
            message: None,
        })
        .await
        .map_err(|err| err.to_string())?;

    Ok((checkout_ref, repository_url.to_string(), git_ref.to_string()))
}

async fn default_convoy_placement_policy(backend: &ResourceBackend, namespace: &str) -> Option<String> {
    let mut policies = match backend.clone().using::<PlacementPolicy>(namespace).list().await {
        Ok(list) => list.items,
        Err(err) => {
            warn!(%namespace, error = %err, "failed to list placement policies; convoy will remain Pending until one is registered");
            return None;
        }
    };
    policies.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    let policy = policies
        .iter()
        .find(|policy| policy.metadata.name.starts_with("host-direct-"))
        .or_else(|| policies.first())
        .map(|policy| policy.metadata.name.clone());
    if policy.is_none() {
        warn!(%namespace, "no placement policy found; convoy will remain Pending until one is registered");
    }
    policy
}

fn repo_identity_from_bag_or_path(path: &Path, bag: &EnvironmentBag) -> flotilla_protocol::RepoIdentity {
    bag.repo_identity().unwrap_or_else(|| fallback_repo_identity(path))
}

fn normalize_checkout_for_environment(
    environment_manager: &EnvironmentManager,
    environment_id: Option<&EnvironmentId>,
    host_name: &HostName,
    checkout: QualifiedPath,
) -> QualifiedPath {
    let Some(environment_id) = environment_id else {
        return crate::refresh::normalize_checkout_publication(checkout, None, host_name);
    };

    let environment_path = ExecutionEnvironmentPath::new(checkout.path.clone());
    if let Some(host_path) = environment_manager.resolve_environment_path_to_host_path(environment_id, &environment_path) {
        return host_path;
    }

    if matches!(checkout.qualifier, flotilla_protocol::qualified_path::PathQualifier::Environment(_)) {
        return QualifiedPath::environment(environment_id.clone(), checkout.path);
    }

    let host_id = environment_manager.host_id_for_environment(environment_id);
    crate::refresh::normalize_checkout_publication(checkout, host_id.as_ref(), host_name)
}

fn normalize_correlation_keys_for_environment(
    environment_manager: &EnvironmentManager,
    environment_id: Option<&EnvironmentId>,
    host_name: &HostName,
    keys: Vec<CorrelationKey>,
) -> Vec<CorrelationKey> {
    keys.into_iter()
        .map(|key| match key {
            CorrelationKey::CheckoutPath(path) => {
                CorrelationKey::CheckoutPath(normalize_checkout_for_environment(environment_manager, environment_id, host_name, path))
            }
            other => other,
        })
        .collect()
}

async fn discover_repo_for_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config: &ConfigStore,
    local_environment_id: &EnvironmentId,
    environment_id: &EnvironmentId,
    repo_path: &Path,
) -> Result<DiscoveryResult, String> {
    let host_bag = environment_manager.environment_bag(environment_id).ok_or_else(|| format!("environment not found: {environment_id}"))?;
    let runner =
        environment_manager.environment_runner(environment_id).ok_or_else(|| format!("environment runner not found: {environment_id}"))?;
    let ee_path = ExecutionEnvironmentPath::new(repo_path);
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn crate::providers::discovery::EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };

    let host_scoped = discovery
        .host_scoped_providers
        .discover_for_environment(environment_id, &host_bag, &discovery.factories, config, &ee_path, Arc::clone(&runner))
        .await;
    Ok(discover_providers_with_host_scoped(
        &host_bag,
        &ee_path,
        &discovery.repo_detectors,
        &discovery.factories,
        config,
        runner,
        env,
        &host_scoped,
    )
    .await)
}

fn normalize_local_provider_hosts(
    mut providers: ProviderData,
    environment_manager: &EnvironmentManager,
    environment_id: Option<&EnvironmentId>,
    host_name: &HostName,
) -> ProviderData {
    providers.checkouts = providers
        .checkouts
        .into_iter()
        .map(|(host_path, mut checkout)| {
            checkout.correlation_keys =
                normalize_correlation_keys_for_environment(environment_manager, environment_id, host_name, checkout.correlation_keys);
            checkout.host_name.get_or_insert_with(|| host_name.clone());
            (normalize_checkout_for_environment(environment_manager, environment_id, host_name, host_path), checkout)
        })
        .collect();

    for change_request in providers.change_requests.values_mut() {
        change_request.correlation_keys = normalize_correlation_keys_for_environment(
            environment_manager,
            environment_id,
            host_name,
            std::mem::take(&mut change_request.correlation_keys),
        );
    }

    for session in providers.sessions.values_mut() {
        session.correlation_keys = normalize_correlation_keys_for_environment(
            environment_manager,
            environment_id,
            host_name,
            std::mem::take(&mut session.correlation_keys),
        );
    }

    for workspace in providers.workspaces.values_mut() {
        workspace.correlation_keys = normalize_correlation_keys_for_environment(
            environment_manager,
            environment_id,
            host_name,
            std::mem::take(&mut workspace.correlation_keys),
        );
    }

    providers
}

fn merge_local_provider_data(base: &mut ProviderData, other: &ProviderData) {
    for (host_path, checkout) in &other.checkouts {
        // Preferred root data is merged first and remains authoritative on collisions.
        base.checkouts.entry(host_path.clone()).or_insert_with(|| checkout.clone());
    }
    for (id, terminal) in &other.managed_terminals {
        base.managed_terminals.entry(id.clone()).or_insert_with(|| terminal.clone());
    }
    for (name, branch) in &other.branches {
        base.branches.entry(name.clone()).or_insert_with(|| branch.clone());
    }
    for (name, workspace) in &other.workspaces {
        base.workspaces.entry(name.clone()).or_insert_with(|| workspace.clone());
    }
    for (id, set) in &other.attachable_sets {
        base.attachable_sets.entry(id.clone()).or_insert_with(|| set.clone());
    }
    for (key, cr) in &other.change_requests {
        base.change_requests.entry(key.clone()).or_insert_with(|| cr.clone());
    }
    for (key, issue) in &other.issues {
        base.issues.entry(key.clone()).or_insert_with(|| issue.clone());
    }
    for (key, session) in &other.sessions {
        base.sessions.entry(key.clone()).or_insert_with(|| session.clone());
    }
}

fn merge_provider_health(merged: &mut HashMap<(&'static str, String), bool>, next: &HashMap<(&'static str, String), bool>) {
    for (provider, healthy) in next {
        merged.entry(provider.clone()).and_modify(|existing| *existing &= *healthy).or_insert(*healthy);
    }
}

fn merge_provider_errors(merged: &mut Vec<crate::data::RefreshError>, next: &[crate::data::RefreshError]) {
    for err in next {
        if !merged
            .iter()
            .any(|existing| existing.category == err.category && existing.provider == err.provider && existing.message == err.message)
        {
            merged.push(err.clone());
        }
    }
}

/// Build a proto RepoSnapshot, optionally merging peer provider data before correlation.
fn build_repo_snapshot_with_peers(
    ctx: SnapshotBuildContext<'_>,
    seq: u64,
    peer_overlay: Option<&[(NodeInfo, ProviderData)]>,
) -> RepoSnapshot {
    let SnapshotBuildContext {
        repo_identity,
        path,
        local_providers,
        errors,
        provider_health,
        host_name,
        node_id,
        environment_manager,
        environment_id,
    } = ctx;
    let local_providers = normalize_local_provider_hosts(local_providers.clone(), environment_manager, environment_id, host_name);

    // Merge peer provider data if any
    let providers = if let Some(peers) = peer_overlay {
        let peer_refs: Vec<(NodeInfo, &ProviderData)> = peers.iter().map(|(node, data)| (node.clone(), data)).collect();
        Arc::new(crate::merge::merge_provider_data(&local_providers, host_name, node_id, &peer_refs))
    } else {
        Arc::new(local_providers.clone())
    };

    let (work_items, correlation_groups) = crate::data::correlate(&providers);
    let re_snapshot =
        RefreshSnapshot { providers, work_items, correlation_groups, errors: errors.to_vec(), provider_health: provider_health.clone() };
    snapshot_to_proto(repo_identity, path, seq, &re_snapshot, &local_providers, node_id, peer_overlay.unwrap_or(&[]))
}

/// Choose whether to broadcast a full snapshot or a delta.
///
/// Sends a full snapshot when:
/// - This is the first broadcast (prev_seq == 0)
/// - The delta has no changes (shouldn't happen, but avoids empty deltas)
/// - The serialized delta is larger than the serialized full snapshot
///
/// Otherwise sends a delta.
fn choose_event(snapshot: RepoSnapshot, delta: DeltaEntry) -> DaemonEvent {
    // First broadcast or empty delta → always send full
    if delta.prev_seq == 0 || delta.changes.is_empty() {
        return DaemonEvent::RepoSnapshot(Box::new(snapshot));
    }

    let snapshot_delta = RepoDelta {
        seq: delta.seq,
        prev_seq: delta.prev_seq,
        repo_identity: snapshot.repo_identity.clone(),
        repo: snapshot.repo.clone(),
        changes: delta.changes,
        work_items: snapshot.work_items.clone(),
    };

    // Compare serialized sizes — if delta is larger, send full
    let delta_size = serde_json::to_string(&snapshot_delta).map(|s| s.len());
    let full_size = serde_json::to_string(&snapshot).map(|s| s.len());

    match (delta_size, full_size) {
        (Ok(d), Ok(f)) if d < f => {
            debug!(delta_bytes = d, full_bytes = f, "delta smaller than full, sending delta");
            DaemonEvent::RepoDelta(Box::new(snapshot_delta))
        }
        _ => {
            debug!("sending full snapshot (delta not smaller)");
            DaemonEvent::RepoSnapshot(Box::new(snapshot))
        }
    }
}

/// Scan change requests and checkouts for `AssociationKey::IssueRef` and
/// collect the unique issue IDs referenced. These are the issues that
/// correlation needs in `ProviderData.issues` to build linked-issue lists.
fn collect_linked_issue_ids(providers: &ProviderData) -> Vec<String> {
    use std::collections::HashSet;

    use flotilla_protocol::AssociationKey;

    let mut ids = HashSet::new();
    for cr in providers.change_requests.values() {
        for key in &cr.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            ids.insert(issue_id.clone());
        }
    }
    for co in providers.checkouts.values() {
        for key in &co.association_keys {
            let AssociationKey::IssueRef(_, issue_id) = key;
            ids.insert(issue_id.clone());
        }
    }
    ids.into_iter().collect()
}

#[derive(Debug, Clone)]
struct FleetReplicaCacheEntry {
    rows: Vec<FleetListRow>,
    result_sets: Vec<ResultSet>,
    last_sync: Option<DateTime<Utc>>,
    generation: Option<String>,
    last_error: Option<String>,
}

#[derive(bon::Builder)]
struct ResolvedCrewContext {
    namespace: String,
    convoy: String,
    vessel_ref: String,
    vessel: String,
    caller_role: String,
    caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
}

fn input_meta_from_resource<T: Resource>(resource: &flotilla_resources::ResourceObject<T>) -> InputMeta {
    InputMeta::builder()
        .name(resource.metadata.name.clone())
        .labels(resource.metadata.labels.clone())
        .annotations(resource.metadata.annotations.clone())
        .owner_references(resource.metadata.owner_references.clone())
        .finalizers(resource.metadata.finalizers.clone())
        .maybe_deletion_timestamp(resource.metadata.deletion_timestamp)
        .build()
}

fn handoff_crew_brief(context: &ResolvedCrewContext, target: &str, prompt: Option<&str>, members: &[CrewListMember]) -> TerminalBrief {
    crate::agent_adapter::build_crew_brief(
        &TerminalCrewContext {
            namespace: context.namespace.clone(),
            convoy: context.convoy.clone(),
            vessel_ref: context.vessel_ref.clone(),
        },
        &context.vessel,
        target,
        prompt,
        &members
            .iter()
            .map(|member| crate::agent_adapter::CrewBriefMember {
                role: member.role.clone(),
                state: if member.role == target { "active".to_string() } else { member.state.clone() },
                is_agent: member.kind == "agent",
            })
            .collect::<Vec<_>>(),
    )
}

fn pending_crew_message(text: &str) -> TerminalCrewMessage {
    TerminalCrewMessage { id: uuid::Uuid::new_v4().to_string(), text: text.to_string() }
}

async fn queue_pending_crew_message(
    sessions: &flotilla_resources::TypedResolver<ResourceTerminalSession>,
    existing: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    message: &str,
) -> Result<(), String> {
    let mut spec = existing.spec.clone();
    let TerminalSessionSource::Agent { message: pending, .. } = &mut spec.source else {
        return Err(format!("crew target `{}` is not an agent session", existing.spec.role));
    };
    *pending = Some(pending_crew_message(message));
    sessions
        .update(&input_meta_from_resource(existing), &existing.metadata.resource_version, &spec)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<flotilla_protocol::RepoIdentity, RepoState>>,
    repo_order: RwLock<Vec<flotilla_protocol::RepoIdentity>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    next_command_id: AtomicU64,
    node_id: NodeId,
    host_name: HostName,
    /// When true, only local providers (VCS, checkout manager, workspace
    /// manager, terminal pool) are registered. External providers (code
    /// review, issue tracker, cloud agents, AI utilities) are skipped
    /// because the follower receives that data from the leader via PeerData.
    follower: bool,
    /// Peer provider data overlay, keyed by repo identity.
    /// Set by the DaemonServer when peer snapshots arrive. Merged into
    /// the local snapshot during broadcast.
    peer_providers: RwLock<HashMap<flotilla_protocol::RepoIdentity, Vec<(NodeInfo, ProviderData)>>>,
    /// Last applied overlay version per repo. `set_peer_providers` rejects
    /// applies whose version is older than the stored value, preventing stale
    /// data from overwriting fresher writes.
    peer_overlay_versions: RwLock<HashMap<flotilla_protocol::RepoIdentity, u64>>,
    /// Maps local tracked paths (including virtual synthetic paths) to RepoIdentity.
    // Lock ordering: do not hold path_identities across awaits that later take
    // repos/repo_order; add_repo intentionally takes it last while already
    // holding those write locks.
    path_identities: RwLock<HashMap<PathBuf, flotilla_protocol::RepoIdentity>>,
    /// Repository identity last projected for each local tracked path.
    /// Mutated under `observed_checkout_reconciliation` so removal deletes
    /// observations using the identity that originally created them.
    repository_keys_by_path: RwLock<HashMap<PathBuf, RepositoryKey>>,
    host_registry: crate::host_registry::HostRegistry,
    local_environment_id: EnvironmentId,
    environment_manager: Arc<EnvironmentManager>,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: DiscoveryRuntime,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: crate::agents::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
    resource_backend: ResourceBackend,
    observed_resource_backend: ResourceBackend,
    /// Serializes observed Checkout publication with repository removal so a
    /// refresh captured before untracking cannot recreate deleted resources.
    observed_checkout_reconciliation: Mutex<()>,
    aggregator_projection_state: RwLock<AggregatorProjectionState>,
    /// Provisioning namespace used by daemon-side resource operations (e.g.
    /// looking up the Convoy whose task is being marked complete). Set by the
    /// daemon runtime at startup; defaults to [`DEFAULT_PROVISIONING_NAMESPACE`].
    provisioning_namespace: RwLock<String>,
    fleet_replica_cache: RwLock<HashMap<HostName, FleetReplicaCacheEntry>>,
    fleet_replica_tx: broadcast::Sender<Vec<FleetReplicaSnapshot>>,
    repository_inspector: RwLock<Option<Arc<dyn RepositoryInspector>>>,
}

/// Default provisioning namespace used until [`InProcessDaemon::set_provisioning_namespace`]
/// is called. Matches `RuntimeOptions::namespace`'s default so tests that construct
/// the daemon directly hit the same namespace the runtime uses.
pub const DEFAULT_PROVISIONING_NAMESPACE: &str = "flotilla";
const FLEET_REPLICA_FRESH_SECS: i64 = 90;
const FLEET_REPLICA_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

impl InProcessDaemon {
    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because a background poll task is spawned that
    /// holds a reference. The poll loop checks every 100ms for new refresh
    /// snapshots and broadcasts delta or full events for each change.
    pub async fn new(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>, discovery: DiscoveryRuntime, host_name: HostName) -> Arc<Self> {
        Self::new_with_resource_backend(repo_paths, config, discovery, host_name, ResourceBackend::InMemory(Default::default())).await
    }

    pub async fn new_with_resource_backend(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
    ) -> Arc<Self> {
        use crate::providers::discovery::DiscoveryResult;

        let follower = discovery.is_follower();
        let (event_tx, _) = broadcast::channel(256);
        let mut repos: HashMap<flotilla_protocol::RepoIdentity, RepoState> = HashMap::new();
        let mut order = Vec::new();
        let mut path_identities = HashMap::new();

        let daemon_config = config.load_daemon_config().expect("failed to load daemon config");
        let config_machine_id = daemon_config.machine_id.as_deref();
        let local_environment_state_dir =
            resolve_local_environment_state_dir(config.state_dir().as_path(), config_machine_id, &*discovery.runner).await;
        let local_node_id = resolve_local_node_id(config.base_path().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local node id");
        let local_environment_id =
            resolve_or_create_environment_id(&local_environment_state_dir).expect("failed to resolve local direct environment id");
        let local_host_id = resolve_local_host_id(config.state_dir().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local host id");
        let environment_manager =
            Arc::new(EnvironmentManager::new_local(&discovery, local_environment_id.clone(), local_host_id.clone()).await);
        register_static_ssh_direct_environments(&config, &discovery, &environment_manager).await;
        let agent_state_store = crate::agents::shared_file_backed_agent_state_store(config.base_path());

        for path in repo_paths {
            if path_identities.contains_key(&path) {
                continue;
            }
            let attachable_store = discovery.shared_attachable_store(&config);
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
                &environment_manager,
                &discovery,
                &config,
                &local_environment_id,
                &local_environment_id,
                &path,
            )
            .await
            .expect("local direct environment discovery should always be available");
            if !unmet.is_empty() {
                debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
            }

            let identity = repo_identity_from_bag_or_path(&path, &host_repo_bag);
            let slug = repo_slug.clone();
            let mut model = RepoModel::new(
                path.clone(),
                registry,
                repo_slug,
                Some(local_environment_id.clone()),
                Some(local_host_id.clone()),
                attachable_store,
                Arc::clone(&agent_state_store),
            );
            model.data.loading = true;
            let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                order.push(identity.clone());
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
            }
            path_identities.insert(path.clone(), identity);
        }

        let local_host_summary = crate::host_summary::build_local_host_summary(
            &local_node_id,
            &host_name,
            EnvironmentId::host(environment_manager.local_host_id().clone()),
            &environment_manager,
            crate::host_summary::provider_statuses_from_registries(
                repos.values().map(|state| state.preferred_root().model.registry.as_ref()),
            ),
            &*discovery.env,
        )
        .await;

        let (fleet_replica_tx, _) = broadcast::channel(32);
        let daemon = Arc::new(Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
            config,
            next_command_id: AtomicU64::new(1),
            node_id: local_node_id.clone(),
            host_name: host_name.clone(),
            follower,
            peer_providers: RwLock::new(HashMap::new()),
            peer_overlay_versions: RwLock::new(HashMap::new()),
            path_identities: RwLock::new(path_identities),
            repository_keys_by_path: RwLock::new(HashMap::new()),
            host_registry: crate::host_registry::HostRegistry::new(
                NodeInfo::new(local_node_id.clone(), host_name.to_string()),
                local_host_summary,
            ),
            local_environment_id,
            environment_manager,
            discovery,
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
            resource_backend,
            observed_resource_backend: ResourceBackend::InMemory(InMemoryBackend::observed()),
            observed_checkout_reconciliation: Mutex::new(()),
            aggregator_projection_state: RwLock::new(AggregatorProjectionState::new()),
            provisioning_namespace: RwLock::new(DEFAULT_PROVISIONING_NAMESPACE.to_string()),
            fleet_replica_cache: RwLock::new(HashMap::new()),
            fleet_replica_tx,
            repository_inspector: RwLock::new(None),
        });

        // Spawn self-driving poll loop with a Weak reference.
        // The loop exits naturally when all external Arc owners drop.
        let weak = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                match weak.upgrade() {
                    Some(d) => d.poll_snapshots().await,
                    None => break,
                }
            }
        });

        daemon
    }

    /// Returns the host name for this daemon.
    pub fn host_name(&self) -> &HostName {
        &self.host_name
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Returns the session ID for this daemon instance.
    ///
    /// Generated once at startup via `Uuid::new_v4()`. Used in peer Hello
    /// handshake so peers can detect daemon restarts.
    pub fn session_id(&self) -> uuid::Uuid {
        self.session_id
    }

    pub async fn local_host_summary(&self) -> HostSummary {
        self.refresh_local_host_summary().await
    }

    pub fn local_environment_id(&self) -> &EnvironmentId {
        &self.local_environment_id
    }

    pub fn local_command_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(&self.local_environment_id)
    }

    pub async fn set_repository_inspector(&self, inspector: Arc<dyn RepositoryInspector>) {
        *self.repository_inspector.write().await = Some(inspector);
    }

    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        if let Some(inspector) = self.repository_inspector.read().await.clone() {
            return Ok(inspector);
        }
        let runner = self.local_command_runner().ok_or_else(|| "local repository inspector is unavailable".to_string())?;
        let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?;
        Ok(Arc::new(GitRepositoryInspector::new(runner, host_ref.to_string())))
    }

    pub async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        self.repository_inspector().await?.inspect_path(path, remote).await
    }

    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        self.repository_inspector().await?.resolve_remote(remote).await
    }

    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        if let (Some(repository_url), Some(git_ref)) = (repository_url, git_ref) {
            if let Ok(spec) = RepositorySpec::remote(repository_url) {
                let path = std::fs::canonicalize(path)
                    .map_err(|error| format!("adopted checkout path {} cannot be resolved: {error}", path.display()))?;
                let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?.to_string();
                return Ok(RepositoryInspection {
                    spec,
                    checkout: crate::repository_inspection::LocalCheckoutInspection {
                        path,
                        host_ref,
                        git_ref: git_ref.to_string(),
                        is_main: matches!(git_ref, "main" | "master" | "trunk"),
                    },
                    transport_url: Some(repository_url.to_string()),
                });
            }
        }
        self.inspect_repository_path(path, repository_url).await
    }

    pub fn local_environment_bag(&self) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(&self.local_environment_id)
    }

    pub fn command_runner_for_environment(&self, env_id: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(env_id)
    }

    pub fn environment_bag_for_environment(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(env_id)
    }

    pub fn environment_registry_for_environment(
        &self,
        env_id: &EnvironmentId,
    ) -> Option<Arc<crate::providers::registry::ProviderRegistry>> {
        self.environment_manager.environment_registry(env_id)
    }

    pub fn environment_container_name(&self, env_id: &EnvironmentId) -> Option<String> {
        self.environment_manager.environment_container_name(env_id)
    }

    pub fn register_provisioned_environment(
        &self,
        env_id: EnvironmentId,
        handle: crate::providers::environment::EnvironmentHandle,
        env_bag: EnvironmentBag,
        registry: Option<Arc<crate::providers::registry::ProviderRegistry>>,
    ) -> Result<(), String> {
        self.environment_manager.register_provisioned_environment(env_id, handle, env_bag, registry)
    }

    pub fn remove_provisioned_environment(&self, env_id: &EnvironmentId) -> bool {
        self.environment_manager.remove_provisioned_environment(env_id).is_some()
    }

    pub fn discovery_runtime(&self) -> &DiscoveryRuntime {
        &self.discovery
    }

    pub fn local_host_id(&self) -> Option<flotilla_protocol::qualified_path::HostId> {
        self.environment_manager.host_id_for_environment(&self.local_environment_id)
    }

    pub fn host_id_for_environment(&self, env_id: &EnvironmentId) -> Option<flotilla_protocol::qualified_path::HostId> {
        self.environment_manager.host_id_for_environment(env_id)
    }

    pub fn agent_state_store(&self) -> &crate::agents::SharedAgentStateStore {
        &self.agent_state_store
    }

    pub async fn set_daemon_socket_path(&self, path: PathBuf) {
        *self.daemon_socket_path.write().await = Some(path);
    }

    pub async fn daemon_socket_path(&self) -> Option<PathBuf> {
        self.daemon_socket_path.read().await.clone()
    }

    /// Override the provisioning namespace used for daemon-side resource lookups
    /// (e.g. `ConvoyWorkForceComplete`). Called by the daemon runtime at startup with
    /// `RuntimeOptions::namespace`.
    pub async fn set_provisioning_namespace(&self, namespace: String) {
        *self.provisioning_namespace.write().await = namespace;
    }

    pub async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().await.clone()
    }

    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity {
        let repo_identity = empty_repo_identity();
        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id,
            node_id: self.node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: None,
            description,
        });
        repo_identity
    }

    fn finish_context_free_command(
        &self,
        command_id: u64,
        repo_identity: flotilla_protocol::RepoIdentity,
        result: flotilla_protocol::CommandValue,
    ) {
        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
            command_id,
            node_id: self.node_id.clone(),
            repo_identity,
            repo: None,
            result,
        });
    }

    pub async fn set_aggregator_projection_state(&self, state: AggregatorProjectionState) {
        *self.aggregator_projection_state.write().await = state;
    }

    pub async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.read().await.clone()
    }

    pub fn subscribe_fleet_replicas(&self) -> broadcast::Receiver<Vec<FleetReplicaSnapshot>> {
        self.fleet_replica_tx.subscribe()
    }

    pub async fn cached_fleet_replica_snapshots(&self) -> Vec<FleetReplicaSnapshot> {
        self.fleet_replica_cache
            .read()
            .await
            .iter()
            .map(|(host, entry)| FleetReplicaSnapshot {
                host: host.clone(),
                generation: entry.generation.clone(),
                rows: entry.rows.clone(),
                result_sets: entry.result_sets.clone(),
            })
            .collect()
    }

    pub fn resource_backend(&self) -> ResourceBackend {
        self.resource_backend.clone()
    }

    pub fn observed_resource_backend(&self) -> ResourceBackend {
        self.observed_resource_backend.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn register_direct_environment_for_test(
        &self,
        env_id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_id: Option<flotilla_protocol::qualified_path::HostId>,
    ) -> Result<(), String> {
        self.environment_manager.register_direct_environment(env_id, runner, env_bag, host_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn register_provisioned_environment_for_test(
        &self,
        env_id: EnvironmentId,
        handle: crate::providers::environment::EnvironmentHandle,
        env_bag: EnvironmentBag,
    ) -> Result<(), String> {
        self.environment_manager.register_provisioned_environment(env_id, handle, env_bag, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String> {
        self.environment_manager.replace_local_environment_bag_for_test(env_bag)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn managed_environment_ids_for_test(&self) -> Vec<EnvironmentId> {
        self.environment_manager.managed_environments().into_iter().map(|(env_id, _)| env_id).collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn environment_bag_for_test(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(env_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn discover_repo_for_environment_for_test(
        &self,
        repo_path: &Path,
        environment_id: &EnvironmentId,
    ) -> Result<DiscoveryResult, String> {
        discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            environment_id,
            repo_path,
        )
        .await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn trigger_root_refresh_for_test(&self, repo_path: &Path) -> Result<(), String> {
        let identity =
            self.tracked_repo_identity_for_path(repo_path).await.ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        let root = state
            .roots
            .iter()
            .find(|root| root.path == repo_path)
            .ok_or_else(|| format!("repo root not tracked: {}", repo_path.display()))?;
        root.model.refresh_handle.trigger_refresh();
        Ok(())
    }

    /// Returns the current connection status for a peer host.
    pub async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        self.host_registry.peer_connection_status(node_id).await
    }

    pub async fn set_configured_peers(&self, peers: Vec<NodeInfo>) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .set_configured_peers(peers, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_peer_host_summaries(&self, summaries: HashMap<EnvironmentId, HostSummary>) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .set_peer_host_summaries(summaries, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_connection_status(&self, node: &NodeInfo, status: PeerConnectionState) {
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .publish_peer_connection_status(node, status, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_summary(&self, summary: HostSummary) {
        self.host_registry
            .publish_peer_summary(summary, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_topology_routes(&self, routes: Vec<TopologyRoute>) {
        self.host_registry.set_topology_routes(routes).await;
    }

    async fn local_host_counts(&self) -> HashMap<EnvironmentId, HostCounts> {
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut counts: HashMap<EnvironmentId, HostCounts> = HashMap::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            let Some(environment_id) = state.preferred_environment_id().cloned() else {
                continue;
            };
            let entry = counts.entry(environment_id).or_default();
            entry.repo_count += 1;
            if let Some(snapshot) = state.cached_snapshot() {
                entry.work_item_count += snapshot.work_items.len();
            }
        }

        counts
    }

    async fn remote_host_counts(&self) -> HashMap<EnvironmentId, HostCounts> {
        let peer_providers = self.peer_providers.read().await;
        let mut counts: HashMap<EnvironmentId, HostCounts> = HashMap::new();

        for peers in peer_providers.values() {
            for (_node, providers) in peers {
                let environment_ids = host_environment_ids_in_provider_data(providers);
                if environment_ids.is_empty() {
                    continue;
                }
                let work_item_count = crate::data::correlate(providers).0.len();
                for environment_id in environment_ids {
                    let entry = counts.entry(environment_id).or_default();
                    entry.repo_count += 1;
                    entry.work_item_count += work_item_count;
                }
            }
        }

        counts
    }

    /// Returns whether this daemon is running in follower mode.
    pub fn is_follower(&self) -> bool {
        self.follower
    }

    /// Resolve a repo identity to the preferred local path for execution or overlay updates.
    pub async fn preferred_local_path_for_identity(&self, identity: &flotilla_protocol::RepoIdentity) -> Option<PathBuf> {
        self.repos.read().await.get(identity).map(|state| state.preferred_path().to_path_buf())
    }

    /// Resolve a tracked local or synthetic repo path to its stable repo identity.
    pub async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        self.path_identities.read().await.get(repo_path).cloned()
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        match discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &self.local_environment_id,
            repo_path,
        )
        .await
        {
            Ok(result) => repo_identity_from_bag_or_path(repo_path, &result.host_repo_bag),
            Err(_) => fallback_repo_identity(repo_path),
        }
    }

    /// Returns the paths of all locally tracked repos.
    ///
    /// Only local repo paths, not remote/virtual ones. Used by the outbound
    /// task to send local state to a newly connected peer.
    pub async fn tracked_repo_paths(&self) -> Vec<PathBuf> {
        self.repos.read().await.values().flat_map(RepoState::local_paths).collect()
    }

    async fn resolve_repo_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        match selector {
            flotilla_protocol::RepoSelector::Path(path) => {
                let identities = self.path_identities.read().await;
                if identities.contains_key(path) {
                    Ok(path.clone())
                } else {
                    Err(format!("repo not tracked: {}", path.display()))
                }
            }
            flotilla_protocol::RepoSelector::Query(query) => {
                let repos = self.repos.read().await;
                let entries: Vec<_> = repos.values().map(|state| (state.preferred_path(), state.slug())).collect();
                crate::resolve::resolve_repo(query, entries.into_iter()).map_err(|e| e.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => self
                .repos
                .read()
                .await
                .get(identity)
                .map(|state| state.preferred_path().to_path_buf())
                .ok_or_else(|| format!("repo not tracked: {identity}")),
        }
    }

    async fn resolve_checkout_selector(
        &self,
        selector: &flotilla_protocol::CheckoutSelector,
        scope: &CheckoutResolutionScope,
    ) -> Result<(PathBuf, String), String> {
        let peer_providers = self.peer_providers.read().await;
        let repos = self.repos.read().await;
        let mut matches = Vec::new();
        for state in repos.values() {
            let snapshot_owned;
            let providers = if let Some(snapshot) = state.cached_snapshot() {
                &snapshot.providers
            } else {
                snapshot_owned = build_repo_snapshot_with_peers(
                    state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                    state.seq(),
                    peer_providers.get(state.identity()).map(|peers| peers.as_slice()),
                );
                &snapshot_owned.providers
            };
            for (host_path, checkout) in &providers.checkouts {
                if !checkout_matches_scope(host_path, checkout, &self.host_name, scope) {
                    continue;
                }
                let matched = match selector {
                    flotilla_protocol::CheckoutSelector::Path(path) => host_path.path == *path,
                    flotilla_protocol::CheckoutSelector::Query(query) => {
                        checkout.branch == *query || checkout.branch.contains(query) || host_path.path.to_string_lossy().contains(query)
                    }
                };
                if matched {
                    matches.push((state.preferred_path().to_path_buf(), checkout.branch.clone()));
                }
            }
        }
        match matches.len() {
            0 => Err("checkout not found".into()),
            1 => Ok(matches.remove(0)),
            _ => Err("checkout selector is ambiguous".into()),
        }
    }

    async fn resolve_repo_for_command(&self, command: &Command) -> Result<PathBuf, String> {
        use flotilla_protocol::CommandAction;

        let checkout_scope = match (&command.provisioning_target, command.node_id.as_ref()) {
            (Some(flotilla_protocol::ProvisioningTarget::Host { host }), _) => CheckoutResolutionScope::Host(host.clone()),
            (_, Some(node_id)) if *node_id != self.node_id => CheckoutResolutionScope::RemoteAny,
            _ => CheckoutResolutionScope::Any,
        };

        match &command.action {
            CommandAction::Checkout { repo, .. } => self.resolve_repo_selector(repo).await,
            CommandAction::RemoveCheckout { checkout, .. } => {
                if let Some(selector) = command.context_repo.as_ref() {
                    self.resolve_repo_selector(selector).await
                } else {
                    self.resolve_checkout_selector(checkout, &checkout_scope).await.map(|(repo, _)| repo)
                }
            }
            CommandAction::Refresh { repo: Some(selector) } => self.resolve_repo_selector(selector).await,
            CommandAction::FetchCheckoutStatus { .. }
            | CommandAction::OpenChangeRequest { .. }
            | CommandAction::CloseChangeRequest { .. }
            | CommandAction::OpenIssue { .. }
            | CommandAction::LinkIssuesToChangeRequest { .. }
            | CommandAction::ArchiveSession { .. }
            | CommandAction::GenerateBranchName { .. }
            | CommandAction::TeleportSession { .. }
            | CommandAction::CreateWorkspaceForCheckout { .. }
            | CommandAction::CreateWorkspaceFromPreparedTerminal { .. }
            | CommandAction::PrepareTerminalForCheckout { .. }
            | CommandAction::SelectWorkspace { .. } => {
                let selector = command.context_repo.as_ref().ok_or_else(|| "command requires repo context".to_string())?;
                self.resolve_repo_selector(selector).await
            }
            _ => Err("command does not resolve to a single repo".to_string()),
        }
    }

    /// Get the local-only provider data for a repo (without peer overlay).
    ///
    /// Used by the outbound replication task to send only this host's
    /// authoritative data to peers, avoiding echo-back of merged peer data.
    pub async fn get_local_providers(&self, repo: &Path) -> Option<(ProviderData, u64)> {
        let identity = self.tracked_repo_identity_for_path(repo).await?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity)?;
        // add_root() keeps any local root ahead of synthetic remote-only
        // roots, so a non-local preferred root means this identity currently
        // has no executable local instance.
        if !state.preferred_root().is_local {
            return None;
        }
        // last_local_providers excludes peer overlay data; normalize so
        // outbound replication only sends this host's authoritative state.
        let providers = normalize_local_provider_hosts(
            state.last_local_providers.clone(),
            &self.environment_manager,
            state.preferred_environment_id(),
            &self.host_name,
        );
        Some((providers, state.local_data_version()))
    }

    /// Update the peer provider data overlay for a repo and trigger re-broadcast.
    ///
    /// Called by the DaemonServer when PeerManager receives updated peer data.
    /// The peer data is merged into the local snapshot during the next broadcast.
    pub async fn set_peer_providers(&self, repo_path: &Path, peers: Vec<(NodeInfo, ProviderData)>, overlay_version: u64) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo_path).await else {
            return;
        };
        {
            let mut versions = self.peer_overlay_versions.write().await;
            let stored = versions.entry(identity.clone()).or_insert(0);
            if overlay_version < *stored {
                return; // stale — a newer version has already been applied
            }
            *stored = overlay_version;
        }
        {
            let mut pp = self.peer_providers.write().await;
            if peers.is_empty() {
                pp.remove(&identity);
            } else {
                pp.insert(identity.clone(), peers);
            }
        }
        for (node, providers) in self.peer_providers.read().await.get(&identity).cloned().unwrap_or_default() {
            for environment_id in host_environment_ids_in_provider_data(&providers) {
                let Some(host_name) = host_name_for_provider_environment(&providers, &environment_id) else {
                    continue;
                };
                self.host_registry
                    .publish_peer_summary(
                        HostSummary {
                            environment_id,
                            host_name: Some(host_name),
                            node: node.clone(),
                            system: SystemInfo::default(),
                            inventory: ToolInventory::default(),
                            providers: vec![],
                            environments: vec![],
                        },
                        &|e| {
                            let _ = self.event_tx.send(e);
                        },
                    )
                    .await;
            }
        }
        let remote_counts = self.remote_host_counts().await;
        self.host_registry
            .sync_host_membership(&remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
        self.broadcast_snapshot_inner(repo_path, false).await;
    }

    /// Test accessor: return the current peer providers for a given repo identity.
    #[cfg(feature = "test-support")]
    pub async fn peer_providers_for_test(&self, identity: &flotilla_protocol::RepoIdentity) -> Vec<(NodeInfo, ProviderData)> {
        self.peer_providers.read().await.get(identity).cloned().unwrap_or_default()
    }

    /// Poll all repos for new refresh snapshots.
    ///
    /// For each repo whose background refresh has produced a new snapshot,
    /// update internal state, increment the sequence number, and broadcast
    /// a `DaemonEvent::RepoSnapshot` or `DaemonEvent::RepoDelta`.
    ///
    /// Called automatically by the background poll loop spawned in `new()`.
    async fn poll_snapshots(&self) {
        // Collect changed snapshots under a brief write lock (need &mut for borrow_and_update),
        // then do correlation work outside the lock to avoid blocking other operations.
        let changed: Vec<_> = {
            let mut repos = self.repos.write().await;
            repos
                .iter_mut()
                .filter_map(|(identity, state)| {
                    let mut any_changed = false;
                    let mut preferred_changed = false;
                    let mut snapshots = Vec::new();
                    for (root_index, root) in state.roots.iter_mut().enumerate() {
                        let handle = &mut root.model.refresh_handle;
                        if handle.snapshot_rx.has_changed().unwrap_or(false) {
                            let _ = handle.snapshot_rx.borrow_and_update();
                            any_changed = true;
                            preferred_changed |= root_index == 0;
                        }
                        snapshots.push(handle.snapshot_rx.borrow().clone());
                    }
                    if !any_changed {
                        return None;
                    }
                    Some((identity.clone(), snapshots, preferred_changed))
                })
                .collect()
        };
        // Write lock released here

        if changed.is_empty() {
            return;
        }

        // Read peer overlay once (brief read lock)
        let peer_overlay = self.peer_providers.read().await.clone();

        // Correlate and build proto snapshots outside any lock
        let mut updates = Vec::new();
        for (identity, snapshots, preferred_changed) in changed {
            let environment_id = {
                let repos = self.repos.read().await;
                repos.get(&identity).and_then(|state| state.preferred_environment_id().cloned())
            };
            let mut local_providers = ProviderData::default();
            let mut provider_health = HashMap::new();
            let mut errors = Vec::new();
            let mut initialized = false;

            for snapshot in &snapshots {
                let providers = normalize_local_provider_hosts(
                    (*snapshot.providers).clone(),
                    &self.environment_manager,
                    environment_id.as_ref(),
                    &self.host_name,
                );
                if !initialized {
                    local_providers = providers;
                    initialized = true;
                } else {
                    merge_local_provider_data(&mut local_providers, &providers);
                }
                merge_provider_health(&mut provider_health, &snapshot.provider_health);
                merge_provider_errors(&mut errors, &snapshot.errors);
            }

            let last_local_providers = local_providers.clone();
            // Merge peer provider data if any
            let providers = if let Some(peers) = peer_overlay.get(&identity) {
                let peer_refs: Vec<(NodeInfo, &ProviderData)> = peers.iter().map(|(node, data)| (node.clone(), data)).collect();
                Arc::new(crate::merge::merge_provider_data(&local_providers, &self.host_name, &self.node_id, &peer_refs))
            } else {
                Arc::new(local_providers)
            };
            let (work_items, correlation_groups) = crate::data::correlate(&providers);

            let re_snapshot = RefreshSnapshot { providers, work_items, correlation_groups, errors, provider_health };
            updates.push((identity, last_local_providers, re_snapshot, preferred_changed));
        }

        let namespace = self.provisioning_namespace().await;
        for (identity, local_providers, snapshot, _) in &updates {
            if snapshot.errors.iter().any(|error| error.category == "checkouts") {
                warn!(repo = %identity.path, "skipping observed checkout reconciliation after checkout discovery failed");
                continue;
            }
            let local_root = self.repos.read().await.get(identity).and_then(|state| state.local_paths().into_iter().next());
            let Some(local_root) = local_root else {
                continue;
            };
            let inspection = match self.inspect_repository_path(&local_root, None).await {
                Ok(inspection) => inspection,
                Err(error) => {
                    warn!(repo = %identity.path, %error, "failed to resolve repository identity for observed checkouts");
                    continue;
                }
            };
            let repository_key = inspection.key();
            if let Err(error) = flotilla_resources::ensure_repository(
                &self.resource_backend.clone().using::<Repository>(&namespace),
                &repository_key,
                &inspection.spec,
            )
            .await
            {
                warn!(repo = %identity.path, %error, "failed to ensure repository for observed checkouts");
                continue;
            }
            let _reconciliation = self.observed_checkout_reconciliation.lock().await;
            let local_paths = self.repos.read().await.get(identity).map(RepoState::local_paths).unwrap_or_default();
            if !local_paths.contains(&local_root) {
                continue;
            }
            {
                let mut keys_by_path = self.repository_keys_by_path.write().await;
                for path in local_paths {
                    keys_by_path.insert(path, repository_key.clone());
                }
            }
            if let Err(error) = crate::observed_resources::reconcile_checkouts(
                &self.observed_resource_backend,
                &namespace,
                &repository_key,
                &inspection.spec.catalog_slug(),
                local_providers,
                &self.local_host_id().expect("local host id is established at daemon construction").to_string(),
            )
            .await
            {
                warn!(repo = %identity.path, %error, "failed to reconcile observed checkouts");
            }
        }

        // Apply updates under write lock and broadcast
        let mut repos = self.repos.write().await;
        for (identity, last_local_providers, re_snapshot, preferred_changed) in updates {
            let Some(state) = repos.get_mut(&identity) else {
                continue;
            };

            state.preferred_root_mut().model.data.providers = Arc::clone(&re_snapshot.providers);
            state.preferred_root_mut().model.data.correlation_groups = re_snapshot.correlation_groups.clone();
            state.preferred_root_mut().model.data.provider_health = re_snapshot.provider_health.clone();
            state.preferred_root_mut().model.data.loading = false;

            let mut proto_snapshot = snapshot_to_proto(
                state.identity().clone(),
                state.preferred_path(),
                state.seq() + 1,
                &re_snapshot,
                &last_local_providers,
                &self.node_id,
                peer_overlay.get(&identity).map(Vec::as_slice).unwrap_or(&[]),
            );
            proto_snapshot.provider_health = crate::convert::health_to_proto(&state.preferred_root().model.data.provider_health);

            // Compute and log delta (also advances seq)
            let delta_entry = state.record_delta(
                &proto_snapshot.providers,
                &proto_snapshot.provider_health,
                &proto_snapshot.errors,
                proto_snapshot.work_items.clone(),
            );
            debug!(
                repo = %state.preferred_path().display(),
                prev_seq = delta_entry.prev_seq,
                seq = delta_entry.seq,
                change_count = delta_entry.changes.len(),
                "recorded repo delta"
            );

            state.mark_local_change();
            state.last_local_providers = last_local_providers;
            // Store a local-only snapshot (errors + health from the refresh,
            // providers from last_local_providers). Callers that need peer data
            // merge it on-demand via peer_providers; storing merged data here
            // would cause double-merge bugs in normalize_local_provider_hosts.
            state.last_snapshot = Arc::new(RefreshSnapshot {
                providers: Arc::new(state.last_local_providers.clone()),
                errors: re_snapshot.errors.clone(),
                provider_health: re_snapshot.provider_health.clone(),
                ..Default::default()
            });
            state.set_cached_snapshot(proto_snapshot.clone());

            let event = choose_event(proto_snapshot, delta_entry);
            let _ = self.event_tx.send(event);
            if preferred_changed {
                let _ = self.event_tx.send(DaemonEvent::RepoRefreshCompleted {
                    repo_identity: state.identity().clone(),
                    repo: Some(state.preferred_path().to_path_buf()),
                });
            }
        }

        drop(repos);

        self.fetch_missing_linked_issues().await;
    }

    /// For each tracked repo, scan change requests and checkouts for
    /// `AssociationKey::IssueRef` references, fetch any issues not already
    /// present in `ProviderData.issues`, and re-broadcast the snapshot so
    /// correlation can link them.
    async fn fetch_missing_linked_issues(&self) {
        let tasks: Vec<_> = {
            let repos = self.repos.read().await;
            repos
                .iter()
                .filter_map(|(identity, state)| {
                    let linked_ids = collect_linked_issue_ids(&state.last_local_providers);
                    if linked_ids.is_empty() {
                        return None;
                    }
                    let missing: Vec<String> =
                        linked_ids.into_iter().filter(|id| !state.last_local_providers.issues.contains_key(id.as_str())).collect();
                    if missing.is_empty() {
                        return None;
                    }
                    let registry = state.registry();
                    if registry.issue_trackers.is_empty() {
                        return None;
                    }
                    Some((identity.clone(), state.preferred_path().to_path_buf(), missing, registry))
                })
                .collect()
        };

        for (identity, path, missing, registry) in tasks {
            let Some(tracker) = registry.issue_trackers.preferred() else {
                continue;
            };
            match tracker.fetch_issues_by_id(&path, &missing).await {
                Ok(fetched) if !fetched.is_empty() => {
                    {
                        let mut repos = self.repos.write().await;
                        if let Some(state) = repos.get_mut(&identity) {
                            for (id, issue) in &fetched {
                                state.last_local_providers.issues.insert(id.clone(), issue.clone());
                            }
                        }
                    }
                    self.broadcast_snapshot_inner(&path, false).await;
                }
                Ok(_) => {} // no missing issues found
                Err(e) => {
                    debug!(err = %e, "failed to fetch linked issues");
                }
            }
        }
    }

    /// Add a virtual repo (no local filesystem path) for a remote-only repo.
    ///
    /// Unlike `add_repo`, this skips provider discovery entirely — there is
    /// no local path to scan. Instead it creates a dormant `RepoState` with
    /// an empty provider registry and an idle refresh handle.
    ///
    /// The `synthetic_path` serves as a stable key for tab identity (e.g.
    /// `<remote>/desktop/home/dev/repo`).
    ///
    /// `peers` and `overlay_version` seed the peer overlay so the repo
    /// is immediately queryable — there is no window where the repo is
    /// visible but has empty data.
    ///
    /// Emits `DaemonEvent::RepoTracked` followed by a snapshot broadcast.
    pub async fn add_virtual_repo(
        &self,
        identity: flotilla_protocol::RepoIdentity,
        synthetic_path: PathBuf,
        peers: Vec<(NodeInfo, ProviderData)>,
        overlay_version: u64,
    ) -> Result<(), String> {
        // Check if already tracked
        {
            let repos = self.repos.read().await;
            if repos.contains_key(&identity) {
                return Ok(());
            }
        }

        let mut model = RepoModel::new_virtual();
        model.data.loading = false;

        let repo_info = RepoInfo {
            identity: identity.clone(),
            path: Some(synthetic_path.clone()),
            name: repo_name(&synthetic_path),
            labels: model.labels.clone(),
            provider_names: provider_names_from_registry(&model.registry)
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.contains_key(&identity) {
                return Ok(());
            }
            repos.insert(
                identity.clone(),
                RepoState::new(identity.clone(), RepoRootState {
                    path: synthetic_path.clone(),
                    model,
                    slug: None,
                    repo_bag: EnvironmentBag::new(),
                    unmet: Vec::new(),
                    is_local: false,
                }),
            );
            order.push(identity.clone());
        }

        self.path_identities.write().await.insert(synthetic_path.clone(), identity);

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));

        // Set up the peer overlay and broadcast atomically — no window
        // where the repo is visible but has empty data.
        self.set_peer_providers(&synthetic_path, peers, overlay_version).await;

        Ok(())
    }

    async fn broadcast_snapshot_inner(&self, repo: &Path, is_local_change: bool) {
        let Some(identity) = self.tracked_repo_identity_for_path(repo).await else {
            return;
        };
        // Read peer overlay (brief read lock)
        let peer_overlay = {
            let pp = self.peer_providers.read().await;
            pp.get(&identity).cloned()
        };

        let mut repos = self.repos.write().await;
        let Some(state) = repos.get_mut(&identity) else {
            return;
        };

        let proto_snapshot = build_repo_snapshot_with_peers(
            state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
            state.seq() + 1,
            peer_overlay.as_deref(),
        );

        // Compute and log delta (also advances seq)
        let delta_entry = state.record_delta(
            &proto_snapshot.providers,
            &proto_snapshot.provider_health,
            &proto_snapshot.errors,
            proto_snapshot.work_items.clone(),
        );
        if is_local_change {
            state.mark_local_change();
        }
        state.set_cached_snapshot(proto_snapshot.clone());

        let event = choose_event(proto_snapshot, delta_entry);
        let _ = self.event_tx.send(event);
    }

    /// Send an arbitrary event to all subscribers.
    ///
    /// Mirrors host events into daemon-owned host state so replay/query paths
    /// can use a single authoritative source of truth.
    ///
    /// For peer status changes, prefer [`publish_peer_connection_status`](Self::publish_peer_connection_status)
    /// which emits both a `PeerStatusChanged` and a `HostSnapshot` for live subscribers.
    /// Calling `send_event(PeerStatusChanged)` directly only updates replay state.
    pub fn send_event(&self, event: DaemonEvent) {
        self.host_registry.apply_event(&event);
        let _ = self.event_tx.send(event);
    }

    /// Return a clone of the broadcast sender so background tasks (e.g.
    /// the Aggregator) can emit events into the daemon-wide event bus.
    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }
}

/// Non-trait methods that are called directly on the concrete `InProcessDaemon`
/// type by the daemon server peer-overlay code and by the `execute()` implementation.
fn repository_matches_target(repository: &ResourceObject<Repository>, target: &str) -> bool {
    repository.metadata.name == target || repository.spec.matches_catalog_target(target)
}

async fn ensure_single_agent_contained_workflow(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    let meta = InputMeta::builder().name("single-agent-contained".to_string()).build();
    match templates.create(&meta, &flotilla_resources::single_agent_contained_workflow_spec()).await {
        Ok(_) | Err(ResourceError::Conflict { .. }) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn normalize_project_name(name: &str) -> Result<String, String> {
    let normalized = name
        .trim()
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        return Err("project name must contain an alphanumeric character".to_string());
    }
    if normalized != name {
        return Err(format!("project name `{name}` is invalid; use `{normalized}`"));
    }
    Ok(normalized)
}

fn normalize_workspace_slug(candidate: &str) -> String {
    let normalized = candidate
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let normalized = if normalized.is_empty() { "repository" } else { &normalized };
    normalized.chars().take(48).collect::<String>().trim_matches('-').to_string()
}

fn disambiguate_workspace_slug(slug: &str, repo_ref: &RepositoryKey) -> String {
    let suffix = repo_ref.0.chars().take(8).collect::<String>();
    let max_base_len = 48_usize.saturating_sub(suffix.len() + 1);
    let base = slug.chars().take(max_base_len).collect::<String>().trim_matches('-').to_string();
    format!("{base}-{suffix}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectTargetSyntax {
    ExplicitPath,
    QualifiedSlug,
    Ambiguous,
}

fn project_target_syntax(target: &str) -> ProjectTargetSyntax {
    let path = Path::new(target);
    if path.is_absolute() || target.starts_with("./") || target.starts_with("../") {
        ProjectTargetSyntax::ExplicitPath
    } else if target.contains('/') {
        ProjectTargetSyntax::QualifiedSlug
    } else {
        ProjectTargetSyntax::Ambiguous
    }
}

impl InProcessDaemon {
    async fn snapshot_project_repositories(&self, namespace: &str, project_ref: &str) -> Result<Vec<ConvoyRepositorySpec>, String> {
        let project = self
            .resource_backend
            .clone()
            .using::<Project>(namespace)
            .get(project_ref)
            .await
            .map_err(|error| format!("project {project_ref} is not ready: {error}"))?;
        let repositories = self.resource_backend.clone().using::<Repository>(namespace);
        let mut unresolved = Vec::new();
        let mut snapshots = BTreeMap::<RepositoryKey, (String, String, String, Option<String>, BTreeSet<String>)>::new();
        for entry in &project.spec.repositories {
            match repositories.get(&entry.repo.to_string()).await {
                Ok(repository) => {
                    if let Err(error) = repository.spec.verify_key(&entry.repo) {
                        unresolved.push(error);
                        continue;
                    }
                    let url = match repository.spec.identity() {
                        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
                        flotilla_resources::RepositoryIdentity::Local { .. } => {
                            unresolved.push(format!("repository {} has no transport remote", entry.repo));
                            continue;
                        }
                    };
                    let base_ref = entry.default_branch.clone().or_else(|| repository.status.as_ref()?.default_branch.clone());
                    let snapshot = snapshots.entry(entry.repo.clone()).or_insert_with(|| {
                        (url, repository.spec.leaf_slug(), repository.spec.catalog_slug(), base_ref.clone(), BTreeSet::new())
                    });
                    if snapshot.3 != base_ref {
                        unresolved.push(format!("repository {} has conflicting project default branches", entry.repo));
                    }
                    if let Some(subpath) = &entry.subpath {
                        snapshot.4.insert(subpath.clone());
                    }
                }
                Err(error) => unresolved.push(format!("repository {}: {error}", entry.repo)),
            }
        }
        if let Err(error) = self.resource_backend.clone().using::<WorkflowTemplate>(namespace).get(&project.spec.default_workflow_ref).await
        {
            unresolved.push(format!("workflow template {}: {error}", project.spec.default_workflow_ref));
        }
        for (repo_ref, (_, _, _, base_ref, _)) in &snapshots {
            if base_ref.is_none() {
                unresolved.push(format!("repository {repo_ref} has no resolved default branch"));
            }
        }
        if !unresolved.is_empty() {
            return Err(format!("project {project_ref} is not ready: {}", unresolved.join("; ")));
        }

        let leaf_slugs = snapshots
            .iter()
            .map(|(repo_ref, (_, leaf_slug, _, _, _))| (repo_ref.clone(), normalize_workspace_slug(leaf_slug)))
            .collect::<BTreeMap<_, _>>();
        let mut leaf_slug_counts = BTreeMap::<String, usize>::new();
        for slug in leaf_slugs.values() {
            *leaf_slug_counts.entry(slug.clone()).or_default() += 1;
        }
        let candidate_slugs = snapshots
            .iter()
            .map(|(repo_ref, (_, _, catalog_slug, _, _))| {
                let leaf_slug = &leaf_slugs[repo_ref];
                let slug = if leaf_slug_counts[leaf_slug] == 1 { leaf_slug.clone() } else { normalize_workspace_slug(catalog_slug) };
                (repo_ref.clone(), slug)
            })
            .collect::<BTreeMap<_, _>>();
        let mut candidate_slug_counts = BTreeMap::<String, usize>::new();
        for slug in candidate_slugs.values() {
            *candidate_slug_counts.entry(slug.clone()).or_default() += 1;
        }
        let mut repositories = snapshots
            .into_iter()
            .map(|(repo_ref, (url, _, _, base_ref, subpaths))| {
                let candidate = &candidate_slugs[&repo_ref];
                let workspace_slug = if candidate_slug_counts[candidate] == 1 {
                    candidate.clone()
                } else {
                    disambiguate_workspace_slug(candidate, &repo_ref)
                };
                ConvoyRepositorySpec {
                    url,
                    repo_ref,
                    base_ref: base_ref.expect("missing base refs were rejected"),
                    workspace_slug,
                    subpaths: subpaths.into_iter().collect(),
                }
            })
            .collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.workspace_slug.cmp(&right.workspace_slug).then_with(|| left.repo_ref.cmp(&right.repo_ref)));
        Ok(repositories)
    }

    async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String> {
        let namespace = self.provisioning_namespace().await;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let target_path = Path::new(target);
        let target_syntax = project_target_syntax(target);
        let path_is_explicit = target_syntax == ProjectTargetSyntax::ExplicitPath;
        let qualified_slug = target_syntax == ProjectTargetSyntax::QualifiedSlug;
        let path_candidate = if !qualified_slug && target_path.exists() {
            Some(self.repository_inspector().await?.inspect_path(target_path, remote).await?)
        } else if path_is_explicit {
            return Err(format!("repository path {} does not exist", target_path.display()));
        } else {
            None
        };

        let catalog_matches = if path_is_explicit {
            Vec::new()
        } else {
            repositories
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|repository| repository_matches_target(repository, target))
                .collect::<Vec<_>>()
        };
        let mut catalog_by_key = BTreeMap::new();
        for repository in catalog_matches {
            let key = RepositoryKey(repository.metadata.name.clone());
            repository.spec.verify_key(&key)?;
            catalog_by_key.insert(key, repository.spec);
        }
        if catalog_by_key.len() > 1 {
            return Err(format!(
                "repository slug `{target}` is ambiguous: {}",
                catalog_by_key.keys().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ));
        }
        let catalog_candidate = catalog_by_key.into_iter().next();
        if remote.is_some() && path_candidate.is_none() && catalog_candidate.is_some() {
            return Err("--remote can only select identity while inspecting a local repository path".to_string());
        }

        let (key, repository_spec, checkout) = match (path_candidate, catalog_candidate) {
            (Some(inspection), Some((catalog_key, _))) if inspection.key() != catalog_key => {
                return Err(format!(
                    "`{target}` resolves to different path and catalog repositories: {} and {catalog_key}",
                    inspection.key()
                ));
            }
            (Some(inspection), _) => (inspection.key(), inspection.spec, Some(inspection.checkout)),
            (None, Some((key, spec))) => (key, spec, None),
            (None, None) => return Err(format!("`{target}` is neither a repository path nor a repository catalog slug")),
        };

        flotilla_resources::ensure_repository(&repositories, &key, &repository_spec).await.map_err(|error| error.to_string())?;
        if let Some(checkout) = checkout {
            self.ensure_project_checkout(&namespace, &key, &repository_spec, checkout).await?;
        }
        ensure_single_agent_contained_workflow(&self.resource_backend, &namespace).await?;

        let default_name = normalize_project_name(&repository_spec.leaf_slug())?;
        let project_name = explicit_name.map(str::to_string).unwrap_or(default_name.clone());
        normalize_project_name(&project_name)?;
        let projects = self.resource_backend.clone().using::<Project>(&namespace);
        match projects.get(&project_name).await {
            Ok(existing) => {
                let same_whole_repository = matches!(
                    existing.spec.repositories.as_slice(),
                    [entry] if entry.repo == key && entry.subpath.is_none()
                );
                if !same_whole_repository {
                    return Err(format!("project {project_name} already exists with a different repository definition"));
                }
                if explicit_display_name.is_some_and(|display_name| display_name != existing.spec.display_name) {
                    return Err(format!(
                        "project {project_name} already exists with display name `{}`; use project apply to change it",
                        existing.spec.display_name
                    ));
                }
                return Ok(project_name);
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }

        let spec = normalize_project_spec(ProjectSpec {
            display_name: explicit_display_name.map(str::to_string).unwrap_or(default_name),
            default_workflow_ref: "single-agent-contained".to_string(),
            issue_source: None,
            repositories: vec![ProjectRepositorySpec { repo: key, subpath: None, default_branch: None }],
        })?;
        projects.create(&InputMeta::builder().name(project_name.clone()).build(), &spec).await.map_err(|error| error.to_string())?;
        Ok(project_name)
    }

    async fn ensure_project_checkout(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
        checkout: crate::repository_inspection::LocalCheckoutInspection,
    ) -> Result<(), String> {
        let checkouts = self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace);
        let checkout_name = format!(
            "checkout-{}",
            flotilla_resources::repo_key(&format!("{}\0{}\0{}", repository_key, checkout.host_ref, checkout.path.display()))
        );
        let spec = ResourceCheckoutSpec::Observed(ResourceObservedCheckoutSpec {
            r#ref: checkout.git_ref,
            path: checkout.path.to_string_lossy().into_owned(),
            repo_ref: repository_key.clone(),
            host_ref: checkout.host_ref,
            is_main: checkout.is_main,
        });
        let meta = InputMeta::builder()
            .name(checkout_name.clone())
            .labels(BTreeMap::from([
                (REPO_KEY_LABEL.to_string(), repository_key.to_string()),
                (REPO_LABEL.to_string(), repository_spec.catalog_slug()),
            ]))
            .build()
            .with_lifecycle_authority(LifecycleAuthority::Observed);
        let stored = match checkouts.create(&meta, &spec).await {
            Ok(created) => created,
            Err(ResourceError::Conflict { .. }) => {
                let existing = checkouts.get(&checkout_name).await.map_err(|error| error.to_string())?;
                if existing.spec != spec {
                    return Err(format!("checkout {checkout_name} already exists with a different observation"));
                }
                existing
            }
            Err(error) => return Err(error.to_string()),
        };
        checkouts
            .update_status(&checkout_name, &stored.metadata.resource_version, &ResourceCheckoutStatus {
                phase: ResourceCheckoutPhase::Ready,
                path: spec.target_path().map(str::to_string).or_else(|| match &spec {
                    ResourceCheckoutSpec::Observed(observed) => Some(observed.path.clone()),
                    _ => None,
                }),
                commit: None,
                message: None,
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<(), String> {
        let repo = self.resolve_repo_selector(repo).await?;
        {
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let repos = self.repos.read().await;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            for root in &state.roots {
                if root.is_local {
                    root.model.refresh_handle.trigger_refresh();
                }
            }
        };

        Ok(())
    }

    /// Resolve a path that might be a git worktree to the main repo root.
    ///
    /// Returns `(resolved_path, Some(original_path))` if normalization changed
    /// the path, or `(original_path, None)` if no change was needed.
    async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
        use crate::{
            path_context::ExecutionEnvironmentPath,
            providers::vcs::{git::GitVcs, Vcs},
        };

        let vcs = GitVcs::new(self.discovery.runner.clone());
        let ee_path = ExecutionEnvironmentPath::new(path);
        match vcs.resolve_repo_root(&ee_path).await {
            Some(repo_root) => {
                let repo_root_raw = repo_root.into_path_buf();
                // Canonicalize to handle symlinks (e.g. /var -> /private/var on macOS).
                let canonical_root = std::fs::canonicalize(&repo_root_raw).unwrap_or(repo_root_raw);
                let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if canonical_root != canonical_path {
                    debug!(
                        worktree = %path.display(),
                        repo_root = %canonical_root.display(),
                        "normalized worktree path to main repo root"
                    );
                    (canonical_root, Some(path.to_path_buf()))
                } else {
                    (canonical_root, None)
                }
            }
            None => (path.to_path_buf(), None),
        }
    }

    /// Add a repo to tracking, returning `(tracked_path, resolved_from)`.
    ///
    /// If `path` is a git worktree, the main repo root is resolved via
    /// `git rev-parse --path-format=absolute --git-common-dir` and tracked
    /// instead. `resolved_from` is `Some(original_path)` in that case.
    pub async fn add_repo(&self, path: &Path) -> Result<(PathBuf, Option<PathBuf>), String> {
        let (path, resolved_from) = self.normalize_repo_path(path).await;

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &self.local_environment_id,
            &path,
        )
        .await?;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let identity = repo_identity_from_bag_or_path(&path, &host_repo_bag);
        if let Some(tracked_identity) = self.tracked_repo_identity_for_path(&path).await {
            if tracked_identity == identity {
                return Ok((path, resolved_from));
            }
            if let Err(error) = self.remove_repo(&path).await {
                // Another add_repo call may have removed or migrated this path
                // after our identity lookup. Continue through the idempotent
                // insertion path unless it is still tracked elsewhere.
                if self.tracked_repo_identity_for_path(&path).await.is_some_and(|current| current != identity) {
                    return Err(error);
                }
            }
        }
        let slug = repo_slug.clone();
        let mut model = RepoModel::new(
            path.clone(),
            registry,
            repo_slug,
            Some(self.local_environment_id.clone()),
            Some(self.environment_manager.host_id_for_environment(&self.local_environment_id).expect("local host id must be available")),
            self.discovery.shared_attachable_store(&self.config),
            Arc::clone(&self.agent_state_store),
        );
        model.data.loading = true;
        let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

        let repo_info = RepoInfo {
            identity: identity.clone(),
            path: Some(path.clone()),
            name: repo_name(&path),
            labels: root.model.labels.clone(),
            provider_names: provider_names_from_registry(&root.model.registry)
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: crate::convert::health_to_proto(&root.model.data.provider_health),
            loading: true,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        let mut added_new_identity = false;
        let mut preferred_changed = false;
        let already_tracked = self.path_identities.read().await.contains_key(&path);
        if already_tracked {
            return Ok((path, resolved_from));
        }
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if let Some(state) = repos.get_mut(&identity) {
                preferred_changed = state.add_root(root);
            } else {
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
                order.push(identity.clone());
                added_new_identity = true;
            }
            self.path_identities.write().await.insert(path.clone(), identity.clone());
        }

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        self.config.save_repo(&ExecutionEnvironmentPath::new(&path));

        info!(repo = %path.display(), "added repo");
        if added_new_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));
        } else if preferred_changed {
            self.broadcast_snapshot_inner(&path, false).await;
        }

        Ok((path, resolved_from))
    }

    pub async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        let repo_identity = self.tracked_repo_identity_for_path(&path).await.unwrap_or_else(|| fallback_repo_identity(&path));
        let observed_reconciliation = self.observed_checkout_reconciliation.lock().await;
        let repository_key = match self.repository_keys_by_path.read().await.get(&path).cloned() {
            Some(key) => Some(key),
            None => self.inspect_repository_path(&path, None).await.ok().map(|inspection| inspection.key()),
        };

        let mut removed_identity = false;
        let removed_final_local_root;
        let mut new_preferred_path = None;
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            let Some(state) = repos.get_mut(&repo_identity) else {
                return Err(format!("repo not tracked: {}", path.display()));
            };
            let previous_preferred = state.preferred_path().to_path_buf();
            if !state.remove_root(&path) {
                return Err(format!("repo not tracked: {}", path.display()));
            }
            removed_final_local_root = state.local_paths().is_empty();
            if !removed_final_local_root {
                for root in state.roots.iter().filter(|root| root.is_local) {
                    root.model.refresh_handle.trigger_refresh();
                }
            }
            if state.roots.is_empty() {
                repos.remove(&repo_identity);
                order.retain(|repo| repo != &repo_identity);
                removed_identity = true;
            } else if previous_preferred == path {
                new_preferred_path = Some(state.preferred_path().to_path_buf());
            }
        }

        // Remove from identity map and peer overlay
        self.path_identities.write().await.remove(&path);
        self.repository_keys_by_path.write().await.remove(&path);
        if removed_identity {
            let mut pp = self.peer_providers.write().await;
            pp.remove(&repo_identity);
            drop(pp);
            self.peer_overlay_versions.write().await.remove(&repo_identity);
        }

        if removed_final_local_root {
            let namespace = self.provisioning_namespace().await;
            if let Some(repository_key) = repository_key {
                if let Err(error) =
                    crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, &repository_key).await
                {
                    warn!(repo = %repo_identity.path, %error, "failed to delete observed checkouts for untracked repo");
                }
            } else {
                warn!(repo = %repo_identity.path, "could not resolve repository identity while deleting observed checkouts");
            }
        }
        drop(observed_reconciliation);

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        self.config.remove_repo(&ExecutionEnvironmentPath::new(&path));

        info!(repo = %path.display(), "removed repo");
        if removed_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoUntracked { repo_identity, path: Some(path) });
        } else if let Some(preferred_path) = new_preferred_path {
            self.broadcast_snapshot_inner(&preferred_path, false).await;
        }

        Ok(())
    }

    // --- Internal query helpers (formerly DaemonHandle trait methods) ---

    pub async fn get_repo_detail_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoDetailResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };
        Ok(RepoDetailResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            provider_health: snapshot.provider_health.clone(),
            work_items: snapshot.work_items.clone(),
            errors: snapshot.errors.clone(),
        })
    }

    pub async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };

        let host_bag = state
            .preferred_environment_id()
            .and_then(|env_id| self.environment_manager.environment_bag(env_id))
            .unwrap_or_else(|| self.environment_manager.local_environment_bag());
        let host_discovery = host_bag.assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();
        let repo_discovery = state.repo_bag().assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();

        let provider_infos = state
            .preferred_root()
            .model
            .registry
            .provider_infos()
            .into_iter()
            .map(|(category, name)| {
                let healthy = snapshot.provider_health.get(&category).and_then(|providers| providers.get(&name)).copied().unwrap_or(true);
                ProviderInfo { category, name, healthy }
            })
            .collect();

        let unmet_requirements =
            state.unmet().iter().map(|(factory, req)| crate::convert::unmet_requirement_to_proto(factory, req)).collect();

        Ok(RepoProvidersResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            host_discovery,
            repo_discovery,
            providers: provider_infos,
            unmet_requirements,
        })
    }

    pub async fn get_repo_work_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoWorkResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                state.seq(),
                peer_overlay.as_deref(),
            )),
        };
        Ok(RepoWorkResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            work_items: snapshot.work_items.clone(),
        })
    }

    pub async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        let _ = self.refresh_local_host_summary().await;
        let mut counts = self.local_host_counts().await;
        merge_host_counts(&mut counts, self.remote_host_counts().await);
        Ok(self.host_registry.list_hosts(&counts).await)
    }

    pub async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        let mut counts = self.local_host_counts().await;
        merge_host_counts(&mut counts, self.remote_host_counts().await);
        let mut response = self.host_registry.get_host_status(environment_id, &counts).await?;
        if environment_id == &local_summary.environment_id {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        let mut counts = self.local_host_counts().await;
        merge_host_counts(&mut counts, self.remote_host_counts().await);
        let mut response = self.host_registry.get_host_providers(environment_id, &counts).await?;
        if environment_id == &local_summary.environment_id {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub async fn fleet_replica_snapshot_internal(&self) -> Result<FleetReplicaSnapshot, String> {
        let namespace = self.provisioning_namespace().await;
        let (rows, generation) = self.local_fleet_rows(&namespace).await?;
        let result_sets = self.aggregator_projection_state().await.local_result_sets().await;
        Ok(FleetReplicaSnapshot { host: self.host_name.clone(), generation, rows, result_sets })
    }

    pub async fn fleet_list_internal(&self) -> Result<FleetListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        let (mut rows, _generation) = self.local_fleet_rows(&namespace).await?;
        let mut replicas = Vec::new();
        let now = Utc::now();
        let configured_hosts = self.config.load_hosts().map(|hosts| hosts.hosts).unwrap_or_default();
        let cache = self.fleet_replica_cache.read().await;

        for (label, remote) in configured_hosts {
            let host = HostName::new(remote.expected_host_name);
            match cache.get(&host) {
                Some(entry) => {
                    let staleness = replica_staleness(entry, now);
                    rows.extend(entry.rows.iter().cloned().map(|mut row| {
                        row.staleness = staleness.clone();
                        row
                    }));
                    replicas.push(FleetReplicaStatus {
                        host,
                        reachable: entry.last_error.is_none(),
                        last_sync: entry.last_sync,
                        generation: entry.generation.clone(),
                        message: entry.last_error.clone(),
                    });
                }
                None => {
                    replicas.push(FleetReplicaStatus {
                        host,
                        reachable: false,
                        last_sync: None,
                        generation: None,
                        message: Some(format!("replica source '{label}' has not synced yet")),
                    });
                }
            }
        }

        rows.sort_by(|left, right| {
            (&left.convoy, left.host.as_str(), &left.vessel, &left.crew).cmp(&(
                &right.convoy,
                right.host.as_str(),
                &right.vessel,
                &right.crew,
            ))
        });
        replicas.sort_by(|left, right| left.host.as_str().cmp(right.host.as_str()));
        Ok(FleetListResponse { rows, replicas })
    }

    async fn resolve_crew_context(&self, requested: &CrewCommandContext) -> Result<ResolvedCrewContext, String> {
        let provisioning_namespace = self.provisioning_namespace().await;
        let namespace = requested.namespace.clone().unwrap_or_else(|| provisioning_namespace.clone());
        if namespace != provisioning_namespace {
            return Err(format!("crew namespace `{namespace}` is not served by this daemon"));
        }
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace);
        let session_list = sessions.list().await.map_err(|err| err.to_string())?.items;

        if let Some(crew_id) = requested.crew_id.as_deref() {
            let session = session_list
                .into_iter()
                .find(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
                .ok_or_else(|| format!("unknown FLOTILLA_CREW_ID `{crew_id}`"))?;
            let role = session.spec.role.clone();
            let (convoy, vessel_ref) = match &session.spec.source {
                TerminalSessionSource::Agent { context, .. } => (context.convoy.clone(), context.vessel_ref.clone()),
                TerminalSessionSource::Tool { .. } => {
                    return Err(format!("crew identity `{crew_id}` belongs to a non-agent process"));
                }
            };
            return self.resolved_crew_context(namespace, convoy, vessel_ref, role, Some(session)).await;
        }

        let convoy = requested
            .convoy
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let vessel_ref = requested
            .vessel_ref
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let role = requested
            .role
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let caller = session_list.into_iter().find(|session| {
            session.spec.role == role && session.metadata.labels.get(VESSEL_REF_LABEL).map(String::as_str) == Some(vessel_ref.as_str())
        });
        self.resolved_crew_context(namespace, convoy, vessel_ref, role, caller).await
    }

    async fn resolved_crew_context(
        &self,
        namespace: String,
        convoy: String,
        vessel_ref: String,
        caller_role: String,
        caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
    ) -> Result<ResolvedCrewContext, String> {
        let workspace = self.resource_backend.clone().using::<Vessel>(&namespace).get(&vessel_ref).await.map_err(|err| err.to_string())?;
        if workspace.spec.convoy_ref != convoy {
            return Err(format!("vessel `{vessel_ref}` does not belong to convoy `{convoy}`"));
        }
        Ok(ResolvedCrewContext::builder()
            .namespace(namespace)
            .convoy(convoy)
            .vessel_ref(vessel_ref)
            .vessel(workspace.spec.vessel_name)
            .caller_role(caller_role)
            .maybe_caller_session(caller_session)
            .build())
    }

    pub async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let task = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&context.namespace);
        let by_role: HashMap<_, _> = sessions
            .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|session| (session.spec.role.clone(), session))
            .collect();
        let members = task
            .crew
            .iter()
            .map(|process| {
                let session = by_role.get(&process.role);
                let state = match session.and_then(|session| session.status.as_ref().map(|status| status.phase)) {
                    Some(ResourceTerminalSessionPhase::Starting) => "starting",
                    Some(ResourceTerminalSessionPhase::Running) => "active",
                    Some(ResourceTerminalSessionPhase::Stopped) => "stopped",
                    Some(ResourceTerminalSessionPhase::Failed) => "failed",
                    None if matches!(process.source, CrewSource::Agent { .. }) => "latent",
                    None => "pending",
                };
                let crew = session.and_then(|session| session.status.as_ref()).and_then(|status| status.crew.as_ref());
                CrewListMember::builder()
                    .role(process.role.clone())
                    .kind(if matches!(process.source, CrewSource::Agent { .. }) { "agent" } else { "tool" }.to_string())
                    .state(state.to_string())
                    .maybe_adapter(crew.map(|crew| crew.adapter.clone()))
                    .maybe_model(crew.and_then(|crew| crew.model.clone()))
                    .maybe_stance(crew.map(|crew| crew.stance.clone()))
                    .build()
            })
            .collect();
        Ok(CrewListResponse::builder()
            .convoy(context.convoy)
            .vessel_ref(context.vessel_ref)
            .vessel(context.vessel)
            .members(members)
            .build())
    }

    pub async fn crew_complete_internal(&self, requested: &CrewCommandContext, message: Option<String>) -> Result<(), String> {
        self.apply_crew_work_patch(requested, |context| {
            convoy_external_patches::mark_crew_completed(context.vessel.clone(), context.caller_role.clone(), chrono::Utc::now(), message)
        })
        .await
    }

    pub async fn crew_fail_internal(&self, requested: &CrewCommandContext, message: String) -> Result<(), String> {
        self.apply_crew_work_patch(requested, |context| {
            convoy_external_patches::mark_crew_failed(context.vessel.clone(), context.caller_role.clone(), chrono::Utc::now(), message)
        })
        .await
    }

    async fn apply_crew_work_patch(
        &self,
        requested: &CrewCommandContext,
        patch: impl FnOnce(&ResolvedCrewContext) -> ConvoyStatusPatch,
    ) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let known_agent = convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .is_some_and(|crew| crew.contains_key(&context.caller_role));
        if !known_agent {
            return Err(format!("crew work for role `{}` is not defined on vessel `{}`", context.caller_role, context.vessel));
        }
        apply_resource_status_patch(&convoys, &context.convoy, &patch(&context)).await.map(|_| ()).map_err(|err| err.to_string())
    }

    pub async fn crew_handoff_internal(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let (task_index, task) = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let (process_index, process) = task
            .crew
            .iter()
            .enumerate()
            .find(|(_, process)| process.role == target)
            .ok_or_else(|| format!("crew target `{target}` is not defined for vessel `{}`", context.vessel))?;
        let CrewSource::Agent { selector, prompt } = &process.source else {
            return Err(format!("crew target `{target}` is a tool process and cannot receive a handoff"));
        };
        if convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(target))
            .is_some_and(|state| state.phase == flotilla_resources::CrewWorkPhase::Failed)
        {
            return Err(format!("crew target `{target}` has failed work and cannot receive a handoff"));
        }

        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&context.namespace);
        let identity = TerminalSessionIdentity::builder()
            .vessel_ref(context.vessel_ref.clone())
            .convoy(context.convoy.clone())
            .vessel(context.vessel.clone())
            .role(target.to_string())
            .vessel_index(task_index)
            .crew_index(process_index)
            .labels(process.labels.clone())
            .build();
        let terminal_name = identity.name();
        let handoff_result = match sessions.get(&terminal_name).await {
            Ok(existing) => match existing.status.as_ref().map(|status| status.phase) {
                Some(ResourceTerminalSessionPhase::Running) => self.deliver_to_crew_session(&existing, message).await,
                Some(ResourceTerminalSessionPhase::Stopped) => {
                    queue_pending_crew_message(&sessions, &existing, message).await?;
                    apply_resource_status_patch(&sessions, &terminal_name, &TerminalSessionStatusPatch::MarkStarting)
                        .await
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                }
                Some(ResourceTerminalSessionPhase::Failed) => {
                    Err(format!("crew target `{target}` failed provisioning and cannot be revived"))
                }
                Some(ResourceTerminalSessionPhase::Starting) | None => queue_pending_crew_message(&sessions, &existing, message).await,
            },
            Err(ResourceError::NotFound { .. }) => {
                let anchor = if let Some(caller) = context.caller_session.as_ref() {
                    caller.clone()
                } else {
                    sessions
                        .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
                        .await
                        .map_err(|err| err.to_string())?
                        .items
                        .into_iter()
                        .next()
                        .ok_or_else(|| format!("vessel `{}` has no active session to anchor the handoff", context.vessel_ref))?
                };
                let current = self.crew_list_internal(requested).await?;
                let brief = handoff_crew_brief(&context, target, prompt.as_deref(), &current.members);
                sessions
                    .create(&identity.input_meta(), &flotilla_resources::TerminalSessionSpec {
                        env_ref: anchor.spec.env_ref,
                        role: target.to_string(),
                        source: TerminalSessionSource::Agent {
                            selector: selector.clone(),
                            brief,
                            context: TerminalCrewContext {
                                namespace: context.namespace.clone(),
                                convoy: context.convoy.clone(),
                                vessel_ref: context.vessel_ref.clone(),
                            },
                            message: Some(pending_crew_message(message)),
                        },
                        cwd: anchor.spec.cwd,
                        pool: anchor.spec.pool,
                    })
                    .await
                    .map(|_| ())
                    .map_err(|err| err.to_string())
            }
            Err(err) => Err(err.to_string()),
        };
        handoff_result?;
        apply_resource_status_patch(
            &convoys,
            &context.convoy,
            &convoy_external_patches::handoff_crew_work(
                context.vessel,
                context.caller_role,
                target.to_string(),
                chrono::Utc::now(),
                message.to_string(),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
    }

    async fn deliver_to_crew_session(
        &self,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        message: &str,
    ) -> Result<(), String> {
        let namespace = session.metadata.namespace.clone();
        let environment = self
            .resource_backend
            .clone()
            .using::<ResourceEnvironment>(&namespace)
            .get(&session.spec.env_ref)
            .await
            .map_err(|err| err.to_string())?;
        let registry = self.registry_for_resource_environment(&environment, Path::new(&session.spec.cwd)).await?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .or_else(|| registry.terminal_pools.preferred().cloned())
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;
        let session_id = session.status.as_ref().and_then(|status| status.session_id.as_deref()).unwrap_or(session.metadata.name.as_str());
        pool.deliver(session_id, message, true).await
    }

    pub async fn refresh_fleet_replicas_once(&self) -> Result<(), String> {
        let hosts = self.config.load_hosts()?;
        let namespace = self.provisioning_namespace().await;
        let runner = self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
        let configured: HashSet<_> = hosts.hosts.values().map(|remote| HostName::new(remote.expected_host_name.clone())).collect();
        {
            let mut cache = self.fleet_replica_cache.write().await;
            cache.retain(|host, _| configured.contains(host));
        }
        for (label, remote) in &hosts.hosts {
            let host = HostName::new(remote.expected_host_name.clone());
            let multiplex = hosts.resolved_ssh_multiplex(label);
            let result = self.fetch_fleet_replica_snapshot(remote, multiplex, Arc::clone(&runner)).await;
            match result {
                Ok(snapshot) => {
                    let now = Utc::now();
                    let snapshot_host = snapshot.host;
                    let generation = snapshot.generation;
                    let result_sets = snapshot.result_sets.clone();
                    let staleness = FleetStaleness::Fresh { last_sync: now };
                    let mut rows: Vec<_> = snapshot
                        .rows
                        .into_iter()
                        .map(|mut row| {
                            row.host = snapshot_host.clone();
                            row.staleness = staleness.clone();
                            row
                        })
                        .collect();
                    // Replica rows from current daemons already include crewless rows via local_fleet_rows.
                    // Keep result-set rows as a secondary source for direct snapshots; existing rows win.
                    append_crewless_convoy_rows(&mut rows, &namespace, &snapshot.result_sets, &snapshot_host, staleness);
                    self.fleet_replica_cache.write().await.insert(host, FleetReplicaCacheEntry {
                        rows,
                        result_sets,
                        last_sync: Some(now),
                        generation,
                        last_error: None,
                    });
                }
                Err(message) => {
                    let mut cache = self.fleet_replica_cache.write().await;
                    cache.entry(host).and_modify(|entry| entry.last_error = Some(message.clone())).or_insert_with(|| {
                        FleetReplicaCacheEntry {
                            rows: Vec::new(),
                            result_sets: Vec::new(),
                            last_sync: None,
                            generation: None,
                            last_error: Some(message),
                        }
                    });
                }
            }
        }
        let _ = self.fleet_replica_tx.send(self.cached_fleet_replica_snapshots().await);
        Ok(())
    }

    async fn fetch_fleet_replica_snapshot(
        &self,
        remote: &RemoteHostConfig,
        multiplex: bool,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<FleetReplicaSnapshot, String> {
        let args = fleet_replica_ssh_args(remote, multiplex);
        let arg_refs: Vec<_> = args.iter().map(String::as_str).collect();
        let output =
            tokio::time::timeout(FLEET_REPLICA_REFRESH_TIMEOUT, runner.run_output("ssh", &arg_refs, Path::new("/"), &ChannelLabel::Noop))
                .await
                .map_err(|_| format!("replica snapshot timed out after {}s", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()))?
                .map_err(|err| format!("replica snapshot ssh failed: {err}"))?;
        if !output.success {
            let message = if output.stderr.trim().is_empty() { output.stdout.trim() } else { output.stderr.trim() };
            return Err(format!("replica snapshot command failed: {message}"));
        }
        serde_json::from_str(output.stdout.trim()).map_err(|err| format!("replica snapshot parse failed: {err}"))
    }

    async fn local_fleet_rows(&self, namespace: &str) -> Result<(Vec<FleetListRow>, Option<String>), String> {
        let terminal_sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(namespace);
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let observed_checkouts = self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace);

        let session_list = terminal_sessions.list().await.map_err(|err| err.to_string())?;
        let observed_generation = observed_checkouts.list().await.map_err(|err| err.to_string())?.generation;
        let environment_map: HashMap<_, _> = environments
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|environment| (environment.metadata.name.clone(), environment))
            .collect();
        let mut authority_by_convoy = HashMap::new();
        for checkout in checkouts.list().await.map_err(|err| err.to_string())?.items {
            let Some(convoy) = checkout.metadata.labels.get(CONVOY_LABEL).cloned() else {
                continue;
            };
            let authority = checkout
                .metadata
                .lifecycle_authority()
                .map_err(|err| err.to_string())?
                .map(|authority| authority.as_label_value().to_string());
            if authority.is_some() {
                authority_by_convoy.insert(convoy, authority);
            }
        }

        let mut rows = Vec::new();
        for session in session_list.items {
            let labels = &session.metadata.labels;
            let convoy = labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let task = labels.get(VESSEL_LABEL).cloned();
            let role = labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = match task {
                Some(task) => format!("{task}/{role}"),
                None => role,
            };
            let host = environment_map
                .get(&session.spec.env_ref)
                .and_then(|environment| resource_environment_host_ref(environment))
                .map(|host_ref| self.target_host_for_resource_ref(host_ref))
                .unwrap_or_else(|| self.host_name.clone());
            rows.push(
                FleetListRow::builder()
                    .convoy(convoy.clone())
                    .vessel(session.spec.env_ref.clone())
                    .maybe_authority(authority_by_convoy.get(&convoy).cloned().flatten())
                    .crew(crew)
                    .crew_state(session_status_label(session.status.as_ref().map(|status| status.phase)))
                    .host(host)
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .staleness(FleetStaleness::Local)
                    .build(),
            );
        }
        let result_sets = self.aggregator_projection_state().await.local_result_sets().await;
        append_crewless_convoy_rows(&mut rows, namespace, &result_sets, &self.host_name, FleetStaleness::Local);
        rows.sort_by(|left, right| {
            (&left.convoy, left.host.as_str(), &left.vessel, &left.crew).cmp(&(
                &right.convoy,
                right.host.as_str(),
                &right.vessel,
                &right.crew,
            ))
        });
        Ok((rows, observed_generation))
    }

    pub async fn resolve_attach_command_internal(&self, reference: &str) -> Result<ResolvedAttach, String> {
        // Preserve validation precedence without paying to build the candidate index.
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        let index = self.attach_candidate_index().await?;
        index.resolve(self, reference, None, false).await
    }

    pub async fn resolve_transient_attach_command_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
    ) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        let index = self.attach_candidate_index().await?;
        index.resolve(self, reference, host, true).await
    }

    pub async fn resolvable_attach_references_internal(&self, references: &[String]) -> Result<HashSet<String>, String> {
        if references.is_empty() {
            return Ok(HashSet::new());
        }
        let index = self.attach_candidate_index().await?;
        let mut resolved = HashSet::new();
        for reference in references {
            if index.resolve(self, reference, Some(&self.host_name), false).await.is_ok() {
                resolved.insert(reference.clone());
            }
        }
        Ok(resolved)
    }

    async fn attach_candidate_index(&self) -> Result<AttachCandidateIndex, String> {
        let namespace = self.provisioning_namespace().await;
        let durable_sessions =
            self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).list().await.map_err(|err| err.to_string())?.items;
        let observed_sessions = self
            .observed_resource_backend
            .clone()
            .using::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let mut sessions_by_name = HashMap::new();
        for session in durable_sessions.into_iter().chain(observed_sessions) {
            sessions_by_name.insert(session.metadata.name.clone(), session);
        }
        let mut candidates = Vec::new();
        for session in sessions_by_name.into_values() {
            if session.status.as_ref().map(|status| status.phase) != Some(ResourceTerminalSessionPhase::Running) {
                continue;
            }
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels),
                host: self.host_name.clone(),
                target: AttachTarget::Local(Box::new(session)),
            });
        }

        let configured_replica_hosts: HashSet<HostName> = self
            .config
            .load_hosts()
            .map(|hosts| hosts.hosts.into_values().map(|remote| HostName::new(remote.expected_host_name)).collect())
            .unwrap_or_default();
        let cache = self.fleet_replica_cache.read().await;
        for host in configured_replica_hosts {
            if let Some(entry) = cache.get(&host) {
                let independent_references = entry
                    .result_sets
                    .iter()
                    .filter_map(|result_set| result_set.rows.as_independents())
                    .flatten()
                    .filter_map(|row| row.attach.as_deref())
                    .collect::<HashSet<_>>();
                let mut indexed_sessions = HashSet::new();
                for row in &entry.rows {
                    if row.crew_state != "running" {
                        continue;
                    }
                    if let Some(session) = &row.session {
                        if independent_references.contains(session.as_str()) {
                            continue;
                        }
                        indexed_sessions.insert(session.clone());
                    }
                    candidates.push(AttachCandidate {
                        label: fleet_row_attach_reference_label(row),
                        references: fleet_row_attach_reference_keys(row),
                        host: row.host.clone(),
                        target: AttachTarget::Replica { row: Box::new(row.clone()) },
                    });
                }
                for result_set in &entry.result_sets {
                    let Rows::Independents(rows) = &result_set.rows else { continue };
                    for row in rows {
                        let Some(reference) = &row.attach else { continue };
                        if row.phase != flotilla_protocol::SessionPhase::Running || !indexed_sessions.insert(reference.clone()) {
                            continue;
                        }
                        let fleet_row = FleetListRow::builder()
                            .convoy("-")
                            .vessel("-")
                            .crew("-")
                            .crew_state("running")
                            .host(host.clone())
                            .namespace(row.resource.namespace.clone())
                            .session(reference.clone())
                            .staleness(FleetStaleness::Local)
                            .build();
                        candidates.push(AttachCandidate {
                            label: format!("{} ({host})", row.name),
                            references: vec![reference.clone()],
                            host: host.clone(),
                            target: AttachTarget::Replica { row: Box::new(fleet_row) },
                        });
                    }
                }
            }
        }
        drop(cache);
        Ok(AttachCandidateIndex::new(candidates))
    }

    /// Resolve the attach command for a locally-known session, returning it
    /// with the host that actually owns the session (the binding host).
    async fn attach_command_for_session(
        &self,
        reference: &str,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        transient: bool,
    ) -> Result<(String, HostName), String> {
        let namespace = self.provisioning_namespace().await;
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(&namespace);
        let environment = environments
            .get(&session.spec.env_ref)
            .await
            .map_err(|err| format!("environment {} lookup failed: {err}", session.spec.env_ref))?;
        let host_ref = environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .ok_or_else(|| format!("environment {} has no host binding", session.spec.env_ref))?;
        let target_host = self.target_host_for_resource_ref(host_ref);
        if target_host != self.host_name {
            let command = self.recursive_attach_command_for_remote(&target_host, reference, transient).await?;
            return Ok((command, target_host));
        }

        let command = self.local_attach_command_for_session(session, &environment).await?;
        Ok((command, self.host_name.clone()))
    }

    async fn recursive_attach_command_for_remote(
        &self,
        target_host: &HostName,
        reference: &str,
        transient: bool,
    ) -> Result<String, String> {
        let next_hop = self.host_registry.next_hop_host_for_target_host(target_host).await?.unwrap_or_else(|| target_host.clone());
        if next_hop == self.host_name {
            return Err(format!("unreachable next hop for host '{target_host}': route points back to local host"));
        }

        let resolver = ssh_resolver_from_config(self.config.base_path())?;
        let mut command =
            vec![flotilla_protocol::arg::Arg::Literal("flotilla".to_string()), flotilla_protocol::arg::Arg::Literal("attach".to_string())];
        if transient {
            command.push(flotilla_protocol::arg::Arg::Literal("--host".to_string()));
            command.push(flotilla_protocol::arg::Arg::Quoted(target_host.to_string()));
            command.push(flotilla_protocol::arg::Arg::Literal("--transient".to_string()));
        }
        command.push(flotilla_protocol::arg::Arg::Quoted(reference.to_string()));
        let args = resolver
            .one_hop_command_args(&next_hop, command)
            .map_err(|err| format!("unreachable next hop '{next_hop}' for host '{target_host}': {err}"))?;
        Ok(flotilla_protocol::arg::flatten(&args, 0))
    }

    async fn local_attach_command_for_session(
        &self,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
    ) -> Result<String, String> {
        let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let registry = self.registry_for_resource_environment(environment, cwd.as_path()).await?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;
        let attach_target = terminal_session_attach_target(session)?;
        let attach_args = pool.attach_args(attach_target.session_id, attach_target.launch_command, &cwd, &Vec::new())?;
        if environment.spec.docker.is_some() {
            let command = ResolvedPaneCommand { role: session.spec.role.clone(), args: attach_args };
            let environment_id = EnvironmentId::new(session.spec.env_ref.clone());
            let container_name = environment.status.as_ref().and_then(|status| status.docker_container_id.as_deref());
            let resolved = executor::workspace::resolve_prepared_commands_via_hop_chain(
                &self.host_name,
                cwd.as_path(),
                &[command],
                self.config.base_path().as_path(),
                &self.host_name,
                Some(&environment_id),
                container_name,
            )?;
            return resolved
                .into_iter()
                .next()
                .map(|(_, command)| command)
                .ok_or_else(|| "attach command resolution produced no command".to_string());
        }
        Ok(flotilla_protocol::arg::flatten(&attach_args, 0))
    }

    fn target_host_for_resource_ref(&self, host_ref: &str) -> HostName {
        if self.local_host_id().as_ref().is_some_and(|host_id| host_id.as_str() == host_ref) {
            self.host_name.clone()
        } else {
            HostName::new(host_ref)
        }
    }

    async fn registry_for_resource_environment(
        &self,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &Path,
    ) -> Result<Arc<crate::providers::registry::ProviderRegistry>, String> {
        let environment_id = if let Some(host_direct) = environment.spec.host_direct.as_ref() {
            if self.local_host_id().as_ref().is_some_and(|host_id| host_id.as_str() == host_direct.host_ref) {
                self.local_environment_id.clone()
            } else {
                EnvironmentId::new(environment.metadata.name.clone())
            }
        } else {
            EnvironmentId::new(environment.metadata.name.clone())
        };

        if let Some(registry) = self.environment_registry_for_environment(&environment_id) {
            return Ok(registry);
        }

        discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &environment_id,
            cwd,
        )
        .await
        .map(|result| Arc::new(result.registry))
    }

    async fn refresh_local_host_summary(&self) -> HostSummary {
        let summary = crate::host_summary::build_local_host_summary(
            &self.node_id,
            &self.host_name,
            EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            &self.environment_manager,
            crate::host_summary::provider_statuses_from_registries(
                self.repos.read().await.values().map(|state| state.preferred_root().model.registry.as_ref()),
            ),
            &*self.discovery.env,
        )
        .await;
        self.host_registry.set_local_host_summary(summary.clone()).await;
        summary
    }

    async fn get_issue_query_service(&self, repo: &Path) -> Result<Arc<dyn crate::providers::issue_query::IssueQueryService>, String> {
        let identity = self.tracked_repo_identity_for_path(repo).await.ok_or_else(|| "no tracked repo for path".to_string())?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| "repo not found".to_string())?;
        state
            .registry()
            .issue_query_services
            .preferred()
            .cloned()
            .ok_or_else(|| "no issue query service available on this host".to_string())
    }

    pub async fn execute_with_remote_executor(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
    ) -> Result<u64, String> {
        self.execute_impl(command, remote_executor, true).await
    }

    pub async fn execute_remote_step_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
        cancel: CancellationToken,
    ) -> Result<Vec<StepOutcome>, String> {
        let local_repo_path = self
            .preferred_local_path_for_identity(&request.repo_identity)
            .await
            .ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
        let (registry, providers_data, refresh_trigger) = {
            let repos = self.repos.read().await;
            let state = repos.get(&request.repo_identity).ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
            (state.registry(), state.providers(), state.refresh_trigger())
        };

        let config_base = DaemonHostPath::new(self.config.base_path().as_path());
        let attachable_store = self.discovery.shared_attachable_store(&self.config);
        let daemon_socket_path = self.daemon_socket_path.read().await.clone().map(DaemonHostPath::new);
        let resolver = executor::ExecutorStepResolver {
            repo: executor::RepoExecutionContext {
                identity: request.repo_identity.clone(),
                root: ExecutionEnvironmentPath::new(&local_repo_path),
            },
            registry,
            providers_data,
            runner: Arc::clone(&self.discovery.runner),
            env: Arc::clone(&self.discovery.env),
            config_base,
            attachable_store,
            daemon_socket_path,
            local_node_id: self.node_id.clone(),
            local_host: self.host_name.clone(),
            environment_manager: Arc::clone(&self.environment_manager),
        };

        let result = execute_local_remote_step_batch(self.node_id.clone(), request, progress_sink, cancel, &resolver).await;
        refresh_trigger.notify_one();
        result
    }

    async fn execute_impl(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
        allow_remote_host: bool,
    ) -> Result<u64, String> {
        let command_node_id = command.node_id.clone().unwrap_or_else(|| self.node_id.clone());
        debug!(
            %command_node_id, local_node = %self.node_id, %allow_remote_host,
            desc = %command.description(), "execute_impl"
        );
        if !allow_remote_host && command_node_id != self.node_id {
            return Err(format!("remote command routing not implemented yet for node {command_node_id}"));
        }

        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        if command.action.is_query() {
            // Query commands should be dispatched through `execute_query`,
            // not through `execute`. Return an error to surface misrouting.
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = flotilla_protocol::CommandValue::Error { message: "query commands should use execute_query, not execute".into() };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if matches!(command.action, flotilla_protocol::CommandAction::Refresh { repo: None }) {
            let repo_paths = {
                let repos = self.repos.read().await;
                let order = self.repo_order.read().await;
                order
                    .iter()
                    .filter_map(|identity| repos.get(identity).map(|state| state.preferred_path().to_path_buf()))
                    .collect::<Vec<_>>()
            };
            let repo_path = repo_paths.first().cloned().unwrap_or_default();
            let repo_identity = self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
            let description = command.description().to_string();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let mut refreshed = Vec::new();
            let result = match async {
                for repo in &repo_paths {
                    self.refresh(&flotilla_protocol::RepoSelector::Path(repo.clone())).await?;
                    refreshed.push(repo.clone());
                }
                Ok::<(), String>(())
            }
            .await
            {
                Ok(()) => flotilla_protocol::CommandValue::Refreshed { repos: refreshed },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewHandoff { context, target, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_handoff_internal(context, target, message).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewComplete { context, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_complete_internal(context, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewFail { context, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_fail_internal(context, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, work, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = self.provisioning_namespace().await;
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
            let check_work_is_completable = |current: &ResourceObject<ResourceConvoy>| match current.status.as_ref() {
                None => Err(ResourceError::other(format!("convoy {convoy} has no status"))),
                Some(status) => match status.work.get(work) {
                    None => Err(ResourceError::other(format!("convoy {convoy} does not contain work {work}"))),
                    Some(state) if state.phase.is_terminal() => {
                        Err(ResourceError::other(format!("convoy {convoy} work {work} is already terminal")))
                    }
                    Some(_) => Ok(()),
                },
            };
            let result = match apply_resource_status_patch_checked(
                &convoys,
                convoy,
                &convoy_external_patches::force_work_completed(work.clone(), chrono::Utc::now(), message.clone()),
                check_work_is_completable,
            )
            .await
            {
                Ok(_) => flotilla_protocol::CommandValue::Ok,
                Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyCreate {
            name,
            workflow_ref,
            inputs,
            repository_url,
            r#ref,
            project_ref,
            placement_policy,
            adopted_checkout,
        } = &command.action
        {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
            match convoys.get(name).await {
                Ok(_) => {
                    let result = flotilla_protocol::CommandValue::Error { message: format!("convoy {name} already exists") };
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result,
                    });
                    return Ok(id);
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(err) => {
                    let result = flotilla_protocol::CommandValue::Error { message: err.to_string() };
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result,
                    });
                    return Ok(id);
                }
            }
            let project_repositories = if let Some(project_ref) = project_ref {
                match self.snapshot_project_repositories(&namespace, project_ref).await {
                    Ok(repositories) => Some(repositories),
                    Err(message) => {
                        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.node_id.clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                None
            };
            if project_repositories.is_some() && repository_url.is_some() {
                let message = "convoy repository selection is not allowed when a project is supplied".to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement_policy = match placement_policy {
                Some(policy) => Some(policy.clone()),
                None => default_convoy_placement_policy(&self.resource_backend, &namespace).await,
            };
            let mut direct_repository_url = repository_url.clone();
            let mut r#ref = r#ref.clone();
            let adopted_checkout = match adopted_checkout {
                Some(path) => {
                    let adopted_result = async {
                        let inspection =
                            self.inspect_adopted_checkout(path.as_ref(), direct_repository_url.as_deref(), r#ref.as_deref()).await?;
                        let repo_ref = inspection.spec.key();
                        let transport_url = inspection
                            .transport_url
                            .as_deref()
                            .ok_or_else(|| "an adopted checkout requires a repository transport URL".to_string())?;
                        let git_ref = r#ref.as_deref().unwrap_or(&inspection.checkout.git_ref);
                        let (checkout_ref, inferred_repository_url, inferred_ref) = create_adopted_checkout_resource(
                            &self.resource_backend,
                            AdoptedCheckoutRequest::builder()
                                .namespace(&namespace)
                                .convoy_name(name)
                                .checkout_path(&inspection.checkout.path)
                                .repository_spec(&inspection.spec)
                                .repository_url(transport_url)
                                .git_ref(git_ref)
                                .host_ref(&inspection.checkout.host_ref)
                                .build(),
                        )
                        .await?;
                        Ok::<_, String>((repo_ref, checkout_ref, inferred_repository_url, inferred_ref))
                    }
                    .await;
                    match adopted_result {
                        Ok((repo_ref, checkout_ref, inferred_repository_url, inferred_ref)) => {
                            if project_repositories.is_none() {
                                direct_repository_url.get_or_insert(inferred_repository_url);
                            }
                            r#ref.get_or_insert(inferred_ref);
                            Some((repo_ref, checkout_ref))
                        }
                        Err(message) => {
                            let result = flotilla_protocol::CommandValue::Error { message };
                            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                                command_id: id,
                                node_id: self.node_id.clone(),
                                repo_identity: empty_identity,
                                repo: None,
                                result,
                            });
                            return Ok(id);
                        }
                    }
                }
                None => None,
            };
            let repositories = if let Some(repositories) = project_repositories {
                repositories
            } else if let Some(url) = direct_repository_url {
                let resolved = async {
                    let repository_spec = self.resolve_repository_remote(&url).await?;
                    let repo_ref = repository_spec.key();
                    let repository = flotilla_resources::ensure_repository(
                        &self.resource_backend.clone().using::<Repository>(&namespace),
                        &repo_ref,
                        &repository_spec,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let base_ref = repository
                        .status
                        .as_ref()
                        .and_then(|status| status.default_branch.clone())
                        .or_else(|| if adopted_checkout.is_some() { r#ref.clone() } else { None })
                        .ok_or_else(|| format!("repository {repo_ref} has no resolved default branch"))?;
                    Ok::<_, String>(vec![ConvoyRepositorySpec {
                        url,
                        repo_ref,
                        base_ref,
                        workspace_slug: normalize_workspace_slug(&repository_spec.leaf_slug()),
                        subpaths: Vec::new(),
                    }])
                }
                .await;
                match resolved {
                    Ok(repositories) => repositories,
                    Err(message) => {
                        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.node_id.clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                Vec::new()
            };
            let mut adopted_checkout_refs = BTreeMap::new();
            if let Some((repo_ref, checkout_ref)) = adopted_checkout {
                if !repositories.iter().any(|repository| repository.repo_ref == repo_ref) {
                    let message =
                        format!("adopted checkout repository {repo_ref} is not part of project {}", project_ref.as_deref().unwrap_or(""));
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
                adopted_checkout_refs.insert(repo_ref, checkout_ref);
            }
            let spec = ConvoySpec {
                workflow_ref: workflow_ref.clone(),
                inputs: inputs.iter().map(|(k, v)| (k.clone(), InputValue::String(v.clone()))).collect(),
                placement_policy,
                repositories,
                r#ref,
                project_ref: project_ref.clone(),
                adopted_checkout_refs,
            };
            let meta = InputMeta::builder().name(name.clone()).build();
            let result = match convoys.create(&meta, &spec).await {
                Ok(_) => flotilla_protocol::CommandValue::ConvoyCreated { name: name.clone() },
                Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::WorkflowTemplateApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let templates = self.resource_backend.clone().using::<WorkflowTemplate>(&namespace);
            let result = match parse_and_validate_workflow_template_yaml(spec_yaml) {
                Ok(spec) => {
                    let meta = InputMeta::builder().name(name.clone()).build();
                    let outcome = match templates.get(name).await {
                        Ok(existing) => templates.update(&meta, &existing.metadata.resource_version, &spec).await.map(|_| ()),
                        Err(ResourceError::NotFound { .. }) => templates.create(&meta, &spec).await.map(|_| ()),
                        Err(err) => Err(err),
                    };
                    match outcome {
                        Ok(()) => flotilla_protocol::CommandValue::WorkflowTemplateApplied { name: name.clone() },
                        Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
                    }
                }
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectAdd { target, name, display_name, remote } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_add(target, name.as_deref(), display_name.as_deref(), remote.as_deref()).await {
                Ok(name) => flotilla_protocol::CommandValue::ProjectAdded { name },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let projects = self.resource_backend.clone().using::<Project>(&namespace);
            let result = match normalize_project_name(name).and_then(|_| parse_project_yaml(spec_yaml)) {
                Ok(spec) => match normalize_project_spec(spec) {
                    Ok(spec) => {
                        let meta = InputMeta::builder().name(name.clone()).build();
                        let outcome = match projects.get(name).await {
                            Ok(existing) => projects.update(&meta, &existing.metadata.resource_version, &spec).await.map(|_| ()),
                            Err(ResourceError::NotFound { .. }) => projects.create(&meta, &spec).await.map(|_| ()),
                            Err(err) => Err(err),
                        };
                        match outcome {
                            Ok(()) => flotilla_protocol::CommandValue::ProjectApplied { name: name.clone() },
                            Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
                        }
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::TrackRepoPath { path } = &command.action {
            let description = command.description().to_string();
            let repo_path = path.clone();
            let repo_identity = self.detect_repo_identity(path).await;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.add_repo(path).await {
                Ok((tracked_path, resolved_from)) => flotilla_protocol::CommandValue::RepoTracked { path: tracked_path, resolved_from },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: self.tracked_repo_identity_for_path(path).await.unwrap_or(repo_identity),
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::UntrackRepo { repo } = &command.action {
            let repo_path = self.resolve_repo_selector(repo).await?;
            let description = command.description().to_string();
            let repo_identity =
                self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.remove_repo(&repo_path).await {
                Ok(()) => flotilla_protocol::CommandValue::RepoUntracked { path: repo_path.clone() },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::Refresh { repo: Some(selector) } = &command.action {
            let repo_path = self.resolve_repo_selector(selector).await?;
            let description = command.description().to_string();
            let repo_identity =
                self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.refresh(&flotilla_protocol::RepoSelector::Path(repo_path.clone())).await {
                Ok(()) => flotilla_protocol::CommandValue::Refreshed { repos: vec![repo_path.clone()] },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        // Gather what the spawned task needs — validate repo before broadcasting
        let repo = self.resolve_repo_for_command(&command).await?;
        let runner = Arc::clone(&self.discovery.runner);
        let env = Arc::clone(&self.discovery.env);
        let event_tx = self.event_tx.clone();
        let peer_overlay = self.peer_providers.read().await.clone();
        let (repo_identity, registry, providers_data, refresh_trigger) = {
            let repos = self.repos.read().await;
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let providers_data = if let Some(snapshot) = state.cached_snapshot() {
                Arc::new(snapshot.providers.clone())
            } else {
                Arc::new(
                    build_repo_snapshot_with_peers(
                        state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                        state.seq(),
                        peer_overlay.get(&identity).map(|peers| peers.as_slice()),
                    )
                    .providers,
                )
            };
            (state.identity().clone(), state.registry(), providers_data, state.refresh_trigger())
        };

        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let config_base = DaemonHostPath::new(self.config.base_path().as_path());

        let active_ref = Arc::clone(&self.active_commands);
        let token = CancellationToken::new();
        {
            let mut guard = active_ref.lock().await;
            guard.insert(id, token.clone());
        }

        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id: id,
            node_id: command_node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: Some(repo_path.clone()),
            description,
        });

        let local_host = self.host_name.clone();
        let local_node_id = self.node_id.clone();
        let attachable_store = self.discovery.shared_attachable_store(&self.config);
        let daemon_socket_path = self.daemon_socket_path.read().await.clone();
        let environment_manager = Arc::clone(&self.environment_manager);
        tokio::spawn(async move {
            let resolver_registry = Arc::clone(&registry);
            let resolver_providers_data = Arc::clone(&providers_data);
            let resolver_runner = Arc::clone(&runner);
            let resolver_env = Arc::clone(&env);
            let resolver_config_base = config_base.clone();
            let resolver_attachable_store = attachable_store.clone();
            let resolver_local_host = local_host.clone();
            let ee_repo_path = ExecutionEnvironmentPath::new(&repo_path);
            let resolver_repo = executor::RepoExecutionContext { identity: repo_identity.clone(), root: ee_repo_path.clone() };
            let daemon_socket_dhp = daemon_socket_path.map(DaemonHostPath::new);

            let plan = executor::build_plan(
                command,
                executor::RepoExecutionContext { identity: repo_identity.clone(), root: ee_repo_path },
                registry,
                providers_data,
                config_base,
                attachable_store,
                daemon_socket_dhp.clone(),
                local_node_id.clone(),
                local_host,
            )
            .await;

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
                    refresh_trigger.notify_one();
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id.clone(),
                        repo_identity: repo_identity.clone(),
                        repo: Some(repo_path),
                        result,
                    });
                }
                Ok(step_plan) => {
                    let resolver = executor::ExecutorStepResolver {
                        repo: resolver_repo,
                        registry: resolver_registry,
                        providers_data: resolver_providers_data,
                        runner: resolver_runner,
                        env: resolver_env,
                        config_base: resolver_config_base,
                        attachable_store: resolver_attachable_store,
                        daemon_socket_path: daemon_socket_dhp.clone(),
                        local_node_id: local_node_id.clone(),
                        local_host: resolver_local_host.clone(),
                        environment_manager: Arc::clone(&environment_manager),
                    };
                    let result = run_step_plan_with_remote_executor(
                        step_plan,
                        id,
                        local_node_id,
                        repo_identity.clone(),
                        ExecutionEnvironmentPath::new(&repo_path),
                        token,
                        event_tx.clone(),
                        &resolver,
                        remote_executor.as_ref(),
                    )
                    .await;
                    refresh_trigger.notify_one();
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id,
                        repo_identity,
                        repo: Some(repo_path),
                        result,
                    });
                }
            }
        });

        Ok(id)
    }
}

async fn execute_local_remote_step_batch(
    local_host: NodeId,
    request: RemoteStepBatchRequest,
    progress_sink: Arc<dyn RemoteStepProgressSink>,
    cancel: CancellationToken,
    resolver: &dyn StepResolver,
) -> Result<Vec<StepOutcome>, String> {
    let mut outcomes = Vec::new();
    let step_count = request.steps.len();

    for (index, step) in request.steps.into_iter().enumerate() {
        if step.host.node_id() != &local_host {
            return Err(format!("remote step {} targets {:?}, expected remote node {}", index, step.host, local_host));
        }
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }

        progress_sink
            .emit(crate::step::RemoteStepProgressUpdate {
                batch_step_index: index,
                batch_step_count: step_count,
                description: step.description.clone(),
                status: flotilla_protocol::StepStatus::Started,
            })
            .await;

        let outcome = resolver.resolve(&step.description, &step.host, step.action, &outcomes).await;
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }

        match outcome {
            Ok(step_outcome) => {
                let status = match &step_outcome {
                    StepOutcome::Skipped => flotilla_protocol::StepStatus::Skipped,
                    _ => flotilla_protocol::StepStatus::Succeeded,
                };
                progress_sink
                    .emit(crate::step::RemoteStepProgressUpdate {
                        batch_step_index: index,
                        batch_step_count: step_count,
                        description: step.description,
                        status,
                    })
                    .await;
                outcomes.push(step_outcome);
            }
            Err(message) => {
                progress_sink
                    .emit(crate::step::RemoteStepProgressUpdate {
                        batch_step_index: index,
                        batch_step_count: step_count,
                        description: step.description,
                        status: flotilla_protocol::StepStatus::Failed { message: message.clone() },
                    })
                    .await;
                return Err(message);
            }
        }
    }

    Ok(outcomes)
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoSnapshot, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        let peer_overlay = self.peer_providers.read().await.get(&identity).cloned();
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo_path.display()))?;
        Ok(match state.cached_snapshot() {
            Some(s) => (**s).clone(),
            None => build_repo_snapshot_with_peers(
                state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                state.seq(),
                peer_overlay.as_deref(),
            ),
        })
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut result = Vec::new();
        for identity in order.iter() {
            if let Some(state) = repos.get(identity) {
                result.push(RepoInfo {
                    identity: state.identity().clone(),
                    path: Some(state.preferred_path().to_path_buf()),
                    name: repo_name(state.preferred_path()),
                    labels: state.labels().clone(),
                    provider_names: state.provider_names(),
                    provider_health: crate::convert::health_to_proto(state.provider_health()),
                    loading: state.loading(),
                });
            }
        }
        Ok(result)
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false).await
    }

    async fn execute_query(&self, command: Command, _session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        use flotilla_protocol::CommandAction;
        match &command.action {
            CommandAction::QueryRepoDetail { repo } => match self.get_repo_detail_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoDetail(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryRepoProviders { repo } => match self.get_repo_providers_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoProviders(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryRepoWork { repo } => match self.get_repo_work_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoWork(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostList {} => match self.list_hosts_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostStatus { target_environment_id } => match self.get_host_status_internal(target_environment_id).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostStatus(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostProviders { target_environment_id } => {
                match self.get_host_providers_internal(target_environment_id).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::HostProviders(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryFleetList {} => match self.fleet_list_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryCrewList { context } => match self.crew_list_internal(context).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CrewList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetReplicaSnapshot {} => match self.fleet_replica_snapshot_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetReplicaSnapshot(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::Attach { reference } => match self.resolve_attach_command_internal(reference).await {
                Ok(resolved) => {
                    Ok(flotilla_protocol::CommandValue::AttachCommandResolved { command: resolved.command, binding: resolved.binding })
                }
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::AttachTransient { reference, host } => {
                match self.resolve_transient_attach_command_internal(reference, host.as_ref()).await {
                    Ok(resolved) => {
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { command: resolved.command, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryIssues { repo, params, page, count } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let service = self.get_issue_query_service(&repo_path).await?;
                let page = service.query(&repo_path, params, *page, *count).await?;
                Ok(flotilla_protocol::CommandValue::IssuePage(page))
            }
            CommandAction::QueryIssueFetchByIds { repo, ids } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let service = self.get_issue_query_service(&repo_path).await?;
                let items = service.fetch_by_ids(&repo_path, ids).await?;
                Ok(flotilla_protocol::CommandValue::IssuesByIds { items })
            }
            CommandAction::QueryIssueOpenInBrowser { repo, id } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let service = self.get_issue_query_service(&repo_path).await?;
                service.open_in_browser(&repo_path, id).await?;
                Ok(flotilla_protocol::CommandValue::Ok)
            }
            other => Err(format!("execute_query not implemented for this command type: {:?}", std::mem::discriminant(other))),
        }
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_commands.lock().await;
        match guard.get(&command_id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err("no matching active command".into()),
        }
    }

    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        let _ = self.refresh_local_host_summary().await;
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut events = self.host_registry.replay_host_events(last_seen).await;

        // Emit repo events
        for identity in order.iter() {
            let Some(state) = repos.get(identity) else {
                continue;
            };
            let Some(snapshot) = state.cached_snapshot() else {
                continue;
            };

            let repo_stream_key = StreamKey::Repo { identity: state.identity().clone() };
            match last_seen.get(&repo_stream_key) {
                Some(&client_seq) => match state.deltas_since(client_seq) {
                    Some(deltas) => {
                        for entry in deltas {
                            events.push(DaemonEvent::RepoDelta(Box::new(RepoDelta {
                                seq: entry.seq,
                                prev_seq: entry.prev_seq,
                                repo_identity: state.identity().clone(),
                                repo: Some(state.preferred_path().to_path_buf()),
                                changes: entry.changes.clone(),
                                work_items: snapshot.work_items.clone(),
                            })));
                        }
                    }
                    None => {
                        // Seq not in delta log — send full snapshot
                        events.push(DaemonEvent::RepoSnapshot(Box::new((**snapshot).clone())));
                    }
                },
                None => {
                    // Client has never seen this repo — send full snapshot
                    events.push(DaemonEvent::RepoSnapshot(Box::new((**snapshot).clone())));
                }
            }
        }

        Ok(events)
    }

    async fn subscribe_queries(&self, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        let state = self.aggregator_projection_state().await;
        let mut events = Vec::new();
        for cursor in queries {
            let result_set = state.result_set_for(cursor.query).await;
            if cursor.since.is_none_or(|seq| seq != result_set.seq) {
                events.push(DaemonEvent::ResultSet(Box::new(result_set)));
            }
        }
        Ok(events)
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        let peer_providers = self.peer_providers.read().await;
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut summaries = Vec::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            let snapshot: std::borrow::Cow<'_, RepoSnapshot> = match state.cached_snapshot() {
                Some(s) => std::borrow::Cow::Borrowed(s),
                None => std::borrow::Cow::Owned(build_repo_snapshot_with_peers(
                    state.snapshot_context(&self.node_id, &self.host_name, &self.environment_manager),
                    state.seq(),
                    peer_providers.get(identity).map(|v| v.as_slice()),
                )),
            };
            summaries.push(RepoSummary {
                path: state.preferred_path().to_path_buf(),
                slug: state.slug().map(str::to_string),
                provider_health: snapshot.provider_health.clone(),
                work_item_count: snapshot.work_items.len(),
                error_count: snapshot.errors.len(),
            });
        }
        Ok(StatusResponse { repos: summaries })
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Ok(self.host_registry.get_topology().await)
    }
}

#[cfg(test)]
mod tests;
