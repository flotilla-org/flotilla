//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use flotilla_admission::admission_actions::AdmissionActions;
use flotilla_admission::project_ops::{ProjectOperations, ProjectService, RepositoryIndex};
use flotilla_admission::repository_inspection::inspect_project_ops_entries as inspect_operational_entries;
use flotilla_admission::support::{
    authoritative_placement_host, canonical_placement_host_ref, ensure_repository_and_default_project_workflow,
    placement_actuator_host_ref, placement_target_host, unready_placement_refusal,
};
use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_observation::sources::{
    ChangeRequestQueryPort, IssueQueryPort, ProviderChangeRequestObservationSource, ProviderIssueObservationSource,
};
#[cfg(test)]
use flotilla_protocol::PlacementDecision;
use flotilla_resources::address::{remote_change_request_subject, same_subject};
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::{resolve_convoy_candidate_indices, ConvoyAddressIdentity};
#[cfg(test)]
use flotilla_resources::{CapabilityNeed, ProjectSpec, Resource};
use repository_discovery::discover_repository_change_request_with;
#[cfg(test)]
mod host_registry_tests;
mod host_summary_tests;
mod projection_ports;
use flotilla_daemon_api::action_support::empty_repo_identity;
use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_executor::action_support::{fallback_repo_identity, AddRepoOutcome};
use flotilla_resources::ResumeIntent;
use flotilla_store::get_resource_kind;
use flotilla_store::ResolvedCascadeStoreExt;
use flotilla_store_memory::InMemoryBackend;

use flotilla_daemon_api::action_events;
use flotilla_daemon_api::action_namespace;
#[path = "attach.rs"]
mod attach;
mod checkout_providers;
use flotilla_executor::executor_actions;
use flotilla_projections::projections_actions;

mod change_stream;
mod cleat_roll;
use flotilla_crew::crew_actions;
use flotilla_crew::crew_ops::{ConvoyResumeOutcome, CrewRoutingContext, CrewService, CrewTurnDeliveryActuator, MessageAttribution};
use flotilla_crew::ports::WorkCredentialReconciler;

mod image_check;
use checkout_providers::{CheckoutProvider, CheckoutProviders};
use flotilla_admission::project_image_layers;
mod repository_operations;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use attach::AttachResolver;
use chrono::{Duration as ChronoDuration, Utc};
#[cfg(test)]
use flotilla_admission::convoy_admission::PlacementResolution;
use flotilla_admission::convoy_admission::PreparedConvoyAdmission;
use flotilla_admission::convoy_admission::{ConvoyAdmission, ConvoyStartTask, StaticFulfilmentDecider};
use flotilla_resources::convoy_address::RoleAddress;
mod repository_discovery;
use flotilla_change_request::ChangeRequestTracker;
use flotilla_projections::ports::ResolvedAttach;
#[cfg(test)]
use flotilla_protocol::CheckoutArchiveStatus;
use flotilla_protocol::{
    commands::{AttachMode, RepositoryIdentityChange},
    qualified_path::QualifiedPath,
    result_set::{ConvoyChangeRequest, Rows},
    AttachBinding, CanonicalHostId, CheckoutArchiveOutcome, CliListKind, CliListResponse, CliListRow, Command, CommandAction, CommandValue,
    ConvoyExplanation, CrewCommandContext, CrewListResponse, DaemonEvent, DispatchQueueResponse, EnvironmentId, FleetHealthResponse,
    FleetListResponse, FulfilmentListResponse, HostListResponse, HostName, HostProviderStatus, HostProvidersResponse, HostStatusResponse,
    HostSummary, NodeId, NodeInfo, PeerConnectionState, PrincipalRef, ProjectListResponse, ProviderData, ProviderInfo, QueryCursor,
    RepoIdentity, RepoInfo, RepoProvidersResponse, RepoSummary, ResolvedAttachPlan, ResourceRef, StatusResponse, StreamKey,
    SurfaceDeclaration, TopologyResponse, TopologyRoute, AGENT_ADAPTER_PROVIDER_CATEGORY, TERMINAL_POOL_PROVIDER_CATEGORY,
};
use flotilla_resources::{
    host_direct_environment_name, observed_change_request_subjects, ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout,
    CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, Clock, Convoy as ResourceConvoy, ConvoyEnsure,
    ConvoyRepositorySpec, ConvoySpec, ConvoyStatusPatch, CredentialConsumer, CredentialGrant, CredentialSource, CredentialSpec,
    CrewCompletionPending, Forge, ForgeKind, InputMeta, IssueSourceResolution, IssueSourceUnavailable, LandingCredentialScope,
    LifecycleAuthority, ManifestRoot, ObservedChangeRequestState, PlacementPolicy, Project, Repository, RepositoryIdentity, RepositoryKey,
    RepositorySpec, ResourceError, ResourceObject, ResourceProvenance, SystemClock, TurnDeliveryRung, WatchEvent, WatchStart,
    WorkPhase as ResourceWorkPhase, WorkflowTemplate, WorkflowTemplateSpec, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
};
#[cfg(test)]
use flotilla_resources::{
    CheckoutIntegrationStatus, ConditionValue, IntegrationCondition, TerminalSessionIdentity, CREDENTIAL_SCOPES_ANNOTATION,
};
use flotilla_store::resolve_project_issue_sources;
use flotilla_store::{
    apply_resource_document, apply_status_patch as apply_resource_status_patch, get_resource_kind_including_replicas, ResourceBackend,
};
use futures::{FutureExt, StreamExt};
pub use image_check::{check_image, image_build_report, ImageCheckSources};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::host_identity::resolve_local_environment_state_dir;
use crate::host_identity::resolve_local_host_id;
use crate::host_identity::resolve_local_node_id;
use crate::host_identity::resolve_or_create_environment_id;
use crate::host_identity::resolve_or_create_remote_environment_id;
use crate::host_identity::resolve_or_create_remote_host_id;
use crate::model::repo_name;
use crate::model::RepoModel;
use crate::repo_state::RepoRootState;
use crate::repo_state::RepoState;
use crate::repository_inspection::GitRepositoryInspector;
use flotilla_admission::convoy_ensure::{ConvoyEnsureAdmission, ConvoyEnsureReconciler, StandingConvoyBackingInspector};
use flotilla_admission::regard_lifecycle::RegardLifecycle;
use flotilla_admission::regard_lifecycle::DEFAULT_REGARD_DECAY_SECONDS;
use flotilla_admission::regard_lifecycle::DEFAULT_REGARD_REFRESH_SECONDS;
use flotilla_admission::repository_inspection::RepositoryContinuity;
use flotilla_admission::repository_inspection::RepositoryInspection;
use flotilla_admission::repository_inspection::RepositoryInspector;
use flotilla_cloud_agent::RepoCriteria;
use flotilla_config::ConfigStore;
use flotilla_config::StaticEnvironmentConfig;
use flotilla_crew::leaf_engine::LeafSubscriptionTable;
use flotilla_daemon_api::daemon::{DaemonHandle, QuerySubscription};
use flotilla_daemon_api::event_sink::BroadcastEventSink;
use flotilla_daemon_api::event_sink::EventSink;
use flotilla_discovery::discover_checkout_with_host_scoped;
use flotilla_discovery::environment_manager::EnvironmentManager;
use flotilla_discovery::environment_manager::ResolvedEnvironment;
use flotilla_discovery::environments::{EnvironmentLifecycle, Environments};
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_discovery::run_host_detectors;
use flotilla_discovery::status::provider_names_from_registry;
use flotilla_discovery::DiscoveryResult;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_environment::environment::EnvironmentHandle;
use flotilla_executor::executor;
use flotilla_executor::executor::checkout::CheckoutResolutionScope;
use flotilla_executor::step::RemoteStepBatchRequest;
use flotilla_executor::step::RemoteStepExecutor;
use flotilla_executor::step::RemoteStepProgressSink;
use flotilla_executor::step::StepOutcome;
use flotilla_executor::step::StepResolver;
use flotilla_issue_tracker::IssueProvider;
use flotilla_paths::path_context::{canonical_or_original, DaemonHostPath, ExecutionEnvironmentPath};
use flotilla_projections::aggregator_projection::AggregatorProjectionState;
use flotilla_projections::fleet::FleetService;
use flotilla_projections::host_registry::HostCounts;
use flotilla_projections::host_registry::HostQueryDetails;
use flotilla_provider_runtime::host_environment::{HostEnvironment, HostSupply};
use flotilla_provider_runtime::ssh_runner::SshCommandRunner;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use flotilla_resources::checkout_integration::convoy_change_request_id_for_checkout;
use flotilla_resources::checkout_path::checkout_path;
use flotilla_vcs::checkout_integration::{
    inspect_checkout_integration, inspect_convoy_checkout_integration, ChangeRequestIntegrationInput,
};

struct ProviderChangeRequestQueryPort {
    resource_backend: ResourceBackend,
    config: Arc<ConfigStore>,
    discovery: Arc<DiscoveryRuntime>,
    environment_manager: Arc<EnvironmentManager>,
    local_environment_id: EnvironmentId,
}

#[derive(bon::Builder)]
struct HostIssueProviderLease {
    bag: EnvironmentBag,
    runner: Arc<dyn CommandRunner>,
    config: serde_json::Value,
    provider: Weak<dyn IssueProvider>,
}

struct ProviderIssueQueryPort {
    forge_reads: flotilla_observation::forge_observation::ForgeReads,
    host_providers: Mutex<HashMap<flotilla_protocol::IssueSource, HostIssueProviderLease>>,
    backend: ResourceBackend,
    config: Arc<ConfigStore>,
    discovery: Arc<DiscoveryRuntime>,
    environment_manager: Arc<EnvironmentManager>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
}

#[async_trait]
impl IssueQueryPort for ProviderIssueQueryPort {
    async fn provider_for_source(&self, source: &flotilla_protocol::IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        let namespace = action_namespace::provisioning_namespace(&self.provisioning_namespace);
        let source = flotilla_resources::normalize_issue_source(source);
        let repositories = self.backend.including_replicas::<Repository>(&namespace);
        // Project bindings choose portable sources; local checkout presence never
        // changes which provider or credential can serve that source.
        let projects = self.backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let mut selected_source = source.clone();
        for project in projects {
            if let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources(&repositories, &project.spec).await {
                if let Some(binding) = bindings.into_iter().find(|binding| binding.source == source) {
                    selected_source = binding.source;
                    break;
                }
            }
        }
        let declared = repositories.list().await.map_err(|error| error.to_string())?.items.into_iter().map(|source| source.object).find(
            |repository| {
                repository.spec.issue_source_forge().is_some_and(|forge| {
                    flotilla_resources::normalize_issue_source(&flotilla_protocol::IssueSource {
                        service: forge.service_url,
                        scope: forge.repository,
                    }) == selected_source
                })
            },
        );
        let bag = match declared {
            Some(repository) => {
                // Issue bindings can keep their canonical forge when a mirror
                // becomes the live checkout transport. Scope a copy of provider
                // intent to the source; do not rewrite the stored Repository.
                let live_source = repository.spec.forge().map(|forge| {
                    flotilla_resources::normalize_issue_source(&flotilla_protocol::IssueSource {
                        service: forge.service_url.clone(),
                        scope: forge.repository.clone(),
                    })
                });
                let intent = if live_source.as_ref() == Some(&selected_source) {
                    repository.spec
                } else {
                    repository.spec.update_remotes(format!(
                        "{}/{}",
                        selected_source.service.trim_end_matches('/'),
                        selected_source.scope
                    ))?
                };
                repository_discovery::repository_provider_bag(
                    &self.backend,
                    &self.config,
                    &self.environment_manager,
                    &self.local_environment_id,
                    &namespace,
                    &intent,
                )
                .await?
            }
            None => {
                // Explicit Project bindings can name a forge source without a
                // Repository member. Use the same forge construction and
                // credential selection as repository-backed bindings.
                match RepositorySpec::remote(format!("{}/{}", selected_source.service.trim_end_matches('/'), selected_source.scope)) {
                    Ok(intent) => {
                        repository_discovery::repository_provider_bag(
                            &self.backend,
                            &self.config,
                            &self.environment_manager,
                            &self.local_environment_id,
                            &namespace,
                            &intent,
                        )
                        .await?
                    }
                    Err(_) => self
                        .environment_manager
                        .environment_bag(&self.local_environment_id)
                        .ok_or_else(|| format!("environment not found: {}", self.local_environment_id))?,
                }
            }
        };
        let runner = self
            .environment_manager
            .environment_runner(&self.local_environment_id)
            .ok_or_else(|| format!("environment runner not found: {}", self.local_environment_id))?;
        // Source observers retain a strong lease. Re-resolving a host-only
        // capability must not recreate its ETag cache while that lease lives.
        // Environment or configuration changes invalidate the old capability.
        let config = serde_json::to_value(self.config.load_config()).map_err(|error| error.to_string())?;
        let mut host_providers = self.host_providers.lock().await;
        host_providers.retain(|_, lease| lease.provider.strong_count() > 0);
        if let Some(lease) = host_providers.get(&source) {
            if lease.bag.assertions() == bag.assertions() && Arc::ptr_eq(&lease.runner, &runner) && lease.config == config {
                if let Some(provider) = lease.provider.upgrade() {
                    return Ok(provider);
                }
            }
        }
        host_providers.remove(&source);
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        for factory in &self.discovery.factories.issue_trackers {
            if let Ok(provider) = factory.probe(&bag, &self.config, &probe_root, Arc::clone(&runner)).await {
                if provider.supports(&source) {
                    let provider: Arc<dyn IssueProvider> = Arc::new(flotilla_observation::forge_observation::ObservedIssueProvider {
                        inner: provider,
                        reads: {
                            let mut reads = self.forge_reads.clone();
                            reads.namespace = namespace.clone();
                            reads
                        },
                    });
                    host_providers.insert(
                        source.clone(),
                        HostIssueProviderLease::builder()
                            .bag(bag.clone())
                            .runner(runner.clone())
                            .config(config.clone())
                            .provider(Arc::downgrade(&provider))
                            .build(),
                    );
                    return Ok(provider);
                }
            }
        }
        Err(format!("no issue provider available for {} {}", source.service, source.scope))
    }
}

#[async_trait]
impl ChangeRequestQueryPort for ProviderChangeRequestQueryPort {
    async fn discover_repository_change_request(
        &self,
        namespace: &str,
        repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        discover_repository_change_request_with(
            &self.resource_backend,
            &self.config,
            &self.discovery,
            &self.environment_manager,
            &self.local_environment_id,
            namespace,
            repository,
        )
        .await
    }
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

use flotilla_projections::dispatch_board;
#[cfg(test)]
mod dispatch_board_tests;
use flotilla_projections::read_projections;
#[cfg(test)]
mod read_projection_tests;
#[cfg(test)]
mod repository_lifecycle_tests;
#[cfg(test)]
mod tests;

#[derive(Default)]
struct StaticEnvVars {
    vars: HashMap<String, String>,
}

impl StaticEnvVars {
    fn from_bag(bag: &EnvironmentBag) -> Self {
        let mut vars = HashMap::new();
        for assertion in bag.assertions() {
            if let EnvironmentAssertion::EnvVarSet { key, value } = assertion {
                vars.insert(key.clone(), value.clone());
            }
        }
        Self { vars }
    }
}

impl EnvVars for StaticEnvVars {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
}

async fn load_env_vars(runner: &dyn CommandRunner, cwd: &Path) -> HashMap<String, String> {
    let Ok(output) = runner.run("env", &[], cwd, &ChannelLabel::Default).await else {
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

const STATIC_SSH_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

async fn register_static_ssh_direct_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config_key: &str,
    environment: &StaticEnvironmentConfig,
    host_direct: bool,
    multiplex: bool,
    max_vessels: Option<u32>,
) -> Result<(), String> {
    let fallback_env_id = static_ssh_environment_id(config_key);
    let destination = flotilla_config::ssh_destination(&environment.hostname, environment.user.as_deref());
    let host_environment =
        host_environment(&environment.supplies).map_err(|error| format!("host configuration error for {config_key}: {error}"))?;
    let runner = Arc::new(
        SshCommandRunner::new(destination.clone(), multiplex, Arc::clone(&discovery.runner)).with_environment(host_environment.clone()),
    );
    tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, runner.run("true", &[], Path::new("/"), &ChannelLabel::Default))
        .await
        .map_err(|_| format!("ssh preflight timed out for {}", environment.hostname))?
        .map_err(|err| format!("ssh preflight failed for {}: {err}", environment.hostname))?;
    let remote_env_vars =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, load_env_vars(&*runner, Path::new("/"))).await.unwrap_or_default();
    let remote_env = StaticEnvVars { vars: remote_env_vars };
    let host_id = resolve_or_create_remote_host_id(&*runner, &remote_env).await?;
    let env_id = if host_direct {
        let host_id = host_id.as_ref().ok_or_else(|| format!("SSH host {} has no writable stable host identity", environment.hostname))?;
        EnvironmentId::new(host_direct_environment_name(host_id.as_str()))
    } else {
        resolve_or_create_remote_environment_id(&*runner, &remote_env, fallback_env_id).await?
    };
    let mut env_bag =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, run_host_detectors(&discovery.host_detectors, &*runner, &remote_env))
            .await
            .map_err(|_| format!("host detector execution timed out for {}", environment.hostname))?;
    if let Some(display_name) = environment.display_name.as_ref() {
        env_bag = env_bag.with(EnvironmentAssertion::env_var("DISPLAY_NAME", display_name));
    }
    environment_manager.register_direct_environment(env_id.clone(), runner, env_bag, host_id)?;
    environment_manager.set_direct_environment_host_environment(&env_id, host_environment)?;
    if host_direct {
        environment_manager.set_direct_environment_ssh_destination(&env_id, destination)?;
    }
    environment_manager.set_direct_environment_max_vessels(&env_id, max_vessels)?;
    Ok(())
}

/// #3722: a host's declared supplies, validated for its SSH runner.
fn host_environment(supplies: &[flotilla_config::HostSupply]) -> Result<HostEnvironment, String> {
    HostEnvironment::new(
        supplies
            .iter()
            .map(|supply| HostSupply {
                name: supply.name.clone(),
                path: supply.path.clone(),
                env: supply.env.iter().map(|(name, value)| (name.clone(), value.clone())).collect(),
            })
            .collect(),
    )
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
        if let Err(err) =
            register_static_ssh_direct_environment(environment_manager, discovery, config_key, environment, false, true, None).await
        {
            warn!(
                environment = %config_key,
                hostname = %environment.hostname,
                %err,
                "failed to register static SSH direct environment; continuing startup"
            );
        }
    }
    match config.load_hosts() {
        Ok(hosts) => {
            for (label, remote) in hosts.hosts.iter().filter(|(_, host)| host.agentless_ssh) {
                let environment = StaticEnvironmentConfig {
                    hostname: remote.hostname.clone(),
                    user: remote.user.clone(),
                    display_name: Some(remote.expected_host_name.clone()),
                    flotilla_command: None,
                    supplies: remote.supplies.clone(),
                };
                if let Err(err) = register_static_ssh_direct_environment(
                    environment_manager,
                    discovery,
                    label,
                    &environment,
                    true,
                    hosts.resolved_ssh_multiplex(label),
                    remote.max_vessels,
                )
                .await
                {
                    warn!(host = %label, %err, "failed to register agentless SSH host; continuing startup");
                }
            }
        }
        Err(err) => warn!(%err, "failed to load agentless SSH hosts"),
    }
}

async fn forge_for_remote(
    resource_backend: &ResourceBackend,
    namespace: &str,
    remote: &str,
) -> Result<Option<flotilla_resources::ForgeSpec>, String> {
    let forges = resource_backend.definitions::<flotilla_resources::Forge>(namespace).list().await.map_err(|error| error.to_string())?;
    let mut matching = Vec::new();
    for forge in forges {
        if forge.spec.repository_path(remote)?.is_some() {
            if forge.metadata.name != forge.spec.forge_id {
                return Err(format!("Forge {} must use its forge_id as its resource name", forge.metadata.name));
            }
            matching.push(forge.spec);
        }
    }
    if matching.len() > 1 {
        return Err("repository remote matches multiple Forge definitions".into());
    }
    Ok(matching.into_iter().next())
}

async fn discover_vcs_for_checkout(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config: &ConfigStore,
    local_environment_id: &EnvironmentId,
    environment_id: &EnvironmentId,
    checkout_path: &Path,
) -> Result<CheckoutProvider, String> {
    let runner = environment_manager
        .environment_runner(environment_id)
        .ok_or_else(|| format!("command runner unavailable for environment {environment_id}"))?;
    let host_bag = environment_manager
        .environment_bag(environment_id)
        .ok_or_else(|| format!("discovery environment unavailable: {environment_id}"))?;
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };
    let checkout = ExecutionEnvironmentPath::new(checkout_path);
    let mut bag = host_bag;
    for detector in &discovery.repo_detectors {
        bag = bag.extend(detector.detect(&checkout, &*runner, env).await);
    }
    let mut unmet = Vec::new();
    for factory in &discovery.factories.vcs {
        match factory.probe(&bag, config, &checkout, Arc::clone(&runner)).await {
            Ok(provider) => return Ok(CheckoutProvider { descriptor: factory.descriptor(), vcs: provider }),
            Err(requirements) => unmet.extend(requirements),
        }
    }
    Err(format!("no VCS provider discovered for {} in {environment_id}: {unmet:?}", checkout.as_path().display()))
}

#[allow(clippy::too_many_arguments)]
pub async fn discover_repo_for_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config: &ConfigStore,
    resource_backend: &ResourceBackend,
    namespace: &str,
    local_environment_id: &EnvironmentId,
    environment_id: &EnvironmentId,
    repo_path: &Path,
    repository_spec: Option<&RepositorySpec>,
) -> Result<DiscoveryResult, String> {
    let mut host_bag =
        environment_manager.environment_bag(environment_id).ok_or_else(|| format!("environment not found: {environment_id}"))?;
    let runner =
        environment_manager.environment_runner(environment_id).ok_or_else(|| format!("environment runner not found: {environment_id}"))?;
    if let Some(spec) = repository_spec {
        host_bag =
            repository_discovery::repository_provider_bag(resource_backend, config, environment_manager, environment_id, namespace, spec)
                .await?;
    }
    let ee_path = ExecutionEnvironmentPath::new(repo_path);
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };

    let host_scoped = discovery.discover_host_scoped(environment_id, &host_bag, config, &ee_path, Arc::clone(&runner)).await;
    Ok(discover_checkout_with_host_scoped(
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

const SUPERSEDED_BY_ANNOTATION: &str = "flotilla.work/superseded-by";

#[cfg(test)]
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

/// A resolved, currently-live convoy generation. Callers route by owner and
/// select sessions by `record_name`; neither operation accepts a raw role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConvoyRecord {
    pub address: RoleAddress,
    pub record_name: String,
    pub owner_host: HostName,
}

const CONVOY_ATTACH_WAIT: Duration = Duration::from_secs(60);

fn convoy_start_failure(convoy: &ResourceObject<ResourceConvoy>) -> Option<String> {
    let role = if convoy.spec.role.is_empty() { &convoy.metadata.name } else { &convoy.spec.role };
    let identity = convoy.spec.project_ref.as_ref().map_or_else(|| role.clone(), |project| format!("{role}@{project}"));
    let status = convoy.status.as_ref()?;
    if let Some((work, state)) = status.work.iter().find(|(_, state)| state.phase == ResourceWorkPhase::Failed) {
        let detail = state.message.as_deref().filter(|message| !message.trim().is_empty()).unwrap_or("work failed without a message");
        return Some(format!("convoy {identity} failed while starting work {work}: {detail}"));
    }
    match status.phase {
        flotilla_resources::ConvoyPhase::Failed => Some(match status.message.as_deref().filter(|message| !message.trim().is_empty()) {
            Some(message) => format!("convoy {identity} failed while starting: {message}"),
            None => format!("convoy {identity} failed while starting"),
        }),
        flotilla_resources::ConvoyPhase::Pending
        | flotilla_resources::ConvoyPhase::Active
        | flotilla_resources::ConvoyPhase::Interrupted
        | flotilla_resources::ConvoyPhase::Landing
        | flotilla_resources::ConvoyPhase::Landed
        | flotilla_resources::ConvoyPhase::Abandoned => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingConvoyTarget {
    pub home: HostName,
    pub node_id: NodeId,
    pub namespace: String,
    pub record_name: String,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExistingConvoyTarget {
    pub fn unreachable_message(&self, cause: &str) -> String {
        convoy_home_unreachable_message(&self.namespace, &self.record_name, &self.home, self.last_seen_at, cause)
    }
}

fn convoy_home_unreachable_message(
    namespace: &str,
    record_name: &str,
    home: &HostName,
    last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    cause: &str,
) -> String {
    let last_seen = last_seen_at.map_or_else(|| "unknown".to_string(), |timestamp| timestamp.to_rfc3339());
    format!(
        "convoy {namespace}/{record_name} is homed at {home}, last seen {last_seen}; home is unreachable: {cause}. \
         Break glass: flotilla resource delete convoys {record_name} --namespace {namespace} --host {home}"
    )
}

#[async_trait::async_trait]
impl flotilla_vcs::vcs::CheckoutVcsResolver for InProcessDaemon {
    async fn vcs_for(&self, environment: Option<&EnvironmentId>, path: &Path) -> Result<Arc<dyn flotilla_vcs::vcs::Vcs>, String> {
        self.vcs_for_checkout(environment.unwrap_or(&self.local_environment_id), path).await
    }
}

use flotilla_projections::dispatch_board::DispatchBoardInputs;

pub struct InProcessDaemon {
    change_stream_rounds: change_stream::Rounds,
    decided_store: std::sync::Mutex<Option<Arc<flotilla_schema_sqlite::stream::Service>>>,
    /// The engine's provisioning readings behind `[primitives]` (#3629 layer 4), installed with
    /// the decided store's provisioning homes; none before, and none with the switch off.
    lifecycle_reads: Arc<flotilla_projections::ports::LateLifecycleReads>,
    repos: Arc<RwLock<HashMap<flotilla_protocol::RepoIdentity, RepoState>>>,
    repo_order: RwLock<Vec<flotilla_protocol::RepoIdentity>>,
    event_source: Arc<BroadcastEventSink>,
    event_sink: Arc<dyn EventSink>,
    config: Arc<ConfigStore>,
    next_command_id: AtomicU64,
    node_id: NodeId,
    host_name: HostName,
    #[cfg(test)]
    change_request_observation_source: Arc<ProviderChangeRequestObservationSource>,
    host_registry: flotilla_projections::host_registry::HostRegistry,
    local_environment_id: EnvironmentId,
    environment_manager: Arc<EnvironmentManager>,
    cleat_roll_report: Mutex<Option<crate::cleat_roll::RollReport>>,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: Arc<DiscoveryRuntime>,
    change_request_query_port: Arc<dyn ChangeRequestQueryPort>,
    issue_query_port: Arc<dyn IssueQueryPort>,
    forge_budgets: flotilla_forge_github::forge_budget::ForgeBudgets,
    pub(crate) forge_demand_refresh: Mutex<()>,
    dispatch_board_cache: dispatch_board::DispatchBoardCache,
    /// VCS capabilities are selected once for each checkout in its execution environment.
    checkout_providers: Arc<CheckoutProviders>,
    repository_providers: Mutex<HashMap<(String, RepositoryKey), Arc<repository_operations::RepositoryProviderLease>>>,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    self_weak: Weak<InProcessDaemon>,
    admission: ConvoyAdmission,
    convoy_ensure_reconciler: RwLock<Option<Arc<dyn ConvoyEnsureReconciler>>>,
    crew: Arc<CrewService>,
    leaf_subscription_diagnostics: projection_ports::LeafSubscriptionDiagnostics,
    /// The `[primitives]` Message path, installed once at composition.
    message_path: crate::primitives::MessagePath,
    /// The same path, as crew code reaches it: through its EngineMessages port.
    crew_message_path: flotilla_crew::ports::MessagePath,
    /// The `[primitives]` Convoy path (#3587 layer 6), as crew code and the
    /// convoy reconciler reach it.
    convoy_engine_path: flotilla_resources::convoy::engine::EngineConvoyPath,
    /// Which seats a vessel serves, the engine's answer behind `[primitives]` (#3629 layer 8).
    vessel_seats_path: flotilla_resources::vessel_seats::VesselSeatsPath,
    message_receivers: projection_ports::MessageReceivers,
    brief_artifact_writer: Arc<RwLock<Option<Arc<dyn BriefArtifactWriter>>>>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: flotilla_agent::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
    resource_backend: ResourceBackend,
    clock: Arc<dyn Clock>,
    regard_lifecycle: Arc<RegardLifecycle>,
    observed_resource_backend: ResourceBackend,
    /// Serializes observed Checkout publication with repository removal so a
    /// refresh captured before untracking cannot recreate deleted resources.
    /// Admission cleanup and resource deletion share this lock with adoption.
    observed_checkout_reconciliation: Arc<Mutex<()>>,
    aggregator_projection_state: AggregatorProjectionState,
    /// Project image-layer refusal state, retained across periodic reconciliations.
    image_layer_refresh: Arc<Mutex<project_image_layers::LayerRefreshState>>,
    /// Provisioning namespace used by daemon-side resource operations (e.g.
    /// looking up the Convoy whose task is being marked complete). Set by the
    /// daemon runtime at startup; defaults to [`DEFAULT_PROVISIONING_NAMESPACE`].
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    checkout_namespace_changes: tokio::sync::watch::Sender<()>,
    fleet: FleetService,
    repository_inspector: RwLock<Option<Arc<dyn RepositoryInspector>>>,
    operator_reconciler: RwLock<Option<Arc<dyn OperatorReconciler>>>,
    /// Sessions on agentless hosts run in their vessels' agents (#3792).
    agentless_vessels: std::sync::RwLock<Option<Arc<dyn AgentlessVesselAccess>>>,
    /// Process-lifetime host discovery, independent of tracked-root membership.
    /// Heartbeats publish these observations into Host status.
    local_provider_statuses: Vec<HostProviderStatus>,
    local_placement_provider_statuses: RwLock<Vec<HostProviderStatus>>,
}

/// Default provisioning namespace used until [`InProcessDaemon::set_provisioning_namespace`]
/// is called. Matches `RuntimeOptions::namespace`'s default so tests that construct
/// the daemon directly hit the same namespace the runtime uses.
pub const DEFAULT_PROVISIONING_NAMESPACE: &str = "flotilla";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepositoryRefreshFailurePolicy {
    BestEffort,
    Strict,
}

#[async_trait]
impl ConvoyEnsureAdmission for InProcessDaemon {
    fn local_host_id(&self) -> Option<CanonicalHostId> {
        self.canonical_local_host_id()
    }
    async fn prepare(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String> {
        self.prepare_ensured_convoy(namespace, ensure).await
    }
    async fn commit(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        admission: PreparedConvoyAdmission,
        annotations: BTreeMap<String, String>,
    ) -> Result<String, String> {
        self.admission.admit_ensured_convoy(namespace, ensure, admission, annotations).await
    }
}

/// Runtime-owned reconciliation entry points which need collaborators that do
/// not belong in the surface-independent core daemon.
#[async_trait]
pub trait OperatorReconciler: Send + Sync {
    async fn reconcile_now(&self, namespace: &str, kind: &str, name: &str) -> Result<String, String>;
}

/// #3792: a session on an agentless host runs in its vessel's drydock agent,
/// which the daemon runtime owns. Its files and its attach go through that
/// agent; each method answers `None` for a session elsewhere.
#[async_trait]
pub trait AgentlessVesselAccess: Send + Sync {
    /// The runner for commands and files in the session's vessel.
    async fn runner(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Option<Result<Arc<dyn CommandRunner>, String>>;
    /// Attach to the session's cleat through its vessel's agent: a local
    /// socket forwarded to the vessel's cleat, and the session's name there.
    async fn attach(
        &self,
        spec: &flotilla_resources::TerminalSessionSpec,
        session_id: &str,
        seat: AttachMode,
    ) -> Option<Result<(std::path::PathBuf, String), String>>;
}

#[async_trait]
impl StandingConvoyBackingInspector for InProcessDaemon {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        flotilla_admission::convoy_ensure::ResourceStandingConvoyBackingInspector::new(self.resource_backend.clone())
            .with_lifecycle_reads(self.lifecycle_reads())
            .verify_backing_dead(convoy)
            .await
    }
}

fn rewrite_repository_annotations(meta: &mut InputMeta, replacements: &BTreeSet<RepositoryKey>, target_name: &str) -> bool {
    let mut changed = false;
    for value in meta.annotations.values_mut() {
        if replacements.contains(&RepositoryKey(value.clone())) {
            *value = target_name.to_string();
            changed = true;
        }
    }
    changed
}

fn rewrite_repository_keys(keys: &mut Vec<RepositoryKey>, replacements: &BTreeSet<RepositoryKey>, target: &RepositoryKey) -> bool {
    if !keys.iter().any(|key| replacements.contains(key)) {
        return false;
    }
    let mut changed = false;
    let mut seen = BTreeSet::new();
    keys.retain_mut(|key| {
        if replacements.contains(key) {
            *key = target.clone();
            changed = true;
        }
        if !seen.insert(key.clone()) {
            changed = true;
            return false;
        }
        true
    });
    changed
}

fn rewrite_repository_set(keys: &mut BTreeSet<RepositoryKey>, replacements: &BTreeSet<RepositoryKey>, target: &RepositoryKey) -> bool {
    if !keys.iter().any(|key| replacements.contains(key)) {
        return false;
    }
    keys.retain(|key| !replacements.contains(key));
    keys.insert(target.clone());
    true
}

impl InProcessDaemon {
    fn executor_actions(&self) -> executor_actions::ExecutorActions<'_> {
        executor_actions::ExecutorActions {
            namespace: &self.provisioning_namespace,
            port: self,
            resource_backend: &self.resource_backend,
            config: &self.config,
            active_commands: &self.active_commands,
            observed_checkout_reconciliation: &self.observed_checkout_reconciliation,
            environment_manager: Arc::clone(&self.environment_manager) as Arc<dyn EnvironmentLifecycle>,
            runner: &self.discovery.runner,
            env: &self.discovery.env,
            daemon_socket_path: &self.daemon_socket_path,
            host_name: &self.host_name,
            events: action_events::ActionEvents { node_id: &self.node_id, sink: &self.event_sink },
        }
    }

    fn projections_actions(&self) -> projections_actions::ProjectionsActions<'_> {
        projections_actions::ProjectionsActions {
            port: self,
            resource_backend: &self.resource_backend,
            observed_resource_backend: &self.observed_resource_backend,
            event_sink: &self.event_sink,
            node_id: &self.node_id,
            clock: &self.clock,
            config: &self.config,
            active_commands: &self.active_commands,
        }
    }

    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because daemon-owned background controllers retain
    /// weak references to the process state.
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        peer_overlay: Arc<dyn flotilla_projections::host_registry::PeerOverlay>,
    ) -> Arc<Self> {
        Self::new_with_resource_backend(
            repo_paths,
            config,
            discovery,
            host_name,
            ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()),
            peer_overlay,
        )
        .await
    }

    pub async fn new_with_resource_backend(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
        peer_overlay: Arc<dyn flotilla_projections::host_registry::PeerOverlay>,
    ) -> Arc<Self> {
        Self::new_with_resource_backend_and_clock(
            repo_paths,
            config,
            discovery,
            host_name,
            resource_backend,
            Arc::new(SystemClock),
            peer_overlay,
        )
        .await
    }

    // Construction accepts host-owned peer state alongside its existing clock and stores.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_with_resource_backend_and_clock(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
        clock: Arc<dyn Clock>,
        peer_overlay: Arc<dyn flotilla_projections::host_registry::PeerOverlay>,
    ) -> Arc<Self> {
        use flotilla_discovery::DiscoveryResult;

        let mut discovery = discovery;
        let forge_budgets = flotilla_forge_github::forge_budget::ForgeBudgets::default();
        discovery.runner =
            Arc::new(flotilla_forge_github::forge_budget::BudgetedRunner { inner: discovery.runner, budgets: forge_budgets.clone() });
        let discovery = Arc::new(discovery);
        let (event_tx, _) = broadcast::channel(256);
        let event_source = Arc::new(BroadcastEventSink::new(event_tx));
        let event_sink: Arc<dyn EventSink> = event_source.clone();
        let mut repos: HashMap<flotilla_protocol::RepoIdentity, RepoState> = HashMap::new();
        let mut order = Vec::new();

        let daemon_config = config.load_daemon_config().expect("failed to load daemon config");
        let config_machine_id = daemon_config.machine_id.as_deref();
        let local_environment_state_dir =
            resolve_local_environment_state_dir(config.state_dir().as_path(), config_machine_id, &*discovery.runner).await;
        let local_node_id = resolve_local_node_id(config.base_path().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local node id");
        let resource_backend = resource_backend.with_local_root(local_node_id.clone());
        let observed = if resource_backend.durable_local() {
            let path = config.state_dir().as_path().join("observation-replicas.sqlite");
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.expect("create observation replica directory");
            }
            let replicas = flotilla_store_sqlite::SqliteBackend::open_async(&path).await.expect("open durable observation replicas");
            InMemoryBackend::observed_with_durable_replicas(ResourceBackend::new(replicas))
        } else {
            InMemoryBackend::observed()
        };
        let observed_resource_backend = ResourceBackend::new(observed).with_local_root(local_node_id.clone());
        let local_environment_id =
            resolve_or_create_environment_id(&local_environment_state_dir).expect("failed to resolve local direct environment id");
        let local_host_id = resolve_local_host_id(config.state_dir().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local host id");
        let environment_manager =
            Arc::new(EnvironmentManager::new_local(&discovery, local_environment_id.clone(), local_host_id.clone()).await);
        register_static_ssh_direct_environments(&config, &discovery, &environment_manager).await;
        let local_host_bag =
            environment_manager.environment_bag(&local_environment_id).expect("local direct environment bag should always be available");
        let local_runner = environment_manager
            .environment_runner(&local_environment_id)
            .expect("local direct environment runner should always be available");
        let local_host_discovery = discovery
            .discover_host_scoped(
                &local_environment_id,
                &local_host_bag,
                &config,
                &ExecutionEnvironmentPath::new(config.base_path().as_ref()),
                local_runner,
            )
            .await;
        let agent_state_store = flotilla_agent::shared_file_backed_agent_state_store(config.base_path());
        for path in repo_paths {
            let path = canonical_or_original(&path);
            if repos.values().any(|state| state.contains_path(&path)) {
                continue;
            }
            let initial_vcs =
                discover_vcs_for_checkout(&environment_manager, &discovery, &config, &local_environment_id, &local_environment_id, &path)
                    .await;
            let mut startup_inspection = match &initial_vcs {
                Ok(vcs) => {
                    GitRepositoryInspector::new(
                        discovery.runner.clone(),
                        Arc::new(flotilla_vcs::vcs::FixedVcsResolver(Arc::clone(&vcs.vcs))),
                        local_host_id.to_string(),
                    )
                    .inspect_path(&path, None)
                    .await
                }
                Err(error) => Err(error.clone()),
            };
            if let Ok(inspection) = &mut startup_inspection {
                let mut spec = inspection.spec.clone();
                if let Some(live_remote) = spec.live_remote() {
                    if let Ok(repositories) = resource_backend.including_replicas::<Repository>(DEFAULT_PROVISIONING_NAMESPACE).list().await
                    {
                        if let Some(declared) =
                            repositories.items.into_iter().find(|repository| repository.object.spec.declares_remote(live_remote))
                        {
                            spec = declared.object.spec;
                        }
                    }
                }
                inspection.spec = spec.clone();
                config.set_checkout_config(&ExecutionEnvironmentPath::new(&path), spec.vcs().clone());
            }
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag: _, unmet } = discover_repo_for_environment(
                &environment_manager,
                &discovery,
                &config,
                &resource_backend,
                DEFAULT_PROVISIONING_NAMESPACE,
                &local_environment_id,
                &local_environment_id,
                &path,
                startup_inspection.as_ref().ok().map(|inspection| &inspection.spec),
            )
            .await
            .unwrap_or_else(|error| {
                warn!(repo = %path.display(), %error, "startup repository discovery failed");
                // Keep independently discovered checkout facts available. No
                // forge-dependent provider may activate after bag resolution fails.
                let mut registry = ProviderRegistry::default();
                if let Ok(vcs) = &initial_vcs {
                    registry.vcs.insert(vcs.descriptor.backend.clone(), vcs.descriptor.clone(), Arc::clone(&vcs.vcs));
                }
                DiscoveryResult::degraded(
                    registry,
                    startup_inspection.as_ref().ok().map(|inspection| inspection.spec.catalog_slug()),
                    error,
                )
            });
            if !unmet.is_empty() {
                debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
            }

            if let (Ok(inspection), Some(forge)) = (&mut startup_inspection, host_repo_bag.find_origin_forge()) {
                match inspection.spec.clone().on_forge(forge) {
                    Ok(spec) => inspection.spec = spec,
                    Err(error) => warn!(%error, "could not resolve startup repository forge"),
                }
            }
            let identity = startup_inspection
                .as_ref()
                .ok()
                .map(|inspection| flotilla_executor::action_support::repository_event_identity(&inspection.spec, None))
                .unwrap_or_else(|| fallback_repo_identity(&path));
            let mut repository_key = None;
            match startup_inspection {
                Ok(inspection) => {
                    // Observation roots are producers of Repository/Checkout facts.
                    // Recreate their ephemeral context before identity-based consumers run.
                    let publish = async {
                        let spec = match host_repo_bag.find_origin_forge() {
                            Some(forge) => inspection.spec.clone().on_forge(forge)?,
                            None => inspection.spec.clone(),
                        };
                        let key = spec.key();
                        flotilla_store::ensure_repository(
                            &resource_backend.clone().using::<Repository>(DEFAULT_PROVISIONING_NAMESPACE),
                            &key,
                            &spec,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        let mut providers = ProviderData::default();
                        if let Some(vcs) = registry.vcs.preferred() {
                            for checkout in vcs.enumerate_checkouts().await? {
                                let (checkout_path, checkout) = checkout.into_provider_checkout();
                                providers
                                    .checkouts
                                    .insert(QualifiedPath::host(local_host_id.clone(), checkout_path.into_path_buf()), checkout);
                            }
                        }
                        flotilla_observation::observed_resources::reconcile_checkouts(
                            &observed_resource_backend,
                            DEFAULT_PROVISIONING_NAMESPACE,
                            &key,
                            &spec.catalog_slug(),
                            &providers,
                            local_host_id.as_str(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        Ok::<_, String>(key)
                    }
                    .await;
                    match publish {
                        Ok(key) => {
                            repository_key = Some(key);
                        }
                        Err(error) => warn!(repo = %path.display(), %error, "startup repository observation failed"),
                    }
                }
                Err(error) => {
                    warn!(repo = %path.display(), %error, "repository key is unavailable during daemon startup");
                }
            }
            let slug = repo_slug.clone();
            let model = RepoModel::new_observation(registry, Some(local_environment_id.clone()));
            let root = RepoRootState { path: path.clone(), model, slug, unmet, is_local: true };

            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                order.push(identity.clone());
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
            }
            repos.get_mut(&identity).expect("inserted repository presentation").repository_key = repository_key;
        }

        let local_provider_statuses = local_host_discovery.provider_statuses();
        let local_host_summary = flotilla_projections::host_summary::build_local_host_summary(
            &local_node_id,
            &host_name,
            EnvironmentId::host(environment_manager.local_host_id().clone()),
            &projection_ports::ProjectionEnvironmentSummaries(&environment_manager),
            local_provider_statuses.clone(),
            &*discovery.env,
        )
        .await;

        let repos = Arc::new(RwLock::new(repos));
        let provisioning_namespace = Arc::new(std::sync::RwLock::new(DEFAULT_PROVISIONING_NAMESPACE.to_string()));
        let query_port: Arc<dyn ChangeRequestQueryPort> = Arc::new(ProviderChangeRequestQueryPort {
            resource_backend: resource_backend.clone(),
            config: Arc::clone(&config),
            discovery: Arc::clone(&discovery),
            environment_manager: Arc::clone(&environment_manager),
            local_environment_id: local_environment_id.clone(),
        });
        let observation_source = Arc::new(ProviderChangeRequestObservationSource::new(resource_backend.clone(), Arc::clone(&query_port)));
        let change_request_refresher = flotilla_observation::change_request_observer::ChangeRequestRefresher::new(
            host_name.to_string(),
            resource_backend.clone(),
            local_node_id.to_string(),
            observation_source.clone(),
            flotilla_observation::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        if let Err(error) = change_request_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned change request observations at startup failed");
        }
        let issue_query_port: Arc<dyn IssueQueryPort> = Arc::new(ProviderIssueQueryPort {
            forge_reads: flotilla_observation::forge_observation::ForgeReads::new(
                resource_backend.clone(),
                DEFAULT_PROVISIONING_NAMESPACE.into(),
            ),
            host_providers: Mutex::new(HashMap::new()),
            backend: resource_backend.clone(),
            config: Arc::clone(&config),
            discovery: Arc::clone(&discovery),
            environment_manager: Arc::clone(&environment_manager),
            local_environment_id: local_environment_id.clone(),
            provisioning_namespace: Arc::clone(&provisioning_namespace),
        });
        let issue_refresher = flotilla_observation::issue_observer::IssueRefresher::new(
            resource_backend.clone(),
            local_node_id.to_string(),
            Arc::new(ProviderIssueObservationSource { backend: resource_backend.clone(), query_port: Arc::clone(&issue_query_port) }),
            flotilla_observation::issue_observer::IssueRefreshCadence::default(),
        );
        if let Err(error) = issue_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned issue observations at startup failed");
        }
        let message_inboxes = Arc::new(Mutex::new(HashMap::new()));
        let message_path = crate::primitives::MessagePath::default();
        let crew_message_path = flotilla_crew::ports::MessagePath::default();
        let convoy_engine_path = flotilla_resources::convoy::engine::EngineConvoyPath::default();
        let vessel_seats_path = flotilla_resources::vessel_seats::VesselSeatsPath::default();
        let leaf_subscriptions =
            LeafSubscriptionTable::with_issues(resource_backend.clone(), event_sink.clone(), change_request_refresher, issue_refresher)
                .with_clock(Arc::clone(&clock))
                .with_message_inboxes(Arc::clone(&message_inboxes))
                .with_message_path(Arc::clone(&crew_message_path))
                .with_convoy_engine(Arc::clone(&convoy_engine_path))
                .with_charter_brief_renderer(Arc::new(flotilla_crew::charter_notifications::LiveCharterBriefRenderer::new(
                    resource_backend.clone(),
                    Arc::clone(&config),
                )));
        let admission_free_space_path = config.state_dir().as_path().to_path_buf();
        let aggregator_projection_state = AggregatorProjectionState::new();
        let repository_change_requests = Arc::new(RwLock::new(HashMap::new()));
        let brief_artifact_writer = Arc::new(RwLock::new(None));
        let regard_lifecycle = Arc::new(RegardLifecycle::new(
            resource_backend.clone(),
            Arc::clone(&clock),
            ChronoDuration::seconds(DEFAULT_REGARD_DECAY_SECONDS),
        ));
        let admission_free_space_path = Arc::new(std::sync::RwLock::new(admission_free_space_path));
        let observed_checkout_reconciliation = Arc::new(Mutex::new(()));
        let lifecycle_reads = Arc::new(flotilla_resources::lifecycle_reads::LateLifecycleReads::default());
        let checkout_providers = Arc::new(
            CheckoutProviders::builder()
                .lifecycle_reads(lifecycle_reads.clone() as Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>)
                .resource_backend(resource_backend.clone())
                .observed_resource_backend(observed_resource_backend.clone())
                .config(Arc::clone(&config))
                .discovery(Arc::clone(&discovery))
                .environment_manager(Arc::clone(&environment_manager))
                .local_environment_id(local_environment_id.clone())
                .provisioning_namespace(Arc::clone(&provisioning_namespace))
                .build(),
        );
        let crew = Arc::new(
            CrewService::builder()
                .lifecycle_reads(lifecycle_reads.clone() as Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>)
                .message_inboxes(Arc::clone(&message_inboxes))
                .message_path(Arc::clone(&crew_message_path))
                .convoy_engine(Arc::clone(&convoy_engine_path))
                .vessel_seats(Arc::clone(&vessel_seats_path))
                .capability_cards(flotilla_crew::crew_capabilities::CapabilityCardTracker::with_message_path(Arc::clone(
                    &crew_message_path,
                )))
                .resource_backend(resource_backend.clone())
                .leaf_subscriptions(leaf_subscriptions.clone())
                .clock(Arc::clone(&clock))
                .provisioning_namespace(Arc::clone(&provisioning_namespace))
                .config(Arc::clone(&config))
                .host_name(host_name.clone())
                .brief_artifact_writer(Arc::clone(&brief_artifact_writer))
                .checkout_vcs(Arc::clone(&checkout_providers) as Arc<dyn flotilla_vcs::vcs::CheckoutVcsResolver>)
                .local_environment_id(local_environment_id.clone())
                .build(),
        );
        let daemon = Arc::new_cyclic(|self_weak| Self {
            dispatch_board_cache: dispatch_board::DispatchBoardCache::default(),
            repos: Arc::clone(&repos),
            repo_order: RwLock::new(order),
            event_source,
            event_sink: event_sink.clone(),
            config: Arc::clone(&config),
            next_command_id: AtomicU64::new(1),
            node_id: local_node_id.clone(),
            host_name: host_name.clone(),
            #[cfg(test)]
            change_request_observation_source: Arc::clone(&observation_source),
            host_registry: flotilla_projections::host_registry::HostRegistry::new(
                NodeInfo::new(local_node_id.clone(), host_name.to_string()),
                local_host_summary,
                peer_overlay,
            ),
            local_environment_id: local_environment_id.clone(),
            environment_manager: Arc::clone(&environment_manager),
            cleat_roll_report: Mutex::new(None),
            discovery: Arc::clone(&discovery),
            change_request_query_port: Arc::clone(&query_port),
            issue_query_port: Arc::clone(&issue_query_port),
            forge_budgets,
            forge_demand_refresh: Mutex::new(()),
            checkout_providers: Arc::clone(&checkout_providers),
            checkout_namespace_changes: tokio::sync::watch::channel(()).0,
            repository_providers: Mutex::new(HashMap::new()),
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            self_weak: self_weak.clone(),
            admission: ConvoyAdmission::builder()
                .lifecycle_reads(lifecycle_reads.clone() as Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>)
                .backend(resource_backend.clone())
                .observed_backend(observed_resource_backend.clone())
                .observed_checkout_reconciliation(Arc::clone(&observed_checkout_reconciliation))
                .config(Arc::clone(&config))
                .discovery(Arc::clone(&discovery))
                .environment_manager(Arc::clone(&environment_manager) as Arc<dyn Environments>)
                .local_environment_id(local_environment_id.clone())
                .provisioning_namespace(Arc::clone(&provisioning_namespace))
                .repository_change_requests(Arc::clone(&repository_change_requests))
                .change_request_port(query_port)
                .issue_port(issue_query_port)
                .change_request_observation_source(Arc::clone(&observation_source))
                .brief_artifact_writer(Arc::clone(&brief_artifact_writer))
                .admission_free_space_path(Arc::clone(&admission_free_space_path))
                .regard_lifecycle(Arc::clone(&regard_lifecycle))
                .host_name(host_name.clone())
                .clock(Arc::clone(&clock))
                .fulfilment_decider(Arc::new(StaticFulfilmentDecider))
                .build(),
            convoy_ensure_reconciler: RwLock::new(None),

            crew: Arc::clone(&crew),
            leaf_subscription_diagnostics: projection_ports::LeafSubscriptionDiagnostics(leaf_subscriptions),
            message_receivers: projection_ports::MessageReceivers(Arc::clone(&message_path)),
            message_path,
            crew_message_path,
            convoy_engine_path,
            vessel_seats_path,
            brief_artifact_writer: Arc::clone(&brief_artifact_writer),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
            clock: Arc::clone(&clock),
            regard_lifecycle: Arc::clone(&regard_lifecycle),
            decided_store: Default::default(),
            change_stream_rounds: Default::default(),
            lifecycle_reads: Arc::clone(&lifecycle_reads),
            resource_backend: resource_backend.clone(),
            observed_resource_backend: observed_resource_backend.clone(),
            observed_checkout_reconciliation: Arc::clone(&observed_checkout_reconciliation),
            aggregator_projection_state: aggregator_projection_state.clone(),
            image_layer_refresh: Default::default(),
            provisioning_namespace: Arc::clone(&provisioning_namespace),
            fleet: FleetService::new(
                aggregator_projection_state.clone(),
                host_name.clone(),
                Some(CanonicalHostId::resolved(environment_manager.local_host_id().as_str())),
            ),
            repository_inspector: RwLock::new(None),
            operator_reconciler: RwLock::new(None),
            agentless_vessels: std::sync::RwLock::new(None),
            local_provider_statuses,
            local_placement_provider_statuses: RwLock::new(Vec::new()),
        });
        crew.set_checkout_inspector(Arc::new(CheckoutInspector(Arc::downgrade(&daemon))));
        crew.set_turn_delivery_actuator(Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew) })).await;

        daemon.spawn_checkout_provider_retirement();

        let weak = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            while let Some(daemon) = weak.upgrade() {
                let namespace = daemon.provisioning_namespace().await;
                let repositories = daemon.resource_backend.clone().using::<Repository>(&namespace);
                let listed = match repositories.list().await {
                    Ok(listed) => listed,
                    Err(error) => {
                        warn!(%error, "list repositories for provider cache eviction failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let live = listed.items.iter().map(|repository| repository.metadata.name.as_str()).collect::<HashSet<_>>();
                daemon.admission.repository_change_requests().write().await.retain(|key, _| live.contains(key.to_string().as_str()));
                daemon
                    .repository_providers
                    .lock()
                    .await
                    .retain(|(cached_namespace, key), _| cached_namespace != &namespace || live.contains(key.to_string().as_str()));
                let mut watch = match repositories.watch(WatchStart::resuming_from(&listed)).await {
                    Ok(watch) => watch,
                    Err(error) => {
                        warn!(%error, "watch repositories for provider cache eviction failed");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                drop(daemon);
                let mut namespace_check = tokio::time::interval(Duration::from_secs(1));
                namespace_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                namespace_check.tick().await;
                loop {
                    let event = tokio::select! {
                        event = watch.next() => event,
                        _ = namespace_check.tick() => {
                            let Some(daemon) = weak.upgrade() else { return };
                            if daemon.provisioning_namespace().await != namespace {
                                break;
                            }
                            continue;
                        }
                    };
                    let name = match event {
                        Some(Ok(WatchEvent::Deleted(repository))) => repository.metadata.name,
                        Some(Ok(WatchEvent::DeletedByName(tombstone))) => tombstone.name,
                        Some(Ok(_)) => continue,
                        Some(Err(error)) => {
                            warn!(%error, "repository provider cache watch failed");
                            break;
                        }
                        None => break,
                    };
                    let Some(daemon) = weak.upgrade() else { return };
                    daemon.admission.repository_change_requests().write().await.retain(|key, _| key.to_string() != name);
                    daemon
                        .repository_providers
                        .lock()
                        .await
                        .retain(|(cached_namespace, key), _| cached_namespace != &namespace || key.to_string() != name);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        let weak = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            let mut expiry = tokio::time::interval(Duration::from_secs(1));
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut refresh = tokio::time::interval(Duration::from_secs(DEFAULT_REGARD_REFRESH_SECONDS));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = expiry.tick() => {
                        let Some(daemon) = weak.upgrade() else { break };
                        let namespace = daemon.provisioning_namespace().await;
                        if let Err(error) = daemon.regard_lifecycle.expire_due(&namespace).await {
                            warn!(%error, "failed to expire due regards");
                        }
                    }
                    _ = refresh.tick() => {
                        let Some(daemon) = weak.upgrade() else { break };
                        if let Err(error) = daemon.regard_lifecycle.refresh_focused().await {
                            warn!(%error, "failed to refresh focused regards");
                        }
                    }
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

    /// Identity is available without collecting descriptive observations.
    pub fn local_host_identity(&self) -> flotilla_protocol::HostIdentity {
        flotilla_protocol::HostIdentity {
            environment_id: EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            host_name: Some(self.host_name.clone()),
            node: NodeInfo::new(self.node_id.clone(), self.host_name.to_string()),
        }
    }

    pub async fn local_host_summary(&self) -> HostSummary {
        self.refresh_local_host_summary().await
    }

    /// Full descriptive heartbeat observation. Summary surfaces retain their
    /// provisioned-only environment list through HostStatus::host_summary().
    pub async fn local_host_description(&self) -> HostSummary {
        let mut description = self.refresh_local_host_summary().await;
        // Host queries include direct environments; legacy summaries remain provisioned-only.
        description.environments = self.environment_manager.visible_environments().await;
        description
    }

    pub async fn set_local_placement_capabilities(&self, agent_adapters: &BTreeSet<String>, terminal_pools: &[String]) {
        let mut statuses = agent_adapters
            .iter()
            .map(|adapter| HostProviderStatus::available(AGENT_ADAPTER_PROVIDER_CATEGORY, adapter))
            .chain(terminal_pools.iter().map(|pool| HostProviderStatus::available(TERMINAL_POOL_PROVIDER_CATEGORY, pool)))
            .collect::<Vec<_>>();
        statuses.sort_by(|left, right| (&left.category, &left.implementation).cmp(&(&right.category, &right.implementation)));
        *self.local_placement_provider_statuses.write().await = statuses;
    }

    /// Use `path` as the canonical capacity source for both local and
    /// federated convoy admission.
    pub fn set_admission_free_space_path(&self, path: PathBuf) {
        self.admission.set_free_space_path(path);
    }

    pub async fn set_brief_artifact_writer(&self, writer: Arc<dyn BriefArtifactWriter>) {
        *self.brief_artifact_writer.write().await = Some(writer);
    }

    pub async fn admission_free_space_bytes(&self) -> Result<Option<u64>, String> {
        self.admission.free_space_bytes().await
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

    pub async fn set_operator_reconciler(&self, reconciler: Arc<dyn OperatorReconciler>) {
        *self.operator_reconciler.write().await = Some(reconciler);
    }

    pub fn set_agentless_vessel_access(&self, access: Arc<dyn AgentlessVesselAccess>) {
        *self.agentless_vessels.write().expect("agentless vessels lock poisoned") = Some(access);
    }

    /// The runner for a session's files: its vessel's agent on an agentless
    /// host (#3792), otherwise its environment's runner. `None` when neither
    /// is known; an error when the vessel's agent cannot be reached.
    pub async fn command_runner_for_session(
        &self,
        spec: &flotilla_resources::TerminalSessionSpec,
    ) -> Option<Result<Arc<dyn CommandRunner>, String>> {
        let access = self.agentless_vessels.read().expect("agentless vessels lock poisoned").clone();
        if let Some(runner) = match access {
            Some(access) => access.runner(spec).await,
            None => None,
        } {
            return Some(runner);
        }
        self.command_runner_for_environment_ref(&spec.env_ref).map(Ok)
    }

    pub async fn set_work_credential_reconciler(&self, reconciler: Arc<dyn WorkCredentialReconciler>) {
        self.crew.set_work_credential_reconciler(reconciler).await;
    }

    pub async fn ledger_delivery_environment(&self, namespace: &str, environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        self.crew.ledger_delivery_environment(namespace, environment_ref).await
    }

    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        if let Some(inspector) = self.repository_inspector.read().await.clone() {
            return Ok(inspector);
        }
        let runner = self.local_command_runner().ok_or_else(|| "local repository inspector is unavailable".to_string())?;
        let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?;
        let namespace = self.provisioning_namespace().await;
        let forges =
            self.resource_backend.definitions::<flotilla_resources::Forge>(&namespace).list().await.map_err(|error| error.to_string())?;
        Ok(Arc::new(
            GitRepositoryInspector::new(
                runner,
                self.self_weak.upgrade().ok_or("repository inspector daemon unavailable")?
                    as Arc<dyn flotilla_vcs::vcs::CheckoutVcsResolver>,
                host_ref.to_string(),
            )
            .with_forges(forges.into_iter().map(|forge| forge.spec).collect())
            .with_charter_cache(self.config.state_dir().join("charter-stores").into_path_buf()),
        ))
    }

    pub async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        let mut inspection = self.repository_inspector().await?.inspect_path(path, remote).await?;
        let spec = self.resolve_forge_identity(inspection.spec).await?;
        let (spec, replaces_prior_repository) = self.configure_inspected_repository(&inspection.checkout.path, spec).await?;
        inspection.spec = spec;
        inspection.replaces_prior_repository = replaces_prior_repository;
        Ok(inspection)
    }

    async fn configure_inspected_repository(&self, path: &Path, spec: RepositorySpec) -> Result<(RepositorySpec, bool), String> {
        let repository_key = self.observed_repository_key_for_path(path).await?;
        self.configure_repository_for_checkout(path, spec, repository_key.as_ref()).await
    }

    async fn configure_repository_for_checkout(
        &self,
        path: &Path,
        spec: RepositorySpec,
        repository_key: Option<&RepositoryKey>,
    ) -> Result<(RepositorySpec, bool), String> {
        if let Some(repository_key) = repository_key {
            let namespace = self.provisioning_namespace().await;
            let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
            if let Ok(stored) = repositories.get(&repository_key.to_string()).await {
                if !matches!(stored.spec.identity(), flotilla_resources::RepositoryIdentity::Remote { .. })
                    || !matches!(spec.identity(), flotilla_resources::RepositoryIdentity::Remote { .. })
                {
                    return self.configure_unassociated_repository(path, spec).await.map(|spec| (spec, false));
                }
                let live_remote = spec.live_remote().map(str::to_string);
                if let Some(live_remote) = live_remote.as_deref().filter(|remote| !stored.spec.declares_remote(remote)) {
                    match self.repository_inspector().await?.verify_continuity(path, &stored.spec).await {
                        RepositoryContinuity::Continuous { evidence } => {
                            info!(repo = %path.display(), old_repository = %repository_key, new_remote = %live_remote, %evidence, "preserving repository identity after continuity check");
                        }
                        RepositoryContinuity::Unproven { evidence } => {
                            info!(repo = %path.display(), old_repository = %repository_key, new_remote = %live_remote, %evidence, "minting repository identity because continuity is unproven");
                            return self.configure_unassociated_repository(path, spec).await.map(|spec| (spec, true));
                        }
                    }
                }
                let mut updated = stored.spec.clone();
                if let Some(live_remote) = live_remote {
                    updated = updated.update_remotes(live_remote)?;
                }
                if updated != stored.spec {
                    repositories
                        .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, &updated)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                return Ok((updated, false));
            }
        }
        self.configure_unassociated_repository(path, spec).await.map(|spec| (spec, false))
    }

    async fn configure_unassociated_repository(&self, _path: &Path, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        self.resolve_declared_repository(spec).await
    }

    pub async fn repository_key_for_path(&self, path: &Path) -> Option<RepositoryKey> {
        self.observed_repository_key_for_path(path).await.ok().flatten()
    }

    async fn observed_repository_key_for_path(&self, path: &Path) -> Result<Option<RepositoryKey>, String> {
        let path = canonical_or_original(path);
        let namespace = self.provisioning_namespace().await;
        let checkouts = flotilla_admission::repository_addressing::local_checkouts_with_lifecycle_reads(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
            self.lifecycle_reads.as_ref(),
        )
        .await?;
        let mut keys = checkouts
            .into_iter()
            .filter(|checkout| match &checkout.spec {
                ResourceCheckoutSpec::Observed(spec) => Path::new(&spec.path) == path,
                _ => {
                    checkout.status.as_ref().and_then(|status| status.path.as_deref()).is_some_and(|candidate| Path::new(candidate) == path)
                }
            })
            .map(|checkout| checkout.spec.repo_ref().clone())
            .collect::<BTreeSet<_>>();
        if keys.len() == 1 {
            Ok(keys.pop_first())
        } else if keys.is_empty() {
            Ok(None)
        } else {
            Err(format!("multiple repositories have an observed checkout at {}", path.display()))
        }
    }

    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        let spec = self.repository_inspector().await?.resolve_remote(remote).await?;
        self.resolve_declared_repository(self.resolve_forge_identity(spec).await?).await
    }

    async fn resolve_forge_identity(&self, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        let namespace = self.provisioning_namespace().await;
        self.resolve_forge_identity_in(&namespace, spec).await
    }

    async fn resolve_forge_identity_in(&self, namespace: &str, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        let Some(remote) = spec.live_remote() else {
            return Ok(spec);
        };
        let Some(forge) = forge_for_remote(&self.resource_backend, namespace, remote).await? else { return Ok(spec) };
        let resolved = spec.on_forge(&forge)?;
        self.sweep_split_forge_repositories(namespace, &resolved, &forge).await
    }

    async fn sweep_split_forge_repositories(
        &self,
        namespace: &str,
        observed: &RepositorySpec,
        forge: &flotilla_resources::ForgeSpec,
    ) -> Result<RepositorySpec, String> {
        let repositories = self.resource_backend.clone().using::<Repository>(namespace);
        let target_key = observed.key();
        let mut merged = observed.clone();
        let mut old_sources = Vec::new();
        let mut old_local = Vec::new();
        let mut replacements = BTreeSet::new();
        let sources =
            self.resource_backend.clone().including_replicas::<Repository>(namespace).list().await.map_err(|error| error.to_string())?;
        for source in sources.items {
            let repository = source.object;
            let Ok(normalized) = repository.spec.clone().on_forge(forge) else { continue };
            if normalized.key() != target_key {
                continue;
            }
            merged = merged.merge_forge_migration(&normalized)?;
            if repository.metadata.name != target_key.to_string() {
                replacements.insert(RepositoryKey(repository.metadata.name.clone()));
                if matches!(source.provenance, ResourceProvenance::Local) {
                    old_local.push(repository.clone());
                }
                old_sources.push(repository);
            }
        }
        if replacements.is_empty() {
            return Ok(merged);
        }
        let target_name = target_key.to_string();
        let existing = match repositories.get(&target_name).await {
            Ok(existing) => Some(existing),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let mut target_meta = existing
            .as_ref()
            .map_or_else(|| InputMeta::builder().name(target_name.clone()).build(), |existing| InputMeta::from(&existing.metadata));
        for source in &old_sources {
            for (label, value) in &source.metadata.labels {
                if target_meta.labels.insert(label.clone(), value.clone()).is_some_and(|previous| previous != *value) {
                    return Err(format!("Repository metadata label `{label}` conflicts during forge identity sweep"));
                }
            }
            // Annotations are bookkeeping about each record (bootstrap commit, last sync,
            // authoring root), so split records routinely disagree. The surviving record's
            // values win; a source only fills annotations the survivor lacks.
            for (annotation, value) in &source.metadata.annotations {
                if annotation == SUPERSEDED_BY_ANNOTATION {
                    continue;
                }
                target_meta.annotations.entry(annotation.clone()).or_insert_with(|| value.clone());
            }
        }
        let projects = self.resource_backend.clone().definitions::<Project>(namespace);
        let mut project_updates = Vec::new();
        for project in projects.list().await.map_err(|error| error.to_string())? {
            let mut spec = project.spec.clone();
            let mut meta = InputMeta::from(&project.metadata);
            let mut changed = false;
            for member in &mut spec.repositories {
                if replacements.contains(&member.repo) {
                    member.repo = target_key.clone();
                    changed = true;
                }
            }
            changed |= rewrite_repository_annotations(&mut meta, &replacements, &target_name);
            if changed {
                let mut by_repository = BTreeMap::new();
                for member in spec.repositories.drain(..) {
                    let key = (member.repo.clone(), member.subpath.clone());
                    match by_repository.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(member);
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            let prior: &mut flotilla_resources::ProjectRepositorySpec = entry.get_mut();
                            if prior.alias.is_some() && member.alias.is_some() && prior.alias != member.alias {
                                return Err(format!(
                                    "Project {} has aliases `{}` and `{}` for the same forge Repository {}; resolve the aliases before migration",
                                    project.metadata.name,
                                    prior.alias.as_deref().expect("alias checked"),
                                    member.alias.as_deref().expect("alias checked"),
                                    target_key
                                ));
                            }
                            if prior.default_branch.is_some()
                                && member.default_branch.is_some()
                                && prior.default_branch != member.default_branch
                            {
                                return Err(format!(
                                    "Project {} has conflicting default branches for forge Repository {}; resolve them before migration",
                                    project.metadata.name, target_key
                                ));
                            }
                            prior.alias = prior.alias.take().or(member.alias);
                            prior.default_branch = prior.default_branch.take().or(member.default_branch);
                            prior.roles.extend(member.roles);
                        }
                    }
                }
                spec.repositories = by_repository.into_values().collect();
                project_updates.push((meta, spec));
            }
        }
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace);
        let mut ensure_updates = Vec::new();
        for ensure in ensures.list().await.map_err(|error| error.to_string())? {
            let mut spec = ensure.spec.clone();
            let mut meta = InputMeta::from(&ensure.metadata);
            let mut changed = rewrite_repository_keys(&mut spec.repositories, &replacements, &target_key);
            changed |= rewrite_repository_annotations(&mut meta, &replacements, &target_name);
            if changed {
                ensure_updates.push((meta, spec));
            }
        }
        let templates = self.resource_backend.clone().definitions::<WorkflowTemplate>(namespace);
        let mut template_updates = Vec::new();
        for template in templates.list().await.map_err(|error| error.to_string())? {
            let mut spec = template.spec.clone();
            let mut meta = InputMeta::from(&template.metadata);
            let mut changed = rewrite_repository_annotations(&mut meta, &replacements, &target_name);
            for vessel in &mut spec.vessels {
                if let Some(refs) = &mut vessel.repository_refs {
                    changed |= rewrite_repository_keys(refs, &replacements, &target_key);
                }
                for refs in vessel.credential_scopes.values_mut() {
                    changed |= rewrite_repository_set(refs, &replacements, &target_key);
                }
            }
            if changed {
                template_updates.push((meta, spec));
            }
        }
        let grants = self.resource_backend.clone().definitions::<CredentialGrant>(namespace);
        let mut grant_updates = Vec::new();
        for grant in grants.list().await.map_err(|error| error.to_string())? {
            let mut spec = grant.spec.clone();
            let mut meta = InputMeta::from(&grant.metadata);
            let mut changed = rewrite_repository_annotations(&mut meta, &replacements, &target_name);
            changed |= rewrite_repository_set(&mut spec.selector.repositories, &replacements, &target_key);
            for scope in spec.landing_credentials.values_mut() {
                if let LandingCredentialScope::Branch { repository, .. } = scope {
                    if replacements.contains(repository) {
                        *repository = target_key.clone();
                        changed = true;
                    }
                }
            }
            if changed {
                grant_updates.push((meta, spec));
            }
        }
        match existing {
            Some(existing) if existing.spec != merged || InputMeta::from(&existing.metadata) != target_meta => {
                repositories.update(&target_meta, &existing.metadata.resource_version, &merged).await.map_err(|error| error.to_string())?;
            }
            None => {
                repositories.create(&target_meta, &merged).await.map_err(|error| error.to_string())?;
            }
            Some(_) => {}
        }
        for state in self.repos.write().await.values_mut() {
            if state.repository_key.as_ref().is_some_and(|key| replacements.contains(key)) {
                state.repository_key = Some(target_key.clone());
            }
        }
        for (meta, spec) in project_updates {
            projects.apply(&meta, &spec).await.map_err(|error| error.to_string())?;
        }
        for (meta, spec) in ensure_updates {
            ensures.apply(&meta, &spec).await.map_err(|error| error.to_string())?;
        }
        for (meta, spec) in template_updates {
            templates.apply(&meta, &spec).await.map_err(|error| error.to_string())?;
        }
        for (meta, spec) in grant_updates {
            grants.apply(&meta, &spec).await.map_err(|error| error.to_string())?;
        }
        let durable_checkouts =
            self.resource_backend.clone().using::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?;
        for source in old_local {
            let key = RepositoryKey(source.metadata.name.clone());
            if durable_checkouts.items.iter().any(|checkout| checkout.spec.repo_ref() == &key) {
                let mut meta = InputMeta::from(&source.metadata);
                meta.annotations.insert(SUPERSEDED_BY_ANNOTATION.to_string(), target_name.clone());
                repositories.update(&meta, &source.metadata.resource_version, &source.spec).await.map_err(|error| error.to_string())?;
            } else {
                flotilla_observation::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, namespace, &key)
                    .await
                    .map_err(|error| error.to_string())?;
                repositories.delete(&source.metadata.name).await.map_err(|error| error.to_string())?;
            }
        }
        Ok(merged)
    }

    async fn resolve_declared_repository(&self, observed: RepositorySpec) -> Result<RepositorySpec, String> {
        let flotilla_resources::RepositoryIdentity::Remote { canonical_remote } = observed.identity() else {
            return Ok(observed);
        };
        let namespace = self.provisioning_namespace().await;
        let matching_sources = self
            .resource_backend
            .clone()
            .including_replicas::<Repository>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|repository| repository.object)
            .filter(|repository| repository.spec.declares_remote(canonical_remote))
            .collect::<Vec<_>>();
        let mut matches_by_name = BTreeMap::<String, Vec<ResourceObject<Repository>>>::new();
        for repository in matching_sources {
            matches_by_name.entry(repository.metadata.name.clone()).or_default().push(repository);
        }
        let mut matches = Vec::with_capacity(matches_by_name.len());
        for sources in matches_by_name.into_values() {
            let declared_remotes = sources
                .iter()
                .filter(|repository| repository.spec.remotes().len() > 1)
                .map(|repository| repository.spec.remotes())
                .collect::<BTreeSet<_>>();
            if declared_remotes.len() > 1 {
                return Err(format!("remote `{canonical_remote}` has conflicting declarations for one Repository"));
            }
            matches.push(
                sources
                    .iter()
                    .find(|repository| repository.spec.remotes().len() > 1)
                    .unwrap_or_else(|| sources.first().expect("Repository source group cannot be empty"))
                    .clone(),
            );
        }
        let declared = matches.iter().filter(|repository| repository.spec.remotes().len() > 1).collect::<Vec<_>>();
        match declared.as_slice() {
            [repository] => return repository.spec.clone().update_remotes(canonical_remote),
            [_, _, ..] => return Err(format!("remote `{canonical_remote}` is declared by multiple Repositories")),
            [] => {}
        }
        match matches.as_slice() {
            [] => Ok(observed),
            [repository] => repository.spec.clone().update_remotes(canonical_remote),
            _ => Err(format!("remote `{canonical_remote}` is declared by multiple Repositories")),
        }
    }

    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        if let (Some(repository_url), Some(git_ref)) = (repository_url, git_ref) {
            if let Ok(spec) = RepositorySpec::remote(repository_url) {
                let path = flotilla_provider_runtime::probe::canonicalize(path).await?;
                let (spec, replaces_prior_repository) = self.configure_inspected_repository(&path, spec).await?;
                let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?.to_string();
                return Ok(RepositoryInspection {
                    spec,
                    checkout: flotilla_admission::repository_inspection::LocalCheckoutInspection {
                        path,
                        host_ref,
                        git_ref: git_ref.to_string(),
                        is_main: matches!(git_ref, "main" | "master" | "trunk"),
                    },
                    transport_url: Some(repository_url.to_string()),
                    replaces_prior_repository,
                });
            }
        }
        self.inspect_repository_path(path, repository_url).await
    }

    pub fn local_environment_bag(&self) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(&self.local_environment_id)
    }

    pub async fn fetch_issue_by_ref(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.issue_provider_for_source(&reference.source).await?.fetch_by_id(reference).await
    }

    /// Resolve a portable issue source to a provider capability installed on
    /// this host. Provider names and credentials remain local.
    pub async fn issue_provider_for_source(&self, source: &flotilla_protocol::IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        self.issue_query_port.provider_for_source(source).await
    }

    /// Resolve a curated query scope to external issue sources. Repository
    /// keys live in the daemon provisioning namespace; Project scopes carry
    /// their namespace explicitly.
    pub async fn resolve_issue_sources(
        &self,
        scope: &flotilla_protocol::QueryScope,
    ) -> Result<Vec<flotilla_protocol::IssueSource>, String> {
        Ok(self.resolve_issue_source_bindings(scope).await?.into_iter().map(|binding| binding.source).collect())
    }

    pub async fn resolve_issue_source_bindings(
        &self,
        scope: &flotilla_protocol::QueryScope,
    ) -> Result<Vec<flotilla_resources::ResolvedIssueSourceBinding>, String> {
        let project = self
            .resource_backend
            .clone()
            .definitions::<Project>(&scope.namespace)
            .get(&scope.name)
            .await
            .map_err(|error| format!("project {}/{}: {error}", scope.namespace, scope.name))?;
        match resolve_project_issue_sources(&self.resource_backend.including_replicas::<Repository>(&scope.namespace), &project.spec).await
        {
            IssueSourceResolution::Available { bindings } => Ok(bindings),
            IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { repository, message }) => {
                Err(format!("repository {repository}: {message}"))
            }
            IssueSourceResolution::Unavailable(IssueSourceUnavailable::InvalidBindings { message }) => Err(message),
            IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource) => {
                Err(format!("project {}/{} has no issue source", scope.namespace, scope.name))
            }
        }
    }

    /// Resolve a session or checkout's environment resource reference to its runner.
    pub fn command_runner_for_environment_ref(&self, env_ref: &str) -> Option<Arc<dyn CommandRunner>> {
        self.resolve_environment_ref(env_ref).map(|environment| environment.runner)
    }

    /// Resolve once when a caller needs both the runner and its registered identity.
    pub fn resolve_environment_ref(&self, env_ref: &str) -> Option<ResolvedEnvironment> {
        self.environment_manager.resolve_environment_ref(env_ref)
    }

    /// VCS operations for a private charter object cache do not require a
    /// discovered checkout or a worktree registration.
    pub fn local_charter_vcs(&self) -> Result<Arc<dyn flotilla_vcs::vcs::Vcs>, String> {
        let runner = self.local_command_runner().ok_or("local charter runner unavailable")?;
        Ok(Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(runner)))
    }

    pub async fn local_vcs_for_checkout(&self, checkout: &Path) -> Result<Arc<dyn flotilla_vcs::vcs::Vcs>, String> {
        self.vcs_for_checkout(&self.local_environment_id, checkout).await
    }

    pub fn environment_bag_for_environment(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(env_id)
    }

    pub fn environment_registry_for_environment(&self, env_id: &EnvironmentId) -> Option<Arc<ProviderRegistry>> {
        self.environment_manager.environment_registry(env_id)
    }

    /// Direct SSH environments explicitly opted into host-direct placement.
    /// Their environment key is derived from the remote host's stable ID.
    pub fn agentless_ssh_environments(&self) -> Vec<(EnvironmentId, flotilla_discovery::environment_manager::DirectEnvironmentState)> {
        self.environment_manager
            .managed_environments()
            .into_iter()
            .filter_map(|(id, managed)| match managed {
                flotilla_discovery::environment_manager::ManagedEnvironmentKind::Direct(state)
                    if state.host_id.as_ref().is_some_and(|host| id.as_str() == host_direct_environment_name(host.as_str())) =>
                {
                    Some((id, state))
                }
                _ => None,
            })
            .collect()
    }

    pub fn set_direct_environment_registry(&self, env_id: &EnvironmentId, registry: Arc<ProviderRegistry>) -> Result<(), String> {
        self.environment_manager.set_direct_environment_registry(env_id, registry)
    }

    pub fn environment_container_name(&self, env_id: &EnvironmentId) -> Option<String> {
        self.environment_manager.environment_container_name(env_id)
    }

    pub fn register_provisioned_environment(
        &self,
        env_id: EnvironmentId,
        handle: EnvironmentHandle,
        env_bag: EnvironmentBag,
        registry: Option<Arc<ProviderRegistry>>,
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

    pub fn agent_state_store(&self) -> &flotilla_agent::SharedAgentStateStore {
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
        *self.provisioning_namespace.write().expect("provisioning namespace lock poisoned") = namespace;
        self.checkout_namespace_changes.send_replace(());
    }

    pub async fn provisioning_namespace(&self) -> String {
        action_namespace::provisioning_namespace(&self.provisioning_namespace)
    }

    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity {
        action_events::ActionEvents { node_id: &self.node_id, sink: &self.event_sink }.start(command_id, description)
    }

    fn finish_context_free_command(
        &self,
        command_id: u64,
        repo_identity: flotilla_protocol::RepoIdentity,
        result: flotilla_protocol::CommandValue,
    ) {
        action_events::ActionEvents { node_id: &self.node_id, sink: &self.event_sink }.finish(command_id, repo_identity, result)
    }

    pub async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.clone()
    }

    /// Installs the registered ADR 0064 store during composition, before peer tasks start.
    /// Kind migrations supply their registry without coupling this layer to machine kinds.
    pub fn install_decided_store(&self, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Result<(), String> {
        if !service.with_store(|store| Ok(store.kinds().iter().next().is_some())).map_err(|error| error.to_string())? {
            return Err("decided store has no registered kinds".into());
        }
        let mut slot = self.decided_store.lock().map_err(|_| "decided-store lock poisoned".to_owned())?;
        if slot.is_some() {
            return Err("decided store is already open".into());
        }
        *slot = Some(service);
        Ok(())
    }

    /// Installs the engine's provisioning readings during composition (#3629 layer 4).
    pub fn install_lifecycle_reads(&self, reads: Arc<dyn flotilla_projections::ports::LifecycleReads>) -> Result<(), String> {
        self.lifecycle_reads.install(reads)
    }

    /// Refuse engine-enabled startup until its lifecycle read port is installed.
    pub fn require_lifecycle_reads_installed(&self) -> Result<(), String> {
        self.lifecycle_reads.require_installed()
    }

    /// The provisioning readings the views consult: the engine's once installed, none before.
    pub fn lifecycle_reads(&self) -> Arc<dyn flotilla_projections::ports::LifecycleReads> {
        self.lifecycle_reads.clone()
    }

    /// Installs the `[primitives]` Message path during composition (#3317),
    /// after its decided store. Absent, every Message stays in the old store.
    pub fn install_message_primitives(&self, primitives: Arc<crate::primitives::MessagePrimitives>) -> Result<(), String> {
        self.message_path.set(Arc::clone(&primitives)).map_err(|_| "the Message path is already installed".to_owned())?;
        let convoys: Arc<dyn flotilla_resources::convoy::engine::EngineConvoys> = primitives.convoy_engine().clone();
        self.convoy_engine_path.set(convoys).map_err(|_| "the Convoy path is already installed".to_owned())?;
        let engine: Arc<dyn flotilla_crew::ports::EngineMessages> = primitives;
        self.crew_message_path.set(engine).map_err(|_| "the crew's Message path is already installed".to_owned())
    }

    /// Installs the engine's answer to which seats a vessel serves (#3629 layer 8).
    pub fn install_vessel_seats(&self, seats: Arc<dyn flotilla_resources::vessel_seats::VesselSeats>) -> Result<(), String> {
        self.vessel_seats_path.set(seats).map_err(|_| "the vessel seats port is already installed".to_owned())
    }

    /// The `[primitives]` Convoy path: empty until the switch's primitives are installed.
    pub fn convoy_engine_path(&self) -> flotilla_resources::convoy::engine::EngineConvoyPath {
        Arc::clone(&self.convoy_engine_path)
    }

    /// The `[primitives]` Message path, when the switch is on.
    pub fn message_primitives(&self) -> Option<Arc<crate::primitives::MessagePrimitives>> {
        self.message_path.get().cloned()
    }

    /// Peer replication entry point; the router supplies the originating node from
    /// the trusted peer transport envelope.
    pub async fn next_change_stream_frame(&self, peer: &str, after: Option<u64>, session: Option<String>) -> Result<CommandValue, String> {
        self.change_stream_rounds
            .next(peer, self.decided_store()?, after, session, flotilla_schema::Timestamp::from_datetime(&self.clock.now()))
            .await
    }

    /// Present only after the first kind migration installs its registered store.
    pub fn decided_store(&self) -> Result<Arc<flotilla_schema_sqlite::stream::Service>, String> {
        self.decided_store
            .lock()
            .map_err(|_| "decided-store lock poisoned".to_owned())?
            .as_ref()
            .cloned()
            .ok_or_else(|| "decided store unavailable".to_owned())
    }

    pub fn resource_backend(&self) -> ResourceBackend {
        self.resource_backend.clone()
    }

    pub fn config_store(&self) -> Arc<ConfigStore> {
        Arc::clone(&self.config)
    }

    pub async fn subscribe_wait(
        &self,
        connection_id: uuid::Uuid,
        request: flotilla_protocol::WaitSubscriptionRequest,
    ) -> Result<uuid::Uuid, String> {
        self.crew.subscribe_wait(connection_id, request).await
    }

    pub async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.crew.unsubscribe_waits(connection_id).await;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = flotilla_resources::Convoy>> {
        self.crew.reconciler_wake_watch()
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.crew.change_request_stale_after()
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_address::Address) -> Result<(), String> {
        self.crew.refresh_change_request_hint(hint).await?;
        let Some(hint_text) = flotilla_relay_protocol::hint_subject(hint).filter(|_| hint.kind() == flotilla_address::Kind::ChangeRequest)
        else {
            return Ok(());
        };
        // Transitional (#3359): convoy subjects are still the protocol
        // `Subject`, so the hint goes back through its wire text to match them.
        let subject = flotilla_protocol::ReferenceContext::default().parse(&hint_text)?;
        let Some(hint_repository) = hint.parent() else { return Ok(()) };
        let namespace = self.provisioning_namespace().await;
        let forges = self
            .resource_backend
            .definitions::<Forge>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
        for convoy in convoys.list().await.map_err(|error| error.to_string())?.items {
            let matches_repository = convoy
                .spec
                .repositories
                .iter()
                .any(|repository| flotilla_resources::address::remote_names_repository(&repository.url, &forges, &hint_repository));
            if !matches_repository {
                continue;
            }
            if let Some(branch) = convoy.spec.r#ref.as_deref() {
                if let Err(error) = self.discover_convoy_branch_subjects(&namespace, &convoy.metadata.name, branch).await {
                    tracing::warn!(convoy = %convoy.metadata.name, %error, "relay hint branch discovery failed");
                }
            }
            let refreshed = convoys.get(&convoy.metadata.name).await.map_err(|error| error.to_string())?;
            let relationships = refreshed
                .spec
                .declared_subjects()?
                .into_iter()
                .filter(|entry| entry.subject == subject)
                .map(|entry| entry.relationship)
                .chain(
                    refreshed
                        .status
                        .iter()
                        .flat_map(|status| &status.subjects)
                        .filter(|entry| entry.subject == subject)
                        .map(|entry| entry.relationship),
                )
                .collect::<BTreeSet<_>>();
            if !relationships.is_empty() {
                apply_resource_status_patch(
                    &convoys,
                    &convoy.metadata.name,
                    &ConvoyStatusPatch::DiscoverSubjects {
                        subjects: relationships.into_iter().map(|relationship| (subject.clone(), relationship)).collect(),
                        source: flotilla_resources::SubjectDiscoverySource::Relay,
                        at: self.clock.now(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub async fn watch_change_request_convoy_demands(&self, namespace: &str) {
        self.crew.watch_change_request_convoy_demands(namespace).await;
    }

    pub async fn refresh_demanded_owned_change_requests(&self) -> Result<(), String> {
        self.crew.refresh_demanded_owned_change_requests().await
    }

    pub fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.crew.set_change_request_relay_healthy(healthy);
    }

    pub fn connect_surface(&self, surface_id: uuid::Uuid, declaration: SurfaceDeclaration) {
        self.regard_lifecycle.connect_surface(surface_id, declaration);
    }

    pub fn principal_for_surface(&self, surface_id: uuid::Uuid) -> Result<Option<PrincipalRef>, String> {
        self.regard_lifecycle.principal_for_surface(surface_id)
    }

    fn should_auto_attach(&self, requested: flotilla_protocol::ConvoyAutoAttach) -> bool {
        match requested {
            flotilla_protocol::ConvoyAutoAttach::Always => true,
            flotilla_protocol::ConvoyAutoAttach::Never => false,
            flotilla_protocol::ConvoyAutoAttach::Default => {
                self.config.load_config().convoy.auto_attach.unwrap_or_else(|| !self.regard_lifecycle.has_ambient_surface())
            }
        }
    }

    pub async fn disconnect_surface(&self, surface_id: uuid::Uuid) -> Result<(), String> {
        self.regard_lifecycle.disconnect_surface(surface_id).await
    }

    pub async fn observe_surface_focus(&self, surface_id: uuid::Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        self.regard_lifecycle.observe_focus(surface_id, targets).await
    }

    pub fn observed_resource_backend(&self) -> ResourceBackend {
        self.observed_resource_backend.clone()
    }

    /// Refresh durable integration observations for adopted checkouts, then
    /// restore their ephemeral query-facing projection.
    pub async fn reconcile_adopted_checkouts(&self, namespace: &str) -> Result<(), String> {
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        flotilla_observation::observed_resources::reconcile_adopted_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            namespace,
        )
        .await
        .map_err(|error| error.to_string())?;
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let forges = self.checkout_integration_forges(namespace).await?;
        // Read the Convoy inventory at most once per pass, and only once an
        // adopted Checkout needs it, rather than once per adopted Checkout.
        let convoys = tokio::sync::OnceCell::new();
        for checkout in checkouts.list().await.map_err(|error| error.to_string())?.items {
            if checkout.metadata.lifecycle_authority().map_err(|error| error.to_string())? != Some(LifecycleAuthority::Adopted) {
                continue;
            }
            if checkout_path(&checkout).is_none() {
                continue;
            }
            let convoy_ref = checkout.metadata.labels.get(CONVOY_LABEL).map(String::as_str);
            let source_root = checkout.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map(String::as_str);
            let convoy = convoys
                .get_or_try_init(|| async { self.resource_backend.including_replicas::<ResourceConvoy>(namespace).list().await })
                .await
                .map_err(|error| error.to_string())?
                .items
                .iter()
                .find_map(|source| {
                    let origin_matches = match (source_root, &source.provenance) {
                        (Some(expected), ResourceProvenance::Replica { origin_root, .. }) => origin_root.as_str() == expected,
                        (Some(_), ResourceProvenance::Local) => false,
                        (None, _) => true,
                    };
                    let association_matches = convoy_ref.map_or_else(
                        || {
                            flotilla_resources::expected_checkout_refs(&source.object)
                                .is_ok_and(|expected| expected.contains(&checkout.metadata.name))
                        },
                        |expected| source.object.metadata.name == expected,
                    );
                    (origin_matches && association_matches).then(|| source.object.clone())
                });
            let integration = self.inspect_resource_checkout_integration(&checkout, convoy.as_ref(), &forges).await?;
            apply_resource_status_patch(
                &checkouts,
                &checkout.metadata.name,
                &flotilla_resources::CheckoutStatusPatch::UpdateIntegration { integration: Box::new(integration) },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        flotilla_observation::observed_resources::reconcile_adopted_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            namespace,
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Returns the current connection status for a peer host.
    pub async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        self.host_registry.peer_connection_status(node_id).await
    }

    pub async fn connected_peer_node_ids(&self) -> Vec<NodeId> {
        let mut peers =
            self.host_registry.connected_peer_summaries().await.into_iter().map(|summary| summary.node.node_id).collect::<Vec<_>>();
        peers.sort();
        peers.dedup();
        peers
    }

    pub async fn set_configured_peers(&self, peers: Vec<NodeInfo>) {
        let remote_counts = HashMap::new();
        self.host_registry
            .set_configured_peers(peers, &remote_counts, &|e| {
                self.event_sink.emit(e);
            })
            .await;
    }

    pub async fn set_peer_host_identities(&self, identities: HashMap<EnvironmentId, flotilla_protocol::HostIdentity>) {
        let projection = self.host_registry.description_projection.lock().await;
        self.host_registry
            .sync_peer_identities(identities, &|event| {
                self.event_sink.emit(event);
            })
            .await;
        drop(projection);
        if let Err(error) = self.refresh_resource_host_summaries().await {
            warn!(%error, "refresh resource host descriptions failed");
        }
    }

    pub async fn publish_peer_identity(&self, identity: flotilla_protocol::HostIdentity) {
        let _projection = self.host_registry.description_projection.lock().await;
        self.host_registry
            .publish_peer_identity(identity, &|event| {
                self.event_sink.emit(event);
            })
            .await;
    }

    /// HostRegistry holds a presentation cache only: descriptions originate in
    /// the resource store, connectivity and routes originate in the transport.
    pub async fn refresh_resource_host_summaries(&self) -> Result<(), String> {
        self.refresh_resource_host_summaries_with_backend(&self.resource_backend).await
    }

    async fn refresh_resource_host_summaries_with_backend(&self, backend: &ResourceBackend) -> Result<(), String> {
        use flotilla_protocol::qualified_path::HostId;

        // Serialize list-and-project passes so a slower old read cannot replace a
        // newer description published by the watch or a concurrent query.
        let _projection = self.host_registry.description_projection.lock().await;
        let namespace = self.provisioning_namespace().await;
        let statuses = self.read_projections().with_backend(backend).host_statuses(&namespace).await?;
        let details = statuses
            .iter()
            .map(|(name, status)| {
                (
                    EnvironmentId::host(HostId::new(name)),
                    HostQueryDetails::builder()
                        .maybe_visible_environments(status.description.as_ref().map(|description| description.environments.clone()))
                        .maybe_blob_sync(status.blob_sync.clone())
                        .build(),
                )
            })
            .collect();
        let mut summaries: HashMap<EnvironmentId, HostSummary> = statuses
            .into_values()
            .filter_map(|status| status.host_summary())
            .map(|summary| (summary.environment_id.clone(), summary))
            .collect();
        // Embedded InProcessDaemon callers can query before runtime registers a
        // Host/heartbeat. Bootstrap only until resource-backed observations exist.
        if !summaries.contains_key(&self.local_host_identity().environment_id) {
            let local = self.refresh_local_host_summary().await;
            summaries.insert(local.environment_id.clone(), local);
        }
        self.host_registry
            .sync_resource_summaries(summaries, details, &|event| {
                self.event_sink.emit(event);
            })
            .await;
        Ok(())
    }

    /// Legacy presentation fixtures; production descriptions come from Host status.
    pub async fn set_peer_host_summaries(&self, summaries: HashMap<EnvironmentId, HostSummary>) {
        let remote_counts = HashMap::new();
        self.host_registry
            .set_peer_host_summaries(summaries, &remote_counts, &|e| {
                self.event_sink.emit(e);
            })
            .await;
    }

    pub async fn publish_peer_connection_status(&self, node: &NodeInfo, status: PeerConnectionState) {
        let remote_counts = HashMap::new();
        self.host_registry
            .publish_peer_connection_status(node, status, &remote_counts, &|e| {
                self.event_sink.emit(e);
            })
            .await;
    }

    pub async fn begin_peer_resource_replication(&self, peer: &NodeId) {
        self.fleet.begin_peer_resource_replication(peer).await;
    }

    pub async fn report_resource_replication_failure(&self, peer: &NodeId, kind: &str, message: &str) {
        self.fleet.report_resource_replication_failure(peer, kind, message).await;
    }

    pub async fn report_resource_replication_healthy(&self, peer: &NodeId, kind: &str) {
        self.fleet.report_resource_replication_healthy(peer, kind).await;
    }

    /// Legacy presentation fixtures; production descriptions come from Host status.
    pub async fn publish_peer_summary(&self, summary: HostSummary) {
        self.host_registry
            .publish_peer_summary(summary, &|e| {
                self.event_sink.emit(e);
            })
            .await;
    }

    pub async fn remote_placement_host(
        &self,
        namespace: &str,
        policy_name: Option<&str>,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        let Some(policy_name) = policy_name else {
            return Ok(None);
        };
        let policy = self
            .resource_backend
            .clone()
            .including_replicas::<PlacementPolicy>(namespace)
            .get(policy_name)
            .await
            .map(|source| source.object)
            .map_err(|error| format!("placement policy {policy_name}: {error}"))?;
        let target_host = placement_target_host(&self.resource_backend, namespace, &policy).await?;
        let actuator = placement_actuator_host_ref(&self.resource_backend, namespace, &target_host).await?;
        if self.canonical_local_host_id().as_ref() == Some(&actuator) {
            return Ok(None);
        }
        let host = authoritative_placement_host(&self.resource_backend, namespace, &target_host, &policy.metadata.name).await?;
        if let Some(reason) =
            unready_placement_refusal(&policy.metadata.name, &target_host.display_name, host.status.as_ref(), self.clock.now())
        {
            return Err(reason);
        }
        Ok(Some(flotilla_protocol::qualified_path::HostId::new(actuator.as_str())))
    }

    pub async fn convoy_start_placement_host(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        self.admission.start_placement_host(namespace, intent).await
    }

    pub async fn resolve_existing_convoy_target(
        &self,
        action: &flotilla_protocol::CommandAction,
    ) -> Result<Option<ExistingConvoyTarget>, String> {
        let (namespace, name) = match action {
            flotilla_protocol::CommandAction::ConvoyDelete { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyLink { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyUnlink { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyAbandon { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyResume { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { namespace, name }
            | flotilla_protocol::CommandAction::QueryExplainConvoy { namespace, name } => {
                (namespace.clone().unwrap_or(self.provisioning_namespace().await), name.as_str())
            }
            flotilla_protocol::CommandAction::CrewSupervise { namespace, convoy, .. } => {
                (namespace.clone().unwrap_or(self.provisioning_namespace().await), convoy.as_str())
            }
            flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, .. }
            | flotilla_protocol::CommandAction::ConvoyPromiseVerdict { convoy, .. } => {
                (self.provisioning_namespace().await, convoy.as_str())
            }
            flotilla_protocol::CommandAction::CrewPromise { context, .. }
            | flotilla_protocol::CommandAction::CrewComplete { context, .. }
            | CommandAction::CrewFail { context, .. }
            | flotilla_protocol::CommandAction::CrewStall { context, .. }
            | flotilla_protocol::CommandAction::CrewHandoff { context, .. }
            | flotilla_protocol::CommandAction::QueryMessageContacts { context }
            | flotilla_protocol::CommandAction::QueryCrewList { context } => {
                let namespace = context.namespace.clone().unwrap_or(self.provisioning_namespace().await);
                let name = context.convoy.as_deref().ok_or_else(|| "crew command was not resolved to a convoy".to_string())?;
                (namespace, name)
            }
            _ => return Ok(None),
        };

        let result_set = self.aggregator_projection_state().await.result_set().await;
        let Rows::Convoys { rows, .. } = result_set.rows else {
            return Ok(None);
        };
        let candidates = rows.into_iter().filter(|row| row.resource.namespace == namespace).collect::<Vec<_>>();
        let identities = candidates
            .iter()
            .map(|row| ConvoyAddressIdentity {
                record_name: &row.resource.name,
                role: row.address_role.as_deref(),
                project: row.project_ref.as_deref(),
                terminal: row.phase.is_terminal(),
            })
            .collect::<Vec<_>>();
        let selected = resolve_convoy_candidate_indices(&identities, name)?;
        let record_name = selected.first().map(|index| candidates[*index].resource.name.clone()).unwrap_or_else(|| name.to_string());
        let mut hosts = selected.into_iter().filter_map(|index| candidates[index].resource.host.clone()).collect::<Vec<_>>();
        hosts.sort();
        hosts.dedup();

        let home = match hosts.as_slice() {
            [] => return Ok(None),
            [host] if host == &self.host_name => {
                return Ok(Some(ExistingConvoyTarget {
                    home: host.clone(),
                    node_id: self.node_id.clone(),
                    namespace,
                    record_name,
                    last_seen_at: None,
                }));
            }
            [host] => host.clone(),
            _ => {
                let homes = hosts.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(format!("convoy {name} is present on multiple home hosts: {homes}"));
            }
        };

        let last_seen_at =
            self.resource_backend.including_replicas::<ResourceConvoy>(&namespace).get(&record_name).await.ok().and_then(|source| {
                match source.provenance {
                    flotilla_resources::ResourceProvenance::Replica { last_synced_at, .. } => Some(last_synced_at),
                    flotilla_resources::ResourceProvenance::Local => None,
                }
            });

        let node_id = self.host_registry.node_id_for_host_name(&home).await?.ok_or_else(|| {
            convoy_home_unreachable_message(&namespace, &record_name, &home, last_seen_at, "no routed node address found for host")
        })?;
        Ok(Some(ExistingConvoyTarget { home, node_id, namespace, record_name, last_seen_at }))
    }

    pub async fn has_authoritative_convoy(&self, namespace: &str, name: &str) -> Result<bool, String> {
        match self.resource_backend.clone().using::<ResourceConvoy>(namespace).get(name).await {
            Ok(_) => Ok(true),
            Err(ResourceError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
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
        }

        counts
    }

    /// Resolve a repo identity to the preferred local path for execution or overlay updates.
    pub async fn preferred_local_path_for_identity(&self, identity: &flotilla_protocol::RepoIdentity) -> Option<PathBuf> {
        self.repos.read().await.get(identity).map(|state| state.preferred_path().to_path_buf())
    }

    /// Resolve a tracked local or synthetic repo path to its stable repo identity.
    pub async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        let repo_path = canonical_or_original(repo_path);
        self.repos.read().await.values().find(|state| state.contains_path(&repo_path)).map(|state| state.identity().clone())
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        match self.inspect_repository_path(repo_path, None).await {
            Ok(inspection) => flotilla_executor::action_support::repository_event_identity(&inspection.spec, None),
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

    pub(crate) async fn resolve_repository_selector(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<Option<RepositoryKey>, String> {
        flotilla_admission::repository_addressing::resolve_repository_with_lifecycle_reads(
            &self.resource_backend,
            &self.observed_resource_backend,
            &self.provisioning_namespace().await,
            self.environment_manager.local_host_id().as_str(),
            selector,
            self.lifecycle_reads.as_ref(),
        )
        .await
    }

    async fn resolve_repo_selector(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<PathBuf, flotilla_protocol::DispatcherError> {
        let repository = self.repository_for_selector(selector).await?;
        let key = repository.spec.key();
        self.local_checkout_for_repository(&key).await?.ok_or_else(|| {
            flotilla_protocol::DispatcherError::new(
                flotilla_protocol::DispatcherErrorClass::Missing,
                format!("Repository {key} has no available checkout on this host"),
            )
        })
    }

    fn resolve_observation_root_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        let roots = self.config.load_observation_roots()?;
        match selector {
            flotilla_protocol::RepoSelector::Path(path) => {
                let physical = canonical_or_original(path);
                roots
                    .iter()
                    .find(|root| canonical_or_original(root.as_path()) == physical)
                    .map(|root| root.as_path().to_path_buf())
                    .ok_or_else(|| format!("repo not observed: {}", path.display()))
            }
            flotilla_protocol::RepoSelector::Query(query) => {
                crate::resolve::resolve_repo(query, roots.iter().map(|root| (root.as_path(), None))).map_err(|error| error.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => Err(format!("no observation root matches {identity}")),
            flotilla_protocol::RepoSelector::Repository(key) => Err(format!("no observation root matches Repository {key}")),
        }
    }

    async fn resolve_checkout_selector(
        &self,
        selector: &flotilla_protocol::CheckoutSelector,
        scope: &CheckoutResolutionScope,
    ) -> Result<(PathBuf, String), String> {
        let namespace = self.provisioning_namespace().await;
        let checkouts = flotilla_admission::repository_addressing::local_checkouts_with_lifecycle_reads(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
            self.lifecycle_reads.as_ref(),
        )
        .await?;
        let physical_selector_path = match selector {
            flotilla_protocol::CheckoutSelector::Path(path) => Some(canonical_or_original(path)),
            flotilla_protocol::CheckoutSelector::Query(_) => None,
        };
        let mut matches = Vec::new();
        for checkout in checkouts {
            let Some(path) = checkout_path(&checkout) else { continue };
            let branch = checkout.spec.branch();
            let matched = match selector {
                flotilla_protocol::CheckoutSelector::Path(_) => physical_selector_path.as_deref() == Some(Path::new(path)),
                flotilla_protocol::CheckoutSelector::Query(query) => branch == query || branch.contains(query) || path.contains(query),
            };
            if !matched {
                continue;
            }
            // These are local facts; remote targeting remains a router concern.
            if matches!(scope, CheckoutResolutionScope::RemoteAny)
                || matches!(scope, CheckoutResolutionScope::Host(host) if host != &self.host_name)
            {
                continue;
            }
            let root = self
                .local_checkout_for_repository(checkout.spec.repo_ref())
                .await?
                .ok_or_else(|| format!("Repository {} has no observed checkout", checkout.spec.repo_ref()))?;
            matches.push((root, branch.to_string()));
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
            CommandAction::Checkout { repo, .. } => self.resolve_repo_selector(repo).await.map_err(String::from),
            CommandAction::RemoveCheckout { checkout, .. } => {
                if let Some(selector) = command.context_repo.as_ref() {
                    self.resolve_repo_selector(selector).await.map_err(String::from)
                } else {
                    self.resolve_checkout_selector(checkout, &checkout_scope).await.map(|(repo, _)| repo)
                }
            }
            CommandAction::Refresh { repo: Some(selector) } => self.resolve_repo_selector(selector).await.map_err(String::from),
            CommandAction::FetchCheckoutStatus { .. } | CommandAction::ArchiveSession { .. } | CommandAction::GenerateBranchName { .. } => {
                let selector = command.context_repo.as_ref().ok_or_else(|| "command requires repo context".to_string())?;
                self.resolve_repo_selector(selector).await.map_err(String::from)
            }
            _ => Err("command does not resolve to a single repo".to_string()),
        }
    }

    async fn checkout_has_network_forge(&self, checkout: &ResourceObject<ResourceCheckout>) -> Result<bool, String> {
        let namespace = self.provisioning_namespace().await;
        let repository = self
            .resource_backend
            .including_replicas::<Repository>(&namespace)
            .get(&checkout.spec.repo_ref().to_string())
            .await
            .map_err(|error| error.to_string())?
            .object;
        Ok(!repository
            .spec
            .forge()
            .is_none_or(|forge| !forge.service_url.starts_with("https://") && !forge.service_url.starts_with("http://")))
    }

    /// Creation requires a fresh forge lookup: a cached absence must not
    /// authorize reusing a branch whose earlier request has since closed.
    #[tracing::instrument(name = "checkout_branch_validation", skip_all)]
    pub async fn validate_new_checkout_branch(
        &self,
        checkout: &ResourceObject<ResourceCheckout>,
    ) -> Result<Option<String>, flotilla_provider_runtime::ObservationError> {
        if !self.checkout_has_network_forge(checkout).await? {
            return Ok(None);
        }
        let continuing = if let Some(owner) = checkout.metadata.labels.get(CONVOY_LABEL) {
            match self.resource_backend.including_replicas::<ResourceConvoy>(&checkout.metadata.namespace).get(owner).await {
                Ok(owner) => owner.object.spec.continuation.as_ref() == Some(checkout.spec.repo_ref()),
                Err(ResourceError::NotFound { .. }) => false,
                Err(error) => return Err(error.to_string().into()),
            }
        } else {
            false
        };
        let (candidates, failures) =
            self.admission.repository_change_request_candidates(std::slice::from_ref(checkout.spec.repo_ref())).await;
        if let Some(error) = failures.into_iter().next() {
            return Err(error.into());
        }
        for (_, _, provider) in candidates {
            if let Some((id, request)) = provider.find_change_request_by_branch_for_admission(checkout.spec.branch()).await? {
                if continuing
                    && matches!(
                        request.status,
                        flotilla_protocol::ChangeRequestStatus::Open | flotilla_protocol::ChangeRequestStatus::Draft
                    )
                {
                    continue;
                }
                return Ok(Some(format!(
                    "checkout branch {} conflicts with {:?} change request #{}; choose a fresh branch name",
                    checkout.spec.branch(),
                    request.status,
                    id
                )));
            }
        }
        Ok(None)
    }

    /// Resolve a checkout's PR from live VCS facts rather than its provisioning ref.
    /// New automatic associations require an open request; terminal requests are
    /// retained only when this checkout already observed them while open or has
    /// merge evidence dating from its own lifetime.
    pub async fn resolve_live_checkout_change_request(
        &self,
        checkout: &ResourceObject<ResourceCheckout>,
        vcs: &dyn flotilla_vcs::vcs::Vcs,
        path: &Path,
    ) -> Result<Option<String>, String> {
        if !self.checkout_has_network_forge(checkout).await? {
            return Ok(None);
        }
        let branch = vcs.read_repository(path, flotilla_vcs::vcs::RepositoryRead::CurrentBranch).await?;
        let mut branches = vec![branch.trim().to_string()];
        if let Ok(upstream) = vcs.read_repository(path, flotilla_vcs::vcs::RepositoryRead::UpstreamOf("@{upstream}")).await {
            let remote = vcs.read_repository(path, flotilla_vcs::vcs::RepositoryRead::TrackedRemote(branch.trim())).await?;
            let upstream = upstream.trim();
            // Git uses remote "." for a local upstream; its full branch name has no remote prefix.
            let upstream_branch =
                if remote.trim() == "." { upstream } else { upstream.strip_prefix(&format!("{}/", remote.trim())).unwrap_or(upstream) };
            if !branches.iter().any(|candidate| candidate == upstream_branch) {
                branches.push(upstream_branch.to_string());
            }
        }
        for branch in branches.into_iter().filter(|branch| !branch.is_empty() && branch != "HEAD") {
            if let Some(request) = self.resolve_convoy_change_request(std::slice::from_ref(checkout.spec.repo_ref()), &branch, None).await?
            {
                let terminal = matches!(
                    request.status,
                    flotilla_protocol::ChangeRequestStatus::Merged | flotilla_protocol::ChangeRequestStatus::Closed
                );
                let prior = checkout.status.as_ref().map(|status| &status.integration);
                let previously_open = prior
                    .and_then(|integration| integration.change_request.as_ref())
                    .is_some_and(|observed| observed.id == request.id && observed.state != flotilla_resources::ChangeRequestState::Merged);
                let merged_during_checkout = prior
                    .and_then(|integration| integration.landed_evidence.as_ref())
                    .filter(|evidence| evidence.change_request_id == request.id)
                    .and_then(|evidence| evidence.merged_at.as_deref())
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .is_some_and(|at| at >= checkout.metadata.creation_timestamp);
                if !terminal || previously_open || merged_during_checkout {
                    return Ok(Some(request.id));
                }
            }
        }
        Ok(None)
    }

    /// Persist every branch-matching PR across the convoy's repositories.
    /// Successful lookups are written even when another repository lookup fails;
    /// the first error is returned after those writes.
    pub async fn discover_convoy_branch_subjects(&self, namespace: &str, convoy_name: &str, branch: &str) -> Result<(), String> {
        self.discover_convoy_branch_subjects_with_resolution(namespace, convoy_name, branch, None).await
    }

    pub async fn refresh_convoy_branch(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        binding: Option<&flotilla_resources::BoundChangeRequest>,
    ) -> flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh {
        self.admission.refresh_convoy_branch(repository_keys, branch, binding).await
    }

    pub async fn discover_convoy_branch_subjects_with_resolution(
        &self,
        namespace: &str,
        convoy_name: &str,
        branch: &str,
        resolution: Option<&flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh>,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = match convoys.get(convoy_name).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. })
                if self.resource_backend.including_replicas::<ResourceConvoy>(namespace).get(convoy_name).await.is_ok() =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        let checkout_sources =
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?;
        let checkouts = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items);
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let mut subjects = Vec::new();
        let mut errors = Vec::new();
        let adopted = convoy
            .spec
            .declared_subjects()?
            .into_iter()
            .filter(|entry| entry.relationship == flotilla_protocol::Relationship::Adopts)
            .map(|entry| entry.subject)
            .collect::<Vec<_>>();
        // Subjects keep the spelling they were recorded with; one request is
        // one subject whatever its spelling.
        let should_discover = |subject: &flotilla_protocol::Subject| {
            let same = |other: &flotilla_protocol::Subject| same_subject(other, subject, &forges);
            !adopted.iter().any(same)
                && !convoy.status.as_ref().is_some_and(|status| status.unlinked_subjects.iter().any(same) || status.produces_any(same))
        };
        let observed_subjects = observed_change_request_subjects(&convoy, &checkouts, &forges)?;
        for subject in &observed_subjects {
            if should_discover(subject) {
                subjects.push((subject.clone(), flotilla_protocol::Relationship::Produces));
            }
        }
        for repository in &convoy.spec.repositories {
            // Once a checkout exists, its owning environment resolves live
            // branch/upstream facts. The provisioning ref is no longer evidence
            // of the branch the crew is working on, even before it opens a PR.
            if checkouts.values().any(|checkout| checkout.spec.repo_ref() == &repository.repo_ref) {
                continue;
            }
            if let Some((_, result)) =
                resolution.and_then(|refresh| refresh.repositories.iter().find(|(key, _)| key == &repository.repo_ref))
            {
                match result {
                    Ok(Some(request))
                        if matches!(
                            request.status,
                            flotilla_protocol::ChangeRequestStatus::Open | flotilla_protocol::ChangeRequestStatus::Draft
                        ) =>
                    {
                        let subject = remote_change_request_subject(&repository.url, &request.id, &forges)?;
                        if should_discover(&subject) {
                            subjects.push((subject, flotilla_protocol::Relationship::Produces));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => errors.push(error.clone()),
                }
                continue;
            }
            match self.resolve_convoy_change_request(std::slice::from_ref(&repository.repo_ref), branch, None).await {
                Ok(Some(request))
                    if matches!(
                        request.status,
                        flotilla_protocol::ChangeRequestStatus::Open | flotilla_protocol::ChangeRequestStatus::Draft
                    ) =>
                {
                    let subject = remote_change_request_subject(&repository.url, &request.id, &forges)?;
                    if should_discover(&subject) {
                        subjects.push((subject, flotilla_protocol::Relationship::Produces));
                    }
                }
                Ok(_) => {}
                Err(error) => errors.push(error),
            }
        }
        if !subjects.is_empty() {
            apply_resource_status_patch(
                &convoys,
                convoy_name,
                &ConvoyStatusPatch::DiscoverSubjects {
                    subjects,
                    source: flotilla_resources::SubjectDiscoverySource::Branch,
                    at: self.clock.now(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        // A convoy whose status admission has not written yet still records
        // its scan; otherwise the next scan waits a whole refresh interval.
        if errors.is_empty()
            && convoy
                .status
                .as_ref()
                .is_none_or(|status| status.branch_subject_scan_at.is_none() || status.branch_subject_scan_error.is_some())
        {
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::RecordBranchSubjectScan { at: self.clock.now() })
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(error) = errors.into_iter().next() {
            if convoy.status.as_ref().and_then(|status| status.branch_subject_scan_error.as_deref()) != Some(error.as_str()) {
                apply_resource_status_patch(
                    &convoys,
                    convoy_name,
                    &ConvoyStatusPatch::RecordBranchSubjectScanFailure { error: error.clone() },
                )
                .await
                .map_err(|patch_error| patch_error.to_string())?;
            }
            return Err(error);
        }
        Ok(())
    }

    async fn convoy_reference_context(
        &self,
        namespace: &str,
        convoy: &ResourceObject<ResourceConvoy>,
    ) -> Result<flotilla_protocol::ReferenceContext, String> {
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let project = if let Some(project_ref) = &convoy.spec.project_ref {
            self.resource_backend.including_replicas::<Project>(namespace).get(project_ref).await.ok().map(|project| project.object)
        } else {
            None
        };
        let repositories =
            self.resource_backend.including_replicas::<Repository>(namespace).list().await.map_err(|error| error.to_string())?;
        let repository_specs =
            repositories.items.iter().map(|record| (record.object.spec.key(), &record.object.spec)).collect::<HashMap<_, _>>();
        Ok(flotilla_resources::convoy_reference_context(
            &convoy.spec.repositories,
            convoy.spec.project_ref.as_deref(),
            project.as_ref().map(|project| &project.spec),
            &forges,
            |key| repository_specs.get(key).copied(),
        ))
    }

    pub async fn link_convoy_subject(
        &self,
        namespace: &str,
        convoy_name: &str,
        reference: &str,
        relationship: Option<flotilla_protocol::Relationship>,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let context = self.convoy_reference_context(namespace, &convoy).await?;
        let subject = context.parse(reference)?;
        if let Some(relationship) = relationship {
            // Produced and adopted PRs participate in settlement. Reject a
            // mistyped repository instead of adding an unobservable terminal leaf.
            if subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && !context.repositories.iter().any(|repository| repository.source == subject.source)
            {
                return Err(format!("change request `{reference}` is outside this convoy's repositories"));
            }
            apply_resource_status_patch(
                &convoys,
                convoy_name,
                &ConvoyStatusPatch::DiscoverSubjects {
                    subjects: vec![(subject, relationship)],
                    source: flotilla_resources::SubjectDiscoverySource::Operator,
                    at: self.clock.now(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        } else {
            if convoy.spec.subjects.iter().any(|entry| entry.subject == subject) {
                return Err("declared convoy subjects cannot be unlinked; change the convoy spec".to_string());
            }
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::UnlinkSubject { subject })
                .await
                .map_err(|error| error.to_string())?;
            // Explicit unlink must release retention even for terminal convoys,
            // which the live-demand watch intentionally does not list. If cleanup
            // fails, the marker remains persisted; retrying this command is safe,
            // and the observer also retries cleanup independently.
            self.leaf_subscription_diagnostics.0.release_unlinked_change_request_claims(namespace).await?;
        }
        Ok(())
    }

    /// Resolve the first change request whose head matches a convoy branch
    /// across the convoy's snapshotted repositories.
    pub async fn resolve_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        self.admission.resolve_convoy_change_request(repository_keys, branch, change_request_id).await
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
    /// Emits `DaemonEvent::RepoTracked`.
    pub async fn add_virtual_repo(
        &self,
        identity: flotilla_protocol::RepoIdentity,
        repository_key: Option<RepositoryKey>,
        synthetic_path: PathBuf,
    ) -> Result<(), String> {
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        let existing_path = self.repos.read().await.get(&identity).map(|state| state.preferred_path().to_path_buf());
        if existing_path.is_some() {
            let key_became_available = if let Some(repository_key) = repository_key {
                {
                    let mut repos = self.repos.write().await;
                    let state = repos.get_mut(&identity).expect("existing repository presentation");
                    let changed = state.repository_key.as_ref() != Some(&repository_key);
                    state.repository_key = Some(repository_key);
                    changed
                }
            } else {
                false
            };
            drop(_reconciliation);
            if key_became_available {
                self.publish_repo_info_update(&identity).await;
            }
            return Ok(());
        }

        let model = RepoModel::new_virtual();

        let repo_info = RepoInfo {
            identity: identity.clone(),
            repository_key: repository_key.clone(),
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
                RepoState::new(
                    identity.clone(),
                    RepoRootState { path: synthetic_path.clone(), model, slug: None, unmet: Vec::new(), is_local: false },
                ),
            );
            order.push(identity.clone());
        }

        self.repos.write().await.get_mut(&identity).expect("inserted virtual presentation").repository_key = repository_key;

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        self.event_sink.emit(DaemonEvent::RepoTracked(Box::new(repo_info)));

        Ok(())
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
        self.event_sink.emit(event);
    }

    /// Publication port for background services on the daemon-wide event bus.
    pub fn event_sink(&self) -> Arc<dyn EventSink> {
        self.event_sink.clone()
    }
}

/// Non-trait methods that are called directly on the concrete `InProcessDaemon`
/// type by the daemon server peer-overlay code and by the `execute()` implementation.
fn repository_identity_display(spec: &RepositorySpec) -> String {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Forge { forge_ref, owner, repo_name } => format!("{forge_ref}/{owner}/{repo_name}"),
        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
        flotilla_resources::RepositoryIdentity::Local { .. } => "local".to_string(),
    }
}

fn local_repository_matches_checkout(
    spec: &RepositorySpec,
    checkout: &flotilla_admission::repository_inspection::LocalCheckoutInspection,
) -> bool {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Local { host_ref, git_common_dir } => {
            host_ref == &checkout.host_ref && Path::new(git_common_dir).parent() == Some(checkout.path.as_path())
        }
        flotilla_resources::RepositoryIdentity::Remote { .. } | flotilla_resources::RepositoryIdentity::Forge { .. } => false,
    }
}

impl InProcessDaemon {
    #[cfg(test)]
    async fn resolve_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        self.admission.resolve_workflow(namespace, project_ref, project, repositories, intent).await
    }

    #[cfg(test)]
    async fn resolve_capability_placement(
        &self,
        namespace: &str,
        project_ref: &str,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        self.admission.resolve_capability_placement(namespace, project_ref, repositories, workflow, needs, intent).await
    }

    async fn admit_convoy_start(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<(String, String, Option<flotilla_protocol::ConvoyNaming>, Vec<String>), String> {
        self.admission.admit_convoy_start(namespace, intent, dispatching_principal_ref).await
    }

    /// Teardown handle for composing the standing-convoy reconciler.
    pub fn convoy_teardown(&self) -> Arc<dyn flotilla_admission::ports::ConvoyTeardown> {
        Arc::new(CrewTeardown(Arc::clone(&self.crew)))
    }

    /// Install the controller before driving standing-convoy operations.
    /// Repeated runtime construction retains the transaction guard and retries.
    /// First install wins: subsequent reconcilers are dropped without replacing
    /// the installed controller, including when a different instance is passed.
    pub async fn install_convoy_ensure_reconciler(&self, reconciler: Arc<dyn ConvoyEnsureReconciler>) {
        self.convoy_ensure_reconciler.write().await.get_or_insert(reconciler);
    }
    async fn convoy_ensure_reconciler(&self) -> Result<Arc<dyn ConvoyEnsureReconciler>, String> {
        self.convoy_ensure_reconciler.read().await.clone().ok_or_else(|| "ConvoyEnsure controller is not installed".to_string())
    }
    pub async fn reconcile_convoy_ensures_once(&self, namespace: &str) -> Result<Vec<String>, String> {
        self.reconcile_convoy_ensures_once_with_backing_inspector(namespace, self).await
    }
    pub async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        self.convoy_ensure_reconciler()
            .await?
            .reconcile_convoy_ensures_once_with_backing_inspector(self, namespace, backing_inspector)
            .await
    }
    pub async fn reconcile_convoy_ensure_now(
        &self,
        namespace: &str,
        name: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<String, String> {
        self.convoy_ensure_reconciler().await?.reconcile_convoy_ensure_now(self, namespace, name, backing_inspector).await
    }
    pub async fn roll_convoy_ensure(&self, namespace: &str, name: &str) -> Result<String, String> {
        self.convoy_ensure_reconciler().await?.roll_convoy_ensure(self, namespace, name).await
    }
    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String> {
        self.convoy_ensure_reconciler().await?.reap_ensured_convoy(self, namespace, ensure_name, convoy_name, force).await
    }
    /// The daemon's injected wall clock. Placement judges Host heartbeat
    /// freshness against it, so the heartbeat stamps `heartbeat_at` from it too.
    pub fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }
    pub fn clock_for_scenarios(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    pub async fn prepare_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String> {
        self.admission.prepare_ensured_convoy(self, namespace, ensure).await
    }

    pub async fn reap_convoy_internal(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.crew.reap(namespace, name, force).await
    }

    pub fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        self.admission.admission_free_space_floor_bytes()
    }

    #[cfg(test)]
    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String> {
        self.admission.check_remote_placement_free_space_floor(namespace, placement).await
    }

    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        self.admission.emit_attach_regard(binding, surface_id).await
    }

    async fn run_convoy_start(
        &self,
        intent: flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: PrincipalRef,
    ) -> flotilla_protocol::CommandValue {
        let namespace = self.provisioning_namespace().await;
        let requested_namespace = intent.namespace.as_deref().unwrap_or(&namespace);
        if requested_namespace != namespace {
            return flotilla_protocol::CommandValue::Error {
                message: format!("namespace `{requested_namespace}` is not served by this daemon (configured namespace: `{namespace}`)"),
            };
        }
        let auto_attach = self.should_auto_attach(intent.auto_attach);
        match self.admit_convoy_start(&namespace, &intent, &dispatching_principal_ref).await {
            Ok((record_name, address, naming, mut warnings)) if auto_attach => {
                match self.wait_for_convoy_attach(&namespace, &record_name, &address).await {
                    Ok((resolved, notice)) => {
                        warnings.extend(notice);
                        flotilla_protocol::CommandValue::ConvoyStarted {
                            name: address,
                            warnings,
                            naming,
                            binding: resolved.as_ref().and_then(|resolved| resolved.binding.clone()),
                            attach_plan: resolved.map(|resolved| resolved.plan),
                        }
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                }
            }
            Ok((_record_name, address, naming, warnings)) => {
                flotilla_protocol::CommandValue::ConvoyStarted { name: address, naming, warnings, attach_plan: None, binding: None }
            }
            Err(message) => flotilla_protocol::CommandValue::Error { message },
        }
    }

    async fn supervise_convoy_start(&self, task: ConvoyStartTask) {
        let ConvoyStartTask { command_id, intent, key, dispatching_principal_ref } = task;
        let result = match AssertUnwindSafe(self.run_convoy_start(intent, dispatching_principal_ref)).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                warn!(command_id, "convoy start worker panicked");
                flotilla_protocol::CommandValue::Error { message: "convoy start worker panicked".to_string() }
            }
        };
        self.admission.clear_pending(&key).await;
        self.finish_context_free_command(command_id, empty_repo_identity(), result);
    }

    async fn wait_for_convoy_attach(
        &self,
        namespace: &str,
        name: &str,
        address: &str,
    ) -> Result<(Option<ResolvedAttach>, Option<String>), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let listed = convoys.list().await.map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        if let Some(message) = listed.items.iter().find(|convoy| convoy.metadata.name == name).and_then(convoy_start_failure) {
            return Err(message);
        }
        let mut watch = convoys
            .watch(WatchStart::resuming_from(&listed))
            .await
            .map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        let deadline = tokio::time::Instant::now() + CONVOY_ATTACH_WAIT;
        let mut retry = tokio::time::interval(Duration::from_millis(100));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = retry.tick() => {
                    if let Ok(resolved) = self.resolve_attach_command_internal(name).await {
                        return Ok((Some(resolved), None));
                    }
                }
                event = watch.next() => {
                    match event {
                        Some(Ok(WatchEvent::Added(convoy) | WatchEvent::Modified(convoy))) if convoy.metadata.name == name => {
                            if let Some(message) = convoy_start_failure(&convoy) {
                                return Err(message);
                            }
                        }
                        Some(Ok(WatchEvent::Deleted(convoy))) if convoy.metadata.name == name => {
                            return Err(format!("convoy {address} was deleted while waiting for a crew session"));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(format!("watch convoy {address} while waiting to attach: {error}")),
                        None => return Err(format!("convoy {address} status watch ended while waiting to attach")),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    // Read current state: a queued watch event must not hide a failure
                    // or deletion when the attach deadline fires at the same instant.
                    let convoy = convoys.get(name).await.map_err(|error| match error {
                        ResourceError::NotFound { .. } => format!("convoy {address} was deleted while waiting for a crew session"),
                        error => format!("read convoy {address} while waiting to attach: {error}"),
                    })?;
                    if let Some(message) = convoy_start_failure(&convoy) {
                        return Err(message);
                    }
                    if let Ok(resolved) = self.resolve_attach_command_internal(name).await {
                        return Ok((Some(resolved), None));
                    }
                    let stage = self.convoy_start_stage(namespace, &convoy).await?;
                    return Ok((None, Some(format!(
                        "convoy {address} is still starting: {stage}; attach later with `flotilla attach {address}`"
                    ))));
                }
            }
        }
    }

    async fn convoy_start_stage(&self, namespace: &str, convoy: &ResourceObject<ResourceConvoy>) -> Result<String, String> {
        let vessels = self
            .resource_backend
            .using::<flotilla_resources::Vessel>(namespace)
            .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.metadata.name.clone())]))
            .await
            .map_err(|error| format!("read convoy vessels while waiting to attach: {error}"))?;
        let mut stages = Vec::new();
        // Labels select the candidates; the spec confirms ownership before a
        // vessel's status can describe (or fail) this convoy's start.
        for vessel in vessels.items.iter().filter(|vessel| vessel.spec.convoy_ref == convoy.metadata.name) {
            let stage = match vessel.status.as_ref() {
                Some(status) => {
                    if status.phase == flotilla_resources::VesselPhase::Failed {
                        return Err(format!(
                            "convoy vessel {} failed while starting: {}",
                            vessel.spec.vessel_name,
                            status.message.as_deref().unwrap_or("vessel failed without a message")
                        ));
                    }
                    status.message.clone().unwrap_or_else(|| match status.phase {
                        flotilla_resources::VesselPhase::Ready => "waiting for its terminal session".into(),
                        flotilla_resources::VesselPhase::Pending => "waiting for placement and its environment".into(),
                        flotilla_resources::VesselPhase::Provisioning => "provisioning its environment and checkouts".into(),
                        flotilla_resources::VesselPhase::Interrupted => "interrupted; waiting for recovery".into(),
                        flotilla_resources::VesselPhase::Lost => "lost; waiting for recovery".into(),
                        flotilla_resources::VesselPhase::Failed => unreachable!("failed vessels return above"),
                    })
                }
                None => "waiting for its environment".into(),
            };
            stages.push(format!("vessel {}: {stage}", vessel.spec.vessel_name));
        }
        let phase = match convoy.status.as_ref().map(|status| status.phase).unwrap_or_default() {
            flotilla_resources::ConvoyPhase::Pending => "pending",
            flotilla_resources::ConvoyPhase::Active => "active",
            flotilla_resources::ConvoyPhase::Interrupted => "interrupted",
            flotilla_resources::ConvoyPhase::Landing => "landing",
            flotilla_resources::ConvoyPhase::Landed => "landed",
            flotilla_resources::ConvoyPhase::Failed => "failed",
            flotilla_resources::ConvoyPhase::Abandoned => "abandoned",
        };
        let detail = convoy.status.as_ref().and_then(|status| status.message.as_deref());
        if stages.is_empty() {
            stages.push("waiting for its vessel and environment".into());
        }
        Ok(format!("{phase}{}; {}", detail.map(|detail| format!(": {detail}")).unwrap_or_default(), stages.join("; ")))
    }

    fn project_service(&self) -> ProjectService<'_> {
        ProjectService {
            image_layer_refresh: Arc::clone(&self.image_layer_refresh),
            resource_backend: &self.resource_backend,
            observed_resource_backend: &self.observed_resource_backend,
            clock: &self.clock,
            namespace: &self.provisioning_namespace,
            repository_index: RepositoryIndex {
                lifecycle_reads: self.lifecycle_reads.as_ref(),
                backend: &self.resource_backend,
                observed: &self.observed_resource_backend,
                namespace: &self.provisioning_namespace,
                host: self.environment_manager.local_host_id().as_str(),
            },
            operations: self,
        }
    }

    pub async fn snapshot_project_repositories(
        &self,
        namespace: &str,
        project_ref: &str,
        selected: Option<&[RepositoryKey]>,
    ) -> Result<Vec<ConvoyRepositorySpec>, String> {
        self.project_service().snapshot_project_repositories(namespace, project_ref, selected).await
    }

    async fn project_register(&self, target: &str) -> Result<(String, usize), String> {
        self.project_service().project_register(target).await
    }

    pub async fn reconcile_bound_project_charters(&self) -> Result<(), String> {
        let charters = self.project_service().reconcile_bound_charters().await;
        // Project image layers ride the same periodic charter reconciliation.
        if let Err(error) = self.project_service().reconcile_project_image_layers().await {
            warn!(%error, "project image layer reconciliation failed");
        }
        // Cache retention is housekeeping and never fails the sweep.
        match ProjectOperations::repository_inspector(self).await {
            Ok(inspector) => {
                if let Err(error) = inspector.retire_unused_charter_caches().await {
                    warn!(%error, "charter cache retention failed");
                }
            }
            Err(error) => warn!(%error, "charter cache retention unavailable"),
        }
        charters
    }

    async fn project_refresh(&self, name: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String> {
        self.project_service().project_refresh(name).await
    }

    async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String> {
        self.project_service().project_add(target, explicit_name, explicit_display_name, remote).await
    }

    async fn reconcile_tracked_repository(
        &self,
        inspection: &flotilla_admission::repository_inspection::RepositoryInspection,
    ) -> Result<Option<RepositoryIdentityChange>, String> {
        let previous = self.observed_repository_key_for_path(&inspection.checkout.path).await?;
        self.reconcile_inspected_repository(inspection, previous.as_ref()).await
    }

    async fn reconcile_inspected_repository(
        &self,
        inspection: &RepositoryInspection,
        previous_key: Option<&RepositoryKey>,
    ) -> Result<Option<RepositoryIdentityChange>, String> {
        let namespace = self.provisioning_namespace().await;
        let repository_spec = &inspection.spec;
        let repository_key = repository_spec.key();
        ensure_repository_and_default_project_workflow(&self.resource_backend, &namespace, &repository_key, repository_spec).await?;
        self.reconcile_project_checkouts(&namespace, &repository_key, repository_spec, inspection.checkout.clone()).await?;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let stored = repositories.get(&repository_key.to_string()).await.map_err(|error| error.to_string())?;
        if stored.spec != *repository_spec {
            // This path is authoritative for per-repository config, so it may
            // intentionally clear provenance that identity-only observations
            // preserve in `ensure_repository`.
            repositories
                .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, repository_spec)
                .await
                .map_err(|error| error.to_string())?;
        }

        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        let repository_objects = repositories.list().await.map_err(|error| error.to_string())?.items;
        let repository_specs = repository_objects
            .iter()
            .map(|repository| (RepositoryKey(repository.metadata.name.clone()), repository.spec.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut superseded_keys = BTreeSet::new();
        let mut declared_alias_keys = BTreeSet::new();
        let previous_tracked_key = previous_key.filter(|previous| *previous != &repository_key).cloned();
        if let Some(previous) = previous_tracked_key.as_ref().filter(|_| !inspection.replaces_prior_repository) {
            superseded_keys.insert(previous.clone());
        }
        if !inspection.replaces_prior_repository {
            for (key, spec) in &repository_specs {
                let aliases_current_repository = spec.remotes().iter().any(|remote| repository_spec.declares_remote(remote));
                if key != &repository_key && (local_repository_matches_checkout(spec, &inspection.checkout) || aliases_current_repository) {
                    superseded_keys.insert(key.clone());
                    if aliases_current_repository {
                        declared_alias_keys.insert(key.clone());
                    }
                }
            }
            for checkout in self
                .observed_resource_backend
                .clone()
                .using::<ResourceCheckout>(&namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
            {
                if let ResourceCheckoutSpec::Observed(observed) = checkout.spec {
                    if Path::new(&observed.path) == inspection.checkout.path && observed.repo_ref != repository_key {
                        superseded_keys.insert(observed.repo_ref);
                    }
                }
            }
        } else if let Some(previous) = &previous_tracked_key {
            flotilla_observation::observed_resources::delete_observed_checkout_at_path(
                &self.observed_resource_backend,
                &namespace,
                previous,
                &inspection.checkout.path,
            )
            .await
            .map_err(|error| error.to_string())?;
        }

        let other_tracked_keys = flotilla_admission::repository_addressing::local_checkouts_with_lifecycle_reads(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
            self.lifecycle_reads.as_ref(),
        )
        .await?
        .into_iter()
        .filter(|checkout| match &checkout.spec {
            ResourceCheckoutSpec::Observed(spec) => Path::new(&spec.path) != inspection.checkout.path,
            _ => checkout.status.as_ref().and_then(|status| status.path.as_deref()) != inspection.checkout.path.to_str(),
        })
        .map(|checkout| checkout.spec.repo_ref().clone())
        .collect::<BTreeSet<_>>();
        let mut migratable_keys = superseded_keys.difference(&other_tracked_keys).cloned().collect::<BTreeSet<_>>();
        migratable_keys.extend(declared_alias_keys);

        let remaining_projects = projects.list().await.map_err(|error| error.to_string())?;
        let durable_checkouts =
            self.resource_backend.clone().using::<ResourceCheckout>(&namespace).list().await.map_err(|error| error.to_string())?.items;
        for old_key in &migratable_keys {
            let still_referenced =
                remaining_projects.iter().any(|project| project.spec.repositories.iter().any(|entry| &entry.repo == old_key));
            let has_durable_checkout = durable_checkouts.iter().any(|checkout| checkout.spec.repo_ref() == old_key);
            if !still_referenced && !has_durable_checkout {
                flotilla_observation::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, old_key)
                    .await
                    .map_err(|error| error.to_string())?;
                match repositories.delete(&old_key.to_string()).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            } else if !still_referenced && has_durable_checkout {
                if let Some(old_repository) = repository_objects.iter().find(|repository| repository.metadata.name == old_key.to_string()) {
                    let mut meta = InputMeta::from(&old_repository.metadata);
                    meta.annotations.insert(SUPERSEDED_BY_ANNOTATION.to_string(), repository_key.to_string());
                    match repositories.update(&meta, &old_repository.metadata.resource_version, &old_repository.spec).await {
                        Ok(_) | Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
        }

        let previous_spec = previous_tracked_key
            .as_ref()
            .and_then(|key| repository_specs.get(key))
            .or_else(|| superseded_keys.iter().find_map(|key| repository_specs.get(key)));
        let identity_change = previous_spec.map(|previous| RepositoryIdentityChange {
            previous_display: repository_identity_display(previous),
            current_display: repository_identity_display(repository_spec),
        });
        Ok(identity_change)
    }

    async fn reconcile_repository_config(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
    ) -> Result<(), String> {
        let repositories = self.resource_backend.clone().using::<Repository>(namespace);
        let stored =
            flotilla_store::ensure_repository(&repositories, repository_key, repository_spec).await.map_err(|error| error.to_string())?;
        if stored.spec != *repository_spec {
            // Unlike identity-only observations, the current per-repository
            // config is authoritative and may remove a previously set stance.
            repositories
                .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, repository_spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn reconcile_project_checkouts(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
        checkout: flotilla_admission::repository_inspection::LocalCheckoutInspection,
    ) -> Result<(), String> {
        self.project_service().reconcile_project_checkouts(namespace, repository_key, repository_spec, checkout).await
    }

    pub async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryIdentityChange>, String> {
        self.refresh_repository(repo, RepositoryRefreshFailurePolicy::BestEffort).await
    }

    /// Refresh a Repository and surface checkout inspection failures to the caller.
    ///
    /// Operator-triggered reconciliation uses this path so a successful response
    /// means the requested refresh actually ran. Periodic background refreshes use
    /// [`Self::refresh`] and retain their best-effort behavior.
    pub async fn refresh_strict(&self, repo: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryIdentityChange>, String> {
        self.refresh_repository(repo, RepositoryRefreshFailurePolicy::Strict).await
    }

    async fn refresh_repository(
        &self,
        repo: &flotilla_protocol::RepoSelector,
        failure_policy: RepositoryRefreshFailurePolicy,
    ) -> Result<Option<RepositoryIdentityChange>, String> {
        let repository = self.repository_for_selector(repo).await?;
        let key = repository.spec.key();
        let namespace = self.provisioning_namespace().await;
        let Some(path) = self.local_checkout_for_repository(&key).await? else {
            if let Err(error) = self.repository_providers(&repository).await {
                if failure_policy == RepositoryRefreshFailurePolicy::Strict {
                    return Err(error);
                }
                warn!(repository = %key, %error, "Repository capabilities unavailable during refresh");
            }
            return Ok(None);
        };
        let inspected = async {
            let mut inspection = self.repository_inspector().await?.inspect_path(&path, None).await?;
            inspection.spec = self.resolve_forge_identity(inspection.spec).await?;
            let (spec, replaces_prior_repository) = self.configure_repository_for_checkout(&path, inspection.spec, Some(&key)).await?;
            inspection.spec = spec;
            inspection.replaces_prior_repository = replaces_prior_repository;
            Ok::<_, String>(inspection)
        }
        .await;
        match inspected {
            Ok(inspection) => {
                let changed = inspection.key() != key;
                let result = if changed {
                    self.reconcile_inspected_repository(&inspection, Some(&key)).await?
                } else {
                    self.reconcile_repository_config(&namespace, &key, &inspection.spec).await?;
                    self.reconcile_project_checkouts(&namespace, &key, &inspection.spec, inspection.checkout.clone()).await?;
                    None
                };
                if changed {
                    for state in self.repos.write().await.values_mut().filter(|state| state.contains_path(&path)) {
                        state.repository_key = Some(inspection.key());
                    }
                }
                Ok(result)
            }
            Err(error) if failure_policy == RepositoryRefreshFailurePolicy::Strict => {
                Err(format!("inspect repository {} during refresh: {error}", path.display()))
            }
            Err(error) => {
                warn!(repo = %path.display(), %error, "repository identity is unavailable during refresh");
                Ok(None)
            }
        }
    }

    /// Resolve a path that might be a git worktree to the main repo root.
    ///
    /// Returns `(resolved_path, Some(original_path))` if normalization changed
    /// the path, or `(original_path, None)` if no change was needed.
    async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
        use flotilla_paths::path_context::ExecutionEnvironmentPath;
        use flotilla_vcs::providers::git::GitVcs;
        use flotilla_vcs::providers::VcsInspection;

        let vcs = GitVcs::new(self.discovery.runner.clone());
        let ee_path = ExecutionEnvironmentPath::new(path);
        match vcs.resolve_repo_root(&ee_path).await {
            Some(repo_root) => {
                let repo_root_raw = repo_root.into_path_buf();
                // Canonicalize to handle symlinks (e.g. /var -> /private/var on macOS).
                let canonical_root = flotilla_provider_runtime::probe::canonicalize(&repo_root_raw).await.unwrap_or(repo_root_raw);
                let canonical_path = flotilla_provider_runtime::probe::canonicalize(path).await.unwrap_or_else(|_| path.to_path_buf());
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
            None => (canonical_or_original(path), None),
        }
    }

    async fn publish_repo_info_update(&self, identity: &flotilla_protocol::RepoIdentity) {
        if let Ok(repo_infos) = self.list_repos().await {
            if let Some(info) = repo_infos.into_iter().find(|info| info.identity == *identity) {
                // RepoTracked also carries late identity enrichment: surfaces
                // treat an existing identity as an update.
                self.event_sink.emit(DaemonEvent::RepoTracked(Box::new(info)));
            }
        }
    }

    /// Add a repo to tracking and report path normalization or identity migration.
    ///
    /// If `path` is a git worktree, the main repo root is resolved via
    /// `git rev-parse --path-format=absolute --git-common-dir` and tracked
    /// instead. When the path resolves to a git repo root, the returned
    /// `tracked_path` is canonicalized and `resolved_from` is
    /// `Some(original_path)` when the repo root changes.
    pub async fn add_repo(&self, path: &Path) -> Result<AddRepoOutcome, String> {
        let (path, resolved_from) = self.normalize_repo_path(path).await;

        // Observation is host-local and intentionally precedes inspection: a
        // temporarily unavailable checkout remains adopted and is retried on
        // the next daemon start.
        self.config.add_observation_root(&ExecutionEnvironmentPath::new(&path))?;

        // Resolve fleet-agreed Repository intent before checkout provider
        // construction so provider preferences are identical on every host.
        let repository_inspection = self
            .inspect_repository_path(&path, None)
            .await
            .map_err(|error| format!("cannot adopt checkout {}: {error}", path.display()))?;
        self.config.set_checkout_config(&ExecutionEnvironmentPath::new(&path), repository_inspection.spec.vcs().clone());

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag: _, repo_bag: _, unmet } = discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.resource_backend,
            &self.provisioning_namespace().await,
            &self.local_environment_id,
            &self.local_environment_id,
            &path,
            Some(&repository_inspection.spec),
        )
        .await?;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let identity = flotilla_executor::action_support::repository_event_identity(&repository_inspection.spec, None);
        // Resolve the storage identity before publishing RepoTracked so a
        // surface can subscribe to issues{repository} immediately. The
        // background refresh also reconciles the Repository resource and
        // observed Checkouts.
        let repository_key = Some(repository_inspection.key());
        let identity_change = self.reconcile_tracked_repository(&repository_inspection).await?;
        if let Some(tracked_identity) = self.tracked_repo_identity_for_path(&path).await {
            if tracked_identity == identity {
                let key_became_available = {
                    let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                    if self.tracked_repo_identity_for_path(&path).await.as_ref() != Some(&identity) {
                        false
                    } else if let Some(repository_key) = repository_key.as_ref() {
                        let mut repos = self.repos.write().await;
                        let state = repos.get_mut(&identity).expect("observed presentation");
                        let changed = state.repository_key.as_ref() != Some(repository_key);
                        state.repository_key = Some(repository_key.clone());
                        changed
                    } else {
                        false
                    }
                };
                if key_became_available {
                    self.publish_repo_info_update(&identity).await;
                }
                if self.tracked_repo_identity_for_path(&path).await.as_ref() == Some(&identity) {
                    return Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change });
                }
            }
            if let Err(error) = self.remove_repo_presentation(&path, false).await {
                // Another add_repo call may have removed or migrated this path
                // after our identity lookup. Continue through the idempotent
                // insertion path unless it is still tracked elsewhere.
                if self.tracked_repo_identity_for_path(&path).await.is_some_and(|current| current != identity) {
                    return Err(error);
                }
            }
        }
        let slug = repo_slug.clone();
        let model = RepoModel::new_observation(registry, Some(self.local_environment_id.clone()));
        let root = RepoRootState { path: path.clone(), model, slug, unmet, is_local: true };

        let repo_info = RepoInfo {
            identity: identity.clone(),
            repository_key: repository_key.clone(),
            path: Some(path.clone()),
            name: repo_name(&path),
            labels: root.model.labels.clone(),
            provider_names: root
                .model
                .provider_names()
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        let mut added_new_identity = false;
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        let already_tracked = self.tracked_repo_identity_for_path(&path).await.is_some();
        if already_tracked {
            return Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change });
        }
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
                order.push(identity.clone());
                added_new_identity = true;
            }
            repos.get_mut(&identity).expect("inserted presentation").repository_key = repository_key.clone();
        }

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        info!(repo = %path.display(), "added repo");
        if added_new_identity {
            self.event_sink.emit(DaemonEvent::RepoTracked(Box::new(repo_info)));
        }

        Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change })
    }

    pub async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        self.remove_repo_presentation(path, true).await
    }

    // Identity migration replaces a presentation row after the resources have
    // reconciled. It must preserve those new facts and the observation root.
    async fn remove_repo_presentation(&self, path: &Path, stop_observing: bool) -> Result<(), String> {
        let path = canonical_or_original(path);
        let repo_identity = self.tracked_repo_identity_for_path(&path).await.unwrap_or_else(|| fallback_repo_identity(&path));
        let observed_reconciliation = self.observed_checkout_reconciliation.lock().await;
        let tracked = self.repos.read().await.get(&repo_identity).is_some_and(|state| state.contains_path(&path));
        // Persist first so both tracked repositories and observation roots
        // whose initial inspection failed remain removable and retryable.
        if stop_observing {
            self.config.remove_observation_root(&ExecutionEnvironmentPath::new(&path))?;
        }
        if !tracked {
            self.config.remove_checkout_config(&ExecutionEnvironmentPath::new(&path));
            return Ok(());
        }
        let repository_key = if stop_observing { self.observed_repository_key_for_path(&path).await? } else { None };
        let mut removed_identity = false;
        let removed_final_local_root;
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            let Some(state) = repos.get_mut(&repo_identity) else {
                return Err(format!("no observed checkout at {}", path.display()));
            };
            if !state.remove_root(&path) {
                return Err(format!("no observed checkout at {}", path.display()));
            }
            removed_final_local_root = state.local_paths().is_empty();
            if state.roots.is_empty() {
                repos.remove(&repo_identity);
                order.retain(|repo| repo != &repo_identity);
                removed_identity = true;
            }
        }

        if stop_observing && removed_final_local_root {
            let namespace = self.provisioning_namespace().await;
            if let Some(repository_key) = repository_key {
                if let Err(error) = flotilla_observation::observed_resources::delete_observed_checkouts(
                    &self.observed_resource_backend,
                    &namespace,
                    &repository_key,
                )
                .await
                {
                    warn!(repo = %repo_identity.path, %error, "failed to delete observed checkouts for untracked repo");
                }
            } else {
                warn!(repo = %repo_identity.path, "could not resolve repository identity while deleting observed checkouts");
            }
        }
        self.retire_checkout_providers().await?;
        drop(observed_reconciliation);

        if stop_observing {
            self.config.remove_checkout_config(&ExecutionEnvironmentPath::new(&path));
        }

        info!(repo = %path.display(), "removed repo");
        if removed_identity {
            self.event_sink.emit(DaemonEvent::RepoUntracked { repo_identity, path: Some(path) });
        }

        Ok(())
    }

    // --- Internal query helpers (formerly DaemonHandle trait methods) ---

    pub async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        self.repository_providers_response(repo).await
    }

    fn read_projections(&self) -> read_projections::ReadProjections<'_> {
        read_projections::ReadProjections {
            message_receivers: &self.message_receivers,
            backend: &self.resource_backend,
            config: &self.config,
            host_registry: &self.host_registry,
            environment_manager: self,
            host_name: &self.host_name,
            node_id: &self.node_id,
            lifecycles: self.lifecycle_reads.as_ref(),
            clock: &self.clock,
            leaf_subscriptions: &self.leaf_subscription_diagnostics,
            fleet: &self.fleet,
            convoy_engine: &self.convoy_engine_path,
        }
    }

    pub async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        self.refresh_resource_host_summaries().await?;
        self.read_projections().list_hosts(&self.local_host_counts().await).await
    }

    pub(crate) async fn provisioning_namespace_for_forge(&self) -> String {
        self.provisioning_namespace().await
    }

    pub async fn refresh_dispatch_boards_internal(&self) -> Result<(), String> {
        let daemon = self.self_weak.clone();
        tokio::spawn(async move {
            if let Some(daemon) = daemon.upgrade() {
                if let Err(error) = daemon.refresh_forge_read_demands().await {
                    tracing::debug!(%error, "forge demand refresh unavailable");
                }
            }
        });
        self.dispatch_board_repositories_internal(None).await.map(|_| ())
    }

    pub async fn dispatch_board_internal(&self, project_filter: Option<&str>) -> Result<flotilla_protocol::DispatchBoardResponse, String> {
        self.dispatch_board_projection().await.dispatch_board_internal(project_filter).await
    }
    pub async fn dispatch_board_repositories_internal(
        &self,
        project_filter: Option<&str>,
    ) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String> {
        self.dispatch_board_projection().await.dispatch_board_repositories_internal(project_filter).await
    }
    #[tracing::instrument(name = "dispatch_board", skip_all)]
    pub async fn collect_dispatch_board_inputs(&self, projects: &[ResourceObject<Project>]) -> Result<DispatchBoardInputs, String> {
        self.dispatch_board_projection().await.collect_dispatch_board_inputs(projects).await
    }
    async fn dispatch_board_projection(&self) -> flotilla_projections::dispatch_board::DispatchBoard<'_> {
        flotilla_projections::dispatch_board::DispatchBoard {
            backend: &self.resource_backend,
            cache: &self.dispatch_board_cache,
            resolver: self.self_weak.clone(),
            namespace: self.provisioning_namespace().await,
        }
    }
    pub async fn dispatch_queue_internal(&self, project_filter: Option<&str>) -> Result<DispatchQueueResponse, String> {
        let observed_at = Utc::now();
        read_projections::ReadProjections::dispatch_queue(
            &self.resource_backend,
            &self.provisioning_namespace().await,
            project_filter,
            observed_at,
        )
        .await
    }

    pub async fn fulfilment_list_internal(&self) -> Result<FulfilmentListResponse, String> {
        self.read_projections().fulfilment_list(&self.provisioning_namespace().await).await
    }

    pub fn forge_budget_counter_rows(&self) -> Vec<flotilla_protocol::ForgeBudgetRow> {
        self.forge_budgets.counter_rows(self.host_name().as_str())
    }

    pub fn forge_budget_rows(&self) -> Vec<flotilla_protocol::ForgeBudgetRow> {
        self.forge_budgets.rows(self.host_name().as_str())
    }

    /// Current advisory project source refusals for the host heartbeat.
    pub async fn project_image_layer_conditions(&self) -> Vec<flotilla_resources::HostCondition> {
        self.image_layer_refresh.lock().await.conditions()
    }

    pub async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String> {
        let now = Utc::now();
        let namespace = self.provisioning_namespace().await;
        let reads = self.resource_backend.clone();
        self.refresh_resource_host_summaries_with_backend(&reads).await?;
        let host_list = self.host_registry.list_hosts(&self.local_host_counts().await).await;
        let mut response = self
            .read_projections()
            .fleet_health_request(&reads, &namespace, host_list, self.local_host_id().map(|id| id.to_string()), now)
            .await?;
        response.forge_budgets.retain(|row| row.host != self.host_name().as_str());
        response.forge_budgets.extend(self.forge_budget_rows());
        Ok(response)
    }

    /// Raw bound fleet manifests at the current source head, for candidate-side
    /// validation. Returning raw text lets the candidate use its own decoder.
    pub async fn charter_input_inventory(&self, namespace: &str) -> Result<Vec<flotilla_charter::ops_entry::OperationalEntryFile>, String> {
        let roots = self.resource_backend.using::<ManifestRoot>(namespace).list().await.map_err(|error| error.to_string())?;
        let vcs = self.local_charter_vcs()?;
        let mut files = Vec::new();
        for root in roots.items {
            if root.spec.host != self.environment_manager.local_host_id().as_str() || root.metadata.name.starts_with("ops-") {
                continue;
            }
            let Some(source) = root.spec.binding else {
                continue;
            };
            let snapshot = flotilla_charter::charter_store::read_charter_source(&source, Path::new(&root.spec.path), Some(&*vcs)).await?;
            files.extend(
                snapshot
                    .files
                    .into_iter()
                    .filter(|(path, _)| {
                        Path::new(path).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| matches!(ext, "yaml" | "yml" | "json"))
                    })
                    .map(|(path, contents)| flotilla_charter::ops_entry::OperationalEntryFile {
                        path: format!("{}/{path}", root.metadata.name),
                        contents,
                    }),
            );
        }
        Ok(files)
    }

    /// Raw ops declarations for candidate-side pre-roll validation.
    pub async fn project_operational_entry_inventory(
        &self,
        namespace: &str,
    ) -> Result<flotilla_admission::repository_inspection::OperationalEntryInventory, String> {
        let projects = self.resource_backend.definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        if !projects.iter().any(|project| {
            project.spec.repositories.iter().any(|member| member.roles.contains(&flotilla_resources::ProjectRepositoryRole::Ops))
        }) {
            return Ok(Default::default());
        }
        let mut paths = BTreeMap::<RepositoryKey, Vec<PathBuf>>::new();
        for checkout in flotilla_admission::repository_addressing::local_checkouts_with_lifecycle_reads(
            &self.resource_backend,
            &self.observed_resource_backend,
            namespace,
            self.environment_manager.local_host_id().as_str(),
            self.lifecycle_reads.as_ref(),
        )
        .await?
        {
            let key = checkout.spec.repo_ref().clone();
            if let Some(path) = checkout_path(&checkout) {
                paths.entry(key).or_default().push(PathBuf::from(path));
            }
        }
        let inspector = self.repository_inspector().await?;
        inspect_operational_entries(&projects, &paths, &*inspector).await
    }

    pub async fn list_projects_internal(&self) -> Result<ProjectListResponse, String> {
        read_projections::ReadProjections::list_projects(&self.resource_backend, &self.provisioning_namespace().await, self.clock.now())
            .await
    }

    pub async fn list_cli_items_internal(&self, kind: CliListKind) -> Result<CliListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        let mut items = BTreeMap::new();
        match kind {
            CliListKind::Repo => {
                for source in self
                    .resource_backend
                    .including_replicas::<Repository>(&namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                {
                    let repository = source.object;
                    let reference = repository.metadata.name;
                    let name = repository.spec.leaf_slug();
                    items.insert(
                        (String::new(), reference.clone()),
                        CliListRow { repo: Some(name.clone()), reference, name, status: "declared".into(), provider: None },
                    );
                }
            }
            CliListKind::Checkout => {
                let durable = self
                    .resource_backend
                    .including_replicas::<ResourceCheckout>(&namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?;
                let observed =
                    self.observed_resource_backend.using::<ResourceCheckout>(&namespace).list().await.map_err(|error| error.to_string())?;
                let mut engine_held = BTreeSet::new();
                let durable = durable.items.into_iter().map(|source| {
                    let origin = match source.provenance {
                        ResourceProvenance::Local => None,
                        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.to_string()),
                    };
                    (source.object, origin)
                });
                for (checkout, origin) in durable.chain(observed.items.into_iter().map(|object| (object, None))) {
                    let observed = matches!(checkout.spec, ResourceCheckoutSpec::Observed(_));
                    if observed && engine_held.contains(&checkout.metadata.name) {
                        continue;
                    }
                    let reading = if observed {
                        None
                    } else {
                        self.lifecycle_reads.try_reading(
                            flotilla_resources::lifecycle_reads::ProvisioningKind::Checkout,
                            &checkout.metadata.name,
                            origin.as_deref(),
                        )?
                    };
                    if reading.is_some() {
                        engine_held.insert(checkout.metadata.name.clone());
                    }
                    let status = match reading {
                        Some(flotilla_protocol::LifecycleReading::Ready { .. }) => "ready".to_string(),
                        Some(flotilla_protocol::LifecycleReading::InProgress | flotilla_protocol::LifecycleReading::Retrying { .. }) => {
                            "pending".to_string()
                        }
                        Some(
                            flotilla_protocol::LifecycleReading::GaveUp { .. } | flotilla_protocol::LifecycleReading::Exhausted { .. },
                        ) => "failed".to_string(),
                        Some(_) => continue,
                        None => match checkout.status.as_ref().map(|status| status.phase) {
                            Some(ResourceCheckoutPhase::Gone) => continue,
                            Some(ResourceCheckoutPhase::Pending) => "pending",
                            Some(ResourceCheckoutPhase::Ready) => "ready",
                            Some(ResourceCheckoutPhase::Failed) => "failed",
                            None => "observed",
                        }
                        .to_string(),
                    };
                    let reference = checkout.metadata.name;
                    items.entry((String::new(), reference.clone())).or_insert(CliListRow {
                        repo: Some(checkout.spec.repo_ref().to_string()),
                        reference,
                        name: checkout.spec.branch().to_string(),
                        status,
                        provider: None,
                    });
                }
            }
            CliListKind::Cr => {
                for source in self
                    .resource_backend
                    .including_replicas::<ResourceChangeRequest>(&namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                {
                    let change_request = source.object;
                    let Some(status) = change_request.status else { continue };
                    let state = match status.state.value {
                        Some(ObservedChangeRequestState::Open) => "open",
                        Some(ObservedChangeRequestState::Draft) => "draft",
                        _ => continue,
                    };
                    let reference = change_request.metadata.name;
                    items.insert(
                        (String::new(), reference.clone()),
                        CliListRow {
                            repo: Some(change_request.spec.scope),
                            reference,
                            name: status.title.value.unwrap_or_else(|| format!("#{}", change_request.spec.number)),
                            status: state.into(),
                            provider: Some(change_request.spec.service),
                        },
                    );
                }
            }
            CliListKind::Agent => return self.list_provider_cli_items(kind).await,
        }

        Ok(CliListResponse { list_kind: kind, items: items.into_values().collect() })
    }

    async fn list_provider_cli_items(&self, kind: CliListKind) -> Result<CliListResponse, String> {
        let mut items = BTreeMap::new();
        let mut repositories =
            self.repos.read().await.values().map(|state| (state.identity().path.clone(), state.registry())).collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.0.cmp(&right.0));
        for (repo, registry) in repositories {
            if kind == CliListKind::Agent {
                let criteria = RepoCriteria { repo_slug: Some(repo.clone()) };
                for (descriptor, provider) in registry.cloud_agents.iter() {
                    let sessions = match provider.list_sessions(&criteria).await {
                        Ok(sessions) => sessions,
                        Err(error) => {
                            warn!(repo = %repo, provider = %descriptor.display_name, %error, "failed to list agent sessions");
                            continue;
                        }
                    };
                    for (reference, session) in sessions {
                        let status = match session.status {
                            flotilla_protocol::SessionStatus::Running => "running",
                            flotilla_protocol::SessionStatus::Idle => "idle",
                            flotilla_protocol::SessionStatus::Archived | flotilla_protocol::SessionStatus::Expired => continue,
                        };
                        let provider_name = descriptor.display_name.clone();
                        items.entry((provider_name.clone(), reference.clone())).or_insert(CliListRow {
                            repo: Some(repo.clone()),
                            reference,
                            name: session.title,
                            status: status.to_string(),
                            provider: Some(provider_name),
                        });
                    }
                }
            }
        }
        Ok(CliListResponse { list_kind: kind, items: items.into_values().collect() })
    }

    pub async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String> {
        self.refresh_resource_host_summaries().await?;
        let local_summary = self.host_registry.local_host_summary().await;
        self.read_projections().get_host_status(environment_id, &self.local_host_counts().await, &local_summary).await
    }

    pub async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String> {
        self.refresh_resource_host_summaries().await?;
        let local_summary = self.host_registry.local_host_summary().await;
        self.read_projections().get_host_providers(environment_id, &self.local_host_counts().await, &local_summary).await
    }

    pub async fn fleet_list_internal(&self) -> Result<FleetListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        self.read_projections().fleet_request(&namespace, None, None, None).await
    }

    async fn scoped_fleet_list(
        &self,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String> {
        if project.is_none() && crew_id.is_none() && convoy.is_none() {
            return self.fleet_list_internal().await;
        }
        self.read_projections().fleet_request(&self.provisioning_namespace().await, project, crew_id, convoy).await
    }

    pub async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        self.crew.resolve_crew_routing_context(requested).await
    }

    pub async fn mark_crew_completion_pending(
        &self,
        namespace: &str,
        session_name: &str,
        pending: CrewCompletionPending,
    ) -> Result<(), String> {
        self.crew.mark_crew_completion_pending(namespace, session_name, pending).await
    }

    pub async fn clear_crew_completion_pending(&self, namespace: &str, session_name: &str) -> Result<(), String> {
        self.crew.clear_crew_completion_pending(namespace, session_name).await
    }

    pub async fn pending_crew_completions(&self) -> Result<Vec<(String, CrewCompletionPending, CrewCommandContext)>, String> {
        self.crew.pending_crew_completions().await
    }

    pub async fn set_session_capability_source(&self, source: Arc<dyn flotilla_capability_contract::SessionCapabilitySource>) {
        *self.crew.capability_source().write().await = Some(source);
    }

    pub async fn observe_launch_capability_card(&self, namespace: &str, name: &str, card: &str) -> Result<(), String> {
        flotilla_crew::crew_capabilities::observe_launch_card(&self.resource_backend, namespace, name, card, self.crew.capability_cards())
            .await
    }

    pub async fn refresh_capability_cards(&self, namespace: &str) -> Result<(), String> {
        let source = self.crew.capability_source().read().await.clone().ok_or("session capability source unavailable")?;
        flotilla_crew::crew_capabilities::refresh_cards(&self.resource_backend, namespace, &*source, self.crew.capability_cards()).await
    }

    pub async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        self.crew.crew_list_internal(requested).await
    }

    pub async fn crew_complete_internal(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
    ) -> Result<flotilla_protocol::CommandValue, String> {
        self.crew_complete_with_disposition_internal(requested, message, None, None).await
    }

    pub async fn crew_complete_with_disposition_internal(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    ) -> Result<flotilla_protocol::CommandValue, String> {
        self.crew_complete_as_principal_internal(requested, message, disposition, decision_ledger_ref, false, None).await
    }

    pub async fn crew_complete_as_principal_internal(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        force: bool,
        principal: Option<PrincipalRef>,
    ) -> Result<flotilla_protocol::CommandValue, String> {
        self.crew.complete(requested, message, disposition, decision_ledger_ref, force, principal).await
    }

    pub async fn crew_fail_internal(
        &self,
        requested: &CrewCommandContext,
        message: String,
        force: bool,
        principal: Option<&PrincipalRef>,
    ) -> Result<(), String> {
        self.crew.fail(requested, message, force, principal).await
    }

    pub async fn crew_stall_internal(
        &self,
        requested: &CrewCommandContext,
        reason: flotilla_protocol::StallReason,
        proposed_disposition: Option<flotilla_protocol::StallProposedDisposition>,
        message: String,
    ) -> Result<(), String> {
        self.crew.stall(requested, reason, proposed_disposition, message).await
    }

    async fn checkout_integration_forges(&self, namespace: &str) -> Result<Vec<flotilla_resources::ForgeSpec>, String> {
        Ok(self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect())
    }

    #[tracing::instrument(name = "checkout_landing_evidence", skip_all)]
    async fn inspect_resource_checkout_integration(
        &self,
        checkout: &ResourceObject<ResourceCheckout>,
        convoy: Option<&ResourceObject<ResourceConvoy>>,
        forges: &[flotilla_resources::ForgeSpec],
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        let path = checkout_path(checkout).ok_or("checkout path unavailable")?;
        let runner = self.runner_for_resource_checkout(checkout).await?;
        let environment = checkout.spec.env_ref().unwrap_or(self.local_environment_id.as_str());
        let vcs = self.vcs_for_checkout(&EnvironmentId::new(environment), Path::new(path)).await?;
        Ok(if let Some(convoy) = convoy {
            let id = convoy_change_request_id_for_checkout(convoy, checkout, forges);
            let observed = self.replicated_checkout_change_request(convoy, checkout, id.as_deref()).await?;
            inspect_convoy_checkout_integration(
                &*runner,
                vcs.as_ref(),
                Path::new(path),
                &checkout.spec,
                convoy,
                id.as_deref(),
                ChangeRequestIntegrationInput {
                    status: observed.as_ref(),
                    landed_evidence: checkout.status.as_ref().and_then(|status| status.integration.landed_evidence.as_ref()),
                },
            )
            .await
        } else {
            inspect_checkout_integration(
                &*runner,
                vcs.as_ref(),
                Path::new(path),
                &checkout.spec,
                checkout.metadata.labels.get(flotilla_resources::CHANGE_REQUEST_ID_LABEL).map(String::as_str),
            )
            .await
        })
    }

    async fn replicated_checkout_change_request(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout: &ResourceObject<ResourceCheckout>,
        id: Option<&str>,
    ) -> Result<Option<flotilla_resources::ChangeRequestStatus>, String> {
        let Some(number) = id.and_then(|id| id.parse::<u64>().ok()) else { return Ok(None) };
        let Some(repository) = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()) else {
            return Ok(None);
        };
        let canonical = flotilla_resources::canonicalize_repo_url(&repository.url)?;
        let qualified = canonical.split_once("://").map(|(_, qualified)| qualified).unwrap_or(&canonical);
        let Some((service, scope)) = qualified.split_once('/') else { return Ok(None) };
        let records = self
            .resource_backend
            .query_declared::<ResourceChangeRequest>(
                &convoy.metadata.namespace,
                &flotilla_store::StoreQuery {
                    fields: BTreeMap::from([
                        ("spec.service".into(), service.into()),
                        ("spec.scope".into(), scope.into()),
                        ("spec.number".into(), number.to_string()),
                    ]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(flotilla_resources::pick_replica_read(
            records.items.into_iter().filter(|record| {
                record.object.spec.number == number && record.object.spec.service == service && record.object.spec.scope == scope
            }),
            |record| (&record.object, matches!(record.provenance, ResourceProvenance::Local)),
        )
        .and_then(|record| record.object.status))
    }

    async fn runner_for_resource_checkout(&self, _checkout: &ResourceObject<ResourceCheckout>) -> Result<Arc<dyn CommandRunner>, String> {
        self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())
    }

    pub async fn verify_convoy_teardown_gate(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.crew.teardown(namespace, name, force).await
    }

    pub async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[ResourceObject<ResourceCheckout>],
        force: bool,
    ) -> Result<(), String> {
        self.crew.verify_convoy_teardown_gate_for_checkouts(convoy, checkout_list, force).await
    }

    pub async fn abandon_convoy_internal(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        self.crew.abandon(namespace, name, reason, principal_ref).await
    }

    pub async fn abandon_convoy_internal_with_hook<F, Fut>(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
        before_update: F,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        self.crew.abandon_convoy_internal_with_hook(namespace, name, reason, principal_ref, before_update).await
    }

    pub async fn crew_handoff_internal(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        self.crew.handoff(requested, target, message).await
    }

    pub async fn convoy_resume_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
        intent: ResumeIntent,
    ) -> Result<ConvoyResumeOutcome, String> {
        if intent == ResumeIntent::Reminder {
            return self.crew.resume(namespace, name, prompt, requested_vessel, requested_role).await;
        }
        self.crew
            .convoy_resume_with_sender_internal(
                namespace,
                name,
                prompt,
                (requested_vessel, requested_role),
                MessageAttribution::operator(None),
                intent,
            )
            .await
    }

    pub async fn convoy_withdraw_pending_brief_internal(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        self.crew.withdraw_pending_brief(namespace, name).await
    }

    pub async fn reconcile_crew_stalls_once(&self, namespace: &str) -> Result<(), String> {
        self.crew.reconcile_crew_stalls_once(namespace).await
    }

    pub fn set_resource_intent_publisher(&self, publisher: Weak<dyn flotilla_crew::leaf_engine::ResourceIntentPublisher>) {
        self.crew.set_resource_intent_publisher(publisher);
    }

    /// Sends a Message intent as producers do: deduplicated by its name and
    /// handed to the receiver's home through the receiver-home router.
    pub async fn publish_message(
        &self,
        namespace: &str,
        name: &str,
        intent: &flotilla_resources::MessageSpec,
    ) -> Result<ResourceRef, String> {
        self.crew.publish_message_intent(namespace, name, intent).await
    }

    pub async fn deliver_standing_turn(&self, request: &flotilla_crew::leaf_engine::CrewTurnIntent) -> Result<TurnDeliveryRung, String> {
        self.crew.deliver_turn(request).await.map(|admission| admission.rung)
    }

    /// Recover previous-generation queue intent at startup and periodic resync.
    pub async fn adopt_legacy_convoy_queues_once(&self, namespace: &str) -> Result<(), String> {
        self.crew.adopt_legacy_convoy_queues_once(namespace, None).await
    }

    pub async fn reconcile_pending_supervisor_turns_once(&self, namespace: &str) -> Result<(), String> {
        self.crew.reconcile_pending_supervisor_turns_once(namespace).await
    }

    fn attach_resolver(&self) -> AttachResolver<'_> {
        AttachResolver {
            resource_backend: &self.resource_backend,
            observed_resource_backend: &self.observed_resource_backend,
            aggregator_projection_state: &self.aggregator_projection_state,
            config: &self.config,
            host_registry: &self.host_registry,
            environment_manager: &self.environment_manager,
            discovery: &self.discovery,
            local_environment_id: &self.local_environment_id,
            host_name: &self.host_name,
            namespace: &self.provisioning_namespace,
            lifecycle_reads: self.lifecycle_reads.as_ref(),
            agentless_vessels: self.agentless_vessels.read().expect("agentless vessels lock poisoned").clone(),
        }
    }

    pub async fn resolve_attach_command_internal(&self, reference: &str) -> Result<ResolvedAttach, String> {
        self.attach_resolver().resolve_attach(reference, None, false, AttachMode::PreferTake, None).await
    }

    pub async fn resolve_attach_command_on_host_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
    ) -> Result<ResolvedAttach, String> {
        self.attach_resolver().resolve_attach(reference, host, false, AttachMode::PreferTake, None).await
    }

    async fn resolve_attach_with_context(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        self.attach_resolver().resolve_attach(reference, host, transient, mode, project_context).await
    }

    pub async fn resolve_transient_attach_command_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
    ) -> Result<ResolvedAttach, String> {
        self.attach_resolver().resolve_transient(reference, host).await
    }

    pub async fn resolvable_attach_references_internal(&self, references: &[String]) -> Result<HashSet<String>, String> {
        self.attach_resolver().resolvable_references(references).await
    }

    pub async fn resolvable_attach_targets_internal(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        self.attach_resolver().resolvable_targets(targets).await
    }

    pub async fn origin_host_names_internal(&self, origins: &HashSet<NodeId>) -> HashMap<NodeId, HostName> {
        let mut hosts = HashMap::new();
        for origin in origins {
            if let Some(host) = self.host_registry.host_name_for_node(origin).await {
                hosts.insert(origin.clone(), host);
            }
        }
        hosts
    }

    /// The home root of an existing resource addressed by a mutation. New
    /// resources have no origin yet and are authored by the selected host.
    pub async fn resource_mutation_origin(&self, action: &flotilla_protocol::CommandAction) -> Result<Option<NodeId>, String> {
        use flotilla_protocol::CommandAction;

        if let CommandAction::ConvoyEnsureRoll { namespace, name } = action {
            let active = self.convoy_ensure_reconciler().await?.active_ensured_convoys(self, namespace, name).await?;
            if active.len() > 1 {
                return Err(format!("ConvoyEnsure/{name} has multiple running generations; resolve their ownership before rolling"));
            }
            if let Some(convoy) = active.into_iter().next() {
                return Ok(Some(match convoy.provenance {
                    ResourceProvenance::Local => self.node_id.clone(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root,
                }));
            }
        }
        let target = match action {
            CommandAction::MessageDeliverBatch { namespace, name, .. } | CommandAction::MessageFailBatch { namespace, name, .. } => {
                Some((namespace.as_str(), "Message", name.as_str()))
            }
            CommandAction::ConvoyEnsureRoll { namespace, name } => Some((namespace.as_str(), "ConvoyEnsure", name.as_str())),
            CommandAction::ResourceDelete { namespace, kind, name, replica_origin: None }
            | CommandAction::ResourceStatusPatch { namespace, kind, name, .. }
            | CommandAction::ResourceManifestResolve { namespace, kind, name, .. }
            | CommandAction::ResourceReconcileNow { namespace, kind, name } => Some((namespace.as_str(), kind.as_str(), name.as_str())),
            CommandAction::ArtifactReserveLedgerComment { namespace, name, .. } => Some((namespace.as_str(), "Artifact", name.as_str())),
            CommandAction::RepositoryRemoteRemove { namespace, name, .. } => Some((namespace.as_str(), "Repository", name.as_str())),
            CommandAction::ResourceApply { namespace, document } => match (
                document.get("kind").and_then(serde_json::Value::as_str),
                document.pointer("/metadata/name").and_then(serde_json::Value::as_str),
            ) {
                (Some(kind), Some(name)) => {
                    Some((document.pointer("/metadata/namespace").and_then(serde_json::Value::as_str).unwrap_or(namespace), kind, name))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((namespace, kind, name)) = target else { return Ok(None) };
        let object = match get_resource_kind_including_replicas(&self.resource_backend, namespace, kind, name).await {
            Ok(object) => object,
            Err(ResourceError::NotFound { .. }) => {
                if kind == "Message" {
                    if let CommandAction::ResourceApply { document, .. } = action {
                        return self.message_creation_origin(namespace, document).await;
                    }
                }
                return Ok(None);
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(object.value.pointer("/metadata/annotations/flotilla.work~1origin-root").and_then(serde_json::Value::as_str).map(NodeId::new))
    }

    /// A reply to a fleet role is routed by its subject (#3317): the reply to
    /// turn-rules about convoy X goes to the turn-rules handler at X's home.
    /// Its subject is the convoy it references, else the convoy whose crew
    /// the original was sent to; with neither, the original's home.
    async fn fleet_role_reply_origin(
        &self,
        primitives: &crate::primitives::MessagePrimitives,
        namespace: &str,
        reply: &flotilla_resources::MessageSpec,
        original_id: &str,
    ) -> Result<NodeId, String> {
        // The original was sent to the replying role: its inbox names it at its home on a replica miss.
        let roles = primitives.roles(namespace).await?;
        let records = primitives.records(namespace).await.map_err(|error| error.to_string())?;
        let conversion = crate::primitives::convert::Conversion { roles: &roles, forges: &records.forges, records: &*records };
        let original = primitives
            .read_view(namespace, original_id, conversion.inbox(&reply.sender).ok())
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("replied-to message `{original_id}` is neither in this home's replica nor at a reachable home"))?;
        if original.object.spec.sender != reply.receiver {
            return Err("reply system receiver does not match the original sender".into());
        }
        let referenced = reply.references.iter().chain(original.object.spec.references.iter()).find_map(|reference| match reference {
            flotilla_resources::MessageReference::ControlRecord { resource, .. } if resource.kind == "Convoy" => {
                Some(resource.name.clone())
            }
            _ => None,
        });
        let convoy = referenced.or_else(|| match original.object.spec.receiver.split('/').collect::<Vec<_>>().as_slice() {
            [_, convoy, _, _] => Some((*convoy).to_owned()),
            _ => None,
        });
        if let Some(convoy) = convoy {
            if let Ok(convoy) = self.resource_backend.including_replicas::<ResourceConvoy>(namespace).get(&convoy).await {
                return Ok(match convoy.provenance {
                    ResourceProvenance::Local => self.node_id.clone(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root,
                });
            }
        }
        Ok(match original.provenance {
            ResourceProvenance::Local => self.node_id.clone(),
            ResourceProvenance::Replica { origin_root, .. } => origin_root,
        })
    }

    async fn message_creation_origin(&self, namespace: &str, document: &serde_json::Value) -> Result<Option<NodeId>, String> {
        let namespace = document.pointer("/metadata/namespace").and_then(serde_json::Value::as_str).unwrap_or(namespace);
        let spec = flotilla_resources::qualify_message_spec(
            serde_json::from_value(document.get("spec").cloned().unwrap_or_default()).map_err(|error| format!("Message spec: {error}"))?,
        )
        .map_err(|error| error.to_string())?;
        let receiver = spec.receiver.as_str();
        if receiver.starts_with("system:") {
            let original_id = spec.in_reply_to.as_deref().ok_or("a system receiver requires a correlated reply")?;
            if let Some(primitives) = self.message_primitives() {
                return self.fleet_role_reply_origin(&primitives, namespace, &spec, original_id).await.map(Some);
            }
            let original = self
                .resource_backend
                .including_replicas::<flotilla_resources::Message>(namespace)
                .get(original_id)
                .await
                .map_err(|error| error.to_string())?;
            if original.object.spec.sender != receiver {
                return Err("reply system receiver does not match the original sender".into());
            }
            return Ok(Some(match original.provenance {
                ResourceProvenance::Local => self.node_id.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root,
            }));
        }
        let parts = receiver.split('/').collect::<Vec<_>>();
        let receiver_convoy = if let [project, convoy_name, vessel, _role] = parts.as_slice() {
            let convoy = self
                .resource_backend
                .including_replicas::<ResourceConvoy>(namespace)
                .get(convoy_name)
                .await
                .map_err(|error| format!("receiver `{receiver}` has no admitted convoy: {error}"))?;
            if convoy.object.spec.project_ref.as_deref().unwrap_or(namespace) != *project {
                return Err(format!("receiver `{receiver}` does not belong to the convoy's project"));
            }
            if let Some(status) = &convoy.object.status {
                if status.phase.is_terminal() {
                    return Err(format!("receiver `{receiver}` names a terminal convoy"));
                }
                if status
                    .workflow_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.vessels.iter().any(|declared| declared.name == *vessel))
                {
                    return Err(format!("receiver `{receiver}` names an undeclared vessel"));
                }
            }
            Some(convoy)
        } else {
            None
        };
        let holder = if receiver.starts_with("topic:") {
            flotilla_messaging::resolve_topic_receiver(&self.resource_backend, namespace, receiver, &spec.sender).await
        } else {
            flotilla_messaging::resolve_message_receiver(&self.resource_backend, namespace, receiver).await
        }
        .map_err(|error| error.to_string())?;
        if let Some(holder) = holder {
            return Ok(Some(match holder.provenance {
                ResourceProvenance::Local => self.node_id.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root,
            }));
        }
        // An admitted vessel has a home before its agent starts. This lets its
        // messages wait at the receiver while a holder is absent or provisioning.
        if let (Some(convoy), [_, _, vessel, _]) = (receiver_convoy, parts.as_slice()) {
            if let Some(pin) = flotilla_resources::vessel_placement_pin(&convoy.object, vessel) {
                let actuator = placement_actuator_host_ref(&self.resource_backend, namespace, &pin.decision.target_host).await?;
                if self.canonical_local_host_id().as_ref() == Some(&actuator) {
                    return Ok(Some(self.node_id.clone()));
                }
                let host = canonical_placement_host_ref(&self.resource_backend, namespace, actuator.as_str())
                    .await?
                    .ok_or_else(|| format!("receiver `{receiver}` has an unknown home host"))?;
                return self
                    .host_registry
                    .node_id_for_host_name(&HostName::new(host.display_name))
                    .await?
                    .map(Some)
                    .ok_or_else(|| format!("receiver `{receiver}` home host has no route"));
            }
            return Ok(Some(match convoy.provenance {
                ResourceProvenance::Local => self.node_id.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root,
            }));
        }
        if let [project, role] = parts.as_slice() {
            {
                let declarations = self
                    .resource_backend
                    .including_replicas::<flotilla_resources::ConvoyEnsure>(namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?;
                let mut homes = declarations
                    .items
                    .into_iter()
                    .filter(|declaration| declaration.object.spec.project_ref == *project && declaration.object.spec.role == *role);
                if let Some(home) = homes.next() {
                    if homes.next().is_some() {
                        return Err("project role has multiple holder declarations".into());
                    }
                    // Definitions reads merge sources and report Local even
                    // when the declaration exists only at a remote origin.
                    // Keep the merged spec for selection, but recover the home
                    // from the original stored declaration sources.
                    let home = if self.resource_backend.serves_read_views() {
                        home
                    } else {
                        let sources = self
                            .resource_backend
                            .including_replicas::<flotilla_resources::ConvoyEnsure>(namespace)
                            .list_replica_sources()
                            .await
                            .map_err(|error| error.to_string())?;
                        sources
                            .items
                            .into_iter()
                            .filter(|source| source.object.metadata.name == home.object.metadata.name)
                            .min_by_key(|source| {
                                let origin = match &source.provenance {
                                    ResourceProvenance::Local => self.node_id.clone(),
                                    ResourceProvenance::Replica { origin_root, .. } => origin_root.clone(),
                                };
                                (source.object.metadata.creation_timestamp, origin)
                            })
                            .ok_or_else(|| "project role declaration has no stored origin".to_string())?
                    };
                    return Ok(Some(match home.provenance {
                        ResourceProvenance::Local => self.node_id.clone(),
                        ResourceProvenance::Replica { origin_root, .. } => origin_root,
                    }));
                }
            }
        }
        Err(format!("receiver `{receiver}` has no declared home yet"))
    }

    pub async fn route_remote_attach_binding(&self, binding: &AttachBinding) -> Result<ResolvedAttachPlan, String> {
        self.attach_resolver().route_remote_attach_binding(binding).await
    }

    #[cfg(test)]
    async fn target_host_for_resource_ref(&self, namespace: &str, host_ref: &str) -> Result<HostName, String> {
        self.attach_resolver().target_host_for_resource_ref(namespace, host_ref).await
    }

    pub async fn canonical_host_id_internal(&self, namespace: &str, host_ref: &str) -> Result<CanonicalHostId, String> {
        canonical_placement_host_ref(&self.resource_backend, namespace, host_ref)
            .await?
            .map(|target| target.reference)
            .ok_or_else(|| format!("references unknown host `{host_ref}`"))
    }

    pub fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.local_host_id().map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    async fn refresh_local_host_summary(&self) -> HostSummary {
        let mut providers = self.local_provider_statuses.clone();
        for advertised in self.local_placement_provider_statuses.read().await.iter() {
            if !providers
                .iter()
                .any(|provider| provider.category == advertised.category && provider.implementation == advertised.implementation)
            {
                providers.push(advertised.clone());
            }
        }
        providers.sort_by(|left, right| (&left.category, &left.name).cmp(&(&right.category, &right.name)));
        let summary = flotilla_projections::host_summary::build_local_host_summary(
            &self.node_id,
            &self.host_name,
            EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            &projection_ports::ProjectionEnvironmentSummaries(&self.environment_manager),
            providers,
            &*self.discovery.env,
        )
        .await;
        summary
    }

    async fn get_issue_provider_for_repository(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<(Arc<dyn IssueProvider>, flotilla_protocol::IssueSource), String> {
        let key = self.resolve_repository_selector(selector).await?.ok_or_else(|| {
            format!("no Repository matches '{selector}'; adopt a checkout with `flotilla repo add <path>` or declare a Project member")
        })?;
        let namespace = self.provisioning_namespace().await;
        let repository = self
            .resource_backend
            .including_replicas::<Repository>(&namespace)
            .get(&key.to_string())
            .await
            .map_err(|error| error.to_string())?
            .object;
        let forge = repository.spec.issue_source_forge().ok_or_else(|| format!("Repository {key} has no forge issue source"))?;
        let source = flotilla_protocol::IssueSource { service: forge.service_url, scope: forge.repository };
        let provider = self.issue_provider_for_source(&source).await?;
        Ok((provider, source))
    }

    pub async fn execute_with_remote_executor(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
    ) -> Result<u64, String> {
        self.execute_impl(command, remote_executor, true, None).await.map_err(String::from)
    }

    pub async fn execute_for_principal(&self, command: Command, principal_ref: Option<PrincipalRef>) -> Result<u64, String> {
        let caller = principal_ref.map(|principal_ref| flotilla_protocol::CommandCaller { principal_ref, process: None, crew: None });
        self.execute_for_caller(command, caller).await
    }

    /// Cancel an active command, preserving the refusal class for dispatchers.
    pub async fn cancel_typed(&self, command_id: u64) -> Result<(), flotilla_protocol::DispatcherError> {
        let guard = self.active_commands.lock().await;
        match guard.get(&command_id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => {
                Err(flotilla_protocol::DispatcherError::new(flotilla_protocol::DispatcherErrorClass::Missing, "no matching active command"))
            }
        }
    }

    pub async fn execute_for_caller(&self, command: Command, caller: Option<flotilla_protocol::CommandCaller>) -> Result<u64, String> {
        self.execute_for_caller_typed(command, caller).await.map_err(String::from)
    }

    pub async fn execute_for_caller_typed(
        &self,
        command: Command,
        caller: Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, flotilla_protocol::DispatcherError> {
        self.execute_impl(command, Arc::new(flotilla_executor::step::UnsupportedRemoteStepExecutor), false, caller).await
    }

    async fn executor_provider_data(&self, repo_identity: &RepoIdentity, _repo_root: &Path, registry: &ProviderRegistry) -> ProviderData {
        let mut providers = ProviderData::default();

        if let Ok(vcs) = self.local_vcs_for_checkout(_repo_root).await {
            match vcs.list_checkouts().await {
                Ok(checkouts) => {
                    for (path, mut checkout) in checkouts {
                        checkout.host_name.get_or_insert_with(|| self.host_name.clone());
                        providers
                            .checkouts
                            .insert(QualifiedPath::host(self.environment_manager.local_host_id().clone(), path.into_path_buf()), checkout);
                    }
                }
                Err(error) => warn!(repo = %repo_identity, %error, "failed to read checkouts for command execution"),
            }
        }

        let criteria = RepoCriteria { repo_slug: Some(repo_identity.path.clone()) };
        for (descriptor, agent) in registry.cloud_agents.iter() {
            match agent.list_sessions(&criteria).await {
                Ok(sessions) => providers.sessions.extend(sessions),
                Err(error) => {
                    warn!(repo = %repo_identity, provider = %descriptor.display_name, %error, "failed to read sessions for command execution")
                }
            }
        }

        providers
    }

    pub async fn execute_remote_step_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
        cancel: CancellationToken,
    ) -> Result<Vec<StepOutcome>, String> {
        let repository = self.repository_for_selector(&flotilla_protocol::RepoSelector::Identity(request.repo_identity.clone())).await?;
        let local_repo_path = self
            .local_checkout_for_repository(&repository.spec.key())
            .await?
            .ok_or_else(|| format!("Repository {} has no observed checkout on this host", repository.spec.key()))?;
        let registry = self.execution_registry(&repository, &local_repo_path).await?;
        let providers_data = Arc::new(self.executor_provider_data(&request.repo_identity, &local_repo_path, &registry).await);

        let config_base = DaemonHostPath::new(self.config.base_path().as_path());
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
            daemon_socket_path,

            local_host: self.host_name.clone(),
            environment_manager: Arc::clone(&self.environment_manager) as Arc<dyn EnvironmentLifecycle>,
            vcs_resolver: self.self_weak.upgrade().ok_or("VCS resolver daemon unavailable")?
                as Arc<dyn flotilla_vcs::vcs::CheckoutVcsResolver>,
        };

        execute_local_remote_step_batch(self.node_id.clone(), request, progress_sink, cancel, &resolver).await
    }

    pub async fn message_inbox(&self, namespace: &str) -> flotilla_messaging::MessageInbox {
        self.crew.message_inbox(namespace).await
    }

    /// Returns the canonical admitted record. Suppression before creation does
    /// not create the successor ID. A recovered partial creation stays as a
    /// superseded audit record and durably forwards admission to its predecessor.
    async fn apply_intent_document(
        &self,
        namespace: &str,
        document: serde_json::Value,
    ) -> Result<flotilla_store::DynamicResourceObject, ResourceError> {
        use flotilla_messaging::MessageAdmission;
        use flotilla_resources::MessageSpec;
        if document.get("kind").and_then(serde_json::Value::as_str) != Some("Message") {
            return apply_resource_document(&self.resource_backend, namespace, document).await;
        }
        flotilla_store::validate_resource_document(&document)?;
        let namespace = document.pointer("/metadata/namespace").and_then(serde_json::Value::as_str).unwrap_or(namespace);
        let meta: InputMeta = serde_json::from_value(document.get("metadata").cloned().unwrap_or_default())
            .map_err(|error| ResourceError::decode(format!("message metadata: {error}")))?;
        let spec: MessageSpec = serde_json::from_value(document.get("spec").cloned().unwrap_or_default())
            .map_err(|error| ResourceError::decode(format!("message spec: {error}")))?;
        let spec = flotilla_resources::qualify_message_spec(spec)?;
        if let Some(primitives) = self.message_primitives() {
            let object = primitives.admit(namespace, &meta.name, &spec).await?;
            return Ok(flotilla_store::DynamicResourceObject {
                kind: "Message".into(),
                plural: "messages".into(),
                namespace: namespace.to_owned(),
                value: serde_json::to_value(serde_json::json!({
                    "apiVersion": "flotilla.work/v1",
                    "kind": "Message",
                    "metadata": object.metadata,
                    "spec": object.spec,
                    "status": object.status,
                }))
                .map_err(|error| ResourceError::decode(error.to_string()))?,
            });
        }
        let admission = self.message_inbox(namespace).await.accept(&meta, &spec, self.clock.now()).await?;
        let record = match admission {
            MessageAdmission::Accepted(record) => record,
            MessageAdmission::Suppressed { predecessor } => predecessor,
        };
        get_resource_kind(&self.resource_backend, namespace, "Message", &record.metadata.name).await
    }

    // This executor has many async arms. Box substantial nested futures below
    // so their combined state fits on the default Tokio test-thread stack.
    async fn execute_impl(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
        allow_remote_host: bool,
        caller: Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, flotilla_protocol::DispatcherError> {
        let dispatching_principal_ref = caller.as_ref().map(|caller| caller.principal_ref.clone());
        let command_node_id = command.node_id.clone().unwrap_or_else(|| self.node_id.clone());
        debug!(
            %command_node_id, local_node = %self.node_id, %allow_remote_host,
            desc = %command.description(), "execute_impl"
        );
        if !allow_remote_host && command_node_id != self.node_id {
            return Err(format!("remote command routing not implemented yet for node {command_node_id}").into());
        }

        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        if command.action.is_query() {
            // Query commands should be dispatched through `execute_query`,
            // not through `execute`. Return an error to surface misrouting.
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = CommandValue::Error { message: "query commands should use execute_query, not execute".into() };
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let Some(origin) = Box::pin(self.resource_mutation_origin(&command.action)).await? {
            if origin != self.node_id {
                let empty_identity = self.start_context_free_command(id, command.description().to_string());
                let host =
                    self.host_registry.host_name_for_node(&origin).await.map(|name| name.to_string()).unwrap_or_else(|| origin.to_string());
                let result = CommandValue::Error {
                    message: format!(
                        "resource is a replica on {}; its origin is {host} ({origin}). \
                         Retry when the origin is reachable",
                        self.host_name
                    ),
                };
                self.finish_context_free_command(id, empty_identity, result);
                return Ok(id);
            }
        }

        // The inner box moves each large helper future off this poll frame.
        // Boxing only the helper call still builds that future on the caller stack.
        macro_rules! boxed_action {
            (typed $future:expr) => {
                Box::pin(async { Box::pin($future).await }).await
            };
            ($future:expr) => {
                boxed_action!(typed $future).map_err(flotilla_protocol::DispatcherError::from)
            };
        }

        let admission = AdmissionActions {
            namespace: &self.provisioning_namespace,
            port: self,
            resource_backend: &self.resource_backend,
            observed_resource_backend: &self.observed_resource_backend,
            observed_checkout_reconciliation: &self.observed_checkout_reconciliation,
            admission: &self.admission,
            events: action_events::ActionEvents { node_id: &self.node_id, sink: &self.event_sink },
        };
        let crew = crew_actions::CrewActions {
            crew: &self.crew,
            port: self,
            resource_backend: &self.resource_backend,
            clock: &self.clock,
            events: action_events::ActionEvents { node_id: &self.node_id, sink: &self.event_sink },
        };
        let projections = self.projections_actions();
        let executor = self.executor_actions();
        match &command.action {
            CommandAction::ArtifactReserveLedgerComment { .. } => {
                return boxed_action!(crew.execute_action_artifact_reserve_ledger_comment(id, &command))
            }
            CommandAction::ResourceApply { .. } => return boxed_action!(executor.execute_action_resource_apply(id, &command)),
            CommandAction::RepositoryRemoteRemove { .. } => {
                return boxed_action!(executor.execute_action_repository_remote_remove(id, &command))
            }
            CommandAction::ResourceManifestResolve { .. } => {
                return boxed_action!(executor.execute_action_resource_manifest_resolve(id, &command))
            }
            CommandAction::ConvoyEnsureRoll { .. } => return boxed_action!(admission.execute_action_ensure_roll(id, &command)),
            CommandAction::ResourceReconcileNow { .. } => {
                return boxed_action!(executor.execute_action_resource_reconcile_now(id, &command))
            }
            CommandAction::MessageDeliverBatch { .. } => return boxed_action!(crew.execute_action_message_deliver_batch(id, &command)),
            CommandAction::MessageFailBatch { .. } => return boxed_action!(crew.execute_action_message_fail_batch(id, &command)),
            CommandAction::FleetPostInstall { .. } => return boxed_action!(executor.execute_action_fleet_post_install(id, &command)),
            CommandAction::ResourceStatusPatch { .. } => return boxed_action!(executor.execute_action_resource_status_patch(id, &command)),
            CommandAction::ResourceDelete { .. } => return boxed_action!(executor.execute_action_resource_delete(id, &command)),
            CommandAction::ResourceWatch { .. } => {
                return boxed_action!(projections.execute_action_resource_watch(id, &command, &command_node_id))
            }
            CommandAction::Refresh { repo: None } => return boxed_action!(executor.execute_action_refresh_all(id, &command)),
            CommandAction::CrewHandoff { .. } => return boxed_action!(crew.execute_action_crew_handoff(id, &command)),
            CommandAction::ConvoyResume { .. } => {
                return boxed_action!(crew.execute_action_convoy_resume(id, &command, &caller, &dispatching_principal_ref))
            }
            CommandAction::ConvoyWithdrawPendingBrief { .. } => {
                return boxed_action!(crew.execute_action_convoy_withdraw_pending_brief(id, &command))
            }
            CommandAction::CrewPromise { .. } => return boxed_action!(crew.execute_action_crew_promise(id, &command)),
            CommandAction::CrewComplete { .. } => {
                return boxed_action!(crew.execute_action_crew_complete(id, &command, &caller, &dispatching_principal_ref))
            }
            CommandAction::CrewFail { .. } => return boxed_action!(crew.execute_action_crew_fail(id, &command, &caller)),
            CommandAction::CrewStall { .. } => return boxed_action!(crew.execute_action_crew_stall(id, &command)),
            CommandAction::CrewSupervise { .. } => {
                return boxed_action!(crew.execute_action_crew_supervise(id, &command, &caller, &dispatching_principal_ref))
            }
            CommandAction::ConvoyLink { .. } => return boxed_action!(crew.execute_action_convoy_link(id, &command)),
            CommandAction::ConvoyUnlink { .. } => return boxed_action!(crew.execute_action_convoy_unlink(id, &command)),
            CommandAction::ConvoyDelete { .. } => return boxed_action!(crew.execute_action_convoy_delete(id, &command, &caller)),
            CommandAction::ConvoyAbandon { .. } => {
                return boxed_action!(crew.execute_action_convoy_abandon(id, &command, &caller, &dispatching_principal_ref))
            }
            CommandAction::ConvoyWorkForceComplete { .. } => {
                return boxed_action!(crew.execute_action_convoy_work_force_complete(id, &command, &dispatching_principal_ref))
            }
            CommandAction::ConvoyPromiseVerdict { .. } => {
                return boxed_action!(crew.execute_action_convoy_promise_verdict(id, &command, &dispatching_principal_ref))
            }
            CommandAction::ConvoyStart { .. } => {
                return boxed_action!(admission.execute_action_convoy_start(id, &command, &dispatching_principal_ref))
            }
            CommandAction::ConvoyCreate { .. } => {
                return boxed_action!(admission.execute_action_convoy_create(id, &command, &dispatching_principal_ref))
            }
            CommandAction::WorkflowTemplateApply { .. } => {
                return boxed_action!(admission.execute_action_workflow_template_apply(id, &command))
            }
            CommandAction::ProjectAdd { .. } => return boxed_action!(admission.execute_action_project_add(id, &command)),
            CommandAction::ProjectApply { .. } => return boxed_action!(admission.execute_action_project_apply(id, &command)),
            CommandAction::ProjectRegister { .. } => return boxed_action!(admission.execute_action_project_register(id, &command)),
            CommandAction::ProjectRefresh { .. } => return boxed_action!(admission.execute_action_project_refresh(id, &command)),
            CommandAction::ImageCheck { .. } => return boxed_action!(admission.execute_action_image_check(id, &command)),
            CommandAction::TrackRepoPath { .. } => return boxed_action!(executor.execute_action_track_repo_path(id, &command)),
            CommandAction::UntrackRepo { .. } => return boxed_action!(typed executor.execute_action_untrack_repo(id, &command)),
            CommandAction::OpenChangeRequest { .. }
            | CommandAction::CloseChangeRequest { .. }
            | CommandAction::MergeChangeRequest { .. }
            | CommandAction::OpenIssue { .. }
            | CommandAction::LinkIssuesToChangeRequest { .. } => {
                return boxed_action!(executor.execute_action_repository_forge(id, &command))
            }
            CommandAction::Refresh { repo: Some(_) } => return boxed_action!(typed executor.execute_action_refresh_repo(id, &command)),
            _ => {}
        }

        self.executor_actions().execute_provider_action(id, command, command_node_id, remote_executor).await.map_err(Into::into)
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
            .emit(flotilla_executor::step::RemoteStepProgressUpdate {
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
                    .emit(flotilla_executor::step::RemoteStepProgressUpdate {
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
                    .emit(flotilla_executor::step::RemoteStepProgressUpdate {
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

impl InProcessDaemon {
    pub async fn explain_project_internal(&self, name: &str) -> Result<serde_json::Value, String> {
        let namespace = self.provisioning_namespace().await;
        let project = self.resource_backend.definitions::<Project>(&namespace).get(name).await.map_err(|error| error.to_string())?;
        let cascade = flotilla_resources::ResolvedCascade::load(&self.resource_backend, &namespace, name, &project.spec)
            .await
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({ "namespace": namespace, "project": name, "cascade": cascade }))
    }

    pub async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String> {
        let namespace = requested_namespace.map(ToOwned::to_owned).unwrap_or(self.provisioning_namespace().await);
        let (mut explanation, convoy) = self.read_projections().explain_convoy_with_resource(&namespace, name).await?;
        match self.convoy_dispatch_scores(&namespace, &convoy).await {
            Ok(scores) => explanation.dispatch_scores = scores,
            Err(error) => explanation.dispatch_score_error = Some(error),
        }
        Ok(explanation)
    }

    async fn convoy_dispatch_scores(
        &self,
        namespace: &str,
        convoy: &ResourceObject<ResourceConvoy>,
    ) -> Result<BTreeMap<String, flotilla_protocol::DispatchScore>, String> {
        use flotilla_admission::dispatch_missions::{issue_from_convoy, MissionBoard, MissionOccupancy};
        let Some(project_name) = &convoy.spec.project_ref else {
            return Ok(BTreeMap::new());
        };
        let projects = self
            .resource_backend
            .query_declared::<Project>(namespace, &flotilla_store::StoreQuery::names([project_name.clone()], true))
            .await
            .map_err(|e| e.to_string())?;
        let project = &projects.items.first().ok_or("convoy project unavailable")?.object;
        let Some(policy) = &project.spec.dispatch_policy else {
            return Ok(BTreeMap::new());
        };
        // This reads the shared immutable board snapshot, scheduling background
        // warming when cold/stale; it never waits for HTTP. Explanation rebuilds
        // one project's mission graph and reads only its live convoy subset.
        let inputs = self.collect_dispatch_board_inputs(std::slice::from_ref(project)).await?;
        let (_, snapshots) = inputs.get(project_name).ok_or("project board unavailable")?.as_ref().map_err(Clone::clone)?;
        let board = MissionBoard::new(&snapshots.iter().map(|(_, board)| board.as_ref().clone()).collect::<Vec<_>>())?;
        let convoys = self
            .resource_backend
            .query_declared::<ResourceConvoy>(
                namespace,
                &flotilla_projections::dispatch_board::live_project_convoys_query([project_name.clone()]),
            )
            .await
            .map_err(|e| e.to_string())?;
        let occupancy = MissionOccupancy::collect(&board, policy, convoys.items.iter().map(|source| &source.object))?;
        convoy
            .spec
            .issues
            .iter()
            .map(|served| {
                let mut score = board.score(&issue_from_convoy(served), policy)?;
                occupancy.apply(&mut score);
                Ok((format!("{}/{}#{}", served.reference.source.service, served.reference.source.scope, served.reference.id), score))
            })
            .collect()
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_source.subscribe()
    }

    fn query_subscription(&self, subscriber_id: uuid::Uuid) -> QuerySubscription {
        let state = self.aggregator_projection_state.clone();
        let namespace = action_namespace::provisioning_namespace(&self.provisioning_namespace);
        self.connect_surface(subscriber_id, SurfaceDeclaration::focal_for_namespace(namespace));
        let daemon = self.self_weak.clone();
        QuerySubscription::new(move || {
            state.remove_subscriber(subscriber_id);
            if let Some(daemon) = daemon.upgrade() {
                tokio::spawn(async move {
                    if let Err(error) = daemon.disconnect_surface(subscriber_id).await {
                        warn!(%error, "failed to disconnect in-process surface");
                    }
                });
            }
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
                    repository_key: state.repository_key.clone(),
                    path: Some(state.preferred_path().to_path_buf()),
                    name: repo_name(state.preferred_path()),
                    labels: state.labels().clone(),
                    provider_names: state.provider_names(),
                    provider_health: HashMap::new(),
                    loading: false,
                });
            }
        }
        Ok(result)
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        self.execute_for_caller(command, None).await
    }

    async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<CommandValue, String> {
        if matches!(command.action, CommandAction::QueryChangeStream { .. }) {
            return Err("change-stream queries require peer replication transport".into());
        }
        self.projections_actions().execute_query(command, session_id).await
    }

    async fn observe_focus(&self, surface_id: uuid::Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        self.observe_surface_focus(surface_id, targets).await
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        self.cancel_typed(command_id).await.map_err(|error| error.message)
    }

    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        self.refresh_resource_host_summaries().await?;
        Ok(self.host_registry.replay_host_events(last_seen).await)
    }

    async fn subscribe_queries(&self, subscriber_id: uuid::Uuid, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        let state = self.aggregator_projection_state().await;
        let newly_materialized = state.replace_subscriber(subscriber_id, queries);
        let mut events = Vec::new();
        let mut initial_row_counts = Vec::with_capacity(queries.len());
        for cursor in queries {
            let result_set =
                state.result_set_for(&cursor.query).await.ok_or_else(|| format!("query is not materialized: {}", cursor.query))?;
            initial_row_counts.push((cursor.query.clone(), result_set.rows.len()));
            if newly_materialized.contains(&cursor.query) || cursor.since.is_none_or(|seq| seq != result_set.seq) {
                events.push(DaemonEvent::ResultSet(Box::new(result_set)));
            }
        }
        info!(
            subscriber = %subscriber_id,
            queries = ?queries,
            initial_row_counts = ?initial_row_counts,
            replayed_result_sets = events.len(),
            "query subscription initialized"
        );
        Ok(events)
    }

    async fn recover_queries(&self, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        let state = self.aggregator_projection_state().await;
        // Recovery reads existing materializations without changing demand.
        // Socket dispatch verifies this connection's interest before calling.
        // If demand disappears concurrently, fail the whole batch rather than
        // publish a partially recovered view. The connector discards this
        // session and its cursors; a new session subscribes to rebuild demand.
        let mut events = Vec::new();
        for cursor in queries {
            let set = state.result_set_for(&cursor.query).await.ok_or_else(|| format!("query is not materialized: {}", cursor.query))?;
            if cursor.since != Some(set.seq) {
                events.push(DaemonEvent::ResultSet(Box::new(set)));
            }
        }
        Ok(events)
    }

    async fn unsubscribe_queries(&self, subscriber_id: uuid::Uuid) {
        self.aggregator_projection_state().await.remove_subscriber(subscriber_id);
    }

    async fn fetch_more(&self, query: &flotilla_protocol::QueryId) -> Result<(), String> {
        self.aggregator_projection_state().await.request_fetch_more(query)
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut summaries = Vec::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            summaries.push(RepoSummary {
                path: state.preferred_path().to_path_buf(),
                slug: state.slug().map(str::to_string),
                provider_health: HashMap::new(),
                unmet_requirements: state
                    .unmet()
                    .iter()
                    .map(|(factory, requirement)| crate::convert::unmet_requirement_to_proto(factory, requirement))
                    .collect(),
            });
        }
        Ok(StatusResponse { repos: summaries })
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Ok(self.host_registry.get_topology().await)
    }
}

impl InProcessDaemon {
    /// Write the crew admission materials using the configured admission collaborators.
    pub async fn write_admission_briefs(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<bool, String> {
        self.admission.write_admission_briefs(namespace, name, spec, workflow).await
    }
}

impl InProcessDaemon {
    pub async fn record_project_operational_refusal(&self, namespace: &str, project: &str, reason: &str) {
        self.project_service().record_project_operational_refusal(namespace, project, reason).await;
    }
}

impl InProcessDaemon {
    /// Access the environment collaborator used for discovery and registration.
    pub fn environment_manager(&self) -> &EnvironmentManager {
        &self.environment_manager
    }
}
mod forge_demands;

#[async_trait]
impl crew_actions::CrewActionPort for InProcessDaemon {
    async fn link_convoy_subject(
        &self,
        namespace: &str,
        convoy_name: &str,
        reference: &str,
        relationship: Option<flotilla_protocol::Relationship>,
    ) -> Result<(), String> {
        InProcessDaemon::link_convoy_subject(self, namespace, convoy_name, reference, relationship).await
    }
}

#[async_trait]
impl projections_actions::ProjectionsActionPort for InProcessDaemon {
    async fn image_build_report(&self, namespace: &str, name: &str) -> Result<flotilla_protocol::image_check::ImageBuildReport, String> {
        image_check::image_build_report(&self.resource_backend, namespace, name).await
    }
    async fn crew_capability_card(&self, context: &flotilla_protocol::CrewCommandContext) -> Result<String, String> {
        self.crew.query_capabilities(context).await
    }
    async fn message_contacts_internal(
        &self,
        context: &flotilla_protocol::CrewCommandContext,
    ) -> Result<(String, serde_json::Value), String> {
        let book = self.crew.message_contacts_internal(context).await?;
        Ok((book.render(), serde_json::to_value(book).map_err(|error| error.to_string())?))
    }
    async fn crew_list_internal(
        &self,
        context: &flotilla_protocol::CrewCommandContext,
    ) -> Result<flotilla_protocol::CrewListResponse, String> {
        self.crew.crew_list_internal(context).await
    }

    async fn provisioning_namespace(&self) -> String {
        InProcessDaemon::provisioning_namespace(self).await
    }
    async fn attach_project_context(&self, selector: Option<&flotilla_protocol::RepoSelector>) -> Result<Option<String>, String> {
        self.attach_resolver().attach_project_context(selector).await
    }
    async fn scoped_fleet_list(
        &self,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String> {
        self.scoped_fleet_list(project, crew_id, convoy).await
    }
    async fn resolve_repository_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryKey>, String> {
        self.resolve_repository_selector(selector).await
    }
    async fn resolve_attach_with_context(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        self.resolve_attach_with_context(reference, host, transient, mode, project_context).await
    }
    async fn list_projects_internal(&self) -> Result<ProjectListResponse, String> {
        self.list_projects_internal().await
    }
    async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        self.list_hosts_internal().await
    }
    async fn list_cli_items_internal(&self, kind: CliListKind) -> Result<CliListResponse, String> {
        self.list_cli_items_internal(kind).await
    }
    async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        self.get_repo_providers_internal(repo).await
    }
    async fn get_issue_provider_for_repository(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<(Arc<dyn IssueProvider>, flotilla_protocol::IssueSource), String> {
        self.get_issue_provider_for_repository(selector).await
    }
    async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String> {
        self.get_host_status_internal(environment_id).await
    }
    async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String> {
        self.get_host_providers_internal(environment_id).await
    }
    async fn fulfilment_list_internal(&self) -> Result<FulfilmentListResponse, String> {
        self.fulfilment_list_internal().await
    }
    async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String> {
        self.fleet_health_internal().await
    }
    async fn explain_project_internal(&self, name: &str) -> Result<serde_json::Value, String> {
        self.explain_project_internal(name).await
    }
    async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String> {
        self.explain_convoy_internal(requested_namespace, name).await
    }
    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        self.emit_attach_regard(binding, surface_id).await
    }
    async fn dispatch_queue_internal(&self, project_filter: Option<&str>) -> Result<DispatchQueueResponse, String> {
        self.dispatch_queue_internal(project_filter).await
    }
    async fn dispatch_board_internal(&self, project_filter: Option<&str>) -> Result<flotilla_protocol::DispatchBoardResponse, String> {
        self.dispatch_board_internal(project_filter).await
    }
}

#[async_trait]
impl executor_actions::ExecutorActionPort for InProcessDaemon {
    async fn post_install_cleat(
        &self,
        incoming: &Path,
        generation: &str,
        diagnostics_dir: &Path,
    ) -> Result<executor_actions::PostInstallReport, String> {
        let report = InProcessDaemon::post_install_cleat(self, incoming, generation, diagnostics_dir).await?;
        Ok(executor_actions::PostInstallReport {
            failed: report.failed(),
            report: serde_json::to_value(report).map_err(|error| error.to_string()),
        })
    }
    fn vcs_resolver(&self) -> Result<Arc<dyn flotilla_vcs::vcs::CheckoutVcsResolver>, String> {
        Ok(self.self_weak.upgrade().ok_or("VCS resolver daemon unavailable")? as Arc<dyn flotilla_vcs::vcs::CheckoutVcsResolver>)
    }
    async fn resolve_repo_for_command(&self, command: &Command) -> Result<PathBuf, String> {
        InProcessDaemon::resolve_repo_for_command(self, command).await
    }
    async fn repository_registry(&self, repository: &ResourceObject<Repository>) -> Result<Arc<ProviderRegistry>, String> {
        Ok(Arc::clone(&InProcessDaemon::repository_providers(self, repository).await?.registry))
    }
    async fn executor_provider_data(&self, repo_identity: &RepoIdentity, _repo_root: &Path, registry: &ProviderRegistry) -> ProviderData {
        InProcessDaemon::executor_provider_data(self, repo_identity, _repo_root, registry).await
    }
    async fn execution_registry(&self, repository: &ResourceObject<Repository>, path: &Path) -> Result<Arc<ProviderRegistry>, String> {
        InProcessDaemon::execution_registry(self, repository, path).await
    }
    async fn add_repo(&self, path: &Path) -> Result<AddRepoOutcome, String> {
        InProcessDaemon::add_repo(self, path).await
    }
    async fn apply_intent_document(
        &self,
        namespace: &str,
        document: serde_json::Value,
    ) -> Result<flotilla_store::DynamicResourceObject, ResourceError> {
        InProcessDaemon::apply_intent_document(self, namespace, document).await
    }
    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        InProcessDaemon::detect_repo_identity(self, repo_path).await
    }
    async fn local_checkout_for_repository(&self, key: &RepositoryKey) -> Result<Option<PathBuf>, String> {
        InProcessDaemon::local_checkout_for_repository(self, key).await
    }
    async fn reconcile_now(&self, namespace: &str, kind: &str, name: &str) -> Result<String, String> {
        let reconciler = self.operator_reconciler.read().await.clone();
        match reconciler {
            Some(reconciler) => reconciler.reconcile_now(namespace, kind, name).await,
            None => Err("operator reconciliation is unavailable before runtime startup".to_string()),
        }
    }
    async fn delete_stale_adopted_checkouts(&self, namespace: &str) -> Result<(), ResourceError> {
        flotilla_observation::observed_resources::delete_stale_adopted_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            namespace,
        )
        .await
    }
    async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        InProcessDaemon::peer_connection_status(self, node_id).await
    }
    async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryIdentityChange>, String> {
        InProcessDaemon::refresh(self, repo).await
    }
    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        InProcessDaemon::remove_repo(self, path).await
    }
    async fn repository_for_selector(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<ResourceObject<Repository>, flotilla_protocol::DispatcherError> {
        InProcessDaemon::repository_for_selector(self, selector).await
    }
    fn resolve_observation_root_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        InProcessDaemon::resolve_observation_root_selector(self, selector)
    }
    async fn resolve_repo_selector(
        &self,
        selector: &flotilla_protocol::RepoSelector,
    ) -> Result<PathBuf, flotilla_protocol::DispatcherError> {
        InProcessDaemon::resolve_repo_selector(self, selector).await
    }
    async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        InProcessDaemon::tracked_repo_identity_for_path(self, repo_path).await
    }
}

#[async_trait]
impl image_check::ImageCheckSources for InProcessDaemon {
    async fn layer_snapshot(
        &self,
        source: &flotilla_resources::CharterSource,
    ) -> Result<Option<flotilla_charter::charter_store::LayerDirectorySnapshot>, String> {
        ProjectOperations::repository_inspector(self).await?.image_layer_snapshot(source).await
    }

    async fn default_harness_adapters(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &flotilla_resources::ProjectSpec,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        self.admission.default_workflow_harness_adapters(namespace, project_ref, project).await
    }
}

#[async_trait]
impl ProjectOperations for InProcessDaemon {
    fn remember_bootstrap_checkout(&self, inspection: &RepositoryInspection) -> Result<(), String> {
        use flotilla_paths::path_context::ExecutionEnvironmentPath;
        let path = ExecutionEnvironmentPath::new(&inspection.checkout.path);
        self.config.add_observation_root(&path)?;
        self.config.set_checkout_config(&path, inspection.spec.vcs().clone());
        Ok(())
    }

    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        InProcessDaemon::repository_inspector(self).await
    }

    async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        InProcessDaemon::inspect_repository_path(self, path, remote).await
    }

    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        InProcessDaemon::resolve_repository_remote(self, remote).await
    }

    async fn resolve_forge_identity_in(&self, namespace: &str, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        InProcessDaemon::resolve_forge_identity_in(self, namespace, spec).await
    }

    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String> {
        InProcessDaemon::reap_ensured_convoy(self, namespace, ensure_name, convoy_name, force).await
    }

    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        InProcessDaemon::inspect_adopted_checkout(self, path, repository_url, git_ref).await
    }
    async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String> {
        InProcessDaemon::project_add(self, target, explicit_name, explicit_display_name, remote).await
    }
    async fn project_refresh(&self, name: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String> {
        InProcessDaemon::project_refresh(self, name).await
    }
    async fn image_check(
        &self,
        request: &flotilla_protocol::image_check::ImageCheckRequest,
    ) -> Result<flotilla_protocol::image_check::ImageCheckReport, String> {
        let namespace = self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone();
        let host = self.environment_manager.local_host_id().as_str().to_string();
        image_check::check_image(&self.resource_backend, &namespace, &host, self, request).await
    }
    async fn project_register(&self, target: &str) -> Result<(String, usize), String> {
        InProcessDaemon::project_register(self, target).await
    }
    async fn roll_convoy_ensure(&self, namespace: &str, name: &str) -> Result<String, String> {
        InProcessDaemon::roll_convoy_ensure(self, namespace, name).await
    }
    fn spawn_convoy_start(&self, task: ConvoyStartTask) -> bool {
        if let Some(daemon) = self.self_weak.upgrade() {
            tokio::spawn(async move {
                daemon.supervise_convoy_start(task).await;
            });
            true
        } else {
            false
        }
    }
}
struct CheckoutInspector(Weak<InProcessDaemon>);

#[async_trait]
impl flotilla_crew::ports::CheckoutIntegrationInspector for CheckoutInspector {
    #[tracing::instrument(name = "checkout_landing_evidence", skip_all)]
    async fn inspect(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout: &ResourceObject<ResourceCheckout>,
    ) -> Result<flotilla_resources::CheckoutIntegrationStatus, String> {
        let daemon = self.0.upgrade().ok_or("checkout inspector daemon unavailable")?;
        let forges = daemon.checkout_integration_forges(&convoy.metadata.namespace).await?;
        daemon.inspect_resource_checkout_integration(checkout, Some(convoy), &forges).await
    }
}

/// Admission's standing-convoy teardown, served by the crew service.
struct CrewTeardown(Arc<CrewService>);

#[async_trait]
impl flotilla_admission::ports::ConvoyTeardown for CrewTeardown {
    async fn abandon(&self, namespace: &str, name: &str, reason: &str, principal: Option<&PrincipalRef>) -> Result<(), String> {
        self.0.abandon(namespace, name, reason, principal).await.map(|_| ())
    }
    async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.0.reap(namespace, name, force).await
    }
}

#[cfg(test)]
mod admission_composition_tests;
