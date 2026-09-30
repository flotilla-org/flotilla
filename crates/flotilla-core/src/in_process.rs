//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

#[path = "attach.rs"]
mod attach;
mod project_ops;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
pub use attach::ResolvedAttach;
use attach::{AttachResolver, CachedFleetRows};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use flotilla_protocol::{
    arg::Arg,
    commands::{AttachMode, RepositoryIdentityChange},
    qualified_path::{HostId, QualifiedPath},
    result_set::{CheckoutRow, ConvoyChangeRequest, Rows},
    AttachBinding, CanonicalHostId, Change, CheckoutArchiveOutcome, CheckoutArchiveStatus, Command, CommandAction, CommandValue,
    ConvoyDispatchRegard, ConvoyExplanation, CrewCommandContext, CrewListMember, CrewListResponse, DaemonEvent, DispatchQueueResponse,
    DispatchQueueRow, EntryOp, EnvironmentId, ExplainedArtifact, ExplainedChangeRequest, ExplainedCheckout, ExplainedCrewDelivery,
    ExplainedDecisionLedger, ExplainedEvent, ExplainedLeafFiring, ExplainedSettlement, ExplainedSubscription, ExplainedUnclaimedWork,
    ExplainedUnmetExpectation, FleetHealthResponse, FleetHostRow, FleetHostStaleness, FleetListResponse, FleetListRow,
    FleetReplicaSnapshot, FleetReplicaStatus, FleetStaleness, FulfilmentAllocation, FulfilmentAllocationCandidate, FulfilmentHarness,
    FulfilmentListResponse, FulfilmentModel, FulfilmentRow, HostListResponse, HostName, HostProviderStatus, HostProvidersResponse,
    HostStatusResponse, HostSummary, LeafAddress, ManagedTerminal, NodeId, NodeInfo, PeerConnectionState, PlacementDecision,
    PlacementRefusal, PlacementTargetHost, PlacementViableCandidate, PrincipalRef, ProjectListEntry, ProjectListRepository,
    ProjectListResponse, ProviderData, ProviderInfo, QueryCursor, RepoDelta, RepoIdentity, RepoInfo, RepoProvidersResponse, RepoSummary,
    ResolvedAttachAction, ResolvedAttachPlan, ResourceCursor, ResourceJsonResponse, ResourceRecordType, ResourceRef, StatusResponse,
    StreamKey, SurfaceDeclaration, TopologyResponse, TopologyRoute, ViewAddress, AGENT_ADAPTER_PROVIDER_CATEGORY,
    TERMINAL_POOL_PROVIDER_CATEGORY,
};
use flotilla_resources::{
    api_version, apply_resource_document, apply_status_patch as apply_resource_status_patch,
    apply_status_patch_checked as apply_resource_status_patch_checked, bound_change_request_record_name, capped_github_app_permissions,
    change_request_address, change_request_address_with_forges, change_request_record_name, controller::delete_lifecycle_owned_matching,
    evaluate_crew_completion, evaluate_landing_settlement, expected_change_request_leaves, expected_checkout_refs,
    external_patches as convoy_external_patches, get_resource_kind, get_resource_kind_including_replicas, list_resource_kind,
    list_resource_kind_including_replicas, normalize_issue_source, normalize_project_spec, repository_display_labels,
    resolve_project_issue_sources, terminal_session_attach_target, AllocationDecision, BoundChangeRequest, CapabilityNeed,
    ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout, CheckoutIntegrationStatus,
    CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus, Clock,
    ConditionValue, ControllerRetry, Convoy as ResourceConvoy, ConvoyEnsure, ConvoyEnsureCondition, ConvoyEnsureHoldReason,
    ConvoyEnsureSpec, ConvoyEnsureStatusPatch, ConvoyIssue, ConvoyPhase, ConvoyProvisioningState, ConvoyRepositorySpec, ConvoySpec,
    ConvoyStatus, ConvoyStatusPatch, CredentialConsumer, CredentialGrant, CredentialSource, CredentialSpec, CrewCompletionClaim,
    CrewCompletionPending, CrewMessageDelivery, CrewMessageSender, CrewSource, CrewSpec, CrewWorkPhase, Demand as ResourceDemand,
    DemandExpiry, DemandExpiryDisposition, DemandKind, DemandSpec, DemandState, DocumentKey, Environment as ResourceEnvironment,
    EnvironmentPhase, EventRecorder, EventRegarding, Forge, ForgeKind, FulfilmentCostClass, FulfilmentGrant, FulfilmentKind,
    FulfilmentRealisation, HoldAct, Host as ResourceHost, HostStatus as ResourceHostStatus, InMemoryBackend, InputMeta, InputValue,
    IntegrationCondition, IssueSnapshot, IssueSourceResolution, IssueSourceUnavailable, LandingCredentialScope, LifecycleAuthority,
    ManifestRoot, ObjectEvent, ObjectMeta, ObservedChangeRequestState, ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PendingBrief,
    PlacementPolicy, PlacementPolicySpec, Presentation as ResourcePresentation, Project, ProjectSpec, ReadResourceObject, Repository,
    RepositoryIdentity, RepositoryKey, RepositorySpec, RepositoryTrust, Resolution, ResolutionAction, Resource, ResourceBackend,
    ResourceError, ResourceObject, ResourceProvenance, RetryBackoff, RoleHandoff, SettlementMode, SupervisionTarget, SystemClock,
    TerminalAttentionState, TerminalBrief, TerminalCrewContext, TerminalCrewMessage, TerminalSession as ResourceTerminalSession,
    TerminalSessionIdentity, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionStatusPatch,
    TurnDeliveryRung, Vessel, VesselRequirement, WatchEvent, WatchStart, WorkCompletionAuthority, WorkPhase as ResourceWorkPhase,
    WorkflowTemplate, WorkflowTemplateSpec, WriterIdentity, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
    CREDENTIAL_PERMISSIONS_ANNOTATION, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_SCOPES_ANNOTATION, DRIVER_ADMISSION_CONDITION_TYPE,
    GENERATION_LABEL, HEARTBEAT_READY_TTL_SECS, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use futures::{FutureExt, StreamExt};
use project_ops::{is_declaration_backed_project, validate_project_name};
use read_projections::credential_refresh_alert_for_vessel;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    agent_adapter::{required_agent_adapters, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    change_request_observer::{ChangeRequestObservationSource, ChangeRequestRef},
    checkout_integration::{
        change_request_subjects_from_claim, checkout_path_from_status_and_spec, convoy_change_request_id_for_checkout,
        inspect_checkout_integration, inspect_convoy_checkout_integration, LANDING_EVIDENCE_TTL,
    },
    config::{ConfigStore, StaticEnvironmentConfig},
    daemon::{DaemonHandle, QuerySubscription},
    environment_manager::EnvironmentManager,
    event_sink::{BroadcastEventSink, EventSink},
    executor,
    executor::checkout::{checkout_matches_scope, CheckoutResolutionScope},
    fleet::{
        accumulate_fleet_health_counts, crew_attention, fleet_observation_agreement, format_resource_replication_failures,
        host_credential_attention, join_replica_errors, replica_staleness, FleetService, ResourceReplicationFailure,
        SshFleetReplicaTransport, FLEET_REPLICA_FRESH_SECS,
    },
    hop_chain::{
        environment::DockerEnvironmentHopResolver, remote::ssh_resolver_from_config, resolver::HopResolver,
        terminal::NoopTerminalHopResolver, Hop, HopPlan, ResolutionContext,
    },
    host_identity::{
        resolve_local_environment_state_dir, resolve_local_host_id, resolve_local_node_id, resolve_or_create_environment_id,
        resolve_or_create_remote_environment_id, resolve_or_create_remote_host_id,
    },
    host_registry::HostCounts,
    leaf_engine::{LeafSubscriptionTable, LeafWatcher},
    model::{provider_names_from_registry, repo_name, RepoModel},
    ops_entry::{
        ENSURED_FROM_ANNOTATION, ENSURE_PROVENANCE_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, PRESENTS_AS_ANNOTATION,
        SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
    },
    path_context::{canonical_or_original, DaemonHostPath, ExecutionEnvironmentPath},
    project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION,
    providers::{
        ai_utility::{AiUtility, ConvoyNames},
        change_request::{BoundObservations, ChangeRequestTracker},
        discovery::{
            discover_providers_with_host_scoped, run_host_detectors, DiscoveryResult, DiscoveryRuntime, EnvironmentAssertion,
            EnvironmentBag,
        },
        github_api::rate_limit_reset,
        issue_tracker::{forge_issue_source, IssueProvider},
        registry::ProviderRegistry,
        ssh_runner::SshCommandRunner,
        types::RepoCriteria,
        ChannelLabel, CommandRunner,
    },
    regard_lifecycle::{RegardLifecycle, SurfaceGestureOutcome, DEFAULT_REGARD_DECAY_SECONDS, DEFAULT_REGARD_REFRESH_SECONDS},
    repo_state::{RepoRootState, RepoState},
    repository_inspection::{GitRepositoryInspector, RepositoryContinuity, RepositoryInspection, RepositoryInspector},
    resource_explain::{
        explain_condition, explain_unmet_expectation, explained_provenance, observed_freshness, resource_read_envelope, resource_record,
        run_resource_watch_command, ResourceWatchCommandContext,
    },
    step::{
        run_step_plan_with_remote_executor, RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, StepOutcome, StepResolver,
    },
};

type ObservationScope = (String, String, String);

fn forge_service_matches(service_url: &str, service: &str) -> bool {
    service_url.split_once("://").is_some_and(|(_, authority)| authority.trim_end_matches('/').eq_ignore_ascii_case(service))
}

struct CachedObservation {
    expires_at: tokio::time::Instant,
    queried: BTreeSet<u64>,
    result: Result<BoundObservations, String>,
}

struct ProviderChangeRequestObservationSource {
    daemon: Arc<OnceLock<Weak<InProcessDaemon>>>,
    cache: Mutex<HashMap<ObservationScope, Arc<Mutex<Option<CachedObservation>>>>>,
}

struct ProviderIssueObservationSource {
    daemon: Arc<OnceLock<Weak<InProcessDaemon>>>,
}

fn issue_source_for_subject(
    subject: &crate::issue_observer::IssueRef,
    forges: &[flotilla_resources::ForgeSpec],
) -> Result<flotilla_protocol::IssueSource, String> {
    let service = if let Some(forge) = forges.iter().find(|forge| forge.forge_id == subject.service) {
        forge.https_url.clone()
    } else {
        if !subject.service.starts_with("host%3a") && !subject.service.contains(['.', ':']) && !subject.service.contains("%2f") {
            return Err(format!("issue service `{}` has no Forge declaration or host-derived address", subject.service));
        }
        let encoded = subject.service.strip_prefix("host%3a").unwrap_or(&subject.service);
        let location = encoded.replace("%2f", "/").replace("%3a", ":").replace("%25", "%");
        if location.contains("://") {
            location
        } else {
            format!("https://{location}")
        }
    };
    Ok(flotilla_protocol::IssueSource { service, scope: subject.scope.clone() })
}

#[async_trait]
impl crate::issue_observer::IssueObservationSource for ProviderIssueObservationSource {
    async fn observe(&self, subject: &crate::issue_observer::IssueRef) -> Result<flotilla_resources::IssueStatus, String> {
        let daemon = self.daemon.get().and_then(Weak::upgrade).ok_or("issue observation daemon unavailable")?;
        let forges = daemon
            .resource_backend
            .definitions::<Forge>(&subject.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let fallback = flotilla_protocol::IssueRef { source: issue_source_for_subject(subject, &forges)?, id: subject.number.to_string() };
        let reference = daemon
            .resource_backend
            .including_replicas::<ResourceConvoy>(&subject.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .flat_map(|convoy| convoy.object.spec.issues)
            .map(|issue| issue.reference)
            .find(|reference| {
                flotilla_resources::issue_address_with_forges(reference, &forges).is_ok_and(|address| {
                    address
                        == flotilla_protocol::LeafAddress::Issue {
                            service: subject.service.clone(),
                            scope: subject.scope.clone(),
                            number: subject.number,
                        }
                })
            })
            .unwrap_or(fallback);
        let issue = daemon.fetch_issue_by_ref(&reference).await?;
        let observed_at = chrono::Utc::now();
        Ok(flotilla_resources::IssueStatus {
            state: flotilla_resources::Observation::known(
                match issue.state {
                    flotilla_protocol::IssueState::Open => flotilla_resources::ObservedIssueState::Open,
                    flotilla_protocol::IssueState::Closed => flotilla_resources::ObservedIssueState::Closed,
                },
                observed_at,
            ),
            labels: flotilla_resources::Observation::known(issue.labels, observed_at),
            updated_at: flotilla_resources::Observation::known(issue.as_of, observed_at),
        })
    }
}

impl ProviderChangeRequestObservationSource {
    fn new(daemon: Arc<OnceLock<Weak<InProcessDaemon>>>) -> Self {
        Self { daemon, cache: Mutex::new(HashMap::new()) }
    }

    async fn query(
        &self,
        subjects: &[ChangeRequestRef],
        subject: &ChangeRequestRef,
        fresh: bool,
    ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
        let key = (subject.namespace.clone(), subject.service.clone(), subject.scope.clone());
        let mut numbers = subjects.iter().map(|subject| subject.number).collect::<BTreeSet<_>>();
        numbers.insert(subject.number);
        let scope_cache = {
            let mut cache = self.cache.lock().await;
            Arc::clone(cache.entry(key).or_insert_with(|| Arc::new(Mutex::new(None))))
        };
        // Hold only this repository's lock through its forge read. Other
        // repositories can continue observing even when one query is slow.
        let mut cache = scope_cache.lock().await;
        let daemon = self.daemon.get().and_then(Weak::upgrade).ok_or("change request observation daemon unavailable")?;
        let repositories =
            daemon.resource_backend.including_replicas::<Repository>(&subject.namespace).list().await.map_err(|error| error.to_string())?;
        let repository =
            repositories
                .items
                .into_iter()
                .find(|repository| {
                    repository.object.spec.forge().is_some_and(|forge| {
                        forge.repository == subject.scope && forge_service_matches(&forge.service_url, &subject.service)
                    })
                })
                .ok_or_else(|| format!("repository {}/{} has no discovered change request provider", subject.service, subject.scope))?;
        for convoy in daemon
            .resource_backend
            .including_replicas::<ResourceConvoy>(&subject.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
        {
            if convoy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                continue;
            }
            if let Some(bound) =
                convoy.object.spec.change_request.as_ref().filter(|bound| bound.repository_ref == repository.object.spec.key())
            {
                if let Ok(number) = bound.id.parse() {
                    numbers.insert(number);
                }
            }
        }
        let queried = numbers;
        if !fresh {
            if let Some(entry) = cache.as_ref() {
                if tokio::time::Instant::now() < entry.expires_at && queried.is_subset(&entry.queried) {
                    return entry
                        .result
                        .as_ref()
                        .map_err(Clone::clone)?
                        .get(&subject.number)
                        .cloned()
                        .unwrap_or_else(|| Err(format!("change request {} was not found", subject.number)));
                }
            }
        }
        let numbers = queried.iter().copied().collect::<Vec<_>>();
        let provider = daemon.discover_repository_change_request(&subject.namespace, &repository.object.spec).await?;
        let result = provider.observe_bound(&numbers).await;
        let delay = result
            .as_ref()
            .err()
            .and_then(|error| rate_limit_reset(error))
            .and_then(|reset| reset.signed_duration_since(Utc::now()).to_std().ok())
            .unwrap_or(Duration::from_secs(9));
        let status = result.as_ref().map_err(Clone::clone).and_then(|statuses| {
            statuses.get(&subject.number).cloned().unwrap_or_else(|| Err(format!("change request {} was not found", subject.number)))
        });
        *cache = Some(CachedObservation { expires_at: tokio::time::Instant::now() + delay, queried, result });
        status
    }
}

#[async_trait]
impl ChangeRequestObservationSource for ProviderChangeRequestObservationSource {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<flotilla_resources::ChangeRequestStatus, String> {
        self.query(std::slice::from_ref(subject), subject, false).await
    }

    async fn observe_group(
        &self,
        subjects: &[ChangeRequestRef],
        subject: &ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, String> {
        self.query(subjects, subject, false).await
    }

    async fn observe_for_completion(&self, subject: &ChangeRequestRef) -> Result<flotilla_resources::ChangeRequestStatus, String> {
        self.query(std::slice::from_ref(subject), subject, true).await
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

mod read_projections;
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
) -> Result<(), String> {
    let fallback_env_id = static_ssh_environment_id(config_key);
    let destination = crate::config::ssh_destination(&environment.hostname, environment.user.as_deref());
    let runner = Arc::new(SshCommandRunner::new(destination.clone(), multiplex, Arc::clone(&discovery.runner)));
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
        EnvironmentId::new(format!("host-direct-{host_id}"))
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
    if host_direct {
        environment_manager.set_direct_environment_ssh_destination(&env_id, destination)?;
    }
    Ok(())
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
        if let Err(err) = register_static_ssh_direct_environment(environment_manager, discovery, config_key, environment, false, true).await
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
                };
                if let Err(err) = register_static_ssh_direct_environment(
                    environment_manager,
                    discovery,
                    label,
                    &environment,
                    true,
                    hosts.resolved_ssh_multiplex(label),
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

fn fallback_repo_identity(path: &Path) -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.to_string_lossy().into_owned() }
}

fn empty_repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() }
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
    durable_backend: &ResourceBackend,
    observed_backend: &ResourceBackend,
    request: AdoptedCheckoutRequest<'_>,
) -> Result<(String, String, String), String> {
    let AdoptedCheckoutRequest { namespace, convoy_name, checkout_path, repository_spec, repository_url, git_ref, host_ref } = request;
    let path = std::fs::canonicalize(checkout_path)
        .map_err(|err| format!("adopted checkout path {} cannot be resolved: {err}", checkout_path.display()))?;
    let path_str = path.to_string_lossy().to_string();
    let checkout_ref = adopted_checkout_name(convoy_name);
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&durable_backend.clone().using::<Repository>(namespace), &repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    let meta = InputMeta::builder()
        .name(checkout_ref.clone())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Adopted);
    let spec = ResourceCheckoutSpec::Observed(
        ResourceObservedCheckoutSpec::builder()
            .r#ref(git_ref.to_string())
            .path(path_str.clone())
            .repo_ref(repository_key)
            .host_ref(host_ref.to_string())
            .is_main(matches!(git_ref, "main" | "master" | "trunk"))
            .build(),
    );
    let status = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).path(path_str).build();

    let durable = persist_adopted_checkout(durable_backend, namespace, &checkout_ref, &meta, &spec, &status).await?;
    match crate::observed_resources::project_adopted_checkout(observed_backend, namespace, &durable).await {
        Ok(()) => {}
        Err(ResourceError::Invalid { message }) => return Err(message),
        Err(error) => {
            warn!(checkout = %checkout_ref, %error, "adopted checkout committed durably but observed publication failed; reconciliation will retry");
        }
    }

    Ok((checkout_ref, repository_url.to_string(), git_ref.to_string()))
}

async fn persist_adopted_checkout(
    backend: &ResourceBackend,
    namespace: &str,
    checkout_ref: &str,
    meta: &InputMeta,
    spec: &ResourceCheckoutSpec,
    status: &ResourceCheckoutStatus,
) -> Result<ResourceObject<ResourceCheckout>, String> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let checkout = match checkouts.create(meta, spec).await {
        Ok(checkout) => checkout,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(checkout_ref).await.map_err(|err| err.to_string())?;
            if existing.metadata.lifecycle_authority().map_err(|err| err.to_string())? != Some(LifecycleAuthority::Adopted) {
                return Err(format!("checkout {checkout_ref} already exists but is not adopted"));
            }
            if &existing.spec != spec {
                return Err(format!("checkout {checkout_ref} already exists with different adopted checkout details"));
            }
            existing
        }
        Err(err) => return Err(err.to_string()),
    };
    if checkout.status.is_some() {
        Ok(checkout)
    } else {
        checkouts.update_status(checkout_ref, &checkout.metadata.resource_version, status).await.map_err(|err| err.to_string())
    }
}

#[derive(Debug)]
struct PlacementResolution {
    selected: Option<ResourceObject<PlacementPolicy>>,
    refused_candidates: Vec<PlacementRefusal>,
    viable_not_selected: Vec<PlacementViableCandidate>,
    allocation: Option<FulfilmentAllocation>,
}

struct KindCandidate {
    kind: ResourceObject<FulfilmentKind>,
    placement: PlacementResolution,
    free_slots: Option<u32>,
    host_ready: bool,
    sleeping_until: Option<DateTime<Utc>>,
}

trait FulfilmentDecider {
    fn compare(&self, left: &KindCandidate, right: &KindCandidate) -> std::cmp::Ordering;
}

struct PlacementTieBreak<'a> {
    needs: &'a BTreeSet<CapabilityNeed>,
    now: DateTime<Utc>,
}

impl PlacementTieBreak<'_> {
    fn reserved(&self, candidate: &KindCandidate) -> bool {
        candidate.kind.spec.grants.iter().any(|grant| {
            matches!(grant, FulfilmentGrant::Platform(platform) if matches!(platform.as_str(), "macos" | "windows"))
                && !self
                    .needs
                    .iter()
                    .any(|need| matches!(need, CapabilityNeed::Platform(need_platform) if grant == &FulfilmentGrant::Platform(need_platform.clone())))
        })
    }

    fn available(&self, candidate: &KindCandidate) -> bool {
        candidate.host_ready
            && candidate.sleeping_until.is_none_or(|until| until <= self.now)
            && candidate.free_slots.is_none_or(|slots| slots > 0)
    }
}

impl FulfilmentDecider for PlacementTieBreak<'_> {
    fn compare(&self, left: &KindCandidate, right: &KindCandidate) -> std::cmp::Ordering {
        let key = |candidate: &KindCandidate| {
            let policy = candidate.placement.selected.as_ref().expect("candidate has a validated placement policy");
            (
                !self.available(candidate),
                candidate.kind.spec.cost_class,
                Reverse(policy.spec.priority),
                candidate.kind.metadata.name.clone(),
            )
        };
        key(left).cmp(&key(right))
    }
}

fn home_copy_wins_by_name<T: Resource>(sources: impl IntoIterator<Item = ReadResourceObject<T>>) -> Vec<ResourceObject<T>> {
    let mut resolved = BTreeMap::<String, ReadResourceObject<T>>::new();
    for source in sources {
        let name = source.object.metadata.name.clone();
        match resolved.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if matches!(source.provenance, ResourceProvenance::Local)
                    && matches!(entry.get().provenance, ResourceProvenance::Replica { .. }) =>
            {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    resolved.into_values().map(|source| source.object).collect()
}

fn placement_host_ref(policy: &ResourceObject<PlacementPolicy>) -> Option<&str> {
    policy
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| policy.spec.docker_per_vessel.as_ref().map(|spec| spec.host_ref.as_str()))
}

async fn placement_target_host(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &ResourceObject<PlacementPolicy>,
) -> Result<PlacementTargetHost, String> {
    let host_ref = placement_host_ref(policy).ok_or_else(|| format!("placement `{}` has no target host", policy.metadata.name))?;
    canonical_placement_host_ref(backend, namespace, host_ref)
        .await
        .and_then(|target| target.ok_or_else(|| format!("references unknown host `{host_ref}`")))
        .map_err(|error| format!("placement `{}` {error}", policy.metadata.name))
}

async fn canonical_placement_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let hosts = backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
    canonical_placement_host_ref_from_sources(&hosts.items, host_ref)
}

pub(crate) fn canonical_placement_host_ref_from_sources(
    hosts: &[ReadResourceObject<ResourceHost>],
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let canonical = flotilla_resources::canonical_host_id(hosts.iter().map(|host| &host.object), host_ref)?;
    let Some(canonical) = canonical else {
        return Ok(None);
    };
    let resolved = hosts
        .iter()
        .find(|host| host.object.metadata.name == canonical.as_str())
        .expect("canonical host resolver selected an existing host");
    let display_name = if resolved.object.spec.display_name.is_empty() {
        resolved.object.metadata.name.clone()
    } else {
        resolved.object.spec.display_name.clone()
    };
    Ok(Some(PlacementTargetHost { reference: canonical, display_name }))
}

async fn authoritative_placement_host(
    backend: &ResourceBackend,
    namespace: &str,
    target_host: &PlacementTargetHost,
    placement_name: &str,
) -> Result<ResourceObject<ResourceHost>, String> {
    backend
        .including_replicas::<ResourceHost>(namespace)
        .get(target_host.reference.as_str())
        .await
        .map(|source| source.object)
        .map_err(|error| format!("placement `{placement_name}` target host is not ready: {error}"))
}

fn host_generation(status: Option<&ResourceHostStatus>) -> &str {
    status.and_then(|status| status.daemon_generation.as_deref()).unwrap_or("unknown")
}

fn placement_host_not_ready_reason(placement_name: &str, host_label: &str, generation: &str, status: &ResourceHostStatus) -> String {
    let mut failing_conditions = status
        .conditions
        .iter()
        .filter(|condition| condition.blocks_readiness())
        .map(|condition| format!("{}: {}", condition.reason, condition.message))
        .collect::<Vec<_>>();
    failing_conditions.sort();
    let detail = if failing_conditions.is_empty() { String::new() } else { format!(": {}", failing_conditions.join("; ")) };
    format!("placement `{placement_name}` host `{host_label}` generation `{generation}` is not ready{detail}")
}

fn check_placement_capacity(target_host: &PlacementTargetHost, capacity: Option<(u64, Option<u64>)>) -> Result<(), String> {
    let Some((floor_bytes, free_bytes)) = capacity else {
        return Err(format!("placement refused on host `{}`: admission free-space floor is unavailable", target_host.display_name));
    };
    if floor_bytes == 0 {
        return Ok(());
    }
    let free_bytes =
        free_bytes.ok_or_else(|| format!("placement refused on host `{}`: free space is unavailable", target_host.display_name))?;
    crate::admission::check_measured_free_space(&target_host.display_name, free_bytes, floor_bytes)
}

async fn policy_targets_agentless_ssh(backend: &ResourceBackend, namespace: &str, policy: &ResourceObject<PlacementPolicy>) -> bool {
    let Ok(target) = placement_target_host(backend, namespace, policy).await else {
        return false;
    };
    authoritative_placement_host(backend, namespace, &target, &policy.metadata.name)
        .await
        .ok()
        .is_some_and(|host| matches!(host.spec.connection, flotilla_resources::HostConnection::AgentlessSsh { .. }))
}

async fn validate_docker_placement_host(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &ResourceObject<PlacementPolicy>,
) -> Result<(), String> {
    if policy.spec.docker_per_vessel.is_none() {
        return Ok(());
    }
    let target = placement_target_host(backend, namespace, policy).await?;
    let host = authoritative_placement_host(backend, namespace, &target, &policy.metadata.name).await?;
    let capabilities = host.status.as_ref().map(|status| &status.capabilities);
    if capabilities.and_then(|capabilities| capabilities.get("docker")) != Some(&serde_json::Value::Bool(true)) {
        return Err(format!(
            "placement `{}` host `{}` is missing docker capability required for Docker vessels",
            policy.metadata.name, target.display_name
        ));
    }
    if capabilities.and_then(|capabilities| capabilities.get("os")).and_then(serde_json::Value::as_str) != Some("linux") {
        return Err(format!(
            "placement `{}` host `{}` is missing Linux host capability required for daemon-adjacent injection",
            policy.metadata.name, target.display_name
        ));
    }
    Ok(())
}

async fn placement_actuator_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    target: &PlacementTargetHost,
) -> Result<CanonicalHostId, String> {
    let host = authoritative_placement_host(backend, namespace, target, "actuator routing").await?;
    match host.spec.connection {
        flotilla_resources::HostConnection::AgentlessSsh { owning_daemon, .. } => Ok(CanonicalHostId::resolved(owning_daemon)),
        flotilla_resources::HostConnection::Daemon => Ok(target.reference.clone()),
    }
}

async fn default_convoy_placement_policy(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
    local_host_ref: Option<&CanonicalHostId>,
) -> Result<PlacementResolution, String> {
    let mut policies = match backend.including_replicas::<PlacementPolicy>(namespace).list().await {
        Ok(list) => home_copy_wins_by_name(list.items),
        Err(err) => {
            warn!(%namespace, error = %err, "failed to list placement policies; convoy will remain Pending until one is registered");
            return Ok(PlacementResolution {
                selected: None,
                refused_candidates: Vec::new(),
                viable_not_selected: Vec::new(),
                allocation: None,
            });
        }
    };
    policies.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    let candidate_names = policies.iter().map(|policy| policy.metadata.name.clone()).collect::<Vec<_>>();
    let mut viable = Vec::new();
    let mut refused_candidates = Vec::new();
    for policy in policies {
        let mut candidate_workflow = workflow.clone();
        let agentless_ssh = policy_targets_agentless_ssh(backend, namespace, &policy).await;
        let agentless_unready = if agentless_ssh { placement_agent_adapters(backend, namespace, &policy, false).await.err() } else { None };
        let refusal = if let Err(reason) = validate_docker_placement_host(backend, namespace, &policy).await {
            Some(reason)
        } else if let Some(reason) = agentless_unready {
            Some(reason)
        } else if let Err(reason) = validate_workflow_agent_adapters(backend, namespace, workflow, Some(&policy), false).await {
            Some(reason)
        } else {
            resolve_and_validate_workflow_credentials(backend, namespace, project_ref, repositories, Some(&policy), &mut candidate_workflow)
                .await
                .err()
        };
        if let Some(reason) = refusal {
            let target_host = placement_target_host(backend, namespace, &policy).await.unwrap_or_else(|_| PlacementTargetHost {
                reference: CanonicalHostId::resolved(String::new()),
                display_name: "no target host".to_string(),
            });
            refused_candidates.push(PlacementRefusal { policy_name: policy.metadata.name.clone(), target_host, reason });
        } else {
            viable.push(policy);
        }
    }
    let mut viable_targets = HashMap::new();
    let mut resolved_viable = Vec::with_capacity(viable.len());
    for policy in viable {
        match placement_target_host(backend, namespace, &policy).await {
            Ok(target_host) => {
                viable_targets.insert(policy.metadata.name.clone(), target_host);
                resolved_viable.push(policy);
            }
            Err(reason) => refused_candidates.push(PlacementRefusal {
                policy_name: policy.metadata.name.clone(),
                target_host: PlacementTargetHost {
                    reference: CanonicalHostId::resolved(String::new()),
                    display_name: "no target host".to_string(),
                },
                reason,
            }),
        }
    }
    viable = resolved_viable;
    viable.sort_by_key(|policy| {
        let target_host = &viable_targets[&policy.metadata.name].reference;
        let is_local = local_host_ref.is_some_and(|local| target_host == local);
        let is_host_direct = policy.spec.host_direct.is_some();
        (Reverse(policy.spec.priority), !is_local, !is_host_direct, policy.metadata.name.clone())
    });
    if !viable.is_empty() {
        let selected = viable.remove(0);
        let selected_target = viable_targets.remove(&selected.metadata.name).expect("viable placement target was resolved");
        let mut viable_not_selected = Vec::with_capacity(viable.len());
        for policy in viable {
            let target_host = viable_targets.remove(&policy.metadata.name).expect("viable placement target was resolved");
            let reason = placement_ordering_reason(&selected, &selected_target, &policy, &target_host, local_host_ref);
            viable_not_selected.push(PlacementViableCandidate { policy_name: policy.metadata.name.clone(), target_host, reason });
        }
        return Ok(PlacementResolution { selected: Some(selected), refused_candidates, viable_not_selected, allocation: None });
    }

    let required_adapters = required_workflow_agent_adapters(workflow)?;
    if !required_adapters.is_empty() {
        let requirement = if required_adapters.len() == 1 {
            format!("adapter `{}`", required_adapters.first().expect("one required adapter"))
        } else {
            format!("adapters {}", required_adapters.iter().map(|adapter| format!("`{adapter}`")).collect::<Vec<_>>().join(", "))
        };
        if refused_candidates.is_empty() {
            return Err(format!("no placement policy satisfies {requirement}; candidates: (none)"));
        }
        refused_candidates.sort_by(|left, right| left.policy_name.cmp(&right.policy_name));
        let candidates = refused_candidates
            .iter()
            .map(|candidate| format!("- `{}`: {}", candidate.policy_name, candidate.reason))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("no placement policy satisfies {requirement}; candidates:\n{candidates}"));
    }

    if candidate_names.is_empty() {
        warn!(%namespace, "no placement policy found; convoy will remain Pending until one is registered");
    }
    Ok(PlacementResolution { selected: None, refused_candidates, viable_not_selected: Vec::new(), allocation: None })
}

fn placement_ordering_reason(
    selected: &ResourceObject<PlacementPolicy>,
    selected_target: &PlacementTargetHost,
    candidate: &ResourceObject<PlacementPolicy>,
    candidate_target: &PlacementTargetHost,
    local_host_ref: Option<&CanonicalHostId>,
) -> String {
    if candidate.spec.priority != selected.spec.priority {
        return format!(
            "priority {} is lower than selected policy `{}` priority {}",
            candidate.spec.priority, selected.metadata.name, selected.spec.priority
        );
    }

    let selected_is_local = local_host_ref.is_some_and(|local| &selected_target.reference == local);
    let candidate_is_local = local_host_ref.is_some_and(|local| &candidate_target.reference == local);
    if selected_is_local && !candidate_is_local {
        return format!("fallback ordering preferred local policy `{}`", selected.metadata.name);
    }
    if selected.spec.host_direct.is_some() && candidate.spec.host_direct.is_none() {
        return format!("fallback ordering preferred host-direct policy `{}`", selected.metadata.name);
    }
    format!("fallback ordering preferred policy `{}` by name", selected.metadata.name)
}

fn repo_identity_from_bag_or_path(path: &Path, bag: &EnvironmentBag) -> flotilla_protocol::RepoIdentity {
    bag.repo_identity().unwrap_or_else(|| fallback_repo_identity(path))
}

/// Resolve one remote independently; callers choose which remote's forge to carry.
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
) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
    let runner = environment_manager
        .environment_runner(environment_id)
        .ok_or_else(|| format!("command runner unavailable for environment {environment_id}"))?;
    let host_bag = environment_manager
        .environment_bag(environment_id)
        .ok_or_else(|| format!("discovery environment unavailable: {environment_id}"))?;
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn crate::providers::discovery::EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };
    let checkout = ExecutionEnvironmentPath::new(checkout_path);
    let mut bag = host_bag;
    for detector in &discovery.repo_detectors {
        bag = bag.extend(detector.detect(&checkout, &*runner, env).await);
    }
    let mut unmet = Vec::new();
    for factory in &discovery.factories.vcs {
        match factory.probe(&bag, config, &checkout, Arc::clone(&runner)).await {
            Ok(provider) => return Ok(provider),
            Err(requirements) => unmet.extend(requirements),
        }
    }
    Err(format!("no VCS provider discovered for {} in {environment_id}: {unmet:?}", checkout.as_path().display()))
}

#[allow(clippy::too_many_arguments)]
async fn discover_repo_for_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config: &ConfigStore,
    resource_backend: &ResourceBackend,
    namespace: &str,
    local_environment_id: &EnvironmentId,
    environment_id: &EnvironmentId,
    repo_path: &Path,
) -> Result<DiscoveryResult, String> {
    let mut host_bag =
        environment_manager.environment_bag(environment_id).ok_or_else(|| format!("environment not found: {environment_id}"))?;
    let runner =
        environment_manager.environment_runner(environment_id).ok_or_else(|| format!("environment runner not found: {environment_id}"))?;
    // Resolve the forge while the resource backend is available. Factories only
    // receive assertions, so their probe interface remains independent of storage.
    if let Ok(origin_url) = crate::providers::vcs::detection::origin_url(&*runner, repo_path).await {
        if let Some(remote) = crate::providers::discovery::detectors::git::remote_assertion(origin_url.trim(), "origin") {
            host_bag = host_bag.with(remote);
        }
        if let Some(spec) = forge_for_remote(resource_backend, namespace, origin_url.trim()).await? {
            host_bag = host_bag.with(crate::providers::discovery::EnvironmentAssertion::origin_forge(spec));
        }
    }
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

const SUPERSEDED_BY_ANNOTATION: &str = "flotilla.work/superseded-by";

#[derive(bon::Builder)]
struct ResolvedCrewContext {
    namespace: String,
    convoy: String,
    vessel_ref: String,
    vessel: String,
    caller_role: String,
    caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
}

#[derive(bon::Builder)]
struct CrewSupervisionRequest<'a> {
    namespace: &'a str,
    convoy_name: &'a str,
    vessel: &'a str,
    role: &'a str,
    operation: flotilla_protocol::CrewSupervisionAction,
    message: &'a str,
    actor_crew_id: Option<&'a str>,
    principal: Option<&'a PrincipalRef>,
}

struct DaemonTurnDeliveryActuator {
    daemon: Weak<InProcessDaemon>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDeliverySessionPlan {
    QueueWarm,
    QueueFresh,
    RestartFresh,
}

fn turn_delivery_session_plan(
    phase: Option<ResourceTerminalSessionPhase>,
    vessel: &str,
    role: &str,
) -> Result<TurnDeliverySessionPlan, String> {
    match phase {
        Some(ResourceTerminalSessionPhase::Running) => Ok(TurnDeliverySessionPlan::QueueWarm),
        Some(ResourceTerminalSessionPhase::Starting) | None => Ok(TurnDeliverySessionPlan::QueueFresh),
        Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost) => Ok(TurnDeliverySessionPlan::RestartFresh),
        Some(ResourceTerminalSessionPhase::Failed) => {
            Err(format!("turn-delivery target {vessel}/{role} failed provisioning and cannot be restarted"))
        }
    }
}

#[async_trait]
impl crate::leaf_engine::TurnDeliveryActuator for DaemonTurnDeliveryActuator {
    async fn deliver(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
        self.daemon.upgrade().ok_or_else(|| "daemon stopped before turn delivery".to_string())?.deliver_standing_turn(request).await
    }

    async fn hold(&self, request: &crate::leaf_engine::TurnDeliveryRequest, act: &HoldAct, reason: &str) -> Result<(), String> {
        self.daemon
            .upgrade()
            .ok_or_else(|| "daemon stopped before turn-delivery hold".to_string())?
            .execute_turn_delivery_hold(request, act, reason)
            .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewRoutingContext {
    pub command_context: CrewCommandContext,
    pub session_name: Option<String>,
    pub convoy: String,
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

fn handoff_crew_brief(
    context: &ResolvedCrewContext,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    target: &str,
    prompt: Option<&str>,
    members: &[CrewListMember],
    requirement: &flotilla_resources::VesselRequirement,
    render_options: &crate::agent_adapter::CrewBriefRenderOptions,
) -> Result<TerminalBrief, String> {
    let repository_refs = requirement
        .repository_refs
        .clone()
        .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
    let assignment = match prompt {
        Some(prompt) => crate::agent_adapter::CrewAssignment::Prompt(prompt),
        None if !convoy.spec.issues.is_empty() => crate::agent_adapter::CrewAssignment::CarriedIssue,
        None if convoy.spec.change_request.is_some() => crate::agent_adapter::CrewAssignment::CarriedChangeRequest,
        None => crate::agent_adapter::CrewAssignment::Unassigned,
    };
    let brief = crate::agent_adapter::build_convoy_crew_brief_with_options(
        convoy,
        &TerminalCrewContext {
            namespace: context.namespace.clone(),
            convoy: context.convoy.clone(),
            vessel_ref: context.vessel_ref.clone(),
        },
        &context.vessel,
        target,
        assignment,
        &members
            .iter()
            .map(|member| crate::agent_adapter::CrewBriefMember {
                role: member.role.clone(),
                state: if member.role == target { "active".to_string() } else { member.state.clone() },
                is_agent: member.kind == "agent",
            })
            .collect::<Vec<_>>(),
        render_options,
    );
    let mut brief = brief?;
    crate::agent_adapter::append_convoy_work_context(&mut brief.content, convoy, &repository_refs, &requirement.credential_scopes);
    Ok(brief)
}

async fn crew_brief_repo_roots(
    backend: &ResourceBackend,
    namespace: &str,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    repository_refs: &[RepositoryKey],
) -> Vec<PathBuf> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let mut roots = Vec::new();
    for repository_ref in repository_refs {
        let Some(checkout_ref) = convoy.spec.adopted_checkout_refs.get(repository_ref) else {
            continue;
        };
        let Ok(checkout) = checkouts.get(checkout_ref).await else {
            continue;
        };
        let Some(path) =
            checkout.status.as_ref().and_then(|status| status.path.clone()).or_else(|| checkout.spec.target_path().map(str::to_string))
        else {
            continue;
        };
        roots.push(PathBuf::from(path));
    }
    roots
}

fn safe_header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '[' => '(',
            ']' => ')',
            '·' => '-',
            '`' => '\'',
            character if character.is_control() || (character.is_whitespace() && character != ' ') => ' ',
            character => character,
        })
        .collect()
}

fn crew_message_header(sender: &CrewMessageSender) -> String {
    match sender {
        CrewMessageSender::Unknown => "unknown sender · message".to_string(),
        CrewMessageSender::FlotillaNudge => "flotilla · nudge · reply by running `crew complete` or `crew fail`".to_string(),
        CrewMessageSender::FlotillaTurn { source } => {
            format!("flotilla · turn: {} · reply by running `crew complete`", safe_header_value(source))
        }
        CrewMessageSender::FlotillaEscalation { from } => {
            format!("flotilla · escalated from {} · supervise the stalled crew", safe_header_value(from))
        }
        CrewMessageSender::OperatorResume { principal } => principal.as_ref().map_or_else(
            || "operator (unattributed) · via convoy resume".to_string(),
            |principal| format!("operator {} · via convoy resume", safe_header_value(&principal.name)),
        ),
        CrewMessageSender::OperatorFollowUp { principal } => format!(
            "operator {} · follow-up brief · reply by running `crew complete`",
            principal.as_ref().map_or_else(|| "(unattributed)".to_string(), |principal| safe_header_value(&principal.name))
        ),
        CrewMessageSender::Governor { name } => {
            format!("governor {} · guidance for your stalled work · reply by running `crew complete`", safe_header_value(name))
        }
        CrewMessageSender::Bosun { name } => {
            format!("bosun {} · guidance for your stalled work · reply by running `crew complete`", safe_header_value(name))
        }
        CrewMessageSender::Handoff { from } => format!("handoff from {}", safe_header_value(from)),
    }
}

fn frame_crew_message(sender: &CrewMessageSender, body: &str) -> String {
    format!("[{}]\n\n{body}", crew_message_header(sender))
}

fn pending_crew_message(sender: CrewMessageSender, body: &str) -> TerminalCrewMessage {
    TerminalCrewMessage {
        id: uuid::Uuid::new_v4().to_string(),
        text: frame_crew_message(&sender, body),
        sender,
        delivery: CrewMessageDelivery::Queued,
    }
}

fn ensure_crew_work_is_defined(
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    context: &ResolvedCrewContext,
) -> Result<(), String> {
    let known_agent = convoy
        .status
        .as_ref()
        .and_then(|status| status.crew_work.get(&context.vessel))
        .is_some_and(|crew| crew.contains_key(&context.caller_role));
    if known_agent {
        Ok(())
    } else {
        Err(format!("crew work for role `{}` is not defined on vessel `{}`", context.caller_role, context.vessel))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyResumeOutcome {
    Delivered { displaced: Option<String> },
    Queued { displaced: Option<String> },
}

type ConvoyMessageKey = (String, String);
type ConvoyMessageLock = Arc<Mutex<()>>;
type WeakConvoyMessageLock = Weak<Mutex<()>>;

fn crew_handoff_address_error(target: &str, vessel: &str) -> String {
    format!(
        "no such crew member in your vessel; crew messaging is intra-vessel and requires a different crew member (target `{target}`, vessel `{vessel}`)"
    )
}

fn terminal_meta_with_vessel_credentials(mut meta: InputMeta, requirement: &flotilla_resources::VesselRequirement) -> InputMeta {
    if !requirement.credential_refs.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_REFS_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_refs).expect("credential names serialize"),
        );
    }
    if !requirement.credential_scopes.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_SCOPES_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_scopes).expect("credential scopes serialize"),
        );
    }
    if !requirement.credential_permissions.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_PERMISSIONS_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_permissions).expect("credential permissions serialize"),
        );
    }
    meta
}

async fn queue_pending_crew_message(
    sessions: &flotilla_resources::TypedResolver<ResourceTerminalSession>,
    existing: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    sender: CrewMessageSender,
    message: &str,
) -> Result<(), String> {
    let mut spec = existing.spec.clone();
    let TerminalSessionSource::Agent { message: pending, .. } = &mut spec.source else {
        return Err(format!("crew target `{}` is not an agent session", existing.spec.role));
    };
    *pending = Some(pending_crew_message(sender, message));
    sessions
        .update(&input_meta_from_resource(existing), &existing.metadata.resource_version, &spec)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[derive(bon::Builder)]
struct ConvoyStartTask {
    command_id: u64,
    intent: flotilla_protocol::ConvoyStartIntent,
    key: ConvoyStartKey,
    dispatching_principal_ref: PrincipalRef,
}

#[derive(bon::Builder)]
struct ConvoyAdmission {
    name: String,
    spec: ConvoySpec,
    workflow: WorkflowTemplateSpec,
    placement_policy: Option<PlacementPolicySpec>,
    placement_decision: Option<PlacementDecision>,
    #[builder(default)]
    vessel_placements: BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>,
}

#[derive(Clone, Debug)]
struct AllocationRole {
    crew: CrewSpec,
    hint: String,
    repository_refs: Option<Vec<RepositoryKey>>,
    depends_on: Vec<String>,
    credential_signature: String,
}

fn expand_allocation_roles(workflow: &mut WorkflowTemplateSpec, project: &ProjectSpec) -> Result<Vec<AllocationRole>, String> {
    let mut roles = Vec::new();
    if !workflow.roles.is_empty() {
        let mut declared = BTreeSet::new();
        for crew in &workflow.roles {
            if !declared.insert(crew.role.as_str()) {
                return Err(format!("workflow roles declare `{}` more than once", crew.role));
            }
        }
        let mut hinted = BTreeSet::new();
        for crew in workflow.vessels.iter().flat_map(|vessel| &vessel.crew) {
            if !declared.contains(crew.role.as_str()) {
                return Err(format!("vessel hint includes role `{}` absent from workflow roles", crew.role));
            }
            if !hinted.insert(crew.role.as_str()) {
                return Err(format!("more than one vessel hint includes role `{}`", crew.role));
            }
        }
    }
    let authored = if workflow.roles.is_empty() {
        workflow.vessels.clone()
    } else {
        workflow
            .roles
            .iter()
            .map(|crew| {
                workflow.vessels.iter().find(|hint| hint.crew.iter().any(|member| member.role == crew.role)).map_or_else(
                    || VesselRequirement::builder().name(crew.role.clone()).crew(vec![crew.clone()]).build(),
                    |hint| VesselRequirement { crew: vec![crew.clone()], ..hint.clone() },
                )
            })
            .collect()
    };
    for vessel in authored {
        for crew in vessel.crew {
            let matrix = crew.needs.contains(&CapabilityNeed::Platform("$matrix".to_string()))
                || project.role_needs.get(&crew.role).is_some_and(|needs| needs.contains(&CapabilityNeed::Platform("$matrix".to_string())));
            if matrix {
                if project.platform_matrix.is_empty() {
                    return Err(format!("role `{}` needs platform:$matrix but Project has no platform_matrix", crew.role));
                }
                let mut seen = BTreeSet::new();
                for platform in &project.platform_matrix {
                    let need = format!("platform:{platform}").parse::<CapabilityNeed>()?;
                    if !seen.insert(platform) {
                        continue;
                    }
                    let mut expanded = crew.clone();
                    expanded.needs.remove(&CapabilityNeed::Platform("$matrix".to_string()));
                    expanded.needs.insert(need);
                    if let Some(standing) = project.role_needs.get(&crew.role) {
                        expanded
                            .needs
                            .extend(standing.iter().filter(|need| **need != CapabilityNeed::Platform("$matrix".to_string())).cloned());
                    }
                    roles.push(AllocationRole {
                        crew: expanded,
                        hint: format!("{}[{platform}]", crew.role),
                        repository_refs: vessel.repository_refs.clone().or_else(|| workflow.repository_refs.clone()),
                        depends_on: vessel.depends_on.clone(),
                        credential_signature: String::new(),
                    });
                }
            } else {
                roles.push(AllocationRole {
                    crew,
                    hint: vessel.name.clone(),
                    repository_refs: vessel.repository_refs.clone().or_else(|| workflow.repository_refs.clone()),
                    depends_on: vessel.depends_on.clone(),
                    credential_signature: String::new(),
                });
            }
        }
    }
    workflow.roles.clear();
    workflow.vessels = roles
        .iter()
        .map(|role| {
            VesselRequirement::builder()
                .name(role.hint.clone())
                .crew(vec![role.crew.clone()])
                .maybe_repository_refs(role.repository_refs.clone())
                .build()
        })
        .collect();
    Ok(roles)
}

fn allocate_roles(workflow: &mut WorkflowTemplateSpec, roles: &[AllocationRole]) -> Result<(), String> {
    let mut groups: Vec<Vec<&AllocationRole>> = Vec::new();
    for role in roles {
        let group = groups.iter_mut().find(|group| {
            let first = group[0];
            (first.crew.needs == role.crew.needs
                || (first.hint == role.hint
                    && first.crew.needs.iter().any(|need| role.crew.needs.iter().any(|other| need.conflicts_with(other)))))
                && first.credential_signature == role.credential_signature
                && first.repository_refs == role.repository_refs
                && !group.iter().any(|other| other.crew.role == role.crew.role)
        });
        if let Some(group) = group {
            group.push(role);
        } else {
            groups.push(vec![role]);
        }
    }
    let mut used_names = BTreeSet::new();
    let mut vessels = Vec::new();
    let mut allocation = Vec::new();
    let mut hint_to_vessels = BTreeMap::<String, BTreeSet<String>>::new();
    let mut role_to_vessels = BTreeMap::<String, BTreeSet<String>>::new();
    for group in &groups {
        let shared_hint = group.iter().all(|role| role.hint == group[0].hint);
        let mut name = if shared_hint { group[0].hint.clone() } else { group[0].crew.role.clone() };
        if used_names.contains(&name) {
            let base = name.clone();
            let mut index = 2;
            while used_names.contains(&name) {
                name = format!("{base}-{index}");
                index += 1;
            }
        }
        used_names.insert(name.clone());
        for role in group {
            hint_to_vessels.entry(role.hint.clone()).or_default().insert(name.clone());
            role_to_vessels.entry(role.crew.role.clone()).or_default().insert(name.clone());
        }
        vessels.push(
            VesselRequirement::builder()
                .name(name.clone())
                .crew(group.iter().map(|role| role.crew.clone()).collect())
                .maybe_repository_refs(group[0].repository_refs.clone())
                .build(),
        );
        allocation.push(AllocationDecision {
            vessel: name,
            roles: group.iter().map(|role| role.crew.role.clone()).collect(),
            reason: if group.len() > 1 && group.iter().any(|role| role.crew.needs != group[0].crew.needs) {
                "legacy grouping hint retained for placement; split if its needs cannot be covered".to_string()
            } else if group.len() > 1 {
                "equal needs and credential grants; sharing reduces vessel and handoff cost".to_string()
            } else {
                "separate needs, credential grants, or platform matrix".to_string()
            },
            crossed_handoffs: Vec::new(),
        });
    }
    let mut add_edge = |from: &str, to: &str, label: &str| {
        if from == to {
            return;
        }
        if let Some(vessel) = vessels.iter_mut().find(|vessel| vessel.name == to) {
            if !vessel.depends_on.iter().any(|dependency| dependency == from) {
                vessel.depends_on.push(from.to_string());
            }
        }
        if let Some(decision) = allocation.iter_mut().find(|decision| decision.vessel == to) {
            decision.crossed_handoffs.push(label.to_string());
        }
    };
    for role in roles {
        for dependency in &role.depends_on {
            if let (Some(from), Some(to)) = (hint_to_vessels.get(dependency), hint_to_vessels.get(&role.hint)) {
                for from in from {
                    for to in to {
                        add_edge(from, to, &format!("{dependency} -> {}", role.hint));
                    }
                }
            }
        }
    }
    for RoleHandoff { from, to } in &workflow.handoffs {
        let sources = role_to_vessels.get(from).ok_or_else(|| format!("handoff source role `{from}` is absent"))?;
        let targets = role_to_vessels.get(to).ok_or_else(|| format!("handoff target role `{to}` is absent"))?;
        for source in sources {
            for target in targets {
                if source != target {
                    if let Some(decision) = allocation.iter_mut().find(|decision| decision.vessel == *target) {
                        decision.crossed_handoffs.push(format!("{from} -> {to}"));
                    }
                }
            }
        }
    }
    for (source, rule) in &mut workflow.turn_delivery {
        if let Some(names) = role_to_vessels.get(&rule.to.role) {
            let name = names
                .iter()
                .find(|name| *name == &rule.to.vessel)
                .or_else(|| (names.len() == 1).then(|| names.iter().next()).flatten())
                .ok_or_else(|| {
                    format!(
                        "turn delivery `{source}` targets role `{}` in multiple vessels ({}); name one concrete vessel",
                        rule.to.role,
                        names.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                })?;
            rule.to.vessel = name.clone();
        }
    }
    if let Some(targets) = &mut workflow.supervision {
        *targets = targets
            .iter()
            .flat_map(|target| match target {
                SupervisionTarget::ConvoyCrew { role, .. } => role_to_vessels
                    .get(role)
                    .into_iter()
                    .flat_map(|names| names.iter())
                    .map(|name| SupervisionTarget::ConvoyCrew { vessel: name.clone(), role: role.clone() })
                    .collect::<Vec<_>>(),
                _ => vec![target.clone()],
            })
            .collect();
    }
    let nudges = std::mem::take(&mut workflow.stall_nudges);
    for (address, policy) in nudges {
        if let Some((_, role)) = address.split_once('/') {
            if let Some(names) = role_to_vessels.get(role) {
                for name in names {
                    workflow.stall_nudges.insert(format!("{name}/{role}"), policy.clone());
                }
                continue;
            }
        }
        workflow.stall_nudges.insert(address, policy);
    }
    workflow.vessels = vessels;
    workflow.allocation = allocation;
    refresh_crossed_handoffs(workflow);
    Ok(())
}

fn refresh_crossed_handoffs(workflow: &mut WorkflowTemplateSpec) {
    let mut crossed = BTreeMap::<String, BTreeSet<String>>::new();
    for vessel in &workflow.vessels {
        for dependency in &vessel.depends_on {
            if dependency != &vessel.name {
                crossed.entry(vessel.name.clone()).or_default().insert(format!("{dependency} -> {}", vessel.name));
            }
        }
    }
    for handoff in &workflow.handoffs {
        for source in workflow.vessels.iter().filter(|vessel| vessel.crew.iter().any(|crew| crew.role == handoff.from)) {
            for target in workflow.vessels.iter().filter(|vessel| vessel.crew.iter().any(|crew| crew.role == handoff.to)) {
                if source.name != target.name {
                    crossed.entry(target.name.clone()).or_default().insert(format!("{} -> {}", handoff.from, handoff.to));
                }
            }
        }
    }
    for decision in &mut workflow.allocation {
        decision.crossed_handoffs = crossed.remove(&decision.vessel).unwrap_or_default().into_iter().collect();
    }
}

async fn allocation_credential_grants(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: &str,
    repositories: &[ConvoyRepositorySpec],
    vessels: &[VesselRequirement],
) -> Result<Vec<BTreeSet<String>>, String> {
    let grants = backend
        .including_replicas::<CredentialGrant>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential grants: {error}"))?;
    let repository_trust = backend
        .including_replicas::<Repository>(namespace)
        .list()
        .await
        .map_err(|error| format!("list repositories for credential grants: {error}"))?
        .items
        .into_iter()
        .map(|source| {
            (
                RepositoryKey(source.object.metadata.name),
                if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let all_repositories = repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<BTreeSet<_>>();
    vessels
        .iter()
        .map(|vessel| {
            let keys = vessel
                .repository_refs
                .as_ref()
                .map(|keys| keys.iter().cloned().collect::<BTreeSet<_>>())
                .unwrap_or_else(|| all_repositories.clone());
            let trust = keys
                .iter()
                .map(|key| {
                    repository_trust
                        .get(key)
                        .copied()
                        .map(|trust| (key.clone(), trust))
                        .ok_or_else(|| format!("repository `{key}` unavailable for credential grant selection"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            Ok(grants
                .items
                .iter()
                .filter(|source| source.object.spec.selector.matches(Some(project_ref), &trust, &vessel.crew[0].role))
                .map(|source| source.object.metadata.name.clone())
                .collect())
        })
        .collect()
}

fn convoy_record_name() -> String {
    format!("convoy-{}", uuid::Uuid::new_v4().simple())
}

fn convoy_ensure_name(project: &str, role: &str) -> String {
    let digest = Sha256::digest(format!("{project}\0{role}").as_bytes());
    format!("ensure-{digest:x}")
}

fn convoy_address(role: &str, project: Option<&str>) -> String {
    project.map_or_else(|| role.to_string(), |project| format!("{role}@{project}"))
}

fn convoy_disambiguation_address(role: &str, project: Option<&str>) -> String {
    format!("{role}@{}", project.unwrap_or_default())
}

/// The stable human-facing address of a convoy role, independent of any one
/// generation's resource record name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleAddress {
    pub project: String,
    pub role: String,
}

impl FromStr for RoleAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((role, project)) = value.split_once('@') else {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        };
        if role.is_empty() || project.is_empty() || project.contains('@') {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        }
        Ok(Self { project: project.to_string(), role: role.to_string() })
    }
}

impl fmt::Display for RoleAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.role, self.project)
    }
}

/// A resolved, currently-live convoy generation. Callers route by owner and
/// select sessions by `record_name`; neither operation accepts a raw role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConvoyRecord {
    pub address: RoleAddress,
    pub record_name: String,
    pub owner_host: HostName,
}

async fn allocate_convoy_generation(backend: &ResourceBackend, namespace: &str, project: Option<&str>, role: &str) -> Result<u64, String> {
    let generations = backend.including_replicas::<ResourceConvoy>(namespace).list().await.map_err(|error| error.to_string())?;
    let mut maximum = 0;
    for source in generations
        .items
        .into_iter()
        .filter(|source| source.object.spec.project_ref.as_deref() == project && source.object.spec.role == role)
    {
        let convoy = source.object;
        let generation =
            convoy.metadata.labels.get(GENERATION_LABEL).and_then(|value| value.parse::<u64>().ok()).unwrap_or(convoy.spec.generation);
        maximum = maximum.max(generation);
        let live = convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal());
        if live {
            let provenance = match source.provenance {
                ResourceProvenance::Local => String::new(),
                ResourceProvenance::Replica { origin_root, last_synced_at } => {
                    format!(" (as of root {origin_root}, last synced {last_synced_at})")
                }
            };
            return Err(format!("live convoy {} generation {generation} already exists{provenance}", convoy_address(role, project)));
        }
    }
    let generation =
        maximum.checked_add(1).ok_or_else(|| format!("convoy {} exhausted its generation counter", convoy_address(role, project)))?;
    Ok(generation)
}

fn parse_role_address(value: &str) -> Result<(&str, Option<&str>), String> {
    match value.split_once('@') {
        Some((role, project)) if !role.is_empty() && !project.contains('@') => Ok((role, Some(project))),
        Some(_) => Err(format!("invalid convoy address `{value}`: expected role@project")),
        None if value.is_empty() => Err("convoy role cannot be empty".to_string()),
        None => Ok((value, None)),
    }
}

struct ConvoyAddressIdentity<'a> {
    record_name: &'a str,
    role: Option<&'a str>,
    project: Option<&'a str>,
    terminal: bool,
}

fn resolve_convoy_candidate_indices(identities: &[ConvoyAddressIdentity<'_>], address: &str) -> Result<Vec<usize>, String> {
    let exact = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| (identity.record_name == address).then_some(index))
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return Ok(exact);
    }

    let (role, project) = parse_role_address(address)?;
    let matching = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| {
            (identity.role == Some(role) && project.is_none_or(|project| identity.project.unwrap_or_default() == project)).then_some(index)
        })
        .collect::<Vec<_>>();
    let (live, terminal): (Vec<_>, Vec<_>) = matching.into_iter().partition(|index| !identities[*index].terminal);
    let candidates = if live.is_empty() { terminal } else { live };
    let record_names = candidates.iter().map(|index| identities[*index].record_name).collect::<BTreeSet<_>>();
    if record_names.len() <= 1 {
        return Ok(candidates);
    }

    let address_options = candidates
        .iter()
        .filter_map(|index| identities[*index].role.map(|role| convoy_disambiguation_address(role, identities[*index].project)))
        .collect::<BTreeSet<_>>();
    if candidates.iter().all(|index| identities[*index].terminal) && address_options.len() == 1 {
        return Err(format!(
            "convoy address `{address}` matches multiple terminal records; use an exact record name: {}",
            record_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if address_options.len() > 1 {
        return Err(format!(
            "convoy role `{role}` is ambiguous; use one of: {}",
            address_options.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Err(format!(
        "convoy address `{address}` matches multiple records; use an exact record name: {}",
        record_names.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

async fn resolve_local_convoy_name(backend: &ResourceBackend, namespace: &str, address: &str) -> Result<String, String> {
    let convoys = backend.clone().using::<ResourceConvoy>(namespace);
    let candidates = convoys.list().await.map_err(|error| error.to_string())?.items;
    let identities = candidates
        .iter()
        .map(|convoy| ConvoyAddressIdentity {
            record_name: &convoy.metadata.name,
            role: convoy.metadata.labels.get(ROLE_LABEL).map(String::as_str),
            project: convoy.metadata.labels.get(PROJECT_LABEL).map(String::as_str),
            terminal: convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
        })
        .collect::<Vec<_>>();
    let selected = resolve_convoy_candidate_indices(&identities, address)?;
    match selected.as_slice() {
        [index] => Ok(candidates[*index].metadata.name.clone()),
        [] => Err(format!("no convoy matches `{address}`")),
        _ => Err(format!("convoy record `{address}` is present from multiple sources")),
    }
}

#[derive(bon::Builder)]
struct ConvoySnapshotBundle<'a> {
    spec: &'a ConvoySpec,
    workflow: &'a WorkflowTemplateSpec,
    placement: Option<&'a PlacementPolicySpec>,
    placement_decision: Option<PlacementDecision>,
    vessel_placements: Option<&'a BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>>,
}

/// An issue body is the crew's contract, so admission may only reuse a
/// recently observed snapshot. Keep this deliberately fixed until an
/// operational need establishes that it should be configurable.
const ISSUE_SNAPSHOT_FRESHNESS: ChronoDuration = ChronoDuration::minutes(5);
fn issue_snapshot_is_fresh(issue: &flotilla_protocol::Issue) -> bool {
    let Some(observed_at) = issue.observed_at else { return false };
    let age = Utc::now().signed_duration_since(observed_at);
    (ChronoDuration::zero()..=ISSUE_SNAPSHOT_FRESHNESS).contains(&age)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ConvoyStartKey {
    namespace: String,
    project_ref: String,
    subject: ConvoyStartSubject,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ConvoyStartSubject {
    ChangeRequest(String),
    Issues(Vec<flotilla_protocol::IssueSelector>),
    Name(String),
    Anonymous {
        branch: Option<String>,
        workflow_ref: Option<String>,
        inputs: Vec<(String, String)>,
        instruction: Option<String>,
        placement_policy: Option<String>,
    },
}

impl ConvoyStartKey {
    fn new(namespace: String, intent: &flotilla_protocol::ConvoyStartIntent) -> Self {
        let subject = if let Some(change_request) = &intent.change_request {
            ConvoyStartSubject::ChangeRequest(change_request.clone())
        } else if intent.issues.is_empty() {
            match &intent.name {
                Some(name) => ConvoyStartSubject::Name(name.clone()),
                None => ConvoyStartSubject::Anonymous {
                    branch: intent.branch.clone(),
                    workflow_ref: intent.workflow_ref.clone(),
                    inputs: intent.inputs.clone(),
                    instruction: intent.instruction.clone(),
                    placement_policy: intent.placement_policy.clone(),
                },
            }
        } else {
            ConvoyStartSubject::Issues(intent.issues.clone())
        };
        Self { namespace, project_ref: intent.project_ref.clone(), subject }
    }
}

struct ResolvedConvoyChangeRequestAdmission {
    binding: BoundChangeRequest,
    branch: String,
    base_ref: String,
}

struct RepositoryChangeRequestProvider {
    service_url: String,
    repository: String,
    provider: Arc<dyn ChangeRequestTracker>,
}

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
        flotilla_resources::ConvoyPhase::Cancelled => Some(format!("convoy {identity} was cancelled while starting")),
        flotilla_resources::ConvoyPhase::Pending
        | flotilla_resources::ConvoyPhase::Active
        | flotilla_resources::ConvoyPhase::Interrupted
        | flotilla_resources::ConvoyPhase::Anchored
        | flotilla_resources::ConvoyPhase::Landing
        | flotilla_resources::ConvoyPhase::Landed
        | flotilla_resources::ConvoyPhase::Abandoned => None,
    }
}

fn checkout_path(checkout: &ResourceObject<ResourceCheckout>) -> Option<&str> {
    checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec)
}

fn condition_is_true(condition: &IntegrationCondition) -> bool {
    condition.value == ConditionValue::True
}

fn integration_condition_is_fresh(condition: &IntegrationCondition, now: chrono::DateTime<Utc>) -> bool {
    condition
        .observed_at
        .as_deref()
        .and_then(|observed_at| chrono::DateTime::parse_from_rfc3339(observed_at).ok())
        .and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok())
        .is_some_and(|age| age < LANDING_EVIDENCE_TTL)
}

fn condition_problem(label: &str, condition: &IntegrationCondition) -> Option<String> {
    match condition.value {
        ConditionValue::True => None,
        ConditionValue::False => Some(format!("{label}=False{}", condition_detail_suffix(condition))),
        ConditionValue::Unknown => Some(format!("{label}=Unknown{}", condition_detail_suffix(condition))),
    }
}

fn condition_detail_suffix(condition: &IntegrationCondition) -> String {
    if condition.details.is_empty() {
        String::new()
    } else {
        format!(" ({})", condition.details.join(", "))
    }
}

fn checkout_integration_summary(checkout: &ResourceObject<ResourceCheckout>, integration: &CheckoutIntegrationStatus) -> Option<String> {
    let problems = [
        condition_problem("Clean", &integration.clean),
        condition_problem("Pushed", &integration.pushed),
        condition_problem("Landed", &integration.landed),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if problems.is_empty() {
        None
    } else {
        Some(format!("{} [{}]: {}", checkout.metadata.name, checkout_path(checkout).unwrap_or("<unknown path>"), problems.join("; ")))
    }
}

fn associated_change_request_name_without_checkout_status(
    convoy: &ResourceObject<ResourceConvoy>,
    checkout: &ResourceObject<ResourceCheckout>,
) -> Result<Option<String>, String> {
    let Some(repository) = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()) else {
        return Ok(None);
    };
    if let Some(id) = convoy_change_request_id_for_checkout(convoy, checkout) {
        let LeafAddress::ChangeRequest { service, scope, number } = change_request_address(&repository.url, &id)? else {
            unreachable!("change_request_address always returns a change-request address")
        };
        return Ok(Some(change_request_record_name(&service, &scope, number)));
    }

    // A produced subject can retain PR identity after checkout status is gone.
    // Require exactly one current PR for the repository if nothing singles one out.
    let LeafAddress::ChangeRequest { service, scope, .. } = change_request_address(&repository.url, "1")? else {
        unreachable!("change_request_address always returns a change-request address")
    };
    let names = expected_change_request_leaves(convoy, &BTreeMap::new())?
        .into_iter()
        .filter_map(|leaf| match leaf.address {
            LeafAddress::ChangeRequest { service: candidate_service, scope: candidate_scope, number }
                if candidate_service == service && candidate_scope == scope =>
            {
                Some(change_request_record_name(&service, &scope, number))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    if names.len() == 1 {
        Ok(names.into_iter().next())
    } else {
        Ok(None)
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

fn managed_terminal_changes(
    previous: Option<&HashMap<flotilla_protocol::AttachableId, ManagedTerminal>>,
    current: &HashMap<flotilla_protocol::AttachableId, ManagedTerminal>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, terminal) in current {
        let op = match previous.and_then(|terminals| terminals.get(key)) {
            Some(previous) if previous == terminal => continue,
            Some(_) => EntryOp::Updated(terminal.clone()),
            None => EntryOp::Added(terminal.clone()),
        };
        changes.push(Change::ManagedTerminal { key: key.clone(), op });
    }
    if let Some(previous) = previous {
        for key in previous.keys().filter(|key| !current.contains_key(*key)) {
            changes.push(Change::ManagedTerminal { key: key.clone(), op: EntryOp::Removed });
        }
    }
    changes
}

#[async_trait::async_trait]
impl crate::vcs::CheckoutVcsResolver for InProcessDaemon {
    async fn vcs_for(&self, environment: Option<&EnvironmentId>, path: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        self.vcs_for_checkout(environment.unwrap_or(&self.local_environment_id), path).await
    }
}

type CheckoutVcsCache = HashMap<(EnvironmentId, PathBuf), Arc<tokio::sync::OnceCell<Arc<dyn crate::vcs::Vcs>>>>;

pub struct InProcessDaemon {
    repos: RwLock<HashMap<flotilla_protocol::RepoIdentity, RepoState>>,
    repo_order: RwLock<Vec<flotilla_protocol::RepoIdentity>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    event_sink: Arc<dyn EventSink>,
    config: Arc<ConfigStore>,
    next_command_id: AtomicU64,
    node_id: NodeId,
    host_name: HostName,
    /// Maps local tracked paths (including virtual synthetic paths) to RepoIdentity.
    // Lock ordering: do not hold path_identities across awaits that later take
    // repos/repo_order; add_repo intentionally takes it last while already
    // holding those write locks.
    path_identities: RwLock<HashMap<PathBuf, flotilla_protocol::RepoIdentity>>,
    /// Repository identity last projected for each local tracked path.
    /// Mutated under `observed_checkout_reconciliation` so removal deletes
    /// observations using the identity that originally created them.
    repository_keys_by_path: RwLock<HashMap<PathBuf, RepositoryKey>>,
    repository_change_requests: RwLock<HashMap<RepositoryKey, RepositoryChangeRequestProvider>>,
    change_request_observation_source: Arc<ProviderChangeRequestObservationSource>,
    host_registry: crate::host_registry::HostRegistry,
    local_environment_id: EnvironmentId,
    environment_manager: Arc<EnvironmentManager>,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: DiscoveryRuntime,
    /// VCS capabilities are selected once for each checkout in its execution environment.
    checkout_vcs: Mutex<CheckoutVcsCache>,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    self_weak: Weak<InProcessDaemon>,
    pending_convoy_starts: Mutex<HashSet<ConvoyStartKey>>,
    ensure_admission_retries: Mutex<HashMap<(String, String), EnsureAdmissionRetry>>,
    /// Keep periodic and explicit ensure passes in one transaction, including
    /// status reads, backing inspection, admission, and status publication.
    ensure_reconciliation: Mutex<()>,
    /// Serializes pending-brief state with its terminal-session delivery side effect.
    convoy_message_locks: Mutex<HashMap<ConvoyMessageKey, WeakConvoyMessageLock>>,
    /// Serializes the identity selector check with Convoy creation. The owner
    /// host is the admission authority, so this is the local transaction that
    /// enforces one live generation per `{project, role}`.
    convoy_admission: Mutex<()>,
    brief_artifact_writer: RwLock<Option<Arc<dyn BriefArtifactWriter>>>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: crate::agents::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
    resource_backend: ResourceBackend,
    clock: Arc<dyn Clock>,
    regard_lifecycle: RegardLifecycle,
    observed_resource_backend: ResourceBackend,
    /// Serializes observed Checkout publication with repository removal so a
    /// refresh captured before untracking cannot recreate deleted resources.
    observed_checkout_reconciliation: Mutex<()>,
    aggregator_projection_state: AggregatorProjectionState,
    /// Provisioning namespace used by daemon-side resource operations (e.g.
    /// looking up the Convoy whose task is being marked complete). Set by the
    /// daemon runtime at startup; defaults to [`DEFAULT_PROVISIONING_NAMESPACE`].
    provisioning_namespace: std::sync::RwLock<String>,
    fleet: FleetService,
    repository_inspector: RwLock<Option<Arc<dyn RepositoryInspector>>>,
    operator_reconciler: RwLock<Option<Arc<dyn OperatorReconciler>>>,
    work_credential_reconciler: RwLock<Option<Arc<dyn WorkCredentialReconciler>>>,
    local_placement_provider_statuses: RwLock<Vec<HostProviderStatus>>,
    /// Last terminal state published per repository, used to emit field-scoped
    /// deltas without disturbing unrelated provider snapshot state.
    managed_terminals_by_repo: RwLock<HashMap<RepoIdentity, HashMap<flotilla_protocol::AttachableId, ManagedTerminal>>>,
    /// Filesystem path whose capacity governs convoy admission on this host.
    ///
    /// The daemon runtime sets this to the host-direct checkout root. Keeping
    /// the path here makes the local gate and the capacity published to peers
    /// use the same measurement basis even when daemon state is on another
    /// mount.
    admission_free_space_path: std::sync::RwLock<PathBuf>,
    leaf_subscriptions: LeafSubscriptionTable,
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

const ENSURE_BACKOFF_RESET_AFTER: ChronoDuration = ChronoDuration::minutes(10);
const ENSURE_MAX_CONSECUTIVE_FAILURES: u32 = 3;
const ENSURE_ESCALATION_AFTER: ChronoDuration = ChronoDuration::minutes(15);
const ENSURE_HOLD_ATTENTION_PREFIX: &str = "ensure-attention-";
const RECLAIM_REFUSAL_REASON_ANNOTATION: &str = "flotilla.work/reclaim-refusal-reason";
pub const BRIEF_ARTIFACTS_ANNOTATION: &str = "flotilla.work/brief-artifacts";

#[async_trait]
pub trait BriefArtifactWriter: Send + Sync {
    async fn put_brief(&self, namespace: &str, convoy: &str, role: &str, subject: &str, content: &[u8]) -> Result<String, String>;
}

#[derive(Debug, Clone)]
struct EnsureAdmissionRetry {
    config_hash: String,
    dependency_hash: String,
    retry: ControllerRetry,
}

fn ensure_retry_delay(restart_count: u32) -> ChronoDuration {
    ChronoDuration::from_std(
        RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(15 * 60) }.delay(restart_count.saturating_add(1)),
    )
    .expect("ensure retry delay fits chrono")
}

fn record_ensure_admission_retry(
    retries: &mut HashMap<(String, String), EnsureAdmissionRetry>,
    key: (String, String),
    config_hash: String,
    dependency_hash: String,
    now: DateTime<Utc>,
    persisted: Option<&ControllerRetry>,
) -> (u32, DateTime<Utc>) {
    let previous = retries.get(&key).map(|entry| &entry.retry).or(persisted);
    let retry =
        ControllerRetry::retryable(previous, now, RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) });
    let result = (retry.attempts, retry.next_attempt_at().expect("new admission retry is retryable"));
    retries.insert(key, EnsureAdmissionRetry { config_hash, dependency_hash, retry });
    result
}

fn ensure_config_hash(spec: &ConvoyEnsureSpec) -> Result<String, String> {
    let encoded = serde_json::to_vec(spec).map_err(|error| format!("serialize ensure config: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Verifies the provider backing of a terminal standing convoy before the
/// ensure controller may reclaim it. Implementations must fail closed: `Ok`
/// means the backing was positively observed dead, while any live, unknown,
/// or uninspectable state is an error that holds teardown.
#[async_trait]
pub trait StandingConvoyBackingInspector: Send + Sync {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String>;
}

/// Runtime-owned reconciliation entry points which need collaborators that do
/// not belong in the surface-independent core daemon.
#[async_trait]
pub trait OperatorReconciler: Send + Sync {
    async fn reconcile_now(&self, namespace: &str, kind: &str, name: &str) -> Result<String, String>;
}

#[async_trait]
pub trait WorkCredentialReconciler: Send + Sync {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String>;
}

#[async_trait]
impl StandingConvoyBackingInspector for InProcessDaemon {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        self.verify_standing_convoy_resource_backing_dead(convoy).await
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
    async fn resolve_convoy_issue_snapshot(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        let issue = self.fetch_issue_by_ref(reference).await?;
        if issue_snapshot_is_fresh(&issue) {
            Ok(issue)
        } else {
            Err(format!("issue {} snapshot is too stale to admit", reference.id))
        }
    }

    async fn admission_ai_utility(&self) -> Option<Arc<dyn AiUtility>> {
        let environment = self.environment_manager.environment_bag(&self.local_environment_id)?;
        let runner = self.environment_manager.environment_runner(&self.local_environment_id)?;
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        for factory in &self.discovery.factories.ai_utilities {
            if let Ok(utility) = factory.probe(&environment, &self.config, &probe_root, Arc::clone(&runner)).await {
                return Some(utility);
            }
        }
        None
    }

    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because daemon-owned background controllers retain
    /// weak references to the process state.
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
        Self::new_with_resource_backend_and_clock(repo_paths, config, discovery, host_name, resource_backend, Arc::new(SystemClock)).await
    }

    pub async fn new_with_resource_backend_and_clock(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        use crate::providers::discovery::DiscoveryResult;

        let (event_tx, _) = broadcast::channel(256);
        let event_sink: Arc<dyn EventSink> = Arc::new(BroadcastEventSink::new(event_tx.clone()));
        let mut repos: HashMap<flotilla_protocol::RepoIdentity, RepoState> = HashMap::new();
        let mut order = Vec::new();
        let mut path_identities = HashMap::new();
        let mut repository_keys_by_path = HashMap::new();

        let daemon_config = config.load_daemon_config().expect("failed to load daemon config");
        let config_machine_id = daemon_config.machine_id.as_deref();
        let local_environment_state_dir =
            resolve_local_environment_state_dir(config.state_dir().as_path(), config_machine_id, &*discovery.runner).await;
        let local_node_id = resolve_local_node_id(config.base_path().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local node id");
        let resource_backend = resource_backend.with_local_root(local_node_id.clone());
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
        discovery
            .host_scoped_providers
            .discover_for_environment(
                &local_environment_id,
                &local_host_bag,
                &discovery.factories,
                &config,
                &ExecutionEnvironmentPath::new(config.base_path().as_ref()),
                local_runner,
            )
            .await;
        let agent_state_store = crate::agents::shared_file_backed_agent_state_store(config.base_path());
        let mut checkout_vcs = CheckoutVcsCache::new();
        for path in repo_paths {
            if path_identities.contains_key(&path) {
                continue;
            }
            let initial_vcs =
                discover_vcs_for_checkout(&environment_manager, &discovery, &config, &local_environment_id, &local_environment_id, &path)
                    .await;
            let startup_inspection = match initial_vcs {
                Ok(vcs) => {
                    GitRepositoryInspector::new(
                        discovery.runner.clone(),
                        Arc::new(crate::vcs::FixedVcsResolver(vcs)),
                        local_host_id.to_string(),
                    )
                    .inspect_path(&path, None)
                    .await
                }
                Err(error) => Err(error),
            };
            if let Ok(inspection) = &startup_inspection {
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
                config.set_repository_spec(&ExecutionEnvironmentPath::new(&path), spec);
            }
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
                &environment_manager,
                &discovery,
                &config,
                &resource_backend,
                DEFAULT_PROVISIONING_NAMESPACE,
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
            match startup_inspection {
                Ok(inspection) => {
                    repository_keys_by_path.insert(path.clone(), inspection.key());
                }
                Err(error) => {
                    warn!(repo = %path.display(), %error, "repository key is unavailable during daemon startup");
                }
            }
            let slug = repo_slug.clone();
            if let Some(vcs) = registry.vcs.preferred() {
                let cell = tokio::sync::OnceCell::new();
                let _ = cell.set(Arc::clone(vcs));
                checkout_vcs.insert((local_environment_id.clone(), path.clone()), Arc::new(cell));
            }
            let model = RepoModel::new(registry, Some(local_environment_id.clone()));
            let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                order.push(identity.clone());
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
            }
            path_identities.insert(path.clone(), identity);
        }

        let local_provider_statuses = crate::host_summary::provider_statuses_from_registries(
            repos.values().map(|state| state.preferred_root().model.registry.as_ref()),
        );
        let local_host_summary = crate::host_summary::build_local_host_summary(
            &local_node_id,
            &host_name,
            EnvironmentId::host(environment_manager.local_host_id().clone()),
            &environment_manager,
            local_provider_statuses,
            &*discovery.env,
        )
        .await;

        let observer_daemon = Arc::new(OnceLock::new());
        let observation_source = Arc::new(ProviderChangeRequestObservationSource::new(Arc::clone(&observer_daemon)));
        let change_request_refresher = crate::change_request_observer::ChangeRequestRefresher::new(
            resource_backend.clone(),
            local_node_id.to_string(),
            observation_source.clone(),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        if let Err(error) = change_request_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned change request observations at startup failed");
        }
        let issue_refresher = crate::issue_observer::IssueRefresher::new(
            resource_backend.clone(),
            local_node_id.to_string(),
            Arc::new(ProviderIssueObservationSource { daemon: Arc::clone(&observer_daemon) }),
            crate::issue_observer::IssueRefreshCadence::default(),
        );
        if let Err(error) = issue_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned issue observations at startup failed");
        }
        let leaf_subscriptions =
            LeafSubscriptionTable::with_issues(resource_backend.clone(), event_sink.clone(), change_request_refresher, issue_refresher);
        let admission_free_space_path = config.state_dir().as_path().to_path_buf();
        let observed_resource_backend = ResourceBackend::InMemory(InMemoryBackend::observed());
        let aggregator_projection_state = AggregatorProjectionState::new();
        let daemon = Arc::new_cyclic(|self_weak| Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx: event_tx.clone(),
            event_sink: event_sink.clone(),
            config: Arc::clone(&config),
            next_command_id: AtomicU64::new(1),
            node_id: local_node_id.clone(),
            host_name: host_name.clone(),
            path_identities: RwLock::new(path_identities),
            repository_keys_by_path: RwLock::new(repository_keys_by_path),
            repository_change_requests: RwLock::new(HashMap::new()),
            change_request_observation_source: observation_source,
            host_registry: crate::host_registry::HostRegistry::new(
                NodeInfo::new(local_node_id.clone(), host_name.to_string()),
                local_host_summary,
            ),
            local_environment_id,
            environment_manager: Arc::clone(&environment_manager),
            discovery,
            checkout_vcs: Mutex::new(checkout_vcs),
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            self_weak: self_weak.clone(),
            pending_convoy_starts: Mutex::new(HashSet::new()),
            ensure_admission_retries: Mutex::new(HashMap::new()),
            ensure_reconciliation: Mutex::new(()),
            convoy_message_locks: Mutex::new(HashMap::new()),
            convoy_admission: Mutex::new(()),
            brief_artifact_writer: RwLock::new(None),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
            clock: Arc::clone(&clock),
            regard_lifecycle: RegardLifecycle::new(resource_backend.clone(), clock, ChronoDuration::seconds(DEFAULT_REGARD_DECAY_SECONDS)),
            resource_backend: resource_backend.clone(),
            observed_resource_backend: observed_resource_backend.clone(),
            observed_checkout_reconciliation: Mutex::new(()),
            aggregator_projection_state: aggregator_projection_state.clone(),
            provisioning_namespace: std::sync::RwLock::new(DEFAULT_PROVISIONING_NAMESPACE.to_string()),
            fleet: FleetService::new(
                event_sink.clone(),
                Arc::clone(&config),
                resource_backend.clone(),
                observed_resource_backend.clone(),
                aggregator_projection_state.clone(),
                host_name.clone(),
                Some(CanonicalHostId::resolved(environment_manager.local_host_id().as_str())),
                Arc::new(SshFleetReplicaTransport),
            ),
            repository_inspector: RwLock::new(None),
            operator_reconciler: RwLock::new(None),
            work_credential_reconciler: RwLock::new(None),
            local_placement_provider_statuses: RwLock::new(Vec::new()),
            managed_terminals_by_repo: RwLock::new(HashMap::new()),
            admission_free_space_path: std::sync::RwLock::new(admission_free_space_path),
            leaf_subscriptions: leaf_subscriptions.clone(),
        });
        observer_daemon
            .set(Arc::downgrade(&daemon))
            .map_err(|_| "observation daemon already initialized")
            .expect("initialize observation daemon");
        leaf_subscriptions.set_turn_delivery_actuator(Arc::new(DaemonTurnDeliveryActuator { daemon: Arc::downgrade(&daemon) })).await;

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

    pub async fn local_host_summary(&self) -> HostSummary {
        self.refresh_local_host_summary().await
    }

    pub async fn set_local_placement_capabilities(&self, agent_adapters: &BTreeSet<String>, terminal_pools: &[String]) {
        let mut statuses = agent_adapters
            .iter()
            .map(|adapter| HostProviderStatus::available(AGENT_ADAPTER_PROVIDER_CATEGORY, adapter))
            .chain(terminal_pools.iter().map(|pool| HostProviderStatus::available(TERMINAL_POOL_PROVIDER_CATEGORY, pool)))
            .collect::<Vec<_>>();
        statuses.sort_by(|left, right| (&left.category, &left.implementation).cmp(&(&right.category, &right.implementation)));
        *self.local_placement_provider_statuses.write().await = statuses;
        let _ = self.refresh_local_host_summary().await;
    }

    /// Use `path` as the canonical capacity source for both local and
    /// federated convoy admission.
    pub fn set_admission_free_space_path(&self, path: PathBuf) {
        *self.admission_free_space_path.write().expect("admission free-space path lock poisoned") = path;
    }

    pub async fn set_brief_artifact_writer(&self, writer: Arc<dyn BriefArtifactWriter>) {
        *self.brief_artifact_writer.write().await = Some(writer);
    }

    pub async fn admission_free_space_bytes(&self) -> Result<Option<u64>, String> {
        let path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let probe = Arc::clone(&self.discovery.available_space_probe);
        tokio::task::spawn_blocking(move || probe.measure(&path)).await.map_err(|error| format!("measure available disk space: {error}"))
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

    pub async fn set_work_credential_reconciler(&self, reconciler: Arc<dyn WorkCredentialReconciler>) {
        *self.work_credential_reconciler.write().await = Some(reconciler);
    }

    async fn reconcile_resumed_work_credentials(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        if let Some(reconciler) = self.work_credential_reconciler.read().await.clone() {
            reconciler.reconcile(namespace, environment_ref).await?;
        }
        Ok(())
    }

    async fn reconcile_or_restore_crew_work(
        &self,
        namespace: &str,
        environment_ref: &str,
        convoys: &flotilla_resources::TypedResolver<ResourceConvoy>,
        name: &str,
        previous_status: flotilla_resources::ConvoyStatus,
        reopened: &ResourceObject<ResourceConvoy>,
    ) -> Result<(), String> {
        if let Err(error) = self.reconcile_resumed_work_credentials(namespace, environment_ref).await {
            return match convoys.update_status(name, &reopened.metadata.resource_version, &previous_status).await {
                Ok(_) => Err(error),
                Err(restore_error) => Err(format!("{error}; could not restore crew work after credential failure: {restore_error}")),
            };
        }
        Ok(())
    }

    async fn restore_crew_work_after_delivery_failure(
        &self,
        convoys: &flotilla_resources::TypedResolver<ResourceConvoy>,
        name: &str,
        reopened_version: &str,
        previous_status: &flotilla_resources::ConvoyStatus,
        error: String,
    ) -> String {
        match convoys.update_status(name, reopened_version, previous_status).await {
            Ok(_) => error,
            Err(restore_error) => format!("{error}; could not restore crew work after delivery failure: {restore_error}"),
        }
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
                self.self_weak.upgrade().ok_or("repository inspector daemon unavailable")? as Arc<dyn crate::vcs::CheckoutVcsResolver>,
                host_ref.to_string(),
            )
            .with_forges(forges.into_iter().map(|forge| forge.spec).collect()),
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
        if let Some(repository_key) = self.repository_keys_by_path.read().await.get(path).cloned() {
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
        self.repository_keys_by_path.read().await.get(path).cloned()
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
            for (annotation, value) in &source.metadata.annotations {
                if annotation == SUPERSEDED_BY_ANNOTATION {
                    continue;
                }
                if target_meta.annotations.insert(annotation.clone(), value.clone()).is_some_and(|previous| previous != *value) {
                    return Err(format!("Repository metadata annotation `{annotation}` conflicts during forge identity sweep"));
                }
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
        for tracked in self.repository_keys_by_path.write().await.values_mut() {
            if replacements.contains(tracked) {
                *tracked = target_key.clone();
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
                crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, namespace, &key)
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
                let path = std::fs::canonicalize(path)
                    .map_err(|error| format!("adopted checkout path {} cannot be resolved: {error}", path.display()))?;
                let (spec, replaces_prior_repository) = self.configure_inspected_repository(&path, spec).await?;
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
        for repo in self.repos.read().await.values() {
            if let Some(provider) = repo.registry().issue_provider_for(source) {
                return Ok(provider);
            }
        }
        let host_bag = self
            .environment_manager
            .environment_bag(&self.local_environment_id)
            .ok_or_else(|| format!("environment not found: {}", self.local_environment_id))?;
        let runner = self
            .environment_manager
            .environment_runner(&self.local_environment_id)
            .ok_or_else(|| format!("environment runner not found: {}", self.local_environment_id))?;
        let mut bag = host_bag;
        let namespace = self.provisioning_namespace().await;
        if let Some(forge) = forge_for_remote(&self.resource_backend, &namespace, &format!("{}/{}", source.service, source.scope)).await? {
            bag = bag.with(EnvironmentAssertion::origin_forge(forge));
        }
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        for factory in &self.discovery.factories.issue_trackers {
            if let Ok(provider) = factory.probe(&bag, &self.config, &probe_root, Arc::clone(&runner)).await {
                if provider.supports(source) {
                    return Ok(provider);
                }
            }
        }
        Err(format!("no issue provider available for {} {}", source.service, source.scope))
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

    pub fn command_runner_for_environment(&self, env_id: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(env_id)
    }

    /// Resolve the VCS through the registered discovery factories once per checkout.
    /// The key includes the environment because the same path may name different
    /// checkouts on the host and inside a provisioned environment.
    pub async fn vcs_for_checkout(&self, env_id: &EnvironmentId, checkout: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        let key = (env_id.clone(), checkout.to_path_buf());
        let cell = self.checkout_vcs.lock().await.entry(key).or_insert_with(|| Arc::new(tokio::sync::OnceCell::new())).clone();
        cell.get_or_try_init(|| {
            discover_vcs_for_checkout(
                &self.environment_manager,
                &self.discovery,
                &self.config,
                &self.local_environment_id,
                env_id,
                checkout,
            )
        })
        .await
        .map(Arc::clone)
    }

    pub async fn local_vcs_for_checkout(&self, checkout: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        self.vcs_for_checkout(&self.local_environment_id, checkout).await
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

    /// Direct SSH environments explicitly opted into host-direct placement.
    /// Their environment key is derived from the remote host's stable ID.
    pub fn agentless_ssh_environments(&self) -> Vec<(EnvironmentId, crate::environment_manager::DirectEnvironmentState)> {
        self.environment_manager
            .managed_environments()
            .into_iter()
            .filter_map(|(id, managed)| match managed {
                crate::environment_manager::ManagedEnvironmentKind::Direct(state)
                    if state.host_id.as_ref().is_some_and(|host| id.as_str() == format!("host-direct-{host}")) =>
                {
                    Some((id, state))
                }
                _ => None,
            })
            .collect()
    }

    pub fn set_direct_environment_registry(
        &self,
        env_id: &EnvironmentId,
        registry: Arc<crate::providers::registry::ProviderRegistry>,
    ) -> Result<(), String> {
        self.environment_manager.set_direct_environment_registry(env_id, registry)
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
        *self.provisioning_namespace.write().expect("provisioning namespace lock poisoned") = namespace;
    }

    pub async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
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

    pub async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.clone()
    }

    pub fn subscribe_fleet_replicas(&self) -> broadcast::Receiver<Vec<FleetReplicaSnapshot>> {
        self.fleet.subscribe()
    }

    pub async fn cached_fleet_replica_snapshots(&self) -> Vec<FleetReplicaSnapshot> {
        self.fleet.cached_snapshots().await
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
        self.leaf_subscriptions.subscribe_wait(connection_id, request).await
    }

    pub async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.leaf_subscriptions.unsubscribe_connection(connection_id).await;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn flotilla_resources::controller::SecondaryWatch<Primary = flotilla_resources::Convoy>> {
        self.leaf_subscriptions.reconciler_wake_watch()
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.leaf_subscriptions.change_request_stale_after()
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_relay_protocol::Subject) -> Result<(), String> {
        self.leaf_subscriptions.refresh_change_request_hint(hint).await?;
        if hint.kind != flotilla_relay_protocol::SubjectKind::ChangeRequest {
            return Ok(());
        }
        let subject = flotilla_protocol::ReferenceContext::default().parse(&hint.to_string())?;
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
            let matches_repository = convoy.spec.repositories.iter().any(|repository| {
                change_request_address_with_forges(&repository.url, "1", &forges).ok().is_some_and(|address| {
                    matches!(address, flotilla_protocol::LeafAddress::ChangeRequest { service, scope, .. }
                        if service == subject.source.service && scope == subject.source.scope)
                })
            });
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
                apply_resource_status_patch(&convoys, &convoy.metadata.name, &ConvoyStatusPatch::DiscoverSubjects {
                    subjects: relationships.into_iter().map(|relationship| (subject.clone(), relationship)).collect(),
                    source: flotilla_resources::SubjectDiscoverySource::Relay,
                    at: self.clock.now(),
                })
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub async fn refresh_demanded_owned_change_requests(&self) -> Result<(), String> {
        self.leaf_subscriptions.refresh_demanded_owned_change_requests().await
    }

    pub fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.leaf_subscriptions.set_change_request_relay_healthy(healthy);
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
        crate::observed_resources::reconcile_adopted_checkouts(&self.resource_backend, &self.observed_resource_backend, namespace)
            .await
            .map_err(|error| error.to_string())?;
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        for checkout in checkouts.list().await.map_err(|error| error.to_string())?.items {
            if checkout.metadata.lifecycle_authority().map_err(|error| error.to_string())? != Some(LifecycleAuthority::Adopted) {
                continue;
            }
            let Some(path) = checkout_path(&checkout) else {
                continue;
            };
            let runner = self.runner_for_resource_checkout(&checkout).await?;
            let vcs = self.vcs_for_checkout(&self.local_environment_id, Path::new(path)).await?;
            let convoy_ref = checkout.metadata.labels.get(CONVOY_LABEL).map(String::as_str);
            let source_root = checkout.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map(String::as_str);
            let convoy = self
                .resource_backend
                .clone()
                .including_replicas::<ResourceConvoy>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
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
                    (origin_matches && association_matches).then_some(source.object)
                });
            let integration = if let Some(convoy) = convoy.as_ref() {
                let change_request_id = convoy_change_request_id_for_checkout(convoy, &checkout);
                inspect_convoy_checkout_integration(
                    &*runner,
                    vcs.as_ref(),
                    Path::new(path),
                    &checkout.spec,
                    convoy,
                    change_request_id.as_deref(),
                    None,
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
            };
            apply_resource_status_patch(&checkouts, &checkout.metadata.name, &flotilla_resources::CheckoutStatusPatch::UpdateIntegration {
                integration: Box::new(integration),
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        crate::observed_resources::reconcile_adopted_checkouts(&self.resource_backend, &self.observed_resource_backend, namespace)
            .await
            .map_err(|error| error.to_string())
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
    pub fn set_direct_environment_ssh_destination_for_test(&self, env_id: &EnvironmentId, destination: String) -> Result<(), String> {
        self.environment_manager.set_direct_environment_ssh_destination(env_id, destination)
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
            &self.resource_backend,
            &self.provisioning_namespace().await,
            &self.local_environment_id,
            environment_id,
            repo_path,
        )
        .await
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
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_peer_host_summaries(&self, summaries: HashMap<EnvironmentId, HostSummary>) {
        let remote_counts = HashMap::new();
        self.host_registry
            .set_peer_host_summaries(summaries, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_connection_status(&self, node: &NodeInfo, status: PeerConnectionState) {
        let remote_counts = HashMap::new();
        self.host_registry
            .publish_peer_connection_status(node, status, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
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

    pub async fn publish_peer_summary(&self, summary: HostSummary) {
        self.host_registry
            .publish_peer_summary(summary, &|e| {
                let _ = self.event_tx.send(e);
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
        Ok(Some(flotilla_protocol::qualified_path::HostId::new(actuator.as_str())))
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
            flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, .. } => {
                (self.provisioning_namespace().await, convoy.as_str())
            }
            flotilla_protocol::CommandAction::CrewComplete { context, .. }
            | flotilla_protocol::CommandAction::CrewFail { context, .. }
            | flotilla_protocol::CommandAction::CrewStall { context, .. }
            | flotilla_protocol::CommandAction::CrewHandoff { context, .. }
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
        self.path_identities.read().await.get(repo_path).cloned()
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        match discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.resource_backend,
            &self.provisioning_namespace().await,
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

    fn resolve_observation_root_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        let roots = self.config.load_observation_roots()?;
        match selector {
            flotilla_protocol::RepoSelector::Path(path) if roots.iter().any(|root| root.as_path() == path) => Ok(path.clone()),
            flotilla_protocol::RepoSelector::Path(path) => Err(format!("repo not observed: {}", path.display())),
            flotilla_protocol::RepoSelector::Query(query) => {
                crate::resolve::resolve_repo(query, roots.iter().map(|root| (root.as_path(), None))).map_err(|error| error.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => Err(format!("repo not tracked: {identity}")),
        }
    }

    async fn resolve_checkout_selector(
        &self,
        selector: &flotilla_protocol::CheckoutSelector,
        scope: &CheckoutResolutionScope,
    ) -> Result<(PathBuf, String), String> {
        let repos = self.repos.read().await;
        let mut matches = Vec::new();
        for state in repos.values() {
            let root = state.preferred_root();
            let Some((_, vcs)) = root.model.registry.vcs.preferred_with_desc() else { continue };
            let checkouts = vcs.list_checkouts().await.map_err(|error| format!("checkout discovery failed: {error}"))?;
            for (checkout_path, checkout) in checkouts {
                let host_path =
                    QualifiedPath::host(self.environment_manager.local_host_id().clone(), checkout_path.as_path().to_path_buf());
                if !checkout_matches_scope(&host_path, &checkout, &self.host_name, scope) {
                    continue;
                }
                let matched = match selector {
                    flotilla_protocol::CheckoutSelector::Path(path) => host_path.path == *path,
                    flotilla_protocol::CheckoutSelector::Query(query) => {
                        checkout.branch == *query || checkout.branch.contains(query) || host_path.path.to_string_lossy().contains(query)
                    }
                };
                if matched {
                    matches.push((state.preferred_path().to_path_buf(), checkout.branch));
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
            | CommandAction::MergeChangeRequest { .. }
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

    async fn repository_action_policy_error(&self, command: &Command, repo: &Path) -> Option<String> {
        let CommandAction::MergeChangeRequest { id, .. } = &command.action else {
            return None;
        };
        let repository_key = match self.repository_keys_by_path.read().await.get(repo).cloned() {
            Some(key) => key,
            None => {
                return Some(format!(
                    "cannot determine whether merging change request {id} is permitted: repository policy is unavailable"
                ));
            }
        };
        let namespace = self.provisioning_namespace().await;
        let repository = match self.resource_backend.clone().using::<Repository>(&namespace).get(&repository_key.to_string()).await {
            Ok(repository) => repository,
            Err(error) => {
                return Some(format!(
                    "cannot determine whether merging change request {id} is permitted: repository policy is unavailable: {error}"
                ));
            }
        };
        repository
            .spec
            .is_fork()
            .then(|| format!("merging change request {id} is forbidden for fork-stance repository; landing is human-only"))
    }

    /// Resolve an explicitly requested change request across the project's
    /// snapshotted repositories and capture its admission identity.
    async fn resolve_convoy_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        let (candidates, mut failures) = self.repository_change_request_candidates(repository_keys).await;
        let consulted = candidates.iter().map(|(_, scope, _)| scope.clone()).collect::<Vec<_>>();

        let mut matches = Vec::new();
        let mut matched_repositories = Vec::new();
        for (repository, scope, provider) in candidates {
            match provider.get_change_request_for_admission(requested_id).await {
                Ok(admission) => {
                    let Some(base_ref) = admission.base_ref else {
                        failures.push(format!("repository {scope}: change request {} did not report a base ref", admission.id));
                        continue;
                    };
                    matches.push(ResolvedConvoyChangeRequestAdmission {
                        binding: BoundChangeRequest { id: admission.id, repository_ref: repository, title: admission.change_request.title },
                        branch: admission.change_request.branch,
                        base_ref,
                    });
                    matched_repositories.push(scope);
                }
                Err(error) => failures.push(format!("repository {scope}: {error}")),
            }
        }

        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 if failures.iter().any(|failure| failure.contains("rate limited")) => {
                Err(format!("change request {requested_id} lookup was rate limited: {}", failures.join("; ")))
            }
            0 if consulted.is_empty() => Err(format!(
                "change request {requested_id} could not be resolved because no project repository could be consulted{}",
                if failures.is_empty() { String::new() } else { format!(": {}", failures.join("; ")) }
            )),
            0 => Err(format!(
                "change request {requested_id} was not found in consulted repositories [{}]{}",
                consulted.join(", "),
                if failures.is_empty() { String::new() } else { format!(": {}", failures.join("; ")) }
            )),
            count => Err(format!(
                "change request {requested_id} is ambiguous across {count} consulted repositories [{}]",
                matched_repositories.join(", ")
            )),
        }
    }

    async fn repository_change_request_candidates(
        &self,
        repository_keys: &[RepositoryKey],
    ) -> (Vec<(RepositoryKey, String, Arc<dyn ChangeRequestTracker>)>, Vec<String>) {
        let namespace = self.provisioning_namespace().await;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let mut candidates = Vec::new();
        let mut failures = Vec::new();
        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository,
                Err(error) => {
                    failures.push(format!("repository {repository_key}: {error}"));
                    continue;
                }
            };
            let Some(identity) = repository.spec.forge() else {
                failures.push(format!("repository {repository_key}: no forge identity"));
                continue;
            };
            if let Some(cached) = self.repository_change_requests.read().await.get(repository_key) {
                if cached.service_url == identity.service_url && cached.repository == identity.repository {
                    candidates.push((repository_key.clone(), identity.repository.clone(), Arc::clone(&cached.provider)));
                    continue;
                }
            }
            let provider = match self.discover_repository_change_request(&namespace, &repository.spec).await {
                Ok(provider) => provider,
                Err(error) => {
                    failures.push(format!("repository {}: {error}", identity.repository));
                    continue;
                }
            };
            self.repository_change_requests.write().await.insert(repository_key.clone(), RepositoryChangeRequestProvider {
                service_url: identity.service_url.clone(),
                repository: identity.repository.clone(),
                provider: Arc::clone(&provider),
            });
            candidates.push((repository_key.clone(), identity.repository.clone(), provider));
        }
        (candidates, failures)
    }

    async fn discover_repository_change_request(
        &self,
        namespace: &str,
        repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        let identity = repository.forge().ok_or("no forge identity")?;
        let remote = repository.live_remote().ok_or("no repository remote")?;
        let forge = match repository.identity() {
            RepositoryIdentity::Forge { forge_ref, .. } => Some(
                self.resource_backend
                    .including_replicas::<Forge>(namespace)
                    .get(forge_ref)
                    .await
                    .map_err(|error| format!("Forge {forge_ref}: {error}"))?
                    .object
                    .spec,
            ),
            _ => forge_for_remote(&self.resource_backend, namespace, remote).await?,
        };
        let remote_assertion = crate::providers::discovery::detectors::git::remote_assertion(remote, "origin")
            .ok_or_else(|| format!("invalid repository remote {remote}"))?;
        let mut bag = self.local_environment_bag().unwrap_or_default().with(remote_assertion);
        if let Some(forge) = &forge {
            bag = bag.with(EnvironmentAssertion::origin_forge(forge.clone()));
            if forge.kind == ForgeKind::Forgejo {
                let credentials =
                    self.resource_backend.definitions::<CredentialSpec>(namespace).list().await.map_err(|error| error.to_string())?;
                let paths = credentials
                    .into_iter()
                    .filter_map(|credential| match (&credential.spec.consumer, &credential.spec.source) {
                        (CredentialConsumer::Forgejo { forge_ref, .. }, CredentialSource::File { path })
                            if forge_ref == &forge.forge_id =>
                        {
                            Some(path.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                match paths.as_slice() {
                    [path] => bag = bag.with(EnvironmentAssertion::auth_file("forgejo", path)),
                    [] => {}
                    _ => return Err(format!("multiple Forgejo credentials for Forge {}", forge.forge_id)),
                }
            }
        }
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        let mut unmet = Vec::new();
        for factory in &self.discovery.factories.change_requests {
            match factory.probe(&bag, &self.config, &probe_root, Arc::clone(&self.discovery.runner)).await {
                Ok(provider) => return Ok(provider),
                Err(requirements) => unmet.extend(requirements.into_iter().map(|requirement| format!("{requirement:?}"))),
            }
        }
        Err(format!("change request provider unavailable for {} ({})", identity.service_url, unmet.join(", ")))
    }

    /// Persist every branch-matching PR across the convoy's repositories.
    /// Successful lookups are written even when another repository lookup fails;
    /// the first error is returned after those writes.
    pub async fn discover_convoy_branch_subjects(&self, namespace: &str, convoy_name: &str, branch: &str) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
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
        for repository in &convoy.spec.repositories {
            match self.resolve_convoy_change_request(std::slice::from_ref(&repository.repo_ref), branch, None).await {
                Ok(Some(request)) => {
                    let address = change_request_address_with_forges(&repository.url, &request.id, &forges)?;
                    if let Some(subject) = flotilla_protocol::Subject::from_leaf(&address) {
                        subjects.push((subject, flotilla_protocol::Relationship::Produces));
                    }
                }
                Ok(None) => {}
                Err(error) => errors.push(error),
            }
        }
        if !subjects.is_empty() {
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::DiscoverSubjects {
                subjects,
                source: flotilla_resources::SubjectDiscoverySource::Branch,
                at: self.clock.now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        if let Some(error) = errors.into_iter().next() {
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
        let repositories = convoy
            .spec
            .repositories
            .iter()
            .filter_map(|repository| {
                let address = change_request_address_with_forges(&repository.url, "1", &forges).ok()?;
                let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, .. } = address else { return None };
                let canonical = flotilla_resources::canonicalize_repo_url(&repository.url).ok()?;
                let web_base = forges
                    .iter()
                    .find(|forge| forge.forge_id == service)
                    .map(|forge| forge.https_url.clone())
                    .or_else(|| canonical.strip_suffix(&format!("/{scope}")).map(str::to_string))?;
                let alias = project
                    .as_ref()
                    .and_then(|project| project.spec.repositories.iter().find(|candidate| candidate.repo == repository.repo_ref))
                    .and_then(|repository| repository.alias.clone())
                    .unwrap_or_else(|| scope.rsplit('/').next().unwrap_or(&scope).to_string());
                Some(flotilla_protocol::RepositoryAlias {
                    project: convoy.spec.project_ref.clone(),
                    alias,
                    source: flotilla_protocol::IssueSource { service: service.clone(), scope },
                    web_base,
                    forge_alias: (service != "github.com").then_some(service),
                })
            })
            .collect();
        Ok(flotilla_protocol::ReferenceContext { repositories })
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
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::DiscoverSubjects {
                subjects: vec![(subject, relationship)],
                source: flotilla_resources::SubjectDiscoverySource::Operator,
                at: self.clock.now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        } else {
            if convoy.spec.subjects.iter().any(|entry| entry.subject == subject) {
                return Err("declared convoy subjects cannot be unlinked; change the convoy spec".to_string());
            }
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::UnlinkSubject { subject })
                .await
                .map_err(|error| error.to_string())?;
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
        if let Some(change_request) = self.resolve_observed_convoy_change_request(repository_keys, change_request_id).await? {
            return Ok(Some(change_request));
        }
        if let Some(id) = change_request_id {
            let namespace = self.provisioning_namespace().await;
            let repositories = self.resource_backend.including_replicas::<Repository>(&namespace);
            let mut failures = Vec::new();
            for repository_key in repository_keys {
                let repository = match repositories.get(&repository_key.to_string()).await {
                    Ok(repository) => repository,
                    Err(error) => {
                        failures.push(error.to_string());
                        continue;
                    }
                };
                let Some(remote) = repository.object.spec.live_remote() else { continue };
                let address = match change_request_address(remote, id) {
                    Ok(address) => address,
                    Err(error) => {
                        failures.push(error);
                        continue;
                    }
                };
                let Some(subject) = ChangeRequestRef::from_address(&namespace, &address) else { continue };
                match self.change_request_observation_source.observe(&subject).await {
                    Ok(observation) => {
                        let status = match observation.state.value {
                            Some(ObservedChangeRequestState::Open) => flotilla_protocol::ChangeRequestStatus::Open,
                            Some(ObservedChangeRequestState::Draft) => flotilla_protocol::ChangeRequestStatus::Draft,
                            Some(ObservedChangeRequestState::Merged) => flotilla_protocol::ChangeRequestStatus::Merged,
                            Some(ObservedChangeRequestState::Closed) => flotilla_protocol::ChangeRequestStatus::Closed,
                            None => continue,
                        };
                        return Ok(Some(ConvoyChangeRequest { id: id.to_string(), status, repository_key: repository_key.clone() }));
                    }
                    Err(error) => failures.push(error),
                }
            }
            if let Some(error) = failures.iter().find(|error| error.contains("rate limited")) {
                return Err(error.clone());
            }
            return failures.into_iter().next().map_or(Ok(None), Err);
        }

        let (live_candidates, setup_failures) = self.repository_change_request_candidates(repository_keys).await;

        let mut failures = setup_failures;
        for (repository, _, provider) in live_candidates {
            match provider.find_change_request_by_branch(branch).await {
                Ok(Some((id, request))) => {
                    return Ok(Some(ConvoyChangeRequest { id, status: request.status, repository_key: repository }));
                }
                Ok(None) => {}
                Err(error) => {
                    failures.push(error);
                }
            }
        }
        if let Some(error) = failures.iter().find(|error| error.contains("rate limited")) {
            return Err(error.clone());
        }
        failures.into_iter().next().map_or(Ok(None), Err)
    }

    async fn resolve_observed_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let Some(change_request_id) = change_request_id else { return Ok(None) };
        let Ok(number) = change_request_id.parse::<u64>() else { return Ok(None) };
        let namespace = self.provisioning_namespace().await;
        let repositories = self.resource_backend.clone().including_replicas::<Repository>(&namespace);
        let change_requests = self.resource_backend.clone().including_replicas::<ResourceChangeRequest>(&namespace);

        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            };
            let Some(live_remote) = repository.object.spec.live_remote() else {
                continue;
            };
            let LeafAddress::ChangeRequest { service, scope, .. } = change_request_address(live_remote, change_request_id)? else {
                unreachable!("change_request_address always returns a change-request address")
            };
            let record_name = change_request_record_name(&service, &scope, number);
            let observation = match change_requests.get(&record_name).await {
                Ok(observation) => observation,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            };
            let Some(state) = observation.object.status.as_ref().and_then(|status| status.state.value) else { continue };
            let status = match state {
                ObservedChangeRequestState::Open => flotilla_protocol::ChangeRequestStatus::Open,
                ObservedChangeRequestState::Draft => flotilla_protocol::ChangeRequestStatus::Draft,
                ObservedChangeRequestState::Merged => flotilla_protocol::ChangeRequestStatus::Merged,
                ObservedChangeRequestState::Closed => flotilla_protocol::ChangeRequestStatus::Closed,
            };
            return Ok(Some(ConvoyChangeRequest { id: change_request_id.to_string(), status, repository_key: repository_key.clone() }));
        }
        Ok(None)
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
        if let Some(existing_path) = existing_path {
            let key_became_available = if let Some(repository_key) = repository_key {
                self.repository_keys_by_path.write().await.insert(existing_path, repository_key.clone()).as_ref() != Some(&repository_key)
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
        if let Some(repository_key) = repository_key {
            self.repository_keys_by_path.write().await.insert(synthetic_path.clone(), repository_key);
        }

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));

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

async fn ensure_default_workflows(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    for (name, spec) in [
        ("single-agent", flotilla_resources::single_agent_workflow_spec()),
        ("single-agent-shepherd", flotilla_resources::single_agent_shepherd_workflow_spec()),
        ("implement-review", flotilla_resources::implement_review_workflow_spec()),
    ] {
        let meta = InputMeta::builder().name(name.to_string()).build();
        match templates.create(&meta, &spec).await {
            Ok(_) | Err(ResourceError::Conflict { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn prepared_snapshot_name(kind: &str, spec: &serde_json::Value) -> Result<String, String> {
    let suffix = flotilla_resources::content_hash(spec).map_err(|error| error.to_string())?;
    Ok(format!("{kind}-snapshot-{suffix}"))
}

async fn ensure_prepared_workflow_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &WorkflowTemplateSpec,
) -> Result<(), String> {
    let templates = backend.definitions::<WorkflowTemplate>(namespace);
    match templates.get(name).await {
        Ok(existing) if existing.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared workflow snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => templates
            .apply(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::WORKFLOW_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

async fn ensure_prepared_placement_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &PlacementPolicySpec,
) -> Result<(), String> {
    let policies = backend.including_replicas::<PlacementPolicy>(namespace);
    match policies.get(name).await {
        Ok(existing) if existing.object.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared placement snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => backend
            .clone()
            .using::<PlacementPolicy>(namespace)
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::PLACEMENT_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

async fn ensure_repository_and_default_project_workflow(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
    repository_spec: &RepositorySpec,
) -> Result<(), String> {
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    ensure_default_workflows(backend, namespace).await?;
    flotilla_resources::PreparedSnapshotGarbageCollector::new(backend.clone(), namespace)
        .collect(None)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn repository_identity_display(spec: &RepositorySpec) -> String {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Forge { forge_ref, owner, repo_name } => format!("{forge_ref}/{owner}/{repo_name}"),
        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
        flotilla_resources::RepositoryIdentity::Local { .. } => "local".to_string(),
    }
}

fn local_repository_matches_checkout(spec: &RepositorySpec, checkout: &crate::repository_inspection::LocalCheckoutInspection) -> bool {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Local { host_ref, git_common_dir } => {
            host_ref == &checkout.host_ref && Path::new(git_common_dir).parent() == Some(checkout.path.as_path())
        }
        flotilla_resources::RepositoryIdentity::Remote { .. } | flotilla_resources::RepositoryIdentity::Forge { .. } => false,
    }
}

#[derive(Debug)]
pub struct AddRepoOutcome {
    pub tracked_path: PathBuf,
    pub resolved_from: Option<PathBuf>,
    pub identity_change: Option<RepositoryIdentityChange>,
}

fn required_admission_value<'a>(value: &'a str, field: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value)
    }
}

fn resolve_project_ref(default_namespace: &str, value: &str) -> Result<(String, String), String> {
    let value = required_admission_value(value, "project")?;
    let address_value = value.strip_prefix(flotilla_protocol::view_address::SCHEME_PREFIX).unwrap_or(value);
    let has_scheme = address_value != value;
    if has_scheme || (address_value.starts_with("project/") && address_value.split('/').count() != 2) {
        return match value.parse::<ViewAddress>() {
            Ok(ViewAddress::Project { namespace, name }) => Ok((namespace, name)),
            Ok(address) => Err(format!("invalid project reference {value}: expected a project address, got {}", address.kind_name())),
            Err(error) => Err(format!("invalid project reference {value}: {error}")),
        };
    }
    match value.split('/').collect::<Vec<_>>().as_slice() {
        [name] => Ok((default_namespace.to_string(), (*name).to_string())),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => Ok(((*namespace).to_string(), (*name).to_string())),
        _ => Err(format!("invalid project reference {value}: expected <name>, <namespace>/<name>, or project/<namespace>/<name>")),
    }
}

fn normalize_convoy_start_intent(
    default_namespace: &str,
    intent: &flotilla_protocol::ConvoyStartIntent,
) -> Result<(String, flotilla_protocol::ConvoyStartIntent), String> {
    let (namespace, project_ref) = resolve_project_ref(default_namespace, &intent.project_ref)?;
    let mut intent = intent.clone();
    intent.namespace = Some(namespace.clone());
    intent.project_ref = project_ref;
    Ok((namespace, intent))
}

fn project_not_ready_error(namespace: &str, project_ref: &str, error: ResourceError) -> String {
    match error {
        ResourceError::NotFound { name } => {
            format!("project {namespace}/{project_ref} is not ready: resource not found: {name} (tried {namespace}/{project_ref})")
        }
        error => format!("project {project_ref} is not ready: {error}"),
    }
}

fn workflow_has_in_crew_review(workflow: &WorkflowTemplateSpec) -> bool {
    workflow.vessels.iter().any(|vessel| {
        let agent_count = vessel.crew.iter().filter(|crew| matches!(crew.source, CrewSource::Agent { .. })).count();
        agent_count > 1
            && vessel.crew.iter().any(|crew| {
                matches!(
                    &crew.source,
                    CrewSource::Agent { selector, .. } if matches!(selector.capability.as_str(), "review" | "code-review")
                )
            })
    })
}

async fn validate_fork_workflow_admission(
    backend: &ResourceBackend,
    namespace: &str,
    repositories: &[ConvoyRepositorySpec],
    workflow_ref: &str,
    workflow: &WorkflowTemplateSpec,
) -> Result<(), String> {
    if workflow_has_in_crew_review(workflow) {
        return Ok(());
    }
    let resolver = backend.including_replicas::<Repository>(namespace);
    for repository in repositories {
        let repository =
            resolver.get(&repository.repo_ref.to_string()).await.map_err(|error| format!("repository {}: {error}", repository.repo_ref))?;
        if repository.object.spec.is_fork() && !repository.object.spec.allows_reviewless_workflows() {
            return Err(format!("workflow {workflow_ref} not permitted for fork-stance repository — use implement-review"));
        }
    }
    Ok(())
}

async fn validate_workflow_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    allow_unready: bool,
) -> Result<(), String> {
    let required_adapters = required_workflow_agent_adapters(workflow)?;
    // Resolve each candidate's image once, even for tool-only workflows.
    let capabilities = match placement {
        Some(policy) if !required_adapters.is_empty() || policy.spec.docker_per_vessel.is_some() => {
            Some(placement_agent_adapters(backend, namespace, policy, allow_unready).await?)
        }
        _ => None,
    };
    for adapter in required_adapters {
        let Some((available_adapters, detail)) = &capabilities else {
            return Err(format!("workflow requires agent adapter `{adapter}`, but no placement is available"));
        };
        if available_adapters.contains(&adapter) {
            continue;
        }
        return Err(format!(
            "workflow requires agent adapter `{adapter}`, which is not available in placement `{}` ({detail})",
            placement.expect("capabilities came from a placement").metadata.name
        ));
    }

    Ok(())
}

async fn resolve_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    let grants = backend
        .including_replicas::<CredentialGrant>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential grants: {error}"))?
        .items;
    let specs = backend
        .including_replicas::<CredentialSpec>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .map(|source| (source.object.metadata.name, source.object.spec.consumer))
        .collect::<BTreeMap<_, _>>();
    let all_repositories = repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<BTreeSet<_>>();
    let repository_definitions = backend
        .including_replicas::<Repository>(namespace)
        .list()
        .await
        .map_err(|error| format!("list repositories for credential grants: {error}"))?
        .items
        .into_iter()
        .map(|source| {
            (
                RepositoryKey(source.object.metadata.name),
                if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own },
            )
        })
        .collect::<BTreeMap<_, _>>();

    for vessel in &mut workflow.vessels {
        let vessel_repositories = vessel
            .repository_refs
            .as_ref()
            .map(|repositories| repositories.iter().cloned().collect())
            .unwrap_or_else(|| all_repositories.clone());
        let repository_trust = vessel_repositories
            .iter()
            .map(|key| {
                repository_definitions
                    .get(key)
                    .copied()
                    .map(|trust| (key.clone(), trust))
                    .ok_or_else(|| format!("repository `{key}` unavailable for credential grant selection"))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        if vessel.crew.len() > 1 {
            let grant_sets = vessel
                .crew
                .iter()
                .map(|crew| {
                    grants
                        .iter()
                        .filter(|source| source.object.spec.selector.matches(project_ref, &repository_trust, &crew.role))
                        .map(|source| source.object.metadata.name.clone())
                        .collect::<BTreeSet<_>>()
                })
                .collect::<BTreeSet<_>>();
            if grant_sets.len() > 1 {
                return Err(format!(
                    "vessel `{}` has crew roles with different credential grants; place those roles in separate vessels",
                    vessel.name
                ));
            }
        }
        let matching_grants = grants
            .iter()
            .filter(|source| {
                (vessel.crew.is_empty() && source.object.spec.selector.matches(project_ref, &repository_trust, ""))
                    || vessel.crew.iter().any(|crew| source.object.spec.selector.matches(project_ref, &repository_trust, &crew.role))
            })
            .collect::<Vec<_>>();
        let granted = matching_grants.iter().flat_map(|grant| grant.object.spec.credentials.iter().cloned()).collect::<BTreeSet<_>>();
        if let Some(missing) = granted.iter().find(|name| !specs.contains_key(*name)) {
            return Err(format!("credential grant references missing credential `{missing}`"));
        }
        let mut credential_scopes = BTreeMap::<String, BTreeSet<_>>::new();
        let mut credential_permissions = BTreeMap::<String, BTreeMap<String, String>>::new();
        for grant in matching_grants {
            for name in grant.object.spec.permissions.keys() {
                if !grant.object.spec.credentials.contains(name) {
                    return Err(format!("grant permissions reference ungranted credential `{name}`"));
                }
                if !matches!(specs.get(name), Some(CredentialConsumer::GithubApp { .. })) {
                    return Err(format!("grant permissions require GitHub App credential `{name}`"));
                }
            }
            let covered_repositories = if grant.object.spec.selector.repositories.is_empty() {
                vessel_repositories.clone()
            } else {
                grant.object.spec.selector.repositories.intersection(&vessel_repositories).cloned().collect()
            };
            for credential in &grant.object.spec.credentials {
                credential_scopes.entry(credential.clone()).or_default().extend(covered_repositories.iter().cloned());
                if let Some(permissions) = grant.object.spec.permissions.get(credential) {
                    let resolved = credential_permissions.entry(credential.clone()).or_default();
                    for (name, level) in permissions {
                        let current = resolved.entry(name.clone()).or_insert_with(|| level.clone());
                        if flotilla_resources::permission_level_rank(level)? > flotilla_resources::permission_level_rank(current)? {
                            *current = level.clone();
                        }
                    }
                }
            }
        }
        for name in &granted {
            if let Some(CredentialConsumer::GithubApp { permissions: declaration, .. }) = specs.get(name) {
                let resolved = capped_github_app_permissions(credential_permissions.get(name), declaration.as_ref())?;
                if let Some(resolved) = resolved {
                    credential_permissions.insert(name.clone(), resolved);
                }
            }
        }
        vessel.credential_refs = granted;
        vessel.credential_scopes = credential_scopes;
        vessel.credential_permissions = credential_permissions;
    }
    Ok(())
}

async fn resolve_and_validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    placement: Option<&ResourceObject<PlacementPolicy>>,
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    resolve_workflow_credentials(backend, namespace, project_ref, repositories, workflow).await?;
    validate_workflow_credentials(backend, namespace, workflow, placement).await
}

async fn resolve_and_validate_workflow_credentials_for_capability_admission(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    placement: Option<&ResourceObject<PlacementPolicy>>,
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    resolve_workflow_credentials(backend, namespace, project_ref, repositories, workflow).await?;
    validate_workflow_credentials_with_capabilities_for_admission(backend, namespace, workflow, placement, &CapabilityTable::seeded(), true)
        .await
}

async fn validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
) -> Result<(), String> {
    validate_workflow_credentials_with_capabilities(backend, namespace, workflow, placement, &CapabilityTable::seeded()).await
}

async fn validate_workflow_credentials_with_capabilities(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    capabilities: &CapabilityTable,
) -> Result<(), String> {
    validate_workflow_credentials_with_capabilities_for_admission(backend, namespace, workflow, placement, capabilities, false).await
}

async fn validate_workflow_credentials_with_capabilities_for_admission(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    capabilities: &CapabilityTable,
    allow_unready: bool,
) -> Result<(), String> {
    let specs = backend
        .including_replicas::<CredentialSpec>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .map(|source| (source.object.metadata.name, source.object.spec.consumer))
        .collect::<BTreeMap<_, _>>();
    for vessel in &workflow.vessels {
        for crew in &vessel.crew {
            let CrewSource::Agent { selector, .. } = &crew.source else {
                continue;
            };
            let requirement = capabilities.resolve_selector(selector)?;
            let Some(delivery_slot) = requirement.credential_delivery_slot() else {
                continue;
            };
            let has_granted_credential =
                vessel.credential_refs.iter().any(|name| specs.get(name).is_some_and(|consumer| consumer.delivery_slot() == delivery_slot));
            if has_granted_credential {
                continue;
            }
            let compatible = specs
                .iter()
                .filter(|(_, consumer)| consumer.delivery_slot() == delivery_slot)
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            let credential = match compatible.as_slice() {
                [name] => format!("credential `{name}`"),
                [] => format!("a `{delivery_slot}` credential"),
                names => format!("one of credentials `{}`", names.join("`, `")),
            };
            return Err(format!(
                "agent adapter `{}` requires {credential}, but no matching CredentialGrant selected it",
                requirement.adapter
            ));
        }
    }

    let required = workflow.vessels.iter().flat_map(|vessel| vessel.credential_refs.iter().cloned()).collect::<BTreeSet<_>>();
    let ambient_dependent_vessels = ambient_credential_dependent_vessels(
        capabilities,
        &specs,
        workflow,
        placement.is_some_and(|policy| policy.spec.host_direct.is_some()),
    )?;
    if required.is_empty() && ambient_dependent_vessels.is_empty() {
        return Ok(());
    }
    let Some(placement) = placement else {
        if let Some(first) = required.first() {
            return Err(format!("workflow requires credential `{first}`, but no placement is available"));
        }
        // Ambient-dependent vessels without a placement have no target host to
        // check expiry against; admission proceeds as before.
        return Ok(());
    };
    let target_host = placement_target_host(backend, namespace, placement).await?;
    let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
    let host_label = target_host.display_name;
    let generation = host_generation(host.status.as_ref()).to_string();
    let Some(mut status) = host.status else {
        if required.is_empty() {
            return Ok(());
        }
        return Err(format!(
            "placement `{}` host `{host_label}` generation `{generation}` has no observed status",
            placement.metadata.name
        ));
    };
    status.apply_heartbeat_readiness(Utc::now());
    if !required.is_empty() {
        if !status.ready && !allow_unready {
            return Err(placement_host_not_ready_reason(&placement.metadata.name, &host_label, &generation, &status));
        }
        let held = status.held_credentials().map_err(|error| {
            format!(
                "placement `{}` host `{host_label}` generation `{generation}` has invalid held-credential capability: {error}",
                placement.metadata.name
            )
        })?;
        if let Some(missing) = required.iter().find(|credential| !held.contains(*credential)) {
            return Err(format!(
                "workflow requires credential `{missing}`, which placement `{}` host `{host_label}` generation `{generation}` does not hold",
                placement.metadata.name
            ));
        }
    }
    let expiry = status.credential_expiry().map_err(|error| {
        format!("placement `{}` host `{host_label}` has invalid credential expiry capability: {error}", placement.metadata.name)
    })?;
    let now = Utc::now();
    for credential in &required {
        if let Some(expired_at) = expiry.get(credential).and_then(|entry| entry.expired_at(now)) {
            return Err(format!(
                "credential `{credential}` expired on host `{host_label}` on {} — refresh its material before dispatching",
                expired_at.format("%Y-%m-%d")
            ));
        }
    }
    for (vessel, scope) in &ambient_dependent_vessels {
        if let Some(expired_at) = expiry.get(*scope).and_then(|entry| entry.expired_at(now)) {
            return Err(format!(
                "vessel `{vessel}` depends on the ambient claude login on host `{host_label}`, which expired on {} — \
                 log in again on that host or grant a delivered claude credential",
                expired_at.format("%Y-%m-%d")
            ));
        }
    }
    Ok(())
}

/// Vessels whose agent crews will authenticate through a host's ambient login
/// rather than delivered material: host-direct vessels with a crew on an
/// ambient-capable adapter and no granted credential covering that adapter's
/// delivery slot. Returns `(vessel name, ambient scope)` pairs, the scope
/// being the entry name under the Host `credential_expiry` capability.
/// The seeded adapters currently pair ambient Claude scope with a delivery
/// slot, so this is forward-provisioned for an ambient-only adapter.
fn ambient_credential_dependent_vessels<'workflow>(
    capabilities: &CapabilityTable,
    specs: &BTreeMap<String, CredentialConsumer>,
    workflow: &'workflow WorkflowTemplateSpec,
    host_direct: bool,
) -> Result<Vec<(&'workflow str, &'static str)>, String> {
    let mut vessels = Vec::new();
    for vessel in workflow.vessels.iter().filter(|_| host_direct) {
        for crew in &vessel.crew {
            let CrewSource::Agent { selector, .. } = &crew.source else {
                continue;
            };
            let requirement = capabilities.resolve_selector(selector)?;
            let Some(scope) = requirement.ambient_credential_scope() else {
                continue;
            };
            let delivery_slot = requirement.credential_delivery_slot();
            let has_delivered_credential = delivery_slot.is_some_and(|slot| {
                vessel.credential_refs.iter().any(|name| specs.get(name).is_some_and(|consumer| consumer.delivery_slot() == slot))
            });
            if !has_delivered_credential {
                vessels.push((vessel.name.as_str(), scope));
                break;
            }
        }
    }
    Ok(vessels)
}

/// Write dispatch-time agent choices into the workflow spec that is about to
/// be snapshotted, so every downstream consumer — placement validation, the
/// vessel reconciler, terminal launch — reads the effective requirement from
/// the selector itself. Loud on anything that cannot take effect: a
/// capability named twice, or one no agent selector in the workflow carries.
/// Dispatch overrides cross the protocol boundary from arbitrary clients, but
/// adapter ids and model names land in fields the launch layer treats as
/// resolver-trusted (`Arg`'s safety invariant). Constrain them to the token
/// charset real harness and model names use before they enter the snapshot.
fn valid_agent_override_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn apply_agent_overrides(workflow: &mut WorkflowTemplateSpec, overrides: &[flotilla_protocol::AgentOverride]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for choice in overrides {
        if !seen.insert(choice.capability.as_str()) {
            return Err(format!("duplicate --agent override for capability `{}`", choice.capability));
        }
        if !valid_agent_override_token(&choice.adapter) {
            return Err(format!("agent adapter `{}` may only contain alphanumerics, `.`, `_`, and `-`", choice.adapter));
        }
        if let Some(model) = &choice.model {
            if !valid_agent_override_token(model) {
                return Err(format!("agent model `{model}` may only contain alphanumerics, `.`, `_`, and `-`"));
            }
        }
        let mut matched = false;
        for crew in workflow.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew) {
            if let CrewSource::Agent { selector, .. } = &mut crew.source {
                if selector.capability == choice.capability {
                    selector.adapter = Some(choice.adapter.clone());
                    selector.model = choice.model.clone();
                    matched = true;
                }
            }
        }
        if !matched {
            let available = workflow
                .vessels
                .iter()
                .flat_map(|vessel| &vessel.crew)
                .filter_map(|crew| match &crew.source {
                    CrewSource::Agent { selector, .. } => Some(selector.capability.as_str()),
                    CrewSource::Tool { .. } => None,
                })
                .collect::<BTreeSet<_>>();
            if available.is_empty() {
                return Err(format!(
                    "--agent override names capability `{}`, but this workflow has no agent crew to override",
                    choice.capability
                ));
            }
            return Err(format!(
                "--agent override names capability `{}`, but this workflow's agent capabilities are: {}",
                choice.capability,
                available.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    Ok(())
}

fn required_workflow_agent_adapters(workflow: &WorkflowTemplateSpec) -> Result<BTreeSet<String>, String> {
    required_agent_adapters(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
}

async fn placement_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    placement: &ResourceObject<PlacementPolicy>,
    allow_unready: bool,
) -> Result<(BTreeSet<String>, String), String> {
    if let Some(docker) = &placement.spec.docker_per_vessel {
        let image = docker.image.resolve(&backend.definitions(namespace)).await?;
        Ok((docker.agent_adapters.clone(), format!("image `{image}`")))
    } else if placement.spec.host_direct.is_some() {
        let target_host = placement_target_host(backend, namespace, placement).await?;
        let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
        let host_label = target_host.display_name;
        let generation = host_generation(host.status.as_ref()).to_string();
        let mut status = host.status.ok_or_else(|| {
            format!("placement `{}` host `{host_label}` generation `{generation}` has no observed status", placement.metadata.name)
        })?;
        status.apply_heartbeat_readiness(Utc::now());
        if !status.ready && !allow_unready {
            return Err(placement_host_not_ready_reason(&placement.metadata.name, &host_label, &generation, &status));
        }
        let available_adapters = status.agent_adapters().map_err(|error| {
            format!(
                "placement `{}` host `{}` generation `{generation}` has invalid agent adapter capabilities: {error}",
                placement.metadata.name, host_label
            )
        })?;
        Ok((available_adapters, format!("host `{host_label}`")))
    } else {
        Ok((BTreeSet::new(), "unknown target environment".to_string()))
    }
}

fn convoy_fallback_slug(title: &str, id: &str) -> String {
    let slug = format!("{title}-{id}")
        .chars()
        .fold((String::new(), false), |(mut output, pending_separator), character| {
            if character.is_ascii_alphanumeric() {
                if pending_separator && !output.is_empty() {
                    output.push('-');
                }
                output.push(character.to_ascii_lowercase());
                (output, false)
            } else {
                (output, true)
            }
        })
        .0;
    let slug = if slug.is_empty() { "convoy".to_string() } else { slug };
    const MAX_CONVOY_NAME_LEN: usize = 63;
    if slug.len() <= MAX_CONVOY_NAME_LEN {
        return slug;
    }
    let digest = format!("{:x}", Sha256::digest(slug.as_bytes()));
    let suffix = &digest[..8];
    let max_base_len = MAX_CONVOY_NAME_LEN - suffix.len() - 1;
    let base = slug.chars().take(max_base_len).collect::<String>().trim_matches('-').to_string();
    format!("{base}-{suffix}")
}

fn convoy_issues_fallback_slug(issues: &[ConvoyIssue], project_display_name: &str, project_ref: &str) -> String {
    match issues {
        [] => convoy_fallback_slug(project_display_name, project_ref),
        [issue] => convoy_fallback_slug(&issue.snapshot.title, &issue.reference.id),
        issues => {
            let issue_ids = issues.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>().join("-");
            convoy_fallback_slug("batch-issues", &issue_ids)
        }
    }
}

fn convoy_issue_name_context(issue: &ConvoyIssue) -> String {
    format!("Issue {}: {}\n{}", issue.reference.id, issue.snapshot.title, issue.snapshot.body.as_deref().unwrap_or_default())
}

fn validate_convoy_name(name: &str) -> Result<(), String> {
    if name.len() > 63
        || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !name.bytes().last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("convoy name `{name}` must be a lowercase DNS label of at most 63 characters"));
    }
    Ok(())
}

fn validate_convoy_branch(branch: &str) -> Result<(), String> {
    let invalid_character =
        branch.bytes().any(|byte| byte <= b' ' || byte == 0x7f || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'));
    let invalid_component =
        branch.split('/').any(|component| component.is_empty() || component.starts_with('.') || component.ends_with(".lock"));
    if branch.len() > 1024
        || branch == "@"
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || invalid_character
        || invalid_component
    {
        return Err(format!("branch `{branch}` is not a valid git branch name"));
    }
    Ok(())
}

fn parse_ad_hoc_capability_need(value: &str) -> Result<CapabilityNeed, String> {
    let need = value.parse::<CapabilityNeed>()?;
    if matches!(&need, CapabilityNeed::Platform(platform) if platform == "$matrix") {
        return Err("platform:$matrix is only valid on workflow roles or Project role needs".to_string());
    }
    Ok(need)
}

impl InProcessDaemon {
    async fn resolve_convoy_issue(
        &self,
        namespace: &str,
        project: &ResourceObject<Project>,
        selector: &flotilla_protocol::IssueSelector,
    ) -> Result<ConvoyIssue, String> {
        let sources =
            match resolve_project_issue_sources(&self.resource_backend.including_replicas::<Repository>(namespace), &project.spec).await {
                IssueSourceResolution::Available { bindings } => bindings,
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { repository, message }) => {
                    return Err(format!("repository {repository}: {message}"));
                }
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::InvalidBindings { message }) => return Err(message),
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource) => {
                    return Err(format!("project {} has no issue source", project.metadata.name));
                }
            };
        let issue = match selector {
            flotilla_protocol::IssueSelector::Reference(reference) => {
                let source = normalize_issue_source(&reference.source);
                let Some(binding) = sources.iter().find(|binding| binding.source == source) else {
                    let available = sources
                        .iter()
                        .map(|binding| format!("{} {}", binding.source.service, binding.source.scope))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let requested_host = source.service.split_once("://").map_or(source.service.as_str(), |(_, host)| host);
                    let suggestion = sources.iter().find(|binding| {
                        binding.source.scope == source.scope
                            && binding.source.service.split_once("://").map_or(binding.source.service.as_str(), |(_, host)| host)
                                == requested_host
                    });
                    let hint = suggestion.map_or_else(String::new, |binding| format!("; did you mean `{}`?", binding.source.service));
                    return Err(format!(
                        "issue source {} {} is not part of project {}; available issue sources: {available}{hint}",
                        reference.source.service, reference.source.scope, project.metadata.name
                    ));
                };
                self.resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef {
                    source: binding.source.clone(),
                    id: reference.id.clone(),
                })
                .await?
            }
            flotilla_protocol::IssueSelector::Alias { alias, id } => {
                let binding = sources
                    .iter()
                    .find(|binding| binding.alias == *alias)
                    .ok_or_else(|| format!("project {} has no issue source alias `{alias}`", project.metadata.name))?;
                self.resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef { source: binding.source.clone(), id: id.clone() }).await?
            }
            flotilla_protocol::IssueSelector::Id(id) => {
                if sources.len() != 1 {
                    return Err(format!(
                        "issue {id} requires an alias because project {} has {} issue sources",
                        project.metadata.name,
                        sources.len()
                    ));
                }
                self.resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef { source: sources[0].source.clone(), id: id.clone() })
                    .await?
            }
        };

        let repositories = self.resource_backend.including_replicas::<Repository>(namespace);
        let mut matching_repositories = Vec::new();
        for project_repository in &project.spec.repositories {
            let repository = repositories
                .get(&project_repository.repo.to_string())
                .await
                .map_err(|error| format!("repository {}: {error}", project_repository.repo))?;
            if repository.object.spec.issue_source_forge().is_some_and(|forge| {
                forge.service_url == issue.reference.source.service && forge.repository == issue.reference.source.scope
            }) {
                matching_repositories.push(project_repository.repo.clone());
            }
        }
        let repository_ref = match matching_repositories.as_slice() {
            [repository] => Some(repository.clone()),
            [] if project.spec.repositories.len() == 1 => Some(project.spec.repositories[0].repo.clone()),
            _ => None,
        };

        Ok(ConvoyIssue {
            reference: issue.reference,
            repository_ref,
            snapshot: IssueSnapshot {
                title: issue.title,
                body: issue.body,
                state: issue.state,
                labels: issue.labels,
                as_of: issue.observed_at.expect("admission only accepts observed issue snapshots"),
            },
        })
    }

    async fn prepare_convoy_admission(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<ConvoyAdmission, String> {
        self.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, None).await
    }

    async fn resolve_convoy_admission_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        let workflow_ref = match intent.workflow_ref.as_deref() {
            Some(workflow_ref) => required_admission_value(workflow_ref, "workflow")?.to_string(),
            None if intent.change_request.is_some() => "single-agent-shepherd".to_string(),
            None => project.default_workflow_ref.clone(),
        };
        let templates = self.resource_backend.definitions::<WorkflowTemplate>(namespace);
        let scoped_workflow_ref = crate::ops_entry::materialized_workflow_name(project_ref, &workflow_ref);
        let mut workflow = match templates.get(&scoped_workflow_ref).await {
            Ok(workflow) => workflow,
            Err(ResourceError::NotFound { .. }) => templates
                .get(&workflow_ref)
                .await
                .map_err(|error| format!("workflow template {workflow_ref} for project {project_ref}: {error}"))?,
            Err(error) => return Err(format!("workflow template {workflow_ref} for project {project_ref}: {error}")),
        };
        if workflow.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).is_some_and(|owner| owner != project_ref) {
            return Err(format!("workflow template {workflow_ref} is materialized by another project"));
        }
        apply_agent_overrides(&mut workflow.spec, &intent.agent_overrides)?;
        validate_fork_workflow_admission(&self.resource_backend, namespace, repositories, &workflow_ref, &workflow.spec).await?;
        Ok((workflow_ref, workflow.spec))
    }

    async fn compose_convoy_needs(
        &self,
        namespace: &str,
        project: &ProjectSpec,
        issues: &[ConvoyIssue],
        intent: &flotilla_protocol::ConvoyStartIntent,
        workflow: &mut WorkflowTemplateSpec,
    ) -> Result<BTreeSet<CapabilityNeed>, String> {
        let mut common = BTreeSet::new();
        for issue in issues {
            for label in &issue.snapshot.labels {
                if let Some(value) = label.strip_prefix("needs:") {
                    common.insert(parse_ad_hoc_capability_need(value).map_err(|error| format!("issue {}: {error}", issue.reference.id))?);
                }
            }
        }
        for value in &intent.needs {
            common.insert(parse_ad_hoc_capability_need(value)?);
        }
        let hosts = self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        let mut union = BTreeSet::new();
        for vessel in &mut workflow.vessels {
            let mut vessel_needs = BTreeSet::new();
            for crew in &mut vessel.crew {
                crew.needs.extend(common.iter().cloned());
                if let Some(standing) = project.role_needs.get(&crew.role) {
                    crew.needs.extend(standing.iter().filter(|need| **need != CapabilityNeed::Platform("$matrix".to_string())).cloned());
                }
                if let CrewSource::Agent { selector, .. } = &crew.source {
                    if let (Some(adapter), Some(model)) = (&selector.adapter, &selector.model) {
                        // Only an explicit rejection refuses. Model acceptance is often
                        // unobservable (credential-less probe containers, harnesses with
                        // no model probe), and unknown must not read as "rejected".
                        let observed_harnesses = hosts
                            .items
                            .iter()
                            .flat_map(|host| host.object.status.as_ref().into_iter())
                            .flat_map(|status| status.fulfilment_facts.values())
                            .filter_map(|facts| facts.harnesses.get(adapter))
                            .collect::<Vec<_>>();
                        let minimum = observed_harnesses
                            .iter()
                            .filter(|harness| harness.models.get(model).is_some_and(|model| model.usable))
                            .map(|harness| harness.version.as_str())
                            .reduce(
                                |minimum, version| if flotilla_resources::version_at_least(minimum, version) { version } else { minimum },
                            );
                        let rejected_everywhere = !observed_harnesses.is_empty()
                            && observed_harnesses.iter().all(|harness| harness.models.get(model).is_some_and(|model| !model.usable));
                        if let Some(minimum) = minimum {
                            crew.needs.insert(CapabilityNeed::Harness { adapter: adapter.clone(), minimum_version: minimum.to_string() });
                        } else if rejected_everywhere {
                            return Err(format!("no observed {adapter} harness accepts model {model}"));
                        } else if !observed_harnesses.is_empty() {
                            // Acceptance is unknown on at least one observed harness:
                            // admit without a version floor.
                        } else {
                            let kinds = self
                                .resource_backend
                                .including_replicas::<FulfilmentKind>(namespace)
                                .list()
                                .await
                                .map_err(|error| error.to_string())?;
                            let names = kinds.items.iter().map(|kind| kind.object.metadata.name.as_str()).collect::<Vec<_>>();
                            if !names.is_empty() {
                                return Err(format!(
                                    "facts not yet observed for kind {} (needed for {adapter} model {model})",
                                    names.join(", kind ")
                                ));
                            }
                        }
                    }
                }
                vessel_needs.extend(crew.needs.iter().cloned());
            }
            for left in &vessel_needs {
                for right in &vessel_needs {
                    if left.conflicts_with(right) {
                        return Err(format!("vessel {} has conflicting needs `{left}` and `{right}`", vessel.name));
                    }
                }
            }
            union.extend(vessel_needs);
        }
        Ok(union)
    }

    async fn resolve_capability_placement(
        &self,
        namespace: &str,
        project_ref: &str,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        let pin = intent.placement_policy.as_deref();
        let escalation_reason = intent.escalation_reason.as_deref();
        let kinds = home_copy_wins_by_name(
            self.resource_backend.including_replicas::<FulfilmentKind>(namespace).list().await.map_err(|error| error.to_string())?.items,
        );
        if kinds.is_empty() {
            if let Some(need) = needs.iter().next() {
                return Err(format!("no fulfilment kind covers role need `{need}`"));
            }
            return self
                .resolve_convoy_placement(namespace, Some(project_ref), repositories, workflow, pin, false)
                .await
                .map(|placement| (placement, Vec::new()));
        }
        let hosts = home_copy_wins_by_name(
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?.items,
        );
        if let Some(pin) = pin {
            if !kinds.iter().any(|kind| kind.metadata.name == pin) {
                return Err(format!("fulfilment kind `{pin}` does not exist"));
            }
        }
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
        for mut kind in kinds {
            let canonical_kind_host = match flotilla_resources::canonical_host_id(hosts.iter(), &kind.spec.host_ref) {
                Ok(host) => host,
                Err(error) => {
                    rejected.push(format!("{}: {error}", kind.metadata.name));
                    continue;
                }
            };
            let host = hosts.iter().find(|host| canonical_kind_host.as_ref().is_some_and(|id| host.metadata.name == id.as_str()));
            if let Some(canonical_kind_host) = &canonical_kind_host {
                kind.spec.host_ref = canonical_kind_host.to_string();
            }
            let facts = host.and_then(|host| host.status.as_ref()).and_then(|status| status.fulfilment_facts.get(&kind.metadata.name));
            let structurally_missing = needs
                .iter()
                .filter(|need| match need {
                    CapabilityNeed::GuiSession => !kind.spec.grants.contains(&FulfilmentGrant::GuiSession),
                    CapabilityNeed::Toolchain(_) | CapabilityNeed::Harness { .. } => false,
                    _ => !need.covered_by(&kind.spec.grants, None),
                })
                .collect::<Vec<_>>();
            if !structurally_missing.is_empty() {
                rejected.push(format!(
                    "{}: uncovered {}",
                    kind.metadata.name,
                    structurally_missing.iter().map(|need| format!("`{need}`")).collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
            if facts.is_none()
                && needs
                    .iter()
                    .any(|need| matches!(need, CapabilityNeed::GuiSession | CapabilityNeed::Toolchain(_) | CapabilityNeed::Harness { .. }))
            {
                rejected.push(format!("{}: facts not yet observed for kind {}", kind.metadata.name, kind.metadata.name));
                continue;
            }
            let missing = needs.iter().filter(|need| !need.covered_by(&kind.spec.grants, facts)).collect::<Vec<_>>();
            if !missing.is_empty() {
                rejected.push(format!(
                    "{}: uncovered {}",
                    kind.metadata.name,
                    missing.iter().map(|need| format!("`{need}`")).collect::<Vec<_>>().join(", ")
                ));
                continue;
            }
            match self.resolve_convoy_placement(namespace, Some(project_ref), repositories, workflow, Some(&kind.metadata.name), true).await
            {
                Ok(placement) => {
                    let policy = placement.selected.as_ref().expect("pinned placement has a policy");
                    let policy_host_ref = match (&kind.spec.realisation, &policy.spec.docker_per_vessel, &policy.spec.host_direct) {
                        (flotilla_resources::FulfilmentRealisation::DockerPerVessel { .. }, Some(docker), None) => {
                            Some(docker.host_ref.as_str())
                        }
                        (flotilla_resources::FulfilmentRealisation::HostDirect, None, Some(direct)) => Some(direct.host_ref.as_str()),
                        _ => None,
                    };
                    let policy_host = match policy_host_ref.map(|host_ref| flotilla_resources::canonical_host_id(hosts.iter(), host_ref)) {
                        Some(Ok(host)) => host,
                        Some(Err(error)) => {
                            rejected.push(format!("{}: {error}", kind.metadata.name));
                            continue;
                        }
                        None => None,
                    };
                    let realization_matches = policy_host_ref.is_some() && policy_host == canonical_kind_host;
                    if !realization_matches {
                        rejected
                            .push(format!("{}: fulfilment kind and placement policy disagree on host or realisation", kind.metadata.name));
                        continue;
                    }
                    let free_slots = facts.and_then(|facts| facts.free_vessel_slots);
                    let host_ready = host.and_then(|host| host.status.as_ref()).is_some_and(|status| {
                        let mut status = status.clone();
                        status.apply_heartbeat_readiness(self.clock.now());
                        status.ready
                    });
                    let sleeping_until = host.and_then(|host| host.status.as_ref()).and_then(|status| status.sleeping_until);
                    candidates.push(KindCandidate { kind, placement, free_slots, host_ready, sleeping_until });
                }
                Err(error) => rejected.push(format!("{}: {error}", kind.metadata.name)),
            }
        }
        if candidates.is_empty() {
            let role_needs = workflow
                .vessels
                .iter()
                .flat_map(|vessel| vessel.crew.iter())
                .flat_map(|crew| crew.needs.iter().map(move |need| format!("role {} need `{need}`", crew.role)))
                .collect::<Vec<_>>();
            return Err(format!("no fulfilment kind covers {}; candidates: {}", role_needs.join(", "), rejected.join("; ")));
        }
        let placement_tiebreak = PlacementTieBreak { needs, now: self.clock.now() };
        // A scarce platform is not a fallback for work that did not ask for it.
        // Pins still require an explicit escalation reason through the normal path.
        let mut reserved = Vec::new();
        if pin.is_none() {
            (candidates, reserved) = candidates.into_iter().partition(|candidate| !placement_tiebreak.reserved(candidate));
            if candidates.is_empty() {
                return Err("no fulfilment kind covers needs without consuming reserved macOS or Windows capacity".to_string());
            }
        }
        let minimal = candidates
            .iter()
            .filter(|candidate| {
                !candidates.iter().any(|other| {
                    other.kind.metadata.name != candidate.kind.metadata.name
                        && flotilla_resources::effective_grants(&other.kind.spec.grants)
                            .is_subset(&flotilla_resources::effective_grants(&candidate.kind.spec.grants))
                        && flotilla_resources::effective_grants(&other.kind.spec.grants)
                            != flotilla_resources::effective_grants(&candidate.kind.spec.grants)
                })
            })
            .map(|candidate| candidate.kind.metadata.name.clone())
            .collect::<BTreeSet<_>>();
        candidates.sort_by(|left, right| placement_tiebreak.compare(left, right));
        let index = match pin {
            Some(pin) => candidates
                .iter()
                .position(|candidate| candidate.kind.metadata.name == pin)
                .ok_or_else(|| format!("pinned fulfilment `{pin}` cannot cover vessel needs; candidates: {}", rejected.join("; ")))?,
            None => candidates.iter().position(|candidate| minimal.contains(&candidate.kind.metadata.name)).expect("nonempty minimal set"),
        };
        let chosen_kind = candidates[index].kind.metadata.name.clone();
        let allocation = FulfilmentAllocation {
            chosen_kind,
            candidates: candidates
                .iter()
                .chain(reserved.iter())
                .map(|candidate| FulfilmentAllocationCandidate {
                    kind: candidate.kind.metadata.name.clone(),
                    host: candidate.kind.spec.host_ref.clone(),
                    cost_class: match candidate.kind.spec.cost_class {
                        FulfilmentCostClass::OwnedIdle => "owned_idle",
                        FulfilmentCostClass::SubscriptionIncluded => "subscription_included",
                        FulfilmentCostClass::Metered => "metered",
                    }
                    .to_string(),
                    host_ready: candidate.host_ready,
                    sleeping_until: candidate.sleeping_until,
                    free_vessel_slots: candidate.free_slots,
                    reserved_for_platform: placement_tiebreak.reserved(candidate),
                    minimal: minimal.contains(&candidate.kind.metadata.name),
                    available: placement_tiebreak.available(candidate),
                })
                .collect(),
        };
        let mut selected = candidates.remove(index);
        if placement_tiebreak.reserved(&selected) && escalation_reason.is_none_or(|reason| reason.trim().is_empty()) {
            return Err(format!(
                "fulfilment `{}` reserves scarce platform capacity; supply --escalation-reason to pin it for work without a platform need",
                selected.kind.metadata.name
            ));
        }
        if !minimal.contains(&selected.kind.metadata.name) && escalation_reason.is_none_or(|reason| reason.trim().is_empty()) {
            return Err(format!(
                "fulfilment `{}` exceeds minimal alternatives {}; supply --escalation-reason",
                selected.kind.metadata.name,
                minimal.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        let alternatives = minimal.iter().filter(|name| **name != selected.kind.metadata.name).cloned().collect::<Vec<_>>();
        for candidate in candidates {
            let policy = candidate.placement.selected.as_ref().expect("validated candidate has policy");
            let target_host = placement_target_host(&self.resource_backend, namespace, policy).await?;
            selected.placement.viable_not_selected.push(PlacementViableCandidate {
                policy_name: candidate.kind.metadata.name.clone(),
                target_host,
                reason: if minimal.contains(&candidate.kind.metadata.name) { "minimal alternative" } else { "grants a strict superset" }
                    .to_string(),
            });
        }
        selected.placement.allocation = Some(allocation);
        Ok((selected.placement, alternatives))
    }

    async fn prepare_convoy_admission_with_preferences(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
        repositories: Option<&[RepositoryKey]>,
    ) -> Result<ConvoyAdmission, String> {
        let project_ref = required_admission_value(&intent.project_ref, "project")?;
        let project = self
            .resource_backend
            .clone()
            .including_replicas::<Project>(namespace)
            .get(project_ref)
            .await
            .map(|project| project.object)
            .map_err(|error| project_not_ready_error(namespace, project_ref, error))?;
        let mut repositories_snapshot = self.snapshot_project_repositories(namespace, project_ref, repositories).await?;
        if let Some(selected) = repositories {
            let available = repositories_snapshot.iter().map(|repository| &repository.repo_ref).collect::<BTreeSet<_>>();
            if let Some(missing) = selected.iter().find(|repository| !available.contains(repository)) {
                return Err(format!("standing convoy selects repository {missing} outside project {project_ref}"));
            }
            repositories_snapshot.retain(|repository| selected.contains(&repository.repo_ref));
            if repositories_snapshot.is_empty() {
                return Err("standing convoy must select at least one project repository".to_string());
            }
        }
        if intent.change_request.is_some() && intent.branch.is_some() {
            return Err("change request adoption derives the branch from --pr; do not also provide a branch".to_string());
        }
        if intent.change_request.is_some() && !intent.issues.is_empty() {
            return Err("change request adoption is PR-first; do not also provide issues".to_string());
        }
        let change_request = match intent.change_request.as_deref() {
            Some(id) => {
                let id = required_admission_value(id, "change request")?;
                let resolved = self
                    .resolve_convoy_change_request_admission(
                        &repositories_snapshot.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>(),
                        id,
                    )
                    .await?;
                let repository = repositories_snapshot
                    .iter_mut()
                    .find(|repository| repository.repo_ref == resolved.binding.repository_ref)
                    .expect("admission resolution only returns project repositories");
                repository.source_ref = resolved.base_ref.clone();
                repository.target_ref = resolved.base_ref.clone();
                Some(resolved)
            }
            None => None,
        };
        let mut seen_issue_selectors = HashSet::new();
        let mut issues = Vec::with_capacity(intent.issues.len());
        for selector in &intent.issues {
            if seen_issue_selectors.insert(selector.clone()) {
                issues.push(self.resolve_convoy_issue(namespace, &project, selector).await?);
            }
        }
        let (workflow_ref, mut workflow) =
            self.resolve_convoy_admission_workflow(namespace, project_ref, &project.spec, &repositories_snapshot, intent).await?;
        let mut allocation_roles = expand_allocation_roles(&mut workflow, &project.spec)?;
        self.compose_convoy_needs(namespace, &project.spec, &issues, intent, &mut workflow).await?;
        let grant_sets =
            allocation_credential_grants(&self.resource_backend, namespace, project_ref, &repositories_snapshot, &workflow.vessels).await?;
        for ((role, vessel), grant_set) in allocation_roles.iter_mut().zip(&workflow.vessels).zip(grant_sets) {
            role.crew = vessel.crew[0].clone();
            let mut one = WorkflowTemplateSpec { vessels: vec![vessel.clone()], ..workflow.clone() };
            resolve_workflow_credentials(&self.resource_backend, namespace, Some(project_ref), &repositories_snapshot, &mut one).await?;
            let resolved = &one.vessels[0];
            role.credential_signature = serde_json::to_string(&(
                grant_set,
                &resolved.credential_refs,
                &resolved.credential_scopes,
                &resolved.credential_permissions,
            ))
            .map_err(|error| error.to_string())?;
        }
        allocate_roles(&mut workflow, &allocation_roles)?;
        if intent.placement_policy.is_none() && intent.escalation_reason.is_some() {
            return Err("--escalation-reason requires --fulfilment".to_string());
        }

        let fallback_slug = change_request
            .as_ref()
            .map(|change_request| convoy_fallback_slug(&change_request.binding.title, &change_request.binding.id))
            .unwrap_or_else(|| convoy_issues_fallback_slug(&issues, &project.spec.display_name, project_ref));
        let generated = if change_request.is_none() && (intent.name.is_none() || intent.branch.is_none()) {
            let issue_context = (!issues.is_empty()).then(|| issues.iter().map(convoy_issue_name_context).collect::<Vec<_>>().join("\n\n"));
            let context = [
                Some(format!("Project: {}", project.spec.display_name)),
                issue_context,
                intent.instruction.as_ref().map(|instruction| format!("Instruction: {instruction}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            match self.admission_ai_utility().await {
                Some(utility) => utility.generate_convoy_names(&context).await.ok(),
                None => None,
            }
        } else {
            None
        };
        let generated = generated.unwrap_or_else(|| ConvoyNames { name: fallback_slug.clone(), branch: fallback_slug.clone() });
        let role = intent
            .name
            .as_deref()
            .map(|name| required_admission_value(name, "name").map(str::to_string))
            .transpose()?
            .unwrap_or_else(|| convoy_fallback_slug(&generated.name, "").trim_end_matches('-').to_string());
        validate_convoy_name(&role)?;
        let branch = match (change_request.as_ref(), intent.branch.as_deref()) {
            (Some(change_request), None) => change_request.branch.clone(),
            (Some(_), Some(_)) => unreachable!("change request plus branch was rejected"),
            (None, Some(branch)) => required_admission_value(branch, "branch")?.to_string(),
            (None, None) => required_admission_value(&generated.branch, "generated branch")?.to_string(),
        };
        validate_convoy_branch(&branch)?;
        let mut vessel_placements = BTreeMap::new();
        let has_kinds = !self
            .resource_backend
            .including_replicas::<FulfilmentKind>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .is_empty();
        let (placement, minimal_alternatives) = if has_kinds && !workflow.vessels.is_empty() {
            let mut first = None;
            let mut index = 0;
            while index < workflow.vessels.len() {
                let vessel = workflow.vessels[index].clone();
                let needs = vessel.crew.iter().flat_map(|crew| crew.needs.iter().cloned()).collect::<BTreeSet<_>>();
                let mut one = WorkflowTemplateSpec { vessels: vec![vessel.clone()], ..workflow.clone() };
                let (resolution, alternatives) =
                    match self.resolve_capability_placement(namespace, project_ref, &repositories_snapshot, &one, &needs, intent).await {
                        Ok(result) => result,
                        Err(error) if vessel.crew.len() > 1 => {
                            let split = vessel
                                .crew
                                .iter()
                                .map(|crew| VesselRequirement {
                                    name: format!("{}[{}]", vessel.name, crew.role),
                                    crew: vec![crew.clone()],
                                    ..vessel.clone()
                                })
                                .collect::<Vec<_>>();
                            let split_names = split.iter().map(|part| part.name.clone()).collect::<Vec<_>>();
                            for other in &mut workflow.vessels {
                                if other.depends_on.iter().any(|dependency| dependency == &vessel.name) {
                                    other.depends_on.retain(|dependency| dependency != &vessel.name);
                                    other.depends_on.extend(split_names.iter().cloned());
                                }
                            }
                            for rule in workflow.turn_delivery.values_mut() {
                                if rule.to.vessel == vessel.name {
                                    if let Some(part) = split.iter().find(|part| part.crew[0].role == rule.to.role) {
                                        rule.to.vessel = part.name.clone();
                                    }
                                }
                            }
                            for part in &split {
                                if let Some(policy) = workflow.stall_nudges.shift_remove(&format!("{}/{}", vessel.name, part.crew[0].role))
                                {
                                    workflow.stall_nudges.insert(format!("{}/{}", part.name, part.crew[0].role), policy);
                                }
                            }
                            if let Some(targets) = &mut workflow.supervision {
                                for target in targets {
                                    if let SupervisionTarget::ConvoyCrew { vessel: target_vessel, role } = target {
                                        if *target_vessel == vessel.name {
                                            if let Some(part) = split.iter().find(|part| part.crew[0].role == *role) {
                                                *target_vessel = part.name.clone();
                                            }
                                        }
                                    }
                                }
                            }
                            workflow.vessels.splice(index..=index, split.clone());
                            workflow.allocation.retain(|decision| decision.vessel != vessel.name);
                            workflow.allocation.extend(split.iter().map(|part| AllocationDecision {
                                vessel: part.name.clone(),
                                roles: vec![part.crew[0].role.clone()],
                                reason: format!("split after placement could not cover union: {error}"),
                                crossed_handoffs: Vec::new(),
                            }));
                            continue;
                        }
                        Err(error) => {
                            return Err(format!(
                                "no fulfilment covers role `{}` need {}: {error}",
                                vessel.crew[0].role,
                                needs.iter().map(ToString::to_string).collect::<Vec<_>>().join(" + ")
                            ));
                        }
                    };
                resolve_and_validate_workflow_credentials_for_capability_admission(
                    &self.resource_backend,
                    namespace,
                    Some(project_ref),
                    &repositories_snapshot,
                    resolution.selected.as_ref(),
                    &mut one,
                )
                .await?;
                workflow.vessels[index] = one.vessels.remove(0);
                if let Some(selected) = resolution.selected.as_ref() {
                    let decision = PlacementDecision {
                        minimal_alternatives: alternatives.clone(),
                        escalation_reason: intent.escalation_reason.clone(),
                        policy_name: selected.metadata.name.clone(),
                        target_host: placement_target_host(&self.resource_backend, namespace, selected).await?,
                        refused_candidates: resolution.refused_candidates.clone(),
                        viable_not_selected: resolution.viable_not_selected.clone(),
                        allocation: resolution.allocation.clone(),
                    };
                    vessel_placements.insert(vessel.name.clone(), (selected.spec.clone(), decision));
                }
                if first.is_none() {
                    first = Some((resolution, alternatives));
                }
                index += 1;
            }
            first.expect("nonempty vessels")
        } else {
            let needs = workflow.vessels.iter().flat_map(|vessel| vessel.crew.iter()).flat_map(|crew| crew.needs.iter().cloned()).collect();
            let result =
                self.resolve_capability_placement(namespace, project_ref, &repositories_snapshot, &workflow, &needs, intent).await?;
            if has_kinds {
                resolve_and_validate_workflow_credentials_for_capability_admission(
                    &self.resource_backend,
                    namespace,
                    Some(project_ref),
                    &repositories_snapshot,
                    result.0.selected.as_ref(),
                    &mut workflow,
                )
                .await?;
            } else {
                resolve_and_validate_workflow_credentials(
                    &self.resource_backend,
                    namespace,
                    Some(project_ref),
                    &repositories_snapshot,
                    result.0.selected.as_ref(),
                    &mut workflow,
                )
                .await?;
            }
            result
        };
        refresh_crossed_handoffs(&mut workflow);
        flotilla_resources::validate(&workflow).map_err(|errors| {
            format!("allocated workflow invalid: {}", errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
        })?;
        let placement_policy = placement.selected.as_ref().map(|placement| placement.metadata.name.clone());
        let placement_decision = match placement.selected.as_ref() {
            Some(selected) => Some(PlacementDecision {
                minimal_alternatives,
                escalation_reason: intent.escalation_reason.clone(),
                policy_name: selected.metadata.name.clone(),
                target_host: placement_target_host(&self.resource_backend, namespace, selected).await?,
                refused_candidates: placement.refused_candidates,
                viable_not_selected: placement.viable_not_selected,
                allocation: placement.allocation,
            }),
            None => None,
        };
        let spec = ConvoySpec {
            workflow_ref,
            role,
            generation: 0,
            dispatching_principal_ref: dispatching_principal_ref.clone(),
            inputs: intent.inputs.iter().map(|(key, value)| (key.clone(), InputValue::String(value.clone()))).collect(),
            placement_policy,
            repositories: repositories_snapshot,
            r#ref: Some(branch),
            project_ref: Some(project_ref.to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            subjects: Vec::new(),
            issues,
            change_request: change_request.map(|change_request| change_request.binding),
            instruction: intent.instruction.clone(),
        };
        Ok(ConvoyAdmission::builder()
            .name(String::new())
            .spec(spec)
            .workflow(workflow)
            .maybe_placement_policy(placement.selected.map(|placement| placement.spec))
            .maybe_placement_decision(placement_decision)
            .vessel_placements(vessel_placements)
            .build())
    }

    async fn admit_convoy_start(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<(String, String), String> {
        self.check_local_free_space_floor().await?;
        let mut admission = self.prepare_convoy_admission(namespace, intent, dispatching_principal_ref).await?;
        if admission.vessel_placements.is_empty() {
            self.check_remote_placement_free_space_floor(namespace, admission.placement_decision.as_ref()).await?;
        }
        for (_, decision) in admission.vessel_placements.values() {
            self.check_remote_placement_free_space_floor(namespace, Some(decision)).await?;
        }
        let _admission_guard = self.convoy_admission.lock().await;
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation(&self.resource_backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role)
                .await?;
        self.create_convoy_with_workflow_snapshot(
            namespace,
            &admission.name,
            ConvoySnapshotBundle::builder()
                .spec(&admission.spec)
                .workflow(&admission.workflow)
                .maybe_placement(admission.placement_policy.as_ref())
                .maybe_placement_decision(admission.placement_decision)
                .vessel_placements(&admission.vessel_placements)
                .build(),
            intent.auto_attach.into(),
        )
        .await?;
        let address = convoy_address(&admission.spec.role, admission.spec.project_ref.as_deref());
        Ok((admission.name, address))
    }

    /// Drive one deterministic pass of the standing-convoy ensure loop.
    ///
    /// Tests call this directly with an in-memory backend and virtual clock;
    /// the daemon runtime invokes the same pass on its resync cadence.
    pub async fn reconcile_convoy_ensures_once(&self, namespace: &str) -> Result<Vec<String>, String> {
        self.reconcile_convoy_ensures_once_with_backing_inspector(namespace, self).await
    }

    pub async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).list().await.map_err(|e| e.to_string())?;
        let ensure_names = ensures.iter().map(|ensure| ensure.metadata.name.clone()).collect::<HashSet<_>>();
        self.ensure_admission_retries
            .lock()
            .await
            .retain(|(retry_namespace, retry_name), _| retry_namespace != namespace || ensure_names.contains(retry_name));
        let local_projects = self.resource_backend.clone().using::<Project>(namespace);
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        for ensure in ensures {
            if let Some(driver_ref) = &ensure.spec.driver_ref {
                if self.resource_backend.clone().using::<ConvoyEnsure>(namespace).get(&ensure.metadata.name).await.is_ok()
                    && ensure.status.as_ref().is_some_and(|status| {
                        status.convoy_ref.is_some() || status.running_since.is_some() || status.observed_config_hash.is_some()
                    })
                {
                    if let Err(error) =
                        self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverManaged).await
                    {
                        errors.push(format!(
                            "ConvoyEnsure/{}: could not clear legacy status for driver ownership: {error}",
                            ensure.metadata.name
                        ));
                    }
                }
                let target = match canonical_placement_host_ref(&self.resource_backend, namespace, driver_ref).await {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        if let Err(error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "UnknownDriver",
                                format!("driver host `{driver_ref}` is unknown"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {error}", ensure.metadata.name));
                        }
                        continue;
                    }
                    Err(error) => {
                        if let Err(patch_error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "DriverUnreachable",
                                format!("driver host `{driver_ref}` could not be resolved: {error}"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {patch_error}", ensure.metadata.name));
                        }
                        continue;
                    }
                };
                if self.canonical_local_host_id().as_ref() != Some(&target.reference) {
                    let hosts = match self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await {
                        Ok(hosts) => hosts,
                        Err(error) => {
                            errors.push(format!(
                                "ConvoyEnsure/{}: could not inspect driver host `{driver_ref}` reachability: {error}",
                                ensure.metadata.name
                            ));
                            continue;
                        }
                    };
                    let reachable = hosts
                        .items
                        .iter()
                        .find(|host| host.object.metadata.name == target.reference.as_str())
                        .is_some_and(|host| host.object.status.as_ref().is_some_and(|status| status.ready));
                    if reachable {
                        if let Err(error) = self.clear_ensure_driver_condition(namespace, &ensure).await {
                            errors.push(format!("ConvoyEnsure/{}: could not clear driver condition: {error}", ensure.metadata.name));
                        }
                    } else {
                        if let Err(error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "DriverUnreachable",
                                format!("driver host `{driver_ref}` is not reachable"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {error}", ensure.metadata.name));
                        }
                    }
                    continue;
                }
                match self.reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector, false).await {
                    Ok(Some(change)) => changes.push(change),
                    Ok(None) => {}
                    Err(error) => errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name)),
                }
                continue;
            } else {
                match local_projects.get(&ensure.spec.project_ref).await {
                    Ok(project) if project.metadata.deletion_timestamp.is_none() => {}
                    Ok(_) | Err(ResourceError::NotFound { .. }) => {
                        match self.resource_backend.clone().definitions::<Project>(namespace).get(&ensure.spec.project_ref).await {
                            Ok(_) => {
                                debug!(
                                    ensure = %ensure.metadata.name,
                                    project = %ensure.spec.project_ref,
                                    "skipping standing convoy ensure away from its project home"
                                );
                                continue;
                            }
                            Err(ResourceError::NotFound { .. }) => {
                                errors.push(format!(
                                    "ConvoyEnsure/{}: parent Project/{} is absent",
                                    ensure.metadata.name, ensure.spec.project_ref
                                ));
                                continue;
                            }
                            Err(error) => {
                                errors.push(format!(
                                    "ConvoyEnsure/{}: could not resolve Project/{} authority: {error}",
                                    ensure.metadata.name, ensure.spec.project_ref
                                ));
                                continue;
                            }
                        }
                    }
                    Err(error) => {
                        errors.push(format!(
                            "ConvoyEnsure/{}: could not verify local Project/{} authority: {error}",
                            ensure.metadata.name, ensure.spec.project_ref
                        ));
                        continue;
                    }
                }
            }
            match self.reconcile_convoy_ensure(namespace, &ensure, backing_inspector, false).await {
                Ok(Some(change)) => changes.push(change),
                Ok(None) => {}
                Err(error) => errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name)),
            }
        }
        if errors.is_empty() {
            Ok(changes)
        } else if changes.is_empty() {
            Err(errors.join("; "))
        } else {
            Err(format!("{}; successful changes: {}", errors.join("; "), changes.join(", ")))
        }
    }

    /// Reset one ensure's retry budget and drive its admission synchronously.
    pub async fn reconcile_convoy_ensure_now(
        &self,
        namespace: &str,
        name: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<String, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace);
        let ensure = ensures.get(name).await.map_err(|error| error.to_string())?;
        if ensure.spec.driver_ref.is_some() {
            self.ensure_admission_retries.lock().await.remove(&(namespace.to_string(), name.to_string()));
            return self
                .reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector, true)
                .await
                .map(|change| change.unwrap_or_else(|| format!("ConvoyEnsure/{name} is already reconciled")));
        }
        self.reconcile_convoy_ensure(namespace, &ensure, backing_inspector, true)
            .await
            .map(|change| change.unwrap_or_else(|| format!("ConvoyEnsure/{name} is already reconciled")))
    }

    /// Fingerprint the named resources consulted by standing-convoy admission.
    /// A changed fingerprint invalidates a read-only refusal's deadline so the
    /// next reconciliation pass can retry immediately.
    async fn ensure_admission_dependency_hash(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let mut versions = BTreeMap::new();
        let projects = self.resource_backend.clone().including_replicas::<Project>(namespace);
        let project = match projects.get(&ensure.spec.project_ref).await {
            Ok(project) => {
                versions.insert(format!("Project/{}", ensure.spec.project_ref), project.object.metadata.resource_version.clone());
                Some(project.object)
            }
            Err(error) => {
                versions.insert(format!("Project/{}", ensure.spec.project_ref), format!("absent:{error}"));
                None
            }
        };
        if let Some(project) = project {
            let repositories = self.resource_backend.clone().including_replicas::<Repository>(namespace);
            let repository_sources = repositories.list_replica_sources().await.map_err(|error| error.to_string())?;
            for repository in &project.spec.repositories {
                let name = repository.repo.to_string();
                let mut found = false;
                for source in repository_sources.items.iter().filter(|source| source.object.metadata.name == name) {
                    let provenance = match &source.provenance {
                        ResourceProvenance::Local => "local".to_string(),
                        ResourceProvenance::Replica { origin_root, .. } => format!("replica:{origin_root}"),
                    };
                    versions.insert(format!("Repository/{name}/{provenance}"), source.object.metadata.resource_version.clone());
                    found = true;
                }
                if !found {
                    versions.insert(format!("Repository/{name}"), "absent".to_string());
                }
            }
        }
        let workflows = self.resource_backend.clone().definitions::<WorkflowTemplate>(namespace);
        for name in [
            crate::ops_entry::materialized_workflow_name(&ensure.spec.project_ref, &ensure.spec.workflow_ref),
            ensure.spec.workflow_ref.clone(),
        ] {
            let version = workflows
                .get(&name)
                .await
                .map(|workflow| workflow.metadata.resource_version)
                .unwrap_or_else(|error| format!("absent:{error}"));
            versions.insert(format!("WorkflowTemplate/{name}"), version);
        }
        if let Some(name) = &ensure.spec.placement_policy {
            let policies = self.resource_backend.including_replicas::<PlacementPolicy>(namespace);
            let version = policies
                .get(name)
                .await
                .map(|policy| policy.object.metadata.resource_version)
                .unwrap_or_else(|error| format!("absent:{error}"));
            versions.insert(format!("PlacementPolicy/{name}"), version);
        }
        let encoded = serde_json::to_vec(&versions).map_err(|error| format!("serialize ensure admission dependencies: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    /// Admit a declared-driver ensure from the generation history homed here.
    ///
    /// Driver admission keeps admission retry state on the driver because the
    /// ensure definition may be homed on another root. Failed generations
    /// remain the runtime strike budget; read-only admission refusals retry
    /// indefinitely on their own short, dependency-invalidated backoff.
    async fn reconcile_driver_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
        force_now: bool,
    ) -> Result<Option<String>, String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut generations = convoys
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name))
            .collect::<Vec<_>>();
        generations.sort_by_key(|convoy| convoy.spec.generation);

        let consecutive_failures = generations
            .iter()
            .rev()
            .take_while(|convoy| convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Failed))
            .count() as u32;

        if generations.last().is_some_and(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())) {
            return Ok(None);
        }

        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        let demand_name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{}", ensure.metadata.name);
        let resolved_escalation = match demands.get(&demand_name).await {
            Ok(demand)
                if demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated)) =>
            {
                if consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES && !force_now {
                    return Ok(None);
                }
                demands.delete(&demand_name).await.map_err(|error| error.to_string())?;
                consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES
            }
            Ok(_) => {
                demands.delete(&demand_name).await.map_err(|error| error.to_string())?;
                true
            }
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error.to_string()),
        };

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let config_hash = ensure_config_hash(&ensure.spec)?;
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        {
            let mut retries = self.ensure_admission_retries.lock().await;
            if retries.get(&retry_key).is_some_and(|retry| retry.config_hash != config_hash || retry.dependency_hash != dependency_hash) {
                retries.remove(&retry_key);
            }
            if !resolved_escalation && !force_now {
                if let Some(retry) = retries.get(&retry_key) {
                    if retry.retry.next_attempt_at().is_some_and(|retry_at| retry_at > self.clock.now()) {
                        return Ok(None);
                    }
                }
            } else {
                retries.remove(&retry_key);
            }
        }

        let latest = generations.last();
        if !resolved_escalation && !force_now && consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES {
            let latest = latest.expect("a positive failure count requires a generation");
            let failure = format!("ensured convoy entered a terminal failure phase; {consecutive_failures} consecutive generations failed");
            self.raise_ensure_attention(ensure, latest, &failure, Some(self.clock.now() + ENSURE_ESCALATION_AFTER)).await?;
            return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
        }
        // `reconcile-now` is the operator's explicit acknowledgement that any
        // missing backing records were deliberately wiped. Automatic passes
        // still require positive death evidence before admitting a successor.
        if !resolved_escalation && !force_now && consecutive_failures > 0 {
            let latest = latest.expect("a positive failure count requires a generation");
            backing_inspector.verify_backing_dead(latest).await?;
            let retry_at = latest.metadata.creation_timestamp + ensure_retry_delay(consecutive_failures - 1);
            if !force_now && retry_at > self.clock.now() {
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackoffState {
                    strikes: consecutive_failures,
                    retry_at,
                    failure: "ensured convoy entered a terminal failure phase".to_string(),
                })
                .await?;
                return Ok(None);
            }
        }

        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(_) => {
                self.ensure_admission_retries.lock().await.remove(&retry_key);
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                let now = self.clock.now();
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(
                        &mut retries,
                        retry_key,
                        config_hash,
                        dependency_hash,
                        now,
                        ensure.status.as_ref().and_then(|status| status.retry.as_ref()),
                    )
                };
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackoffState {
                    strikes: consecutive_failures,
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("driver admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn set_ensure_driver_condition(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        reason: &str,
        message: String,
    ) -> Result<(), String> {
        let unchanged = ensure.status.as_ref().is_some_and(|status| {
            status.conditions.iter().any(|condition| {
                condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE && condition.reason == reason && condition.message == message
            })
        });
        if unchanged {
            return Ok(());
        }
        self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverAdmission {
            condition: Some(ConvoyEnsureCondition {
                condition_type: DRIVER_ADMISSION_CONDITION_TYPE.to_string(),
                value: ConditionValue::False,
                reason: reason.to_string(),
                message,
                observed_at: self.clock.now(),
            }),
        })
        .await
    }

    async fn clear_ensure_driver_condition(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<(), String> {
        if ensure
            .status
            .as_ref()
            .is_some_and(|status| status.conditions.iter().any(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE))
        {
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverAdmission { condition: None })
                .await?;
        }
        Ok(())
    }

    async fn reconcile_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
        force_now: bool,
    ) -> Result<Option<String>, String> {
        let now = self.clock.now();
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut status = ensure.status.clone().unwrap_or_default();
        let config_hash = ensure_config_hash(&ensure.spec)?;
        if status.observed_config_hash.as_deref() != Some(&config_hash) {
            let changed = status.observed_config_hash.is_some();
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ObserveConfig {
                config_hash: config_hash.clone(),
                changed,
            })
            .await?;
            status.observed_config_hash = Some(config_hash.clone());
            if changed {
                self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
        }
        let convoy = match status.convoy_ref.as_deref() {
            Some(convoy_ref) => match convoys.get(convoy_ref).await {
                Ok(convoy) if convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name) => Some(convoy),
                Ok(_) => return Err(format!("convoy {convoy_ref} exists without this ensure's provenance")),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            },
            None => convoys
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|convoy| {
                    convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name)
                        && convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                })
                .max_by_key(|convoy| convoy.spec.generation),
        };
        let terminal = convoy
            .as_ref()
            .and_then(|convoy| convoy.status.as_ref())
            .is_some_and(|status| matches!(status.phase, ConvoyPhase::Failed | ConvoyPhase::Cancelled | ConvoyPhase::Abandoned));

        if let Some(convoy) = convoy.as_ref().filter(|_| !terminal) {
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            let convoy_ref = convoy.metadata.name.clone();
            if status.convoy_ref.as_deref() != Some(&convoy_ref) || status.retry_at.is_some() || status.last_failure.is_some() {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref,
                    observed_at: now,
                })
                .await?;
                return Ok(Some(format!("ConvoyEnsure/{} observed running", ensure.metadata.name)));
            }
            if status.restart_count > 0
                && status.running_since.is_some_and(|running_since| now - running_since >= ENSURE_BACKOFF_RESET_AFTER)
            {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                return Ok(Some(format!("ConvoyEnsure/{} reset restart backoff", ensure.metadata.name)));
            }
            return Ok(None);
        }

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        let dependency_changed = {
            let mut retries = self.ensure_admission_retries.lock().await;
            let changed =
                retries.get(&retry_key).is_some_and(|retry| retry.config_hash != config_hash || retry.dependency_hash != dependency_hash);
            if changed {
                retries.remove(&retry_key);
            }
            changed
        };

        if convoy.is_none() {
            if !force_now && !dependency_changed && status.retry_at.is_some_and(|retry_at| retry_at > now) {
                return Ok(None);
            }
            if force_now {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
            return self.restart_absent_ensured_convoy(namespace, ensure, &status, now).await;
        }

        let convoy = convoy.expect("terminal branch requires an existing convoy");
        if status.hold_reason == Some(ConvoyEnsureHoldReason::RestartLimit) {
            if !force_now && self.ensure_attention_is_active(namespace, &ensure.metadata.name).await? {
                return Ok(None);
            }
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            if !force_now {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                return Ok(Some(format!("ConvoyEnsure/{} restart hold cleared", ensure.metadata.name)));
            }
            status.restart_count = 0;
            status.retry_at = None;
            status.last_failure = None;
            status.hold_reason = None;
            status.retry = None;
        }
        let operator_forced = convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
        // A forced pass is the recovery boundary for recordless teardown: the
        // operator has acknowledged that raw deletion destroyed the evidence.
        if !operator_forced && !force_now {
            if let Err(reason) = backing_inspector.verify_backing_dead(&convoy).await {
                let failure = format!("standing convoy teardown held: {reason}");
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(&convoy, "BackingEvidenceRefused", failure.clone()), now)
                    .await
                    .map_err(|error| format!("record backing-evidence event: {error}"))?;
                self.raise_ensure_attention(ensure, &convoy, &failure, None).await?;
                if status.retry_at.is_some() || status.last_failure.as_deref() != Some(&failure) {
                    self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Holding {
                        convoy_ref: convoy.metadata.name.clone(),
                        failure,
                    })
                    .await?;
                    return Ok(Some(format!("ConvoyEnsure/{} held for operator attention", ensure.metadata.name)));
                }
                return Ok(None);
            }
        }
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;

        if status.retry_at.is_none() && !force_now {
            let failure = "ensured convoy entered a terminal failure phase";
            if status.restart_count.saturating_add(1) >= ENSURE_MAX_CONSECUTIVE_FAILURES {
                let failure = format!("{failure}; {} consecutive generations failed", ENSURE_MAX_CONSECUTIVE_FAILURES);
                self.raise_ensure_attention(ensure, &convoy, &failure, Some(now + ENSURE_ESCALATION_AFTER)).await?;
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::RestartLimitReached {
                    convoy_ref: convoy.metadata.name.clone(),
                    failure,
                })
                .await?;
                return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
            }
            let retry_at = now + ensure_retry_delay(status.restart_count);
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackingOff {
                retry_at,
                failure: failure.to_string(),
            })
            .await?;
            return Ok(Some(format!("ConvoyEnsure/{} backing off until {retry_at}", ensure.metadata.name)));
        }
        if !force_now && !dependency_changed && status.retry_at.is_some_and(|retry_at| retry_at > now) {
            return Ok(None);
        }
        if force_now {
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
        }

        let restart = async {
            // The terminal generation remains as history. A successful
            // restart admits the next generation under a fresh record name.
            self.start_ensured_convoy(namespace, ensure).await
        }
        .await;
        match restart {
            Ok(convoy_ref) => {
                self.ensure_admission_retries.lock().await.remove(&retry_key);
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref: convoy_ref.clone(),
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(ensure, "EnsureAdmissionRefused", error.clone()), now)
                    .await
                    .map_err(|record_error| format!("record ensure admission event: {record_error}"))?;
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(&mut retries, retry_key, config_hash, dependency_hash, now, status.retry.as_ref())
                };
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn restart_absent_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        status: &flotilla_resources::ConvoyEnsureStatus,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, String> {
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(convoy_ref) => {
                self.ensure_admission_retries.lock().await.remove(&(namespace.to_string(), ensure.metadata.name.clone()));
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref,
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(ensure, "EnsureAdmissionRefused", error.clone()), now)
                    .await
                    .map_err(|record_error| format!("record ensure admission event: {record_error}"))?;
                let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
                let config_hash = ensure_config_hash(&ensure.spec)?;
                let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(&mut retries, retry_key, config_hash, dependency_hash, now, status.retry.as_ref())
                };
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn verify_standing_convoy_resource_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        let environments = self
            .resource_backend
            .using::<ResourceEnvironment>(&convoy.metadata.namespace)
            .list()
            .await
            .map_err(|error| format!("could not inspect backing environments: {error}"))?
            .items
            .into_iter()
            .filter(|environment| environment.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name))
            .collect::<Vec<_>>();
        if environments.is_empty() {
            if convoy.status.as_ref().and_then(|status| status.provisioning) == Some(ConvoyProvisioningState::NotStarted) {
                return Ok(());
            }
            return Err("no backing environment evidence is available".to_string());
        }
        let not_dead = environments
            .iter()
            .filter(|environment| environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Failed))
            .map(|environment| {
                let phase = environment.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending);
                format!("Environment/{} is {phase:?}", environment.metadata.name)
            })
            .collect::<Vec<_>>();
        if not_dead.is_empty() {
            Ok(())
        } else {
            Err(format!("backing is not verified dead: {}", not_dead.join(", ")))
        }
    }

    async fn raise_ensure_attention(
        &self,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &ResourceObject<ResourceConvoy>,
        reason: &str,
        escalation_deadline: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let demands = self.resource_backend.clone().using::<ResourceDemand>(&convoy.metadata.namespace);
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{}", ensure.metadata.name);
        let target = ResourceRef::new(
            api_version(ResourceConvoy::API_PATHS),
            ResourceConvoy::API_PATHS.kind,
            &convoy.metadata.namespace,
            &convoy.metadata.name,
        );
        let meta = InputMeta::builder()
            .name(name)
            .annotations(BTreeMap::from([(RECLAIM_REFUSAL_REASON_ANNOTATION.to_string(), reason.to_string())]))
            .build();
        let mut spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
        spec.expiry = escalation_deadline.map(|deadline| DemandExpiry { deadline, disposition: DemandExpiryDisposition::Escalate });
        match demands.create(&meta, &spec).await {
            Ok(_) => Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                let current = demands.get(&meta.name).await.map_err(|error| error.to_string())?;
                demands.update(&meta, &current.metadata.resource_version, &spec).await.map(|_| ()).map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn ensure_attention_is_active(&self, namespace: &str, ensure_name: &str) -> Result<bool, String> {
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{ensure_name}");
        match self.resource_backend.clone().using::<ResourceDemand>(namespace).get(&name).await {
            Ok(demand) => {
                Ok(demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated)))
            }
            Err(ResourceError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn clear_ensure_attention(&self, namespace: &str, ensure_name: &str) -> Result<(), String> {
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{ensure_name}");
        match self.resource_backend.clone().using::<ResourceDemand>(namespace).delete(&name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn patch_convoy_ensure(&self, namespace: &str, name: &str, patch: ConvoyEnsureStatusPatch) -> Result<(), String> {
        apply_resource_status_patch(&self.resource_backend.clone().using::<ConvoyEnsure>(namespace), name, &patch)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn patch_driver_ensure_status_if_local(&self, namespace: &str, name: &str, patch: ConvoyEnsureStatusPatch) -> Result<(), String> {
        if self.resource_backend.clone().using::<ConvoyEnsure>(namespace).get(name).await.is_ok() {
            self.patch_convoy_ensure(namespace, name, patch).await?;
        }
        Ok(())
    }

    async fn start_ensured_convoy(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let repositories = self.resource_backend.clone().including_replicas::<Repository>(namespace);
        for key in &ensure.spec.repositories {
            match repositories.get(&key.to_string()).await {
                Ok(repository) => {
                    self.resolve_forge_identity_in(namespace, repository.object.spec)
                        .await
                        .map_err(|error| format!("repository {key}: {error}"))?;
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        // The forge sweep may rewrite both Project membership and this ensure.
        let ensure = match self.resource_backend.clone().including_replicas::<ConvoyEnsure>(namespace).get(&ensure.metadata.name).await {
            Ok(updated) => updated.object,
            Err(ResourceError::NotFound { .. }) => ensure.clone(),
            Err(error) => return Err(error.to_string()),
        };
        let intent = flotilla_protocol::ConvoyStartIntent::builder()
            .namespace(namespace.to_string())
            .project_ref(ensure.spec.project_ref.clone())
            .name(ensure.spec.role.clone())
            .branch(ensure.spec.role.clone())
            .workflow_ref(ensure.spec.workflow_ref.clone())
            .maybe_placement_policy(ensure.spec.placement_policy.clone())
            .maybe_escalation_reason(ensure.spec.escalation_reason.clone())
            .agent_overrides(ensure.spec.agent_overrides.clone())
            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
            .build();
        let mut admission = self
            .prepare_convoy_admission_with_preferences(
                namespace,
                &intent,
                &PrincipalRef::implicit_for_namespace(namespace),
                Some(&ensure.spec.repositories),
            )
            .await?;
        self.check_local_free_space_floor().await?;
        if admission.vessel_placements.is_empty() {
            self.check_remote_placement_free_space_floor(namespace, admission.placement_decision.as_ref()).await?;
        }
        for (_, decision) in admission.vessel_placements.values() {
            self.check_remote_placement_free_space_floor(namespace, Some(decision)).await?;
        }
        if admission.workflow.exit.is_some() {
            return Err(format!(
                "workflow template {} declares an exit table; standing convoys require no exit declaration",
                ensure.spec.workflow_ref
            ));
        }
        let commit = ensure
            .metadata
            .annotations
            .get(SOURCE_COMMIT_ANNOTATION)
            .cloned()
            .ok_or_else(|| "materialized ensure has no source commit provenance".to_string())?;
        let provenance = format!("ensured from {} @ {commit}", ensure.metadata.name);
        let mut annotations = BTreeMap::from([
            (ENSURED_FROM_ANNOTATION.to_string(), ensure.metadata.name.clone()),
            (ENSURE_PROVENANCE_ANNOTATION.to_string(), provenance),
        ]);
        for key in [MATERIALIZED_PROJECT_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION] {
            if let Some(value) = ensure.metadata.annotations.get(key) {
                annotations.insert(key.to_string(), value.clone());
            }
        }
        if let Some(presents_as) = &ensure.spec.presents_as {
            annotations.insert(PRESENTS_AS_ANNOTATION.to_string(), presents_as.clone());
        }
        let _admission_guard = self.convoy_admission.lock().await;
        let existing = self
            .resource_backend
            .clone()
            .using::<ResourceConvoy>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .collect::<Vec<_>>();
        if let Some(existing) = existing
            .iter()
            .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name))
            .max_by_key(|convoy| convoy.spec.generation)
        {
            return Ok(existing.metadata.name.clone());
        }
        if existing
            .iter()
            .any(|convoy| convoy.spec.project_ref.as_deref() == Some(&ensure.spec.project_ref) && convoy.spec.role == ensure.spec.role)
        {
            return Err(format!(
                "live convoy {} already exists outside this ensure",
                convoy_address(&ensure.spec.role, Some(&ensure.spec.project_ref))
            ));
        }
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation(&self.resource_backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role)
                .await?;
        let workflow_value = serde_json::to_value(&admission.workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.resource_backend, namespace, &workflow_name, &admission.workflow).await?;
        annotations.insert(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name);
        if self.write_admission_briefs(namespace, &admission.name, &admission.spec, &admission.workflow).await? {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = &admission.placement_policy {
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = prepared_snapshot_name("placement", &placement_value)?;
            ensure_prepared_placement_snapshot(&self.resource_backend, namespace, &placement_name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        annotations.extend(self.prepare_vessel_placement_annotations(namespace, &admission.vessel_placements).await?);
        self.create_convoy_with_annotations(
            namespace,
            &admission.name,
            &admission.spec,
            admission.placement_decision,
            ConvoyDispatchRegard::Suppress,
            annotations,
        )
        .await?;
        Ok(admission.name)
    }

    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = match convoys.get(convoy_name).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        if convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).map(String::as_str) != Some(ensure_name) {
            return Err(format!("refusing to reap standing convoy: it is not owned by ConvoyEnsure/{ensure_name}"));
        }
        self.reap_convoy_internal(namespace, convoy_name, force).await
    }

    async fn reap_convoy_internal(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.verify_convoy_teardown_gate(namespace, name, force).await?;
        self.cascade_convoy_children(namespace, name).await?;
        self.resource_backend.clone().using::<ResourceConvoy>(namespace).delete(name).await.map_err(|error| error.to_string())
    }

    async fn check_local_free_space_floor(&self) -> Result<(), String> {
        let config = Arc::clone(&self.config);
        let available_space_probe = Arc::clone(&self.discovery.available_space_probe);
        let admission_free_space_path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let host_name = self.host_name.to_string();
        tokio::task::spawn_blocking(move || {
            let daemon_config = config.load_daemon_config()?;
            crate::admission::check_free_space_floor(
                &*available_space_probe,
                &host_name,
                &admission_free_space_path,
                daemon_config.admission.free_space_floor_gib,
            )
        })
        .await
        .map_err(|error| format!("free-space check failed on host `{}`: {error}", self.host_name))?
    }

    pub fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        let floor_gib = self.config.load_daemon_config()?.admission.free_space_floor_gib;
        crate::admission::free_space_floor_bytes(floor_gib)
    }

    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String> {
        let Some(placement) = placement else {
            return Ok(());
        };
        let target_host = &placement.target_host;

        let sources =
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        let matching_sources =
            sources.items.into_iter().filter(|source| source.object.metadata.name == target_host.reference.as_str()).collect::<Vec<_>>();
        let has_replica = matching_sources.iter().any(|source| matches!(source.provenance, ResourceProvenance::Replica { .. }));
        let is_host_targeted_placement = self
            .resource_backend
            .clone()
            .including_replicas::<PlacementPolicy>(namespace)
            .get(&placement.policy_name)
            .await
            .is_ok_and(|source| placement_host_ref(&source.object).is_some());
        if !has_replica && !is_host_targeted_placement {
            return Ok(());
        }

        let owns_target_identity = self.canonical_local_host_id().as_ref().is_some_and(|host_id| host_id == &target_host.reference);
        let capacity = if owns_target_identity {
            matching_sources
                .iter()
                .find(|source| matches!(source.provenance, ResourceProvenance::Local))
                .and_then(|source| source.object.status.as_ref())
                .and_then(|status| status.admission_free_space_floor_bytes.map(|floor| (floor, status.disk_free_bytes)))
        } else {
            matching_sources
                .into_iter()
                .filter_map(|source| source.object.status)
                .find_map(|status| status.admission_free_space_floor_bytes.map(|floor| (floor, status.disk_free_bytes)))
        };
        check_placement_capacity(target_host, capacity)
    }

    async fn create_convoy_with_workflow_snapshot(
        &self,
        namespace: &str,
        name: &str,
        bundle: ConvoySnapshotBundle<'_>,
        dispatch_regard: ConvoyDispatchRegard,
    ) -> Result<(), String> {
        let ConvoySnapshotBundle { spec, workflow, placement, placement_decision, vessel_placements } = bundle;
        let workflow_value = serde_json::to_value(workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.resource_backend, namespace, &workflow_name, workflow).await?;
        let mut annotations = BTreeMap::from([(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name)]);
        if self.write_admission_briefs(namespace, name, spec, workflow).await? {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = placement {
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = prepared_snapshot_name("placement", &placement_value)?;
            ensure_prepared_placement_snapshot(&self.resource_backend, namespace, &placement_name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        if let Some(vessel_placements) = vessel_placements {
            annotations.extend(self.prepare_vessel_placement_annotations(namespace, vessel_placements).await?);
        }
        self.create_convoy_with_annotations(namespace, name, spec, placement_decision, dispatch_regard, annotations).await
    }

    async fn write_admission_briefs(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<bool, String> {
        let Some(writer) = self.brief_artifact_writer.read().await.clone() else { return Ok(false) };
        // Admission brief addresses use (convoy, role, "brief", convoy). Role
        // reuse across vessels would replace a different vessel's body.
        let mut agent_roles = BTreeMap::<&str, &str>::new();
        for vessel in &workflow.vessels {
            for process in &vessel.crew {
                if matches!(process.source, CrewSource::Agent { .. }) {
                    if let Some(previous) = agent_roles.insert(&process.role, &vessel.name) {
                        return Err(format!(
                            "agent role `{}` occurs in vessels `{previous}` and `{}`; brief artifact addresses require convoy-wide unique roles",
                            process.role, vessel.name
                        ));
                    }
                }
            }
        }
        let mut annotations = BTreeMap::new();
        if workflow.exit.is_none() {
            annotations.insert(crate::ops_entry::ENSURED_FROM_ANNOTATION.to_string(), "standing".to_string());
        }
        let convoy = ResourceObject::<ResourceConvoy> {
            metadata: ObjectMeta {
                name: name.to_string(),
                namespace: namespace.to_string(),
                resource_version: String::new(),
                labels: BTreeMap::new(),
                annotations,
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec: spec.clone(),
            status: None,
        };
        let templates = crate::agent_adapter::CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path());
        let repositories = self.resource_backend.clone().using::<Repository>(namespace);
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let tracked_roots = self.repository_keys_by_path.read().await.clone();
        for requirement in &workflow.vessels {
            let repository_refs = requirement
                .repository_refs
                .clone()
                .unwrap_or_else(|| spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>());
            let mut fork_stance = false;
            let mut roots = Vec::new();
            for repository_ref in &repository_refs {
                let mut source_roots =
                    tracked_roots.iter().filter(|(_, key)| *key == repository_ref).map(|(path, _)| path.clone()).collect::<Vec<_>>();
                source_roots.sort();
                roots.extend(source_roots);
                if let Ok(repository) = repositories.get(&repository_ref.to_string()).await {
                    fork_stance |= repository.spec.is_fork();
                }
                if let Some(checkout_ref) = spec.adopted_checkout_refs.get(repository_ref) {
                    if let Ok(checkout) = checkouts.get(checkout_ref).await {
                        if let Some(path) = checkout
                            .status
                            .as_ref()
                            .and_then(|status| status.path.clone())
                            .or_else(|| checkout.spec.target_path().map(str::to_string))
                        {
                            roots.push(PathBuf::from(path));
                        }
                    }
                }
            }
            roots.sort();
            roots.dedup();
            let members = requirement
                .crew
                .iter()
                .enumerate()
                .map(|(index, member)| crate::agent_adapter::CrewBriefMember {
                    role: member.role.clone(),
                    state: if requirement.starts_eagerly(index) { "active" } else { "latent" }.to_string(),
                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                })
                .collect::<Vec<_>>();
            for process in &requirement.crew {
                let CrewSource::Agent { prompt, brief_template, .. } = &process.source else { continue };
                let assignment = match prompt.as_deref() {
                    Some(prompt) => crate::agent_adapter::CrewAssignment::Prompt(prompt),
                    None if !spec.issues.is_empty() => crate::agent_adapter::CrewAssignment::CarriedIssue,
                    None if spec.change_request.is_some() => crate::agent_adapter::CrewAssignment::CarriedChangeRequest,
                    None => crate::agent_adapter::CrewAssignment::Unassigned,
                };
                let mut options = templates.render_options_with_fork_stance(
                    brief_template.as_deref(),
                    spec.project_ref.as_deref(),
                    roots.clone(),
                    fork_stance,
                );
                options.has_credential_scope = !requirement.credential_scopes.is_empty();
                let context = TerminalCrewContext {
                    namespace: namespace.to_string(),
                    convoy: name.to_string(),
                    vessel_ref: format!("{name}-{}", requirement.name),
                };
                let mut brief = crate::agent_adapter::build_convoy_crew_brief_with_options(
                    &convoy,
                    &context,
                    &requirement.name,
                    &process.role,
                    assignment,
                    &members,
                    &options,
                )?;
                crate::agent_adapter::append_convoy_work_context(
                    &mut brief.content,
                    &convoy,
                    &repository_refs,
                    &requirement.credential_scopes,
                );
                writer.put_brief(namespace, name, &process.role, name, brief.content.as_bytes()).await?;
            }
        }
        Ok(true)
    }

    async fn prepare_vessel_placement_annotations(
        &self,
        namespace: &str,
        placements: &BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>,
    ) -> Result<BTreeMap<String, String>, String> {
        if placements.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut pins = BTreeMap::new();
        for (vessel, (policy, decision)) in placements {
            let value = serde_json::to_value(policy).map_err(|error| error.to_string())?;
            let name = prepared_snapshot_name("placement", &value)?;
            ensure_prepared_placement_snapshot(&self.resource_backend, namespace, &name, policy).await?;
            pins.insert(vessel.clone(), flotilla_resources::VesselPlacementPin { policy_ref: name, decision: decision.clone() });
        }
        Ok(BTreeMap::from([(
            flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION.to_string(),
            serde_json::to_string(&pins).map_err(|error| error.to_string())?,
        )]))
    }

    async fn create_convoy_with_annotations(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        placement_decision: Option<PlacementDecision>,
        dispatch_regard: ConvoyDispatchRegard,
        annotations: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let labels = BTreeMap::from([
            (PROJECT_LABEL.to_string(), spec.project_ref.clone().unwrap_or_default()),
            (ROLE_LABEL.to_string(), spec.role.clone()),
            (GENERATION_LABEL.to_string(), spec.generation.to_string()),
        ]);
        convoys
            .create(&InputMeta::builder().name(name.to_string()).labels(labels).annotations(annotations).build(), spec)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(placement_decision) = placement_decision {
            apply_resource_status_patch(&convoys, name, &ConvoyStatusPatch::SetPlacementDecision { placement_decision })
                .await
                .map_err(|error| error.to_string())?;
        }
        if dispatch_regard == ConvoyDispatchRegard::Emit {
            if let Err(error) = self.emit_implicit_convoy_regard(namespace, name, &spec.dispatching_principal_ref).await {
                warn!(%error, %namespace, %name, "failed to emit convoy dispatch regard");
            }
        }
        Ok(())
    }

    async fn emit_implicit_convoy_regard(&self, namespace: &str, name: &str, principal_ref: &PrincipalRef) -> Result<(), String> {
        let target = ResourceRef::new(api_version(ResourceConvoy::API_PATHS), ResourceConvoy::API_PATHS.kind, namespace, name);
        self.regard_lifecycle.emit_implicit(principal_ref, &target, "convoy-dispatch").await
    }

    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        let target = binding.resource_ref().ok_or_else(|| "resolved attach target has no resource identity".to_string())?;
        match self.regard_lifecycle.emit_expressed_for_surface(surface_id, &target).await? {
            SurfaceGestureOutcome::Handled => Ok(()),
            SurfaceGestureOutcome::UnknownSurface => {
                self.regard_lifecycle.emit_expressed(&PrincipalRef::implicit_for_namespace(&binding.namespace), &target).await
            }
        }
    }

    async fn resolve_convoy_placement(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        placement_policy: Option<&str>,
        allow_unready: bool,
    ) -> Result<PlacementResolution, String> {
        let mut placement = match placement_policy {
            Some(policy) => {
                let policy = required_admission_value(policy, "placement policy")?;
                let resolved = self
                    .resource_backend
                    .clone()
                    .including_replicas::<PlacementPolicy>(namespace)
                    .get(policy)
                    .await
                    .map(|source| source.object)
                    .map_err(|error| format!("placement policy {policy}: {error}"))?;
                validate_docker_placement_host(&self.resource_backend, namespace, &resolved).await?;
                PlacementResolution {
                    selected: Some(resolved),
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                }
            }
            None => {
                let local_host_id = self.canonical_local_host_id();
                let placement = default_convoy_placement_policy(
                    &self.resource_backend,
                    namespace,
                    project_ref,
                    repositories,
                    workflow,
                    local_host_id.as_ref(),
                )
                .await?;
                if placement.selected.is_none() && !placement.refused_candidates.is_empty() {
                    let reasons = placement
                        .refused_candidates
                        .iter()
                        .map(|candidate| format!("- `{}`: {}", candidate.policy_name, candidate.reason))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(format!("no placement policy satisfies workflow; candidates:\n{reasons}"));
                }
                placement
            }
        };
        if let Some(docker) = placement.selected.as_mut().and_then(|policy| policy.spec.docker_per_vessel.as_mut()) {
            docker.image = docker.image.resolve(&self.resource_backend.definitions(namespace)).await?.into();
        }
        validate_workflow_agent_adapters(&self.resource_backend, namespace, workflow, placement.selected.as_ref(), allow_unready).await?;
        Ok(placement)
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
            Ok((record_name, address)) if auto_attach => match self.wait_for_convoy_attach(&namespace, &record_name, &address).await {
                Ok(resolved) => flotilla_protocol::CommandValue::ConvoyStarted {
                    name: address,
                    attach_plan: Some(resolved.plan),
                    binding: resolved.binding,
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            },
            Ok((_record_name, address)) => {
                flotilla_protocol::CommandValue::ConvoyStarted { name: address, attach_plan: None, binding: None }
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
        self.pending_convoy_starts.lock().await.remove(&key);
        self.finish_context_free_command(command_id, empty_repo_identity(), result);
    }

    async fn wait_for_convoy_attach(&self, namespace: &str, name: &str, address: &str) -> Result<ResolvedAttach, String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let listed = convoys.list().await.map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        if let Some(message) = listed.items.iter().find(|convoy| convoy.metadata.name == name).and_then(convoy_start_failure) {
            return Err(message);
        }
        let mut watch = convoys
            .watch(WatchStart::resuming_from(&listed))
            .await
            .map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut retry = tokio::time::interval(Duration::from_millis(100));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_attach_error = "attach target is not available yet".to_string();

        loop {
            tokio::select! {
                _ = retry.tick() => {
                    match self.resolve_attach_command_internal(name).await {
                        Ok(resolved) => return Ok(resolved),
                        Err(message) => last_attach_error = message,
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
                    return Err(format!("convoy {address} was created but no crew session became attachable: {last_attach_error}"));
                }
            }
        }
    }

    fn project_service(&self) -> project_ops::ProjectService<'_> {
        project_ops::ProjectService {
            resource_backend: &self.resource_backend,
            observed_resource_backend: &self.observed_resource_backend,
            clock: &self.clock,
            namespace: &self.provisioning_namespace,
            _event_sink: self.event_sink.clone(),
            repository_index: project_ops::RepositoryIndex { keys_by_path: &self.repository_keys_by_path },
            operations: self,
        }
    }

    async fn snapshot_project_repositories(
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
        inspection: &crate::repository_inspection::RepositoryInspection,
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
        let previous_tracked_key = self
            .repository_keys_by_path
            .read()
            .await
            .get(&inspection.checkout.path)
            .filter(|previous| *previous != &repository_key)
            .cloned();
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
            crate::observed_resources::delete_observed_checkout_at_path(
                &self.observed_resource_backend,
                &namespace,
                previous,
                &inspection.checkout.path,
            )
            .await
            .map_err(|error| error.to_string())?;
        }

        let other_tracked_keys = self
            .repository_keys_by_path
            .read()
            .await
            .iter()
            .filter(|(path, _)| *path != &inspection.checkout.path)
            .map(|(_, key)| key.clone())
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
                crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, old_key)
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
        let stored = flotilla_resources::ensure_repository(&repositories, repository_key, repository_spec)
            .await
            .map_err(|error| error.to_string())?;
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
        checkout: crate::repository_inspection::LocalCheckoutInspection,
    ) -> Result<(), String> {
        self.project_service().reconcile_project_checkouts(namespace, repository_key, repository_spec, checkout).await
    }

    pub async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryIdentityChange>, String> {
        self.refresh_repository(repo, RepositoryRefreshFailurePolicy::BestEffort).await
    }

    /// Refresh a tracked repository and surface inspection failures to the caller.
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
        let repo = self.resolve_repo_selector(repo).await?;
        let identity = self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
        let identity_change = match self.inspect_repository_path(&repo, None).await {
            Ok(inspection) => {
                let key_changed = self.repository_keys_by_path.read().await.get(&repo) != Some(&inspection.key());
                let identity_change = if key_changed {
                    self.reconcile_tracked_repository(&inspection).await?
                } else {
                    let namespace = self.provisioning_namespace().await;
                    self.reconcile_repository_config(&namespace, &inspection.key(), &inspection.spec).await?;
                    self.reconcile_project_checkouts(&namespace, &inspection.key(), &inspection.spec, inspection.checkout.clone()).await?;
                    None
                };
                if key_changed {
                    {
                        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                        if self.tracked_repo_identity_for_path(&repo).await.as_ref() != Some(&identity) {
                            return Err(format!("repo not tracked: {}", repo.display()));
                        }
                        self.repository_keys_by_path.write().await.insert(repo.clone(), inspection.key());
                    }
                    self.publish_repo_info_update(&identity).await;
                }
                identity_change
            }
            Err(error) => {
                if failure_policy == RepositoryRefreshFailurePolicy::Strict {
                    return Err(format!("inspect repository {} during refresh: {error}", repo.display()));
                }
                warn!(repo = %repo.display(), %error, "repository identity is unavailable during refresh");
                None
            }
        };
        Ok(identity_change)
    }

    /// Refresh host-local bare pane state and publish field-scoped deltas.
    /// Pools are scanned once even when several tracked repositories share the
    /// same host-scoped provider.
    pub async fn refresh_managed_terminal_attention(&self) {
        struct RepoTerminals {
            identity: RepoIdentity,
            roots: Vec<PathBuf>,
            pool_key: usize,
        }

        let (repos, pools) = {
            let tracked = self.repos.read().await;
            let mut repos = Vec::new();
            let mut pools = HashMap::new();
            for state in tracked.values() {
                let registry = state.registry();
                let Some(pool) = registry.terminal_pools.preferred().cloned() else { continue };
                let pool_key = Arc::as_ptr(&pool) as *const () as usize;
                pools.entry(pool_key).or_insert(pool);
                let roots = state.local_paths().into_iter().map(|root| canonical_or_original(&root)).collect();
                repos.push(RepoTerminals { identity: state.identity().clone(), roots, pool_key });
            }
            (repos, pools)
        };

        let store = self.discovery.shared_attachable_store(&self.config);
        for (pool_key, pool) in pools {
            let manager = crate::terminal_manager::TerminalManager::new(pool, store.clone(), self.host_name.clone());
            let terminals = match manager.refresh().await {
                Ok(terminals) => terminals,
                Err(error) => {
                    warn!(%error, "failed to refresh managed terminal attention");
                    continue;
                }
            };
            let pool_repos = repos.iter().filter(|repo| repo.pool_key == pool_key).collect::<Vec<_>>();
            let mut current = pool_repos.iter().map(|repo| (repo.identity.clone(), HashMap::new())).collect::<HashMap<_, HashMap<_, _>>>();
            for terminal in &terminals {
                let working_directory = canonical_or_original(terminal.working_directory.as_path());
                // A nested checkout can share a path prefix with another
                // tracked repository. Attribute the pane to the most-specific
                // root only so one exit cannot surface on multiple checkouts.
                let owner = pool_repos
                    .iter()
                    .flat_map(|repo| repo.roots.iter().map(move |root| (*repo, root)))
                    .filter(|(_, root)| working_directory.starts_with(root))
                    .max_by_key(|(_, root)| root.components().count())
                    .map(|(repo, _)| repo);
                if let Some(repo) = owner {
                    current.get_mut(&repo.identity).expect("pool repository is initialized").insert(
                        terminal.attachable_id.clone(),
                        ManagedTerminal {
                            set_id: terminal.attachable_set_id.clone(),
                            role: terminal.role.clone(),
                            command: terminal.command.clone(),
                            working_directory: terminal.working_directory.as_path().to_path_buf(),
                            status: terminal.status.clone(),
                            attention: terminal.attention.clone(),
                        },
                    );
                }
            }

            let mut previous = self.managed_terminals_by_repo.write().await;
            for repo in pool_repos {
                let next = current.remove(&repo.identity).expect("pool repository is initialized");
                let changes = managed_terminal_changes(previous.get(&repo.identity), &next);
                previous.insert(repo.identity.clone(), next);
                if changes.is_empty() {
                    continue;
                }
                let _ = self.event_tx.send(DaemonEvent::RepoDelta(Box::new(RepoDelta {
                    seq: 0,
                    prev_seq: 0,
                    repo_identity: repo.identity.clone(),
                    repo: repo.roots.first().cloned(),
                    changes,
                })));
            }
        }
    }

    /// Resolve a path that might be a git worktree to the main repo root.
    ///
    /// Returns `(resolved_path, Some(original_path))` if normalization changed
    /// the path, or `(original_path, None)` if no change was needed.
    async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
        use crate::{
            path_context::ExecutionEnvironmentPath,
            providers::vcs::{git::GitVcs, VcsInspection},
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

    async fn publish_repo_info_update(&self, identity: &flotilla_protocol::RepoIdentity) {
        if let Ok(repo_infos) = self.list_repos().await {
            if let Some(info) = repo_infos.into_iter().find(|info| info.identity == *identity) {
                // RepoTracked also carries late identity enrichment: surfaces
                // treat an existing identity as an update.
                let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(info)));
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
            .map_err(|error| format!("cannot track repository {}: {error}", path.display()))?;
        self.config.set_repository_spec(&ExecutionEnvironmentPath::new(&path), repository_inspection.spec.clone());

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.resource_backend,
            &self.provisioning_namespace().await,
            &self.local_environment_id,
            &self.local_environment_id,
            &path,
        )
        .await?;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let identity = repo_identity_from_bag_or_path(&path, &host_repo_bag);
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
                        self.repository_keys_by_path.write().await.insert(path.clone(), repository_key.clone()).as_ref()
                            != Some(repository_key)
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
        let model = RepoModel::new(registry, Some(self.local_environment_id.clone()));
        let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

        let repo_info = RepoInfo {
            identity: identity.clone(),
            repository_key: repository_key.clone(),
            path: Some(path.clone()),
            name: repo_name(&path),
            labels: root.model.labels.clone(),
            provider_names: provider_names_from_registry(&root.model.registry)
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        let mut added_new_identity = false;
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        let already_tracked = self.path_identities.read().await.contains_key(&path);
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
            self.path_identities.write().await.insert(path.clone(), identity.clone());
        }
        if let Some(repository_key) = repository_key {
            self.repository_keys_by_path.write().await.insert(path.clone(), repository_key);
        }

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        info!(repo = %path.display(), "added repo");
        if added_new_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));
        }

        Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change })
    }

    pub async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        let repo_identity = self.tracked_repo_identity_for_path(&path).await.unwrap_or_else(|| fallback_repo_identity(&path));
        let observed_reconciliation = self.observed_checkout_reconciliation.lock().await;
        let tracked = self.repos.read().await.get(&repo_identity).is_some_and(|state| state.contains_path(&path));
        // Persist first so both tracked repositories and observation roots
        // whose initial inspection failed remain removable and retryable.
        self.config.remove_observation_root(&ExecutionEnvironmentPath::new(&path))?;
        if !tracked {
            self.config.remove_repository_spec(&ExecutionEnvironmentPath::new(&path));
            return Ok(());
        }
        let repository_key = match self.repository_keys_by_path.read().await.get(&path).cloned() {
            Some(key) => Some(key),
            None => self.inspect_repository_path(&path, None).await.ok().map(|inspection| inspection.key()),
        };
        let mut removed_identity = false;
        let removed_final_local_root;
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
            if state.roots.is_empty() {
                repos.remove(&repo_identity);
                order.retain(|repo| repo != &repo_identity);
                removed_identity = true;
            } else if previous_preferred == path {
            }
        }

        // Remove from identity maps.
        self.path_identities.write().await.remove(&path);
        self.repository_keys_by_path.write().await.remove(&path);

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

        self.config.remove_repository_spec(&ExecutionEnvironmentPath::new(&path));

        info!(repo = %path.display(), "removed repo");
        if removed_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoUntracked { repo_identity, path: Some(path) });
        }

        Ok(())
    }

    // --- Internal query helpers (formerly DaemonHandle trait methods) ---

    pub async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;

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
            .map(|(category, name)| ProviderInfo { category, name, healthy: true, disabled_reason: None })
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

    fn read_projections(&self) -> read_projections::ReadProjections<'_> {
        read_projections::ReadProjections {
            _event_sink: self.event_sink.clone(),
            backend: &self.resource_backend,
            config: &self.config,
            host_registry: &self.host_registry,
            environment_manager: &self.environment_manager,
            host_name: &self.host_name,
            node_id: &self.node_id,
            clock: &self.clock,
            leaf_subscriptions: &self.leaf_subscriptions,
            fleet: &self.fleet,
        }
    }

    pub async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        let _ = self.refresh_local_host_summary().await;
        self.read_projections().list_hosts(&self.local_host_counts().await).await
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

    pub async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String> {
        let now = Utc::now();
        let namespace = self.provisioning_namespace().await;
        let host_list = self.list_hosts_internal().await?;
        let (local_rows, _) = self.fleet.rows(&namespace).await?;
        self.read_projections().fleet_health(&namespace, host_list, local_rows, self.local_host_id().map(|id| id.to_string()), now).await
    }

    pub async fn list_projects_internal(&self) -> Result<ProjectListResponse, String> {
        read_projections::ReadProjections::list_projects(&self.resource_backend, &self.provisioning_namespace().await).await
    }

    pub async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        self.read_projections()
            .get_host_status(environment_id, &self.local_host_counts().await, &local_summary, &self.provisioning_namespace().await)
            .await
    }

    pub async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        self.read_projections().get_host_providers(environment_id, &self.local_host_counts().await, &local_summary).await
    }

    pub async fn fleet_replica_snapshot_internal(&self) -> Result<FleetReplicaSnapshot, String> {
        let namespace = self.provisioning_namespace().await;
        let (rows, generation) = self.fleet.rows(&namespace).await?;
        let result_sets = self.aggregator_projection_state().await.local_result_sets().await;
        Ok(FleetReplicaSnapshot { host: self.host_name.clone(), generation, rows, result_sets })
    }

    pub async fn fleet_list_internal(&self) -> Result<FleetListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        let (rows, _) = self.fleet.rows(&namespace).await?;
        self.read_projections().fleet_list(rows, Utc::now()).await
    }

    async fn scoped_fleet_list(
        &self,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String> {
        let fleet = self.fleet_list_internal().await?;
        if project.is_none() && crew_id.is_none() && convoy.is_none() {
            return Ok(fleet);
        }
        self.read_projections().scoped_fleet_list(&self.provisioning_namespace().await, fleet, project, crew_id, convoy).await
    }

    /// Resolve enough crew identity locally to route a verb to the convoy
    /// authority. Unlike `resolve_crew_context`, this does not read the
    /// authority-owned Convoy or Vessel.
    pub async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        let provisioning_namespace = self.provisioning_namespace().await;
        let namespace = requested.namespace.clone().unwrap_or_else(|| provisioning_namespace.clone());
        if namespace != provisioning_namespace {
            return Err(format!("crew namespace `{namespace}` is not served by this daemon"));
        }
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace);
        let session_list = sessions.list().await.map_err(|err| err.to_string())?.items;

        if let Some(crew_id) = requested.crew_id.as_deref() {
            let session = session_list
                .iter()
                .find(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
                .ok_or_else(|| format!("unknown FLOTILLA_CREW_ID `{crew_id}`"))?;
            let role = session.spec.role.clone();
            let (convoy, vessel_ref) = match &session.spec.source {
                TerminalSessionSource::Agent { context, .. } => (context.convoy.clone(), context.vessel_ref.clone()),
                TerminalSessionSource::Tool { .. } => {
                    return Err(format!("crew identity `{crew_id}` belongs to a non-agent process"));
                }
            };
            return Ok(CrewRoutingContext {
                command_context: CrewCommandContext {
                    crew_id: None,
                    namespace: Some(namespace),
                    convoy: Some(convoy.clone()),
                    vessel_ref: Some(vessel_ref),
                    role: Some(role),
                },
                session_name: Some(session.metadata.name.clone()),
                convoy,
            });
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
        let caller = session_list.iter().find(|session| {
            session.spec.role == role
                && (session.metadata.labels.get(VESSEL_REF_LABEL).map(String::as_str) == Some(vessel_ref.as_str())
                    || matches!(
                        &session.spec.source,
                        TerminalSessionSource::Agent { context, .. } if context.vessel_ref == vessel_ref && context.convoy == convoy
                    ))
        });
        Ok(CrewRoutingContext {
            command_context: CrewCommandContext {
                crew_id: None,
                namespace: Some(namespace),
                convoy: Some(convoy.clone()),
                vessel_ref: Some(vessel_ref),
                role: Some(role),
            },
            session_name: caller.map(|session| session.metadata.name.clone()),
            convoy,
        })
    }

    async fn resolve_crew_context(&self, requested: &CrewCommandContext) -> Result<ResolvedCrewContext, String> {
        let routing = self.resolve_crew_routing_context(requested).await?;
        self.resolve_crew_context_from_routing(&routing).await
    }

    async fn resolve_crew_context_from_routing(&self, routing: &CrewRoutingContext) -> Result<ResolvedCrewContext, String> {
        let namespace = routing.command_context.namespace.as_ref().expect("routing context always has namespace").clone();
        let convoy = routing.command_context.convoy.as_ref().expect("routing context always has convoy").clone();
        let vessel_ref = routing.command_context.vessel_ref.as_ref().expect("routing context always has vessel ref").clone();
        let role = routing.command_context.role.as_ref().expect("routing context always has role").clone();
        let caller = match routing.session_name.as_ref() {
            Some(name) => self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).get(name).await.ok(),
            None => None,
        };
        self.resolved_crew_context(namespace, convoy, vessel_ref, role, caller).await
    }

    pub async fn mark_crew_completion_pending(
        &self,
        namespace: &str,
        session_name: &str,
        pending: CrewCompletionPending,
    ) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::MarkCompletionPending { pending },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub async fn clear_crew_completion_pending(&self, namespace: &str, session_name: &str) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::ClearCompletionPending,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub async fn pending_crew_completions(&self) -> Result<Vec<(String, CrewCompletionPending, CrewCommandContext)>, String> {
        let namespace = self.provisioning_namespace().await;
        let sessions =
            self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
        Ok(sessions
            .items
            .into_iter()
            .filter_map(|session| {
                let pending = session.status.as_ref()?.completion_pending.clone()?;
                let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return None };
                Some((session.metadata.name, pending, CrewCommandContext {
                    crew_id: None,
                    namespace: Some(context.namespace.clone()),
                    convoy: Some(context.convoy.clone()),
                    vessel_ref: Some(context.vessel_ref.clone()),
                    role: Some(session.spec.role),
                }))
            })
            .collect())
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
        let credential_alerts = self
            .resource_backend
            .clone()
            .using::<ResourceDemand>(&context.namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .filter_map(|demand| credential_refresh_alert_for_vessel(&demand, &context.convoy, &context.vessel))
            .collect::<Vec<_>>();
        let members = task
            .crew
            .iter()
            .map(|process| {
                let session = by_role.get(&process.role);
                let state = match session.and_then(|session| session.status.as_ref().map(|status| status.phase)) {
                    Some(ResourceTerminalSessionPhase::Starting) => "starting",
                    Some(ResourceTerminalSessionPhase::Running) => "active",
                    Some(ResourceTerminalSessionPhase::Lost) => "lost",
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
                    .maybe_attention(crew_attention(session.and_then(|session| session.status.as_ref()), Utc::now()))
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
            .credential_alerts(credential_alerts)
            .build())
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
        if decision_ledger_ref.as_deref().is_some_and(|reference| !(reference.starts_with("https://") || reference.starts_with("http://")))
        {
            return Err("decision ledger reference must use an HTTP(S) URL".to_string());
        }
        let routing = self.resolve_crew_routing_context(requested).await?;
        let namespace = routing.command_context.namespace.as_deref().expect("resolved crew routing context has a namespace");
        let convoy_name = routing.command_context.convoy.as_deref().expect("resolved crew routing context has a convoy");
        let message_lock = self.convoy_message_lock(namespace, convoy_name).await;
        let _message_guard = message_lock.lock().await;
        let context = self.resolve_crew_context_from_routing(&routing).await?;
        let completed_while_crew_active = context.caller_session.as_ref().is_some_and(|session| {
            let Some(status) = session.status.as_ref() else { return false };
            status.phase == ResourceTerminalSessionPhase::Running
                && status.attention.as_ref().is_none_or(|attention| attention.state != TerminalAttentionState::Idle)
        });
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut convoy = convoys.get(convoy_name).await.map_err(|err| err.to_string())?;
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let claim_subjects = message
            .as_deref()
            .into_iter()
            .flat_map(|text| change_request_subjects_from_claim(text, &convoy.spec.repositories, &forges))
            .collect::<BTreeSet<_>>();
        ensure_crew_work_is_defined(&convoy, &context)?;
        // Discovery is evidence, independent of whether this completion claim
        // passes validation. Record it first so a newly named PR can satisfy
        // readiness; the parser admits only this convoy's repositories.
        if !claim_subjects.is_empty() {
            convoy = apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::DiscoverSubjects {
                subjects: claim_subjects.iter().cloned().map(|subject| (subject, flotilla_protocol::Relationship::Produces)).collect(),
                source: flotilla_resources::SubjectDiscoverySource::Claim,
                at: self.clock.now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        let ledger_name = flotilla_resources::artifact_record_name(convoy_name, &context.caller_role, "decision-ledger", convoy_name);
        let projected_ledger_ref =
            match self.resource_backend.including_replicas::<flotilla_resources::Artifact>(namespace).get(&ledger_name).await {
                Ok(record) => record.object.spec.summary.get("comment_url").and_then(serde_json::Value::as_str).map(str::to_string),
                Err(flotilla_resources::ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
        let decision_ledger_ref = projected_ledger_ref.or(decision_ledger_ref);
        // This records the principal declared by the connected surface. Stronger
        // authentication and operator/agent separation belongs to the caller-
        // identity contract; until then the durable attribution is the audit
        // boundary rather than an authorization boundary.
        let forced_by = if force { Some(principal.ok_or_else(|| "`--force` requires an operator principal".to_string())?) } else { None };
        let existing_claim_is_admitted = convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(&context.caller_role))
            .is_some_and(|claim| {
                claim.phase == CrewWorkPhase::Done && (claim.decision_ledger_ref.is_some() || claim.completion_override.is_some())
            });
        if decision_ledger_ref.is_none() && forced_by.is_none() && existing_claim_is_admitted {
            return Ok(flotilla_protocol::CommandValue::Ok);
        }
        if forced_by.is_none() && !existing_claim_is_admitted {
            // Include the candidate claim while deriving PR subjects. Accepted
            // claims persist this message, so later settlement derives the
            // same leaves from the stored crew work.
            let mut claim_convoy = convoy.clone();
            if let Some(work) = claim_convoy
                .status
                .as_mut()
                .and_then(|status| status.crew_work.get_mut(&context.vessel))
                .and_then(|crew| crew.get_mut(&context.caller_role))
            {
                work.phase = CrewWorkPhase::Done;
                work.message = message.clone();
            }
            let checkout_sources =
                self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?;
            let checkouts = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items);
            let requires_change_request = convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
                .and_then(|vessel| vessel.crew.iter().find(|crew| crew.role == context.caller_role))
                .is_some_and(|crew| {
                    crew.completion_conditions.iter().any(|condition| {
                        matches!(
                            condition,
                            flotilla_resources::CrewCompletionExpectation::Condition(
                                flotilla_resources::CompletionCondition::ChangeRequest { .. }
                            ) | flotilla_resources::CrewCompletionExpectation::Legacy(
                                flotilla_resources::LegacyCompletionExpectation::ChangeRequestReady
                            )
                        )
                    })
                });
            let mut observation_errors = Vec::new();
            if requires_change_request {
                let mut subjects = BTreeSet::new();
                for leaf in expected_change_request_leaves(&claim_convoy, &checkouts)? {
                    if let Some(subject) = crate::change_request_observer::ChangeRequestRef::from_address(namespace, &leaf.address) {
                        subjects.insert((subject.service, subject.scope, subject.number));
                    }
                }
                for (service, scope, number) in subjects {
                    let subject =
                        crate::change_request_observer::ChangeRequestRef { namespace: namespace.to_string(), service, scope, number };
                    if let Err(error) = self.leaf_subscriptions.refresh_change_request_once(&subject).await {
                        observation_errors.push(format!("could not observe PR {}: {error}", subject.number));
                    }
                }
            }
            let change_request_sources = self
                .resource_backend
                .including_replicas::<ResourceChangeRequest>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let mut change_requests = BTreeMap::new();
            for source in change_request_sources.items {
                let name = source.object.metadata.name.clone();
                if !change_requests.contains_key(&name) || matches!(source.provenance, ResourceProvenance::Local) {
                    change_requests.insert(name, source.object);
                }
            }
            let artifact_sources = self
                .resource_backend
                .including_replicas::<flotilla_resources::Artifact>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let mut artifacts = BTreeMap::new();
            for source in artifact_sources.items {
                let name = source.object.metadata.name.clone();
                if !artifacts.contains_key(&name) || matches!(source.provenance, ResourceProvenance::Local) {
                    artifacts.insert(name, source.object);
                }
            }
            let unmet = evaluate_crew_completion(
                &claim_convoy,
                CrewCompletionClaim { vessel: &context.vessel, role: &context.caller_role },
                &checkouts,
                &change_requests,
                &artifacts,
                self.change_request_stale_after(),
                self.clock.now(),
            )?;
            if !unmet.is_empty() || !observation_errors.is_empty() {
                let mut reasons = unmet
                    .into_iter()
                    .map(explain_unmet_expectation)
                    .map(|expectation| format!("{}: {}", expectation.subject, expectation.detail))
                    .collect::<Vec<_>>();
                reasons.extend(observation_errors);
                let expectation = reasons.join("; ");
                apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::RefuseCrewCompletion {
                    vessel: context.vessel.clone(),
                    role: context.caller_role.clone(),
                    expectation: expectation.clone(),
                    message: message.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
                return Err(format!("crew completion expectations unmet: {expectation}"));
            }
        }
        if let Some(pending) = convoy
            .status
            .as_ref()
            .and_then(|status| status.pending_brief())
            .filter(|pending| pending.vessel == context.vessel && pending.role == context.caller_role)
        {
            let session_name = routing.session_name.as_deref().ok_or_else(|| {
                format!("pending brief target `{}/{}` has no intact terminal session", context.vessel, context.caller_role)
            })?;
            let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
            let session = sessions.get(session_name).await.map_err(|err| err.to_string())?;
            let framed = format!("{}\n\n{}", flotilla_protocol::commands::CREW_FOLLOW_UP_INSTRUCTION, pending.content);
            let sender = match &pending.sender {
                CrewMessageSender::OperatorResume { principal } => CrewMessageSender::OperatorFollowUp { principal: principal.clone() },
                other => other.clone(),
            };
            queue_pending_crew_message(&sessions, &session, sender, &framed).await?;
            apply_resource_status_patch(
                &convoys,
                convoy_name,
                &convoy_external_patches::deliver_pending_brief(
                    context.vessel,
                    context.caller_role,
                    chrono::Utc::now(),
                    pending.content.clone(),
                    message,
                    disposition,
                    decision_ledger_ref,
                    completed_while_crew_active,
                    forced_by,
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            self.clear_crew_completion_pending(namespace, session_name).await?;
            return Ok(flotilla_protocol::CommandValue::CrewFollowUpDelivered);
        }
        apply_resource_status_patch(
            &convoys,
            convoy_name,
            &convoy_external_patches::mark_crew_completed_with_context(
                context.vessel,
                context.caller_role,
                chrono::Utc::now(),
                message,
                disposition,
                decision_ledger_ref,
                completed_while_crew_active,
                forced_by,
            ),
        )
        .await
        .map_err(|err| err.to_string())?;
        if let Some(session_name) = routing.session_name {
            self.clear_crew_completion_pending(namespace, &session_name).await?;
        }
        Ok(flotilla_protocol::CommandValue::Ok)
    }

    pub async fn crew_fail_internal(&self, requested: &CrewCommandContext, message: String) -> Result<(), String> {
        self.apply_crew_work_patch(requested, |context| {
            convoy_external_patches::mark_crew_failed(context.vessel.clone(), context.caller_role.clone(), chrono::Utc::now(), message)
        })
        .await
    }

    pub async fn crew_stall_internal(
        &self,
        requested: &CrewCommandContext,
        reason: flotilla_protocol::StallReason,
        message: String,
    ) -> Result<(), String> {
        if message.trim().is_empty() {
            return Err("crew stall requires a non-empty message".to_string());
        }
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|error| error.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        let status = convoy.status.as_ref().ok_or_else(|| "convoy has no status".to_string())?;
        if status.phase.is_terminal() {
            return Err("cannot stall crew work in a terminal convoy".to_string());
        }
        if !status
            .crew_work
            .get(&context.vessel)
            .and_then(|crew| crew.get(&context.caller_role))
            .is_some_and(|state| matches!(state.phase, CrewWorkPhase::Working | CrewWorkPhase::Stalled))
        {
            return Err("only working crew can declare a stall".to_string());
        }
        apply_resource_status_patch(
            &convoys,
            &context.convoy,
            &convoy_external_patches::mark_crew_stalled(
                context.convoy.clone(),
                context.vessel.clone(),
                context.caller_role.clone(),
                chrono::Utc::now(),
                reason,
                message,
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    async fn crew_supervise_internal(&self, request: CrewSupervisionRequest<'_>) -> Result<(), String> {
        let CrewSupervisionRequest { namespace, convoy_name, vessel, role, operation: action, message, actor_crew_id, principal } = request;
        if message.trim().is_empty() {
            return Err("crew supervision requires a non-empty message".to_string());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| "convoy has no status".to_string())?;
        let stalled = status.stalled.as_ref().ok_or_else(|| "crew is not stalled".to_string())?;
        if !stalled.leaves.iter().any(|leaf| {
            matches!(&leaf.address,
            flotilla_protocol::LeafAddress::Work { work, .. } if work == vessel)
                && leaf.field_path == format!(".crew.{role}.phase")
        }) {
            return Err(format!("crew `{vessel}/{role}` is not the stalled obligation"));
        }
        if let Some(crew_id) = actor_crew_id {
            let supervisor = stalled.supervisor.as_ref().ok_or_else(|| "this stall has no crew supervisor".to_string())?;
            let sessions = self
                .resource_backend
                .clone()
                .using::<ResourceTerminalSession>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            let authorized = sessions.items.iter().any(|session| {
                session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id)
                    && session.metadata.labels.get(CONVOY_LABEL) == Some(&supervisor.convoy)
                    && session.metadata.labels.get(VESSEL_LABEL) == Some(&supervisor.vessel)
                    && session.metadata.labels.get(ROLE_LABEL) == Some(&supervisor.role)
            });
            if !authorized {
                return Err("crew identity does not own this supervision rung".to_string());
            }
        } else if principal.is_none() {
            return Err("crew supervision requires the named supervisor or an operator principal".to_string());
        }
        match action {
            flotilla_protocol::CrewSupervisionAction::Resume => {
                let sender = if actor_crew_id.is_some() {
                    let supervisor = stalled.supervisor.as_ref().expect("checked supervisor above");
                    if stalled.rung == flotilla_resources::StallRung::Governor {
                        CrewMessageSender::Governor { name: supervisor.convoy.clone() }
                    } else if stalled.rung == flotilla_resources::StallRung::Bosun {
                        CrewMessageSender::Bosun { name: format!("{}@{}", supervisor.role, supervisor.vessel) }
                    } else {
                        return Err("crew supervisor has no governor or Bosun rung".to_string());
                    }
                } else {
                    CrewMessageSender::OperatorResume { principal: principal.cloned() }
                };
                self.convoy_resume_with_sender_internal(namespace, convoy_name, message, Some(vessel), Some(role), sender).await?;
            }
            flotilla_protocol::CrewSupervisionAction::Fail => {
                apply_resource_status_patch(
                    &convoys,
                    convoy_name,
                    &convoy_external_patches::mark_crew_failed(
                        vessel.to_string(),
                        role.to_string(),
                        chrono::Utc::now(),
                        message.to_string(),
                    ),
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            flotilla_protocol::CrewSupervisionAction::Escalate => {
                if stalled.rung == flotilla_resources::StallRung::Operator {
                    return Err("stall is already at the operator rung".to_string());
                }
                let mut condition = stalled.clone();
                condition.supervisor = None;
                condition.maker = Some(flotilla_resources::LeafMaker::Actor { vessel: vessel.to_string(), role: role.to_string() });
                condition.rung = flotilla_resources::StallRung::Operator;
                apply_resource_status_patch(&convoys, convoy_name, &flotilla_resources::ConvoyStatusPatch::SetStalled {
                    condition: Some(condition),
                })
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    async fn runner_for_resource_checkout(&self, _checkout: &ResourceObject<ResourceCheckout>) -> Result<Arc<dyn CommandRunner>, String> {
        self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())
    }

    pub async fn verify_convoy_teardown_gate(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }

        let checkout_sources =
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|err| err.to_string())?;
        let checkout_list = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items).into_values().collect::<Vec<_>>();
        self.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkout_list, false).await
    }

    async fn cascade_convoy_children(&self, namespace: &str, name: &str) -> Result<(), String> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]);
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourcePresentation>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<Vessel>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceTerminalSession>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceCheckout>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        for demand in demands.list().await.map_err(|error| error.to_string())?.items {
            let target = &demand.spec.originating_work_ref;
            if target.api_version == flotilla_resources::api_version(ResourceConvoy::API_PATHS)
                && target.kind == ResourceConvoy::API_PATHS.kind
                && target.namespace == namespace
                && target.name == name
            {
                match demands.delete(&demand.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    pub async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[ResourceObject<ResourceCheckout>],
        force: bool,
    ) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }
        let expected = flotilla_resources::expected_checkout_refs(convoy)?;
        if expected.is_empty() {
            return Ok(());
        }
        // Once the convoy sanctions checkout reclaim (Landed, or the convoy is
        // being deleted), the checkout authority's `OwnerTerminal` cascade may
        // legitimately collect an expected checkout before this gate runs. An
        // absent or already-deleting expected checkout under that sanction is
        // evidence of completed reclaim, not missing evidence — refusing here
        // would wedge vessel reclaim forever on a deletion the substrate itself
        // authorized. Outside the sanction, absence stays a hard refusal.
        let reclaim_sanctioned = flotilla_resources::convoy_sanctions_checkout_reclaim(convoy);
        let missing = expected
            .iter()
            .filter(|checkout_name| !checkout_list.iter().any(|checkout| &checkout.metadata.name == *checkout_name))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() && !reclaim_sanctioned {
            return Err(format!(
                "convoy {namespace}/{name} is not safe to delete: missing checkout integration evidence for {}",
                missing.join(", ")
            ));
        }

        // A completed convoy can outlive its checkout observation. In
        // particular, checkout cleanup may remove the worktree and a new
        // short-lived record may have no status at all. The merged change
        // request is durable integration evidence for that repository.
        let merged_change_requests = if convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed)
            && checkout_list.iter().any(|checkout| expected.contains(&checkout.metadata.name) && checkout.status.is_none())
        {
            let records = self
                .resource_backend
                .including_replicas::<ResourceChangeRequest>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?;
            records
                .items
                .iter()
                .filter(|record| {
                    record.object.status.as_ref().and_then(|status| status.state.value) == Some(ObservedChangeRequestState::Merged)
                })
                .map(|record| record.object.metadata.name.clone())
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };

        let mut refusals = Vec::new();
        for checkout in checkout_list
            .iter()
            .filter(|checkout| expected.contains(&checkout.metadata.name))
            .filter(|checkout| !(reclaim_sanctioned && checkout.metadata.deletion_timestamp.is_some()))
        {
            let is_adopted = checkout.metadata.lifecycle_authority().map_err(|err| err.to_string())? == Some(LifecycleAuthority::Adopted);
            let Some(integration) = checkout.status.as_ref().map(|status| &status.integration) else {
                let merged_for_checkout = associated_change_request_name_without_checkout_status(convoy, checkout)?
                    .is_some_and(|name| merged_change_requests.contains(&name));
                if merged_for_checkout {
                    continue;
                }
                refusals.push(format!("{}: integration evidence is missing", checkout.metadata.name));
                continue;
            };
            let required = if is_adopted {
                vec![("Landed", &integration.landed)]
            } else {
                vec![("Clean", &integration.clean), ("Pushed", &integration.pushed), ("Landed", &integration.landed)]
            };
            let stale = required
                .iter()
                .filter_map(|(label, condition)| (!integration_condition_is_fresh(condition, self.clock.now())).then_some(*label))
                .collect::<Vec<_>>();
            if !stale.is_empty() {
                refusals.push(format!("{}: {} evidence is missing or stale", checkout.metadata.name, stale.join(", ")));
                continue;
            }
            if is_adopted {
                if !condition_is_true(&integration.landed) {
                    refusals.push(
                        checkout_integration_summary(checkout, integration)
                            .unwrap_or_else(|| format!("{}: Landed is not verified", checkout.metadata.name)),
                    );
                }
                continue;
            }
            if !(condition_is_true(&integration.clean) && condition_is_true(&integration.pushed) && condition_is_true(&integration.landed))
            {
                if let Some(summary) = checkout_integration_summary(checkout, integration) {
                    refusals.push(summary);
                }
            }
        }
        if refusals.is_empty() {
            Ok(())
        } else {
            refusals.sort();
            Err(format!("convoy {namespace}/{name} is not safe to delete:\n{}", refusals.join("\n")))
        }
    }

    async fn archive_convoy_checkouts_best_effort(&self, namespace: &str, name: &str) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let checkout_list = checkouts
            .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]))
            .await
            .map_err(|err| err.to_string())?
            .items;
        let mut outcomes = Vec::new();
        for checkout in checkout_list {
            let Some(path) = checkout_path(&checkout) else {
                continue;
            };
            if checkout.status.as_ref().is_some_and(|status| {
                condition_is_true(&status.integration.pushed)
                    && integration_condition_is_fresh(&status.integration.pushed, self.clock.now())
            }) {
                outcomes.push(
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::NothingToArchive)
                        .build(),
                );
                continue;
            }
            let vcs = match self.local_vcs_for_checkout(Path::new(path)).await {
                Ok(vcs) => vcs,
                Err(error) => {
                    warn!(checkout = %checkout.metadata.name, %error, "best-effort abandon archive push failed");
                    outcomes.push(
                        CheckoutArchiveOutcome::builder()
                            .checkout(checkout.metadata.name)
                            .status(CheckoutArchiveStatus::Failed)
                            .detail(error)
                            .build(),
                    );
                    continue;
                }
            };
            let output = vcs.push_current_branch("origin").await;
            let outcome = match output {
                Ok(output) if output.success => {
                    CheckoutArchiveOutcome::builder().checkout(checkout.metadata.name).status(CheckoutArchiveStatus::Archived).build()
                }
                Ok(output) => {
                    let detail = output.stderr.trim().to_string();
                    warn!(checkout = %checkout.metadata.name, stderr = %detail, "best-effort abandon archive push failed");
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::Failed)
                        .detail(detail)
                        .build()
                }
                Err(error) => {
                    warn!(checkout = %checkout.metadata.name, %error, "best-effort abandon archive push failed");
                    CheckoutArchiveOutcome::builder()
                        .checkout(checkout.metadata.name)
                        .status(CheckoutArchiveStatus::Failed)
                        .detail(error)
                        .build()
                }
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    async fn abandon_convoy_internal(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        if reason.trim().is_empty() {
            return Err("convoy abandon requires a non-empty reason".to_string());
        }
        let archives = self.archive_convoy_checkouts_best_effort(namespace, name).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let authority = match principal_ref {
            Some(principal) if principal.name == PrincipalRef::IMPLICIT_NAME => WorkCompletionAuthority::HumanOverride,
            Some(principal) => WorkCompletionAuthority::Principal(principal.clone()),
            None => WorkCompletionAuthority::Unattributed,
        };
        apply_resource_status_patch(
            &convoys,
            name,
            &convoy_external_patches::mark_convoy_abandoned(Utc::now(), authority, reason.to_string()),
        )
        .await
        .map_err(|err| err.to_string())?;
        // Abandonment is an explicit terminal override: the phase stamp above is
        // the teardown gate, after the best-effort archive push has run. The
        // lifecycle reconciler reclaims children while retaining the convoy.
        Ok(archives)
    }

    async fn record_lifecycle_mutation(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
    ) -> Result<(), String> {
        let Some(caller) = caller else { return Ok(()) };
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        apply_resource_status_patch(&convoys, name, &ConvoyStatusPatch::RecordLifecycleMutation {
            mutation: flotilla_resources::LifecycleMutation { action: action.to_string(), caller: caller.clone(), at: self.clock.now() },
        })
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    async fn record_lifecycle_mutation_best_effort(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
        missing_expected: bool,
    ) {
        if let Err(error) = self.record_lifecycle_mutation(namespace, name, action, caller).await {
            if !missing_expected || !error.contains("not found") {
                warn!(%error, %namespace, convoy = %name, %action, "failed to persist lifecycle mutation attribution");
            }
        }
    }

    async fn apply_crew_work_patch(
        &self,
        requested: &CrewCommandContext,
        patch: impl FnOnce(&ResolvedCrewContext) -> ConvoyStatusPatch,
    ) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
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
            .find(|(_, process)| process.role == target && target != context.caller_role)
            .ok_or_else(|| crew_handoff_address_error(target, &context.vessel))?;
        let CrewSource::Agent { selector, prompt, brief_template } = &process.source else {
            return Err(format!("crew target `{target}` is a tool process and cannot receive a handoff"));
        };
        let repository_refs = task
            .repository_refs
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        if convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(target))
            .is_some_and(|state| state.phase == flotilla_resources::CrewWorkPhase::Failed)
        {
            return Err(format!("crew target `{target}` has failed work and cannot receive a handoff"));
        }

        let sender = CrewMessageSender::Handoff { from: format!("{}@{}", context.caller_role, context.vessel) };
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
        let target_session = match sessions.get(&terminal_name).await {
            Ok(session) => Some(session),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        if target_session
            .as_ref()
            .and_then(|session| session.status.as_ref())
            .is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed)
        {
            return Err(format!("crew target `{target}` failed provisioning and cannot be revived"));
        }
        let anchor = if target_session.is_none() {
            Some(if let Some(caller) = context.caller_session.as_ref() {
                caller.clone()
            } else {
                sessions
                    .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                    .into_iter()
                    .next()
                    .ok_or_else(|| format!("vessel `{}` has no active session to anchor the handoff", context.vessel_ref))?
            })
        } else {
            None
        };
        let environment_ref = target_session.as_ref().or(anchor.as_ref()).expect("target or anchor session").spec.env_ref.clone();
        let previous_status = convoy.status.clone().ok_or_else(|| format!("convoy `{}` has no status", context.convoy))?;
        let reopened = apply_resource_status_patch(
            &convoys,
            &context.convoy,
            &convoy_external_patches::handoff_crew_work(
                context.vessel.clone(),
                context.caller_role.clone(),
                target.to_string(),
                chrono::Utc::now(),
                message.to_string(),
            ),
        )
        .await
        .map_err(|err| err.to_string())?;
        self.reconcile_or_restore_crew_work(
            &context.namespace,
            &environment_ref,
            &convoys,
            &context.convoy,
            previous_status.clone(),
            &reopened,
        )
        .await?;
        let handoff_result: Result<(), String> = async {
            match sessions.get(&terminal_name).await {
                Ok(existing) if existing.spec.env_ref != environment_ref => {
                    Err(format!("crew target `{target}` moved to another environment during credential staging"))
                }
                Ok(existing) => match existing.status.as_ref().map(|status| status.phase) {
                    Some(ResourceTerminalSessionPhase::Running) => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await
                    }
                    Some(ResourceTerminalSessionPhase::Stopped | ResourceTerminalSessionPhase::Lost) => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await?;
                        apply_resource_status_patch(&sessions, &terminal_name, &TerminalSessionStatusPatch::MarkStarting)
                            .await
                            .map(|_| ())
                            .map_err(|err| err.to_string())
                    }
                    Some(ResourceTerminalSessionPhase::Failed) => {
                        Err(format!("crew target `{target}` failed provisioning and cannot be revived"))
                    }
                    Some(ResourceTerminalSessionPhase::Starting) | None => {
                        queue_pending_crew_message(&sessions, &existing, sender.clone(), message).await
                    }
                },
                Err(ResourceError::NotFound { .. }) => {
                    let anchor = anchor.ok_or_else(|| format!("crew target `{target}` disappeared during credential staging"))?;
                    let current = self.crew_list_internal(requested).await?;
                    let repo_roots = crew_brief_repo_roots(&self.resource_backend, &context.namespace, &convoy, &repository_refs).await;
                    let repositories = self.resource_backend.clone().using::<Repository>(&context.namespace);
                    let mut fork_stance = false;
                    for repository_ref in &repository_refs {
                        if let Ok(repository) = repositories.get(&repository_ref.to_string()).await {
                            fork_stance |= repository.spec.is_fork();
                        }
                    }
                    let mut render_options =
                        crate::agent_adapter::CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path())
                            .render_options_with_fork_stance(
                                brief_template.as_deref(),
                                convoy.spec.project_ref.as_deref(),
                                repo_roots,
                                fork_stance,
                            );
                    render_options.has_credential_scope = !task.credential_scopes.is_empty();
                    let mut brief =
                        handoff_crew_brief(&context, &convoy, target, prompt.as_deref(), &current.members, task, &render_options)?;
                    if let Some(writer) = self.brief_artifact_writer.read().await.clone() {
                        let subject = format!("{}/handoff/{}", context.convoy, uuid::Uuid::new_v4().simple());
                        brief.artifact_digest =
                            Some(writer.put_brief(&context.namespace, &context.convoy, target, &subject, brief.content.as_bytes()).await?);
                        brief.content.clear();
                    }
                    let terminal_meta = terminal_meta_with_vessel_credentials(identity.input_meta(), task);
                    sessions
                        .create(&terminal_meta, &flotilla_resources::TerminalSessionSpec {
                            env_ref: anchor.spec.env_ref,
                            role: target.to_string(),
                            source: TerminalSessionSource::Agent {
                                selector: selector.clone(),
                                brief,
                                context: Box::new(TerminalCrewContext {
                                    namespace: context.namespace.clone(),
                                    convoy: context.convoy.clone(),
                                    vessel_ref: context.vessel_ref.clone(),
                                }),
                                message: Some(pending_crew_message(sender.clone(), message)),
                            },
                            cwd: anchor.spec.cwd,
                            pool: anchor.spec.pool,
                        })
                        .await
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                }
                Err(err) => Err(err.to_string()),
            }
        }
        .await;
        if let Err(error) = handoff_result {
            return match convoys.update_status(&context.convoy, &reopened.metadata.resource_version, &previous_status).await {
                Ok(_) => Err(error),
                Err(restore_error) => Err(format!("{error}; could not restore crew work after handoff failure: {restore_error}")),
            };
        }
        Ok(())
    }

    pub async fn convoy_resume_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
    ) -> Result<ConvoyResumeOutcome, String> {
        self.convoy_resume_with_sender_internal(
            namespace,
            name,
            prompt,
            requested_vessel,
            requested_role,
            CrewMessageSender::OperatorResume { principal: None },
        )
        .await
    }

    async fn convoy_resume_with_sender_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
        sender: CrewMessageSender,
    ) -> Result<ConvoyResumeOutcome, String> {
        if prompt.trim().is_empty() {
            return Err("convoy resume requires a non-empty prompt".to_string());
        }
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept a brief", status.phase));
        }
        let candidates = status
            .crew_work
            .iter()
            .flat_map(|(vessel, crew)| crew.iter().map(move |(role, state)| (vessel, role, state)))
            .filter(|(vessel, role, state)| {
                matches!(
                    state.phase,
                    flotilla_resources::CrewWorkPhase::Working
                        | flotilla_resources::CrewWorkPhase::Stalled
                        | flotilla_resources::CrewWorkPhase::Done
                ) && requested_vessel.is_none_or(|requested| requested == vessel.as_str())
                    && requested_role.is_none_or(|requested| requested == role.as_str())
            })
            .map(|(vessel, role, _)| (vessel.clone(), role.clone()))
            .collect::<Vec<_>>();
        let (vessel, role) = match candidates.as_slice() {
            [] => {
                let scope = match (requested_vessel, requested_role) {
                    (Some(vessel), Some(role)) => format!(" for vessel `{vessel}` role `{role}`"),
                    (Some(vessel), None) => format!(" for vessel `{vessel}`"),
                    (None, Some(role)) => format!(" for role `{role}`"),
                    (None, None) => String::new(),
                };
                return Err(format!("convoy `{name}` has no active or completed crew work{scope}"));
            }
            [candidate] => candidate.clone(),
            _ => {
                let matches = candidates.iter().map(|(vessel, role)| format!("{vessel}/{role}")).collect::<Vec<_>>().join(", ");
                return Err(format!(
                    "convoy `{name}` has multiple active or completed crew members ({matches}); select one with --vessel and --role"
                ));
            }
        };

        let crew_phase = status
            .crew_work
            .get(&vessel)
            .and_then(|crew| crew.get(&role))
            .map(|state| state.phase)
            .expect("selected candidate has crew work");
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let session = sessions
            .list_matching_labels(&BTreeMap::from([
                (CONVOY_LABEL.to_string(), name.to_string()),
                (VESSEL_LABEL.to_string(), vessel.clone()),
                (ROLE_LABEL.to_string(), role.clone()),
            ]))
            .await
            .map_err(|err| err.to_string())
            .map(|list| list.items.into_iter().next());
        let at_turn_boundary = session.as_ref().ok().and_then(Option::as_ref).is_some_and(|session| {
            session.status.as_ref().is_some_and(|status| {
                status.phase == ResourceTerminalSessionPhase::Running
                    && status.attention.as_ref().is_some_and(|attention| attention.state == TerminalAttentionState::Idle)
            })
        });
        if crew_phase == flotilla_resources::CrewWorkPhase::Working && !at_turn_boundary {
            let displaced = status.pending_brief().map(|brief| brief.content.clone());
            apply_resource_status_patch(
                &convoys,
                name,
                &convoy_external_patches::set_pending_brief(
                    PendingBrief::builder()
                        .vessel(vessel)
                        .role(role)
                        .content(prompt.to_string())
                        .queued_at(chrono::Utc::now())
                        .sender(sender)
                        .build(),
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            return Ok(ConvoyResumeOutcome::Queued { displaced });
        }

        let displaced =
            status.pending_brief().filter(|brief| brief.vessel == vessel && brief.role == role).map(|brief| brief.content.clone());
        let session = session?.ok_or_else(|| format!("crew member `{role}` on vessel `{vessel}` has no intact terminal session"))?;
        if session.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Failed) {
            return Err(format!("crew member `{role}` on vessel `{vessel}` failed provisioning and cannot be resumed"));
        }
        let reopened = apply_resource_status_patch(
            &convoys,
            name,
            &convoy_external_patches::resume_crew_work(vessel.clone(), role.clone(), chrono::Utc::now(), prompt.to_string()),
        )
        .await
        .map_err(|err| err.to_string())?;
        self.reconcile_or_restore_crew_work(namespace, &session.spec.env_ref, &convoys, name, status.clone(), &reopened).await?;
        let delivery_result: Result<(), String> = async {
            match session.status.as_ref().map(|status| status.phase) {
                Some(ResourceTerminalSessionPhase::Running) => {
                    queue_pending_crew_message(&sessions, &session, sender.clone(), prompt).await?
                }
                Some(ResourceTerminalSessionPhase::Stopped) => {
                    queue_pending_crew_message(&sessions, &session, sender.clone(), prompt).await?;
                    apply_resource_status_patch(&sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting)
                        .await
                        .map_err(|err| err.to_string())?;
                }
                _ => queue_pending_crew_message(&sessions, &session, sender.clone(), prompt).await?,
            }
            Ok(())
        }
        .await;
        if let Err(error) = delivery_result {
            return Err(self
                .restore_crew_work_after_delivery_failure(&convoys, name, &reopened.metadata.resource_version, status, error)
                .await);
        }
        Ok(ConvoyResumeOutcome::Delivered { displaced })
    }

    pub async fn convoy_withdraw_pending_brief_internal(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept message changes", status.phase));
        }
        let withdrawn = status.pending_brief().map(|brief| brief.content.clone());
        if withdrawn.is_some() {
            apply_resource_status_patch(&convoys, name, &convoy_external_patches::clear_pending_brief())
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(withdrawn)
    }

    async fn convoy_message_lock(&self, namespace: &str, name: &str) -> ConvoyMessageLock {
        let key = (namespace.to_string(), name.to_string());
        let mut locks = self.convoy_message_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    }

    async fn deliver_standing_turn(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&request.namespace);
        let session = sessions
            .list_matching_labels(&BTreeMap::from([
                (CONVOY_LABEL.to_string(), request.convoy.clone()),
                (VESSEL_LABEL.to_string(), request.vessel.clone()),
                (ROLE_LABEL.to_string(), request.role.clone()),
            ]))
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .next()
            .ok_or_else(|| format!("turn-delivery target {}/{} has no durable terminal-session record", request.vessel, request.role))?;
        let mut spec = session.spec.clone();
        let TerminalSessionSource::Agent { brief, message, .. } = &mut spec.source else {
            return Err(format!("turn-delivery target {}/{} is not an agent", request.vessel, request.role));
        };
        let mut delivery_message = TerminalCrewMessage {
            id: format!("turn-delivery:{}:{}", request.source, request.subject_revision),
            text: frame_crew_message(&request.sender, &request.brief),
            sender: request.sender.clone(),
            delivery: CrewMessageDelivery::Queued,
        };
        let plan = turn_delivery_session_plan(session.status.as_ref().map(|status| status.phase), &request.vessel, &request.role)?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&request.namespace);
        let previous_status = convoys
            .get(&request.convoy)
            .await
            .map_err(|error| error.to_string())?
            .status
            .ok_or_else(|| format!("convoy `{}` has no status", request.convoy))?;
        let reopened = apply_resource_status_patch(
            &convoys,
            &request.convoy,
            &convoy_external_patches::resume_crew_work(
                request.vessel.clone(),
                request.role.clone(),
                chrono::Utc::now(),
                request.brief.clone(),
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
        self.reconcile_or_restore_crew_work(
            &request.namespace,
            &session.spec.env_ref,
            &convoys,
            &request.convoy,
            previous_status.clone(),
            &reopened,
        )
        .await?;
        match plan {
            TurnDeliverySessionPlan::QueueWarm | TurnDeliverySessionPlan::QueueFresh => {
                *message = Some(delivery_message);
            }
            TurnDeliverySessionPlan::RestartFresh => {
                delivery_message.delivery = CrewMessageDelivery::LaunchBrief;
                brief.content = delivery_message.text.clone();
                brief.artifact_digest = None;
                *message = Some(delivery_message);
            }
        }
        if let Err(error) = sessions.update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec).await {
            return Err(self
                .restore_crew_work_after_delivery_failure(
                    &convoys,
                    &request.convoy,
                    &reopened.metadata.resource_version,
                    &previous_status,
                    error.to_string(),
                )
                .await);
        }
        match plan {
            TurnDeliverySessionPlan::QueueWarm => Ok(TurnDeliveryRung::WarmSession),
            TurnDeliverySessionPlan::RestartFresh => {
                if let Err(error) =
                    apply_resource_status_patch(&sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting).await
                {
                    return Err(self
                        .restore_crew_work_after_delivery_failure(
                            &convoys,
                            &request.convoy,
                            &reopened.metadata.resource_version,
                            &previous_status,
                            error.to_string(),
                        )
                        .await);
                }
                Ok(TurnDeliveryRung::FreshAgent)
            }
            TurnDeliverySessionPlan::QueueFresh => Ok(TurnDeliveryRung::FreshAgent),
        }
    }

    async fn execute_turn_delivery_hold(
        &self,
        request: &crate::leaf_engine::TurnDeliveryRequest,
        act: &HoldAct,
        reason: &str,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&request.namespace);
        let convoy = convoys.get(&request.convoy).await.map_err(|error| error.to_string())?;
        let bound = convoy.spec.change_request.as_ref().ok_or_else(|| "turn-delivery convoy has no bound change request".to_string())?;
        let repository = convoy
            .spec
            .repositories
            .iter()
            .find(|repository| repository.repo_ref == bound.repository_ref)
            .ok_or_else(|| format!("bound repository {} is absent", bound.repository_ref))?;
        let canonical = flotilla_resources::canonicalize_repo_url(&repository.url)?;
        let repository_name = canonical
            .split_once("://")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once('/').map(|(_, scope)| scope))
            .ok_or_else(|| format!("cannot derive repository scope from {}", repository.url))?;
        let HoldAct::ChangeRequestComment { body } = act;
        let comment = format!("{}\n\n{}", body.trim(), reason);
        let runner = self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
        runner
            .run("gh", &["pr", "comment", &bound.id, "-R", repository_name, "--body", &comment], Path::new("/"), &ChannelLabel::Default)
            .await
            .map(|_| ())
    }

    pub async fn refresh_fleet_replicas_once(&self) -> Result<(), String> {
        let namespace = self.provisioning_namespace().await;
        self.fleet.refresh_once(&namespace, self.local_command_runner()).await
    }

    fn attach_resolver(&self) -> AttachResolver<'_> {
        AttachResolver {
            _event_sink: self.event_sink.clone(),
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
            fleet_rows: Box::new(CachedFleetRows { fleet: &self.fleet }),
            repository_keys_by_path: &self.repository_keys_by_path,
            path_identities: &self.path_identities,
            repos: &self.repos,
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

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.local_host_id().map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    async fn refresh_local_host_summary(&self) -> HostSummary {
        let mut providers = crate::host_summary::provider_statuses_from_registries(
            self.repos.read().await.values().map(|state| state.preferred_root().model.registry.as_ref()),
        );
        for advertised in self.local_placement_provider_statuses.read().await.iter() {
            if !providers
                .iter()
                .any(|provider| provider.category == advertised.category && provider.implementation == advertised.implementation)
            {
                providers.push(advertised.clone());
            }
        }
        providers.sort_by(|left, right| (&left.category, &left.name).cmp(&(&right.category, &right.name)));
        let summary = crate::host_summary::build_local_host_summary(
            &self.node_id,
            &self.host_name,
            EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            &self.environment_manager,
            providers,
            &*self.discovery.env,
        )
        .await;
        self.host_registry.set_local_host_summary(summary.clone()).await;
        summary
    }

    async fn get_issue_provider_for_repo(&self, repo: &Path) -> Result<(Arc<dyn IssueProvider>, flotilla_protocol::IssueSource), String> {
        let identity = self.tracked_repo_identity_for_path(repo).await.ok_or_else(|| "no tracked repo for path".to_string())?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| "repo not found".to_string())?;
        let source = forge_issue_source(state.identity());
        let provider = state
            .registry()
            .issue_provider_for(&source)
            .ok_or_else(|| format!("no issue provider available for {} {}", source.service, source.scope))?;
        Ok((provider, source))
    }

    pub async fn execute_with_remote_executor(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
    ) -> Result<u64, String> {
        self.execute_impl(command, remote_executor, true, None).await
    }

    pub async fn execute_for_principal(&self, command: Command, principal_ref: Option<PrincipalRef>) -> Result<u64, String> {
        let caller = principal_ref.map(|principal_ref| flotilla_protocol::CommandCaller { principal_ref, process: None, crew: None });
        self.execute_for_caller(command, caller).await
    }

    pub async fn execute_for_caller(&self, command: Command, caller: Option<flotilla_protocol::CommandCaller>) -> Result<u64, String> {
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false, caller).await
    }

    async fn executor_provider_data(&self, repo_identity: &RepoIdentity, _repo_root: &Path, registry: &ProviderRegistry) -> ProviderData {
        let mut providers = ProviderData::default();

        if let Some(vcs) = registry.vcs.preferred() {
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
        let local_repo_path = self
            .preferred_local_path_for_identity(&request.repo_identity)
            .await
            .ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
        let registry = {
            let repos = self.repos.read().await;
            let state = repos.get(&request.repo_identity).ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
            state.registry()
        };
        let providers_data = Arc::new(self.executor_provider_data(&request.repo_identity, &local_repo_path, &registry).await);

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
            vcs_resolver: self.self_weak.upgrade().ok_or("VCS resolver daemon unavailable")? as Arc<dyn crate::vcs::CheckoutVcsResolver>,
        };

        let result = execute_local_remote_step_batch(self.node_id.clone(), request, progress_sink, cancel, &resolver).await;
        result
    }

    async fn execute_impl(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
        allow_remote_host: bool,
        caller: Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = caller.as_ref().map(|caller| caller.principal_ref.clone());
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

        if let flotilla_protocol::CommandAction::ResourceApply { namespace, document } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match apply_resource_document(&self.resource_backend, namespace, document.clone()).await {
                Ok(applied) => flotilla_protocol::CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                    kind: applied.kind,
                    plural: applied.plural,
                    namespace: applied.namespace,
                    value: applied.value,
                    replica_origin: None,
                })),
                Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::RepositoryRemoteRemove { namespace, name, remote } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let repositories = self.resource_backend.clone().using::<Repository>(namespace);
            let result = match repositories.get(name).await {
                Ok(repository) => match repository.spec.clone().remove_remote(remote) {
                    Ok(spec) => match repositories
                        .update(&InputMeta::from(&repository.metadata), &repository.metadata.resource_version, &spec)
                        .await
                    {
                        Ok(_) => flotilla_protocol::CommandValue::Ok,
                        Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                    },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceManifestResolve { namespace, kind, name, resolution, requested_by } =
            &command.action
        {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = request_manifest_resolution(&self.resource_backend, namespace, kind, name, *resolution, requested_by).await;
            self.finish_context_free_command(id, empty_identity, match result {
                Ok(root) => CommandValue::ResourceObject(Box::new(root)),
                Err(error) => CommandValue::Error { message: error },
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceReconcileNow { namespace, kind, name } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.operator_reconciler.read().await.clone() {
                Some(reconciler) => match reconciler.reconcile_now(namespace, kind, name).await {
                    Ok(message) => CommandValue::ResourceReconciled { resource_kind: kind.clone(), name: name.clone(), message },
                    Err(message) => CommandValue::Error { message },
                },
                None => CommandValue::Error { message: "operator reconciliation is unavailable before runtime startup".to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceStatusPatch { namespace, kind, name, status } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result =
                match flotilla_resources::patch_resource_status(&self.resource_backend, namespace, kind, name, status.clone()).await {
                    Ok(patched) => flotilla_protocol::CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                        kind: patched.kind,
                        plural: patched.plural,
                        namespace: patched.namespace,
                        value: patched.value,
                        replica_origin: None,
                    })),
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceDelete { namespace, kind, name, replica_origin } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = if let Some(origin_root) = replica_origin {
                let deleted = if self.peer_connection_status(origin_root).await == PeerConnectionState::Connected {
                    Err(ResourceError::invalid(format!(
                        "replica origin {origin_root} is connected; delete the authoritative resource instead"
                    )))
                } else {
                    flotilla_resources::collect_resource_replica_kind(&self.resource_backend, namespace, kind, name, origin_root).await
                };
                match deleted {
                    Ok(deleted) => flotilla_protocol::CommandValue::ResourceDeleted(Box::new(ResourceJsonResponse {
                        kind: deleted.kind,
                        plural: deleted.plural,
                        namespace: deleted.namespace,
                        value: deleted.value,
                        replica_origin: replica_origin.clone(),
                    })),
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                }
            } else {
                match flotilla_resources::delete_resource_kind(&self.resource_backend, namespace, kind, name).await {
                    Ok(deleted) => {
                        let response = Box::new(ResourceJsonResponse {
                            kind: deleted.object.kind,
                            plural: deleted.object.plural,
                            namespace: deleted.object.namespace,
                            value: deleted.object.value,
                            replica_origin: None,
                        });
                        if deleted.already_deleted {
                            flotilla_protocol::CommandValue::ResourceAlreadyDeleted(response)
                        } else {
                            flotilla_protocol::CommandValue::ResourceDeleted(response)
                        }
                    }
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                }
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceWatch { namespace, kind, name, include_replicas, replica_sources, cursor } =
            command.action
        {
            let repo_identity = empty_repo_identity();
            let description = format!("watch resource {namespace}/{kind}");
            let token = CancellationToken::new();
            {
                let mut guard = self.active_commands.lock().await;
                guard.insert(id, token.clone());
            }
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: command_node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });

            let backend = self.resource_backend.clone();
            let event_tx = self.event_tx.clone();
            let event_sink = self.event_sink.clone();
            let active_ref = Arc::clone(&self.active_commands);
            tokio::spawn(async move {
                let result = run_resource_watch_command(
                    ResourceWatchCommandContext::builder()
                        .backend(backend)
                        .namespace(namespace)
                        .kind(kind)
                        .maybe_name(name)
                        .include_replicas(include_replicas)
                        .replica_sources(replica_sources)
                        .maybe_cursor(cursor)
                        .command_id(id)
                        .node_id(command_node_id.clone())
                        .repo_identity(repo_identity.clone())
                        .event_sink(event_sink)
                        .token(token)
                        .build(),
                )
                .await;
                active_ref.lock().await.remove(&id);
                let _ = event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: command_node_id,
                    repo_identity,
                    repo: None,
                    result,
                });
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
            let mut identity_changes = Vec::new();
            let result = match async {
                for repo in &repo_paths {
                    if let Some(change) = self.refresh(&flotilla_protocol::RepoSelector::Path(repo.clone())).await? {
                        identity_changes.push(change);
                    }
                    refreshed.push(repo.clone());
                }
                Ok::<(), String>(())
            }
            .await
            {
                Ok(()) => flotilla_protocol::CommandValue::Refreshed { repos: refreshed, identity_changes },
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

        if let flotilla_protocol::CommandAction::ConvoyResume { namespace, name, prompt, vessel, role } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match self
                        .convoy_resume_with_sender_internal(
                            &namespace,
                            &record_name,
                            prompt,
                            vessel.as_deref(),
                            role.as_deref(),
                            CrewMessageSender::OperatorResume { principal: dispatching_principal_ref.clone() },
                        )
                        .await
                    {
                        Ok(ConvoyResumeOutcome::Delivered { displaced }) => {
                            self.record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyBriefDelivered { displaced }
                        }
                        Ok(ConvoyResumeOutcome::Queued { displaced }) => {
                            self.record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_resume", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyBriefQueued { displaced }
                        }
                        Err(message) => flotilla_protocol::CommandValue::Error { message },
                    }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { namespace, name } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.convoy_withdraw_pending_brief_internal(&namespace, &record_name).await {
                    Ok(withdrawn) => flotilla_protocol::CommandValue::ConvoyBriefWithdrawn { withdrawn },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref, force } =
            &command.action
        {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let routing = self.resolve_crew_routing_context(context).await.ok();
            let result = match self
                .crew_complete_as_principal_internal(
                    context,
                    message.clone(),
                    disposition.clone(),
                    decision_ledger_ref.clone(),
                    *force,
                    dispatching_principal_ref.clone(),
                )
                .await
            {
                Ok(value) => {
                    if let Some(resolved) = routing {
                        let namespace = resolved.command_context.namespace.as_deref().unwrap_or("flotilla");
                        self.record_lifecycle_mutation_best_effort(namespace, &resolved.convoy, "crew_complete", caller.as_ref(), false)
                            .await;
                    }
                    value
                }
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

        if let flotilla_protocol::CommandAction::CrewStall { context, reason, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_stall_internal(context, *reason, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewSupervise { namespace, convoy, vessel, role, operation, message, actor_crew_id } =
            &command.action
        {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, convoy).await {
                Ok(name) => match self
                    .crew_supervise_internal(
                        CrewSupervisionRequest::builder()
                            .namespace(&namespace)
                            .convoy_name(&name)
                            .vessel(vessel)
                            .role(role)
                            .operation(*operation)
                            .message(message)
                            .maybe_actor_crew_id(actor_crew_id.as_deref())
                            .maybe_principal(dispatching_principal_ref.as_ref())
                            .build(),
                    )
                    .await
                {
                    Ok(()) => {
                        self.record_lifecycle_mutation_best_effort(
                            &namespace,
                            &name,
                            &format!("crew_supervise_{operation:?}"),
                            caller.as_ref(),
                            false,
                        )
                        .await;
                        flotilla_protocol::CommandValue::Ok
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyLink { namespace, name, reference, relationship } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => self.link_convoy_subject(&namespace, &record_name, reference, Some(*relationship)).await,
                Err(error) => Err(error),
            };
            self.finish_context_free_command(
                id,
                empty_identity,
                result.map_or_else(|message| flotilla_protocol::CommandValue::Error { message }, |()| flotilla_protocol::CommandValue::Ok),
            );
            return Ok(id);
        }
        if let flotilla_protocol::CommandAction::ConvoyUnlink { namespace, name, reference } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => self.link_convoy_subject(&namespace, &record_name, reference, None).await,
                Err(error) => Err(error),
            };
            self.finish_context_free_command(
                id,
                empty_identity,
                result.map_or_else(|message| flotilla_protocol::CommandValue::Error { message }, |()| flotilla_protocol::CommandValue::Ok),
            );
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyDelete { namespace, name, force } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.reap_convoy_internal(&namespace, &record_name, *force).await {
                    Ok(()) => {
                        // Finalizers retain an explainable convoy after delete; a fully
                        // removed convoy has no remaining status to annotate.
                        self.record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_delete", caller.as_ref(), true).await;
                        flotilla_protocol::CommandValue::Ok
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyAbandon { namespace, name, reason } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match self.abandon_convoy_internal(&namespace, &record_name, reason, dispatching_principal_ref.as_ref()).await {
                        Ok(archives) => {
                            self.record_lifecycle_mutation_best_effort(&namespace, &record_name, "convoy_abandon", caller.as_ref(), false)
                                .await;
                            flotilla_protocol::CommandValue::ConvoyAbandoned { name: name.clone(), archives }
                        }
                        Err(message) => flotilla_protocol::CommandValue::Error { message },
                    }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, work, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = self.provisioning_namespace().await;
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
            let record_name = match resolve_local_convoy_name(&self.resource_backend, &namespace, convoy).await {
                Ok(record_name) => record_name,
                Err(message) => {
                    self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let check_work_is_completable = |current: &ResourceObject<ResourceConvoy>| match current.status.as_ref() {
                None => Err(ResourceError::other(format!("convoy {convoy} has no status"))),
                Some(status) => match status.work.get(work) {
                    None => Err(ResourceError::other(format!("convoy {convoy} does not contain work {work}"))),
                    Some(state)
                        if matches!(
                            state.phase,
                            flotilla_resources::WorkPhase::Failed
                                | flotilla_resources::WorkPhase::Cancelled
                                | flotilla_resources::WorkPhase::Abandoned
                        ) =>
                    {
                        Err(ResourceError::other(format!("convoy {convoy} work {work} is already terminal")))
                    }
                    Some(_) => Ok(()),
                },
            };
            let result = match apply_resource_status_patch_checked(
                &convoys,
                &record_name,
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

        if let flotilla_protocol::CommandAction::ConvoyStart { intent } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let acting_namespace = self.provisioning_namespace().await;
            let default_namespace = intent.namespace.clone().unwrap_or_else(|| acting_namespace.clone());
            let (namespace, intent) = match normalize_convoy_start_intent(&default_namespace, intent) {
                Ok(resolved) => resolved,
                Err(message) => {
                    self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let dispatching_principal_ref =
                dispatching_principal_ref.clone().unwrap_or_else(|| PrincipalRef::implicit_for_namespace(&acting_namespace));
            let key = ConvoyStartKey::new(namespace, &intent);
            if !self.pending_convoy_starts.lock().await.insert(key.clone()) {
                self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error {
                    message: format!("convoy start for project {} is already in progress", intent.project_ref),
                });
                return Ok(id);
            }
            let task = ConvoyStartTask::builder()
                .command_id(id)
                .intent(intent)
                .key(key.clone())
                .dispatching_principal_ref(dispatching_principal_ref)
                .build();
            if let Some(daemon) = self.self_weak.upgrade() {
                tokio::spawn(async move {
                    daemon.supervise_convoy_start(task).await;
                });
            } else {
                self.pending_convoy_starts.lock().await.remove(&key);
                self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error {
                    message: "convoy start worker is unavailable".to_string(),
                });
            }
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
            let role = name.clone();
            let project_identity = project_ref.as_deref();
            if let Err(message) = validate_convoy_name(&role) {
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
            if let Err(message) = allocate_convoy_generation(&self.resource_backend, &namespace, project_identity, &role).await {
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
            let record_name = convoy_record_name();
            let name = &record_name;
            if let Err(message) = self.check_local_free_space_floor().await {
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
            let mut workflow = match self
                .resource_backend
                .clone()
                .including_replicas::<WorkflowTemplate>(&namespace)
                .get(workflow_ref)
                .await
                .map(|source| source.object)
                .map_err(|error| format!("workflow template {workflow_ref}: {error}"))
            {
                Ok(workflow) => workflow,
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
            };
            let project_repositories = if let Some(project_ref) = project_ref {
                match self.snapshot_project_repositories(&namespace, project_ref, None).await {
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
                        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                        let (checkout_ref, inferred_repository_url, inferred_ref) = create_adopted_checkout_resource(
                            &self.resource_backend,
                            &self.observed_resource_backend,
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
                    let canonical_url = match repository_spec.live_remote() {
                        Some(remote) => remote.to_string(),
                        None => {
                            return Err(format!("repository {url} did not resolve to a remote identity"));
                        }
                    };
                    let repo_ref = repository_spec.key();
                    let repository = flotilla_resources::ensure_repository(
                        &self.resource_backend.clone().using::<Repository>(&namespace),
                        &repo_ref,
                        &repository_spec,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let default_ref = repository
                        .status
                        .as_ref()
                        .and_then(|status| status.default_branch.clone())
                        .or_else(|| if adopted_checkout.is_some() { r#ref.clone() } else { None })
                        .ok_or_else(|| format!("repository {repo_ref} has no resolved default branch"))?;
                    let workspace_slug = flotilla_resources::repository_workspace_slugs([(&repo_ref, &repository_spec)])
                        .remove(&repo_ref)
                        .expect("repository slug should resolve");
                    Ok::<_, String>(vec![ConvoyRepositorySpec {
                        url: canonical_url,
                        repo_ref,
                        source_ref: default_ref.clone(),
                        target_ref: default_ref,
                        workspace_slug,
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
            let adopted_checkout_ref_to_cleanup = adopted_checkout.as_ref().map(|(_, checkout_ref)| checkout_ref.clone());
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
            let placement = match self
                .resolve_convoy_placement(
                    &namespace,
                    project_ref.as_deref(),
                    &repositories,
                    &workflow.spec,
                    placement_policy.as_deref(),
                    false,
                )
                .await
            {
                Ok(placement) => placement,
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
            };
            let credential_result = resolve_and_validate_workflow_credentials(
                &self.resource_backend,
                &namespace,
                project_ref.as_deref(),
                &repositories,
                placement.selected.as_ref(),
                &mut workflow.spec,
            )
            .await;
            if let Err(message) = credential_result {
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement_decision = match placement.selected.as_ref() {
                Some(selected) => match placement_target_host(&self.resource_backend, &namespace, selected).await {
                    Ok(target_host) => Some(PlacementDecision {
                        minimal_alternatives: Vec::new(),
                        escalation_reason: None,
                        policy_name: selected.metadata.name.clone(),
                        target_host,
                        refused_candidates: placement.refused_candidates,
                        viable_not_selected: placement.viable_not_selected,
                        allocation: placement.allocation,
                    }),
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
                },
                None => None,
            };
            if let Err(message) = self.check_remote_placement_free_space_floor(&namespace, placement_decision.as_ref()).await {
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement_policy = placement.selected.as_ref().map(|placement| placement.metadata.name.clone());
            let _admission_guard = self.convoy_admission.lock().await;
            let generation = match allocate_convoy_generation(&self.resource_backend, &namespace, project_identity, &role).await {
                Ok(generation) => generation,
                Err(message) => {
                    if let Some(checkout_ref) = adopted_checkout_ref_to_cleanup {
                        if let Err(error) = self.resource_backend.clone().using::<ResourceCheckout>(&namespace).delete(&checkout_ref).await
                        {
                            warn!(%error, %checkout_ref, "failed to clean up adopted checkout after convoy identity conflict");
                        }
                    }
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
            };
            let spec = ConvoySpec {
                workflow_ref: workflow_ref.clone(),
                role: role.clone(),
                generation,
                dispatching_principal_ref: dispatching_principal_ref
                    .clone()
                    .unwrap_or_else(|| PrincipalRef::implicit_for_namespace(&namespace)),
                inputs: inputs.iter().map(|(k, v)| (k.clone(), InputValue::String(v.clone()))).collect(),
                placement_policy,
                repositories,
                r#ref,
                project_ref: project_ref.clone(),
                adopted_checkout_refs,
                subjects: Vec::new(),
                issues: Vec::new(),
                change_request: None,
                instruction: None,
            };
            let result = match self
                .create_convoy_with_workflow_snapshot(
                    &namespace,
                    name,
                    ConvoySnapshotBundle::builder()
                        .spec(&spec)
                        .workflow(&workflow.spec)
                        .maybe_placement(placement.selected.as_ref().map(|placement| &placement.spec))
                        .maybe_placement_decision(placement_decision)
                        .build(),
                    ConvoyDispatchRegard::Emit,
                )
                .await
            {
                Ok(()) => flotilla_protocol::CommandValue::ConvoyCreated { name: convoy_address(&role, project_identity) },
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
            let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
            let result = match validate_project_name(name).and_then(|_| parse_project_yaml(spec_yaml)) {
                Ok(spec) => match normalize_project_spec(spec) {
                    Ok(spec) => {
                        let outcome = match projects.get(name).await {
                            Ok(existing) if is_declaration_backed_project(&existing) => {
                                Err(format!("project {name} is managed by a declaration; use project refresh to update it"))
                            }
                            Ok(existing) => projects
                                .apply_as(
                                    &WriterIdentity::operator().with_source("project-apply"),
                                    &InputMeta::from(&existing.metadata),
                                    &spec,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(ResourceError::NotFound { .. }) => projects
                                .apply_as(
                                    &WriterIdentity::operator().with_source("project-apply"),
                                    &InputMeta::builder().name(name.clone()).build(),
                                    &spec,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error.to_string()),
                        };
                        match outcome {
                            Ok(()) => flotilla_protocol::CommandValue::ProjectApplied { name: name.clone() },
                            Err(message) => flotilla_protocol::CommandValue::Error { message },
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

        if let flotilla_protocol::CommandAction::ProjectRegister { target } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_register(target).await {
                Ok((name, members)) => CommandValue::ProjectRegistered { name, members },
                Err(message) => CommandValue::Error { message },
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

        if let flotilla_protocol::CommandAction::ProjectRefresh { name } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_refresh(name).await {
                Ok((members, converged, changes, operational_entries)) => {
                    CommandValue::ProjectRefreshed { name: name.clone(), members, converged, changes, operational_entries }
                }
                Err(message) => CommandValue::Error { message },
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
                Ok(outcome) => flotilla_protocol::CommandValue::RepoTracked {
                    path: outcome.tracked_path,
                    resolved_from: outcome.resolved_from,
                    identity_change: outcome.identity_change,
                },
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
            let repo_path = match self.resolve_repo_selector(repo).await {
                Ok(path) => path,
                Err(tracked_error) => self.resolve_observation_root_selector(repo).map_err(|_| tracked_error)?,
            };
            let description = command.description().to_string();
            let repo_identity = self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
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
                Ok(identity_change) => flotilla_protocol::CommandValue::Refreshed {
                    repos: vec![repo_path.clone()],
                    identity_changes: identity_change.into_iter().collect(),
                },
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
        let repository_action_policy_error = self.repository_action_policy_error(&command, &repo).await;
        let runner = Arc::clone(&self.discovery.runner);
        let env = Arc::clone(&self.discovery.env);
        let event_tx = self.event_tx.clone();
        let event_sink = self.event_sink.clone();
        let (repo_identity, registry) = {
            let repos = self.repos.read().await;
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            (state.identity().clone(), state.registry())
        };
        let providers_data = Arc::new(self.executor_provider_data(&repo_identity, &repo, &registry).await);

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
        let vcs_resolver = self.self_weak.upgrade().ok_or("VCS resolver daemon unavailable")? as Arc<dyn crate::vcs::CheckoutVcsResolver>;
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

            let plan = match repository_action_policy_error {
                Some(message) => Err(CommandValue::Error { message }),
                None => executor::build_plan(
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
                .await
                .map_err(executor::PlannerRefusal::into_command_value),
            };

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
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
                        vcs_resolver: Arc::clone(&vcs_resolver),
                    };
                    let result = run_step_plan_with_remote_executor(
                        step_plan,
                        id,
                        local_node_id,
                        repo_identity.clone(),
                        ExecutionEnvironmentPath::new(&repo_path),
                        token,
                        event_sink,
                        &resolver,
                        remote_executor.as_ref(),
                    )
                    .await;
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

impl InProcessDaemon {
    async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String> {
        let namespace = requested_namespace.map(ToOwned::to_owned).unwrap_or(self.provisioning_namespace().await);
        self.read_projections().explain_convoy(&namespace, name).await
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    fn query_subscription(&self, subscriber_id: uuid::Uuid) -> QuerySubscription {
        let state = self.aggregator_projection_state.clone();
        let namespace = self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone();
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
        let repository_keys = self.repository_keys_by_path.read().await;
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut result = Vec::new();
        for identity in order.iter() {
            if let Some(state) = repos.get(identity) {
                result.push(RepoInfo {
                    identity: state.identity().clone(),
                    repository_key: repository_keys.get(state.preferred_path()).cloned(),
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
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false, None).await
    }

    async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        use flotilla_protocol::CommandAction;
        match &command.action {
            CommandAction::QueryRepoProviders { repo } => match self.get_repo_providers_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoProviders(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostList {} => match self.list_hosts_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryProjectList {} => match self.list_projects_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::ProjectList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDispatchQueue { project } => match self.dispatch_queue_internal(project.as_deref()).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::DispatchQueue(Box::new(v))),
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
            CommandAction::QueryFleetHealth {} => match self.fleet_health_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetHealth(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFulfilmentList {} => match self.fulfilment_list_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FulfilmentList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetList { project, crew_id, convoy } => {
                match self.scoped_fleet_list(project.as_deref(), crew_id.as_deref(), convoy.as_deref()).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::FleetList(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryCrewList { context } => match self.crew_list_internal(context).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CrewList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetReplicaSnapshot {} => match self.fleet_replica_snapshot_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetReplicaSnapshot(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDaemonLogs { query } => {
                let generations = self.config.load_daemon_config()?.logging.generations;
                let state_dir = self.config.state_dir().as_path().to_path_buf();
                let query = query.clone();
                let read_result = tokio::task::spawn_blocking(move || crate::log_file::read_daemon_logs(&state_dir, generations, &query))
                    .await
                    .map_err(|error| format!("daemon log reader task failed: {error}"))?;
                match read_result {
                    Ok(lines) => Ok(flotilla_protocol::CommandValue::DaemonLogs { lines }),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryExplainConvoy { namespace, name } => match self.explain_convoy_internal(namespace.as_deref(), name).await {
                Ok(explanation) => Ok(CommandValue::ConvoyExplanation(Box::new(explanation))),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryResourceList { namespace, kind, include_replicas } => {
                let listed = if *include_replicas {
                    list_resource_kind_including_replicas(&self.resource_backend, namespace, kind).await
                } else {
                    list_resource_kind(&self.resource_backend, namespace, kind).await
                };
                match listed {
                    Ok(v) => {
                        let resource_version = v.value["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
                        let generation = v.value["metadata"]["generation"].as_str().map(ToOwned::to_owned);
                        let records = v.value["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .cloned()
                            .map(|object| resource_record(ResourceRecordType::Current, object, &self.node_id))
                            .collect();
                        Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                            v.kind,
                            v.plural,
                            v.namespace,
                            ResourceCursor::from_position(resource_version, generation),
                            records,
                        ))))
                    }
                    Err(error) => Ok(CommandValue::Error { message: error.to_string() }),
                }
            }
            CommandAction::QueryResourceGet { namespace, kind, name } => {
                // Take the collection cursor before reading the object. A
                // concurrent mutation can then be replayed (at worst as a
                // duplicate) instead of being hidden behind a newer cursor.
                let cursor_list = match list_resource_kind(&self.resource_backend, namespace, kind).await {
                    Ok(listed) => listed,
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let visible = match get_resource_kind_including_replicas(&self.resource_backend, namespace, kind, name).await {
                    Ok(object) => object,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(CommandValue::Error { message: format!("resource {kind}/{namespace}/{name} not found") });
                    }
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let resource_version = cursor_list.value["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
                let generation = cursor_list.value["metadata"]["generation"].as_str().map(ToOwned::to_owned);
                let mut value = visible.value;
                if visible.kind == "Project" {
                    match serde_json::from_value::<flotilla_resources::ProjectSpec>(value["spec"].clone()) {
                        Ok(spec) => {
                            match resolve_project_issue_sources(&self.resource_backend.including_replicas::<Repository>(namespace), &spec)
                                .await
                            {
                                IssueSourceResolution::Available { bindings } => {
                                    value["resolvedIssueSources"] = serde_json::Value::Array(
                                        bindings
                                            .into_iter()
                                            .map(|binding| {
                                                serde_json::json!({
                                                    "service": binding.source.service,
                                                    "scope": binding.source.scope,
                                                    "alias": binding.alias,
                                                    "creatable": binding.creatable,
                                                })
                                            })
                                            .collect(),
                                    );
                                }
                                IssueSourceResolution::Unavailable(reason) => {
                                    value["resolvedIssueSources"] = serde_json::Value::Array(Vec::new());
                                    let message = match reason {
                                        IssueSourceUnavailable::RepositoryUnavailable { repository, message } => {
                                            format!("repository {repository}: {message}")
                                        }
                                        IssueSourceUnavailable::InvalidBindings { message } => message,
                                        IssueSourceUnavailable::NoIssueSource => format!("project {name} has no issue source"),
                                    };
                                    value["issueSourceResolutionError"] = serde_json::Value::String(message);
                                }
                            }
                        }
                        Err(error) => {
                            warn!(resource_kind = %visible.kind, resource = %name, %error, "failed to decode project spec for resource read");
                        }
                    }
                }
                if visible.kind != "Event" {
                    let object_name = value["metadata"]["name"].as_str().unwrap_or(name);
                    let regarding = EventRegarding {
                        api_version: value["apiVersion"].as_str().unwrap_or("flotilla.work/v1").to_string(),
                        kind: visible.kind.clone(),
                        namespace: namespace.clone(),
                        name: object_name.to_string(),
                    };
                    match EventRecorder::new(self.resource_backend.clone()).recent_for(&regarding, Utc::now()).await {
                        Ok(events) if !events.is_empty() => {
                            value["recentEvents"] =
                                serde_json::Value::Array(events.into_iter().filter_map(|event| serde_json::to_value(event).ok()).collect());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            warn!(resource_kind = %visible.kind, resource = %name, %error, "failed to enrich resource read with recent events")
                        }
                    }
                }
                let record = resource_record(ResourceRecordType::Current, value, &self.node_id);
                Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                    visible.kind,
                    visible.plural,
                    visible.namespace,
                    ResourceCursor::from_position(resource_version, generation),
                    vec![record],
                ))))
            }
            CommandAction::Attach { reference, host, mode } => {
                let project_context = self.attach_resolver().attach_project_context(command.context_repo.as_ref()).await?;
                match self.resolve_attach_with_context(reference, host.as_ref(), false, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        if let Some(binding) = &resolved.binding {
                            if let Err(error) = self.emit_attach_regard(binding, session_id).await {
                                warn!(%error, "failed to emit attach regard");
                            }
                        }
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::AttachTransient { reference, host, mode } => {
                let project_context = self.attach_resolver().attach_project_context(command.context_repo.as_ref()).await?;
                match self.resolve_attach_with_context(reference, host.as_ref(), true, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryIssues { repo, params, page, count } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                let page = provider.query(&source, params, *page, *count).await?;
                Ok(flotilla_protocol::CommandValue::IssuePage(page))
            }
            CommandAction::QueryIssueFetchByIds { repo, ids } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                let items = provider.fetch_by_ids(&source, ids).await?;
                Ok(flotilla_protocol::CommandValue::IssuesByIds { items })
            }
            CommandAction::QueryIssueOpenInBrowser { repo, id } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                provider.open_in_browser(&flotilla_protocol::IssueRef { source, id: id.clone() }).await?;
                Ok(flotilla_protocol::CommandValue::Ok)
            }
            other => Err(format!("execute_query not implemented for this command type: {:?}", std::mem::discriminant(other))),
        }
    }

    async fn observe_focus(&self, surface_id: uuid::Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        self.observe_surface_focus(surface_id, targets).await
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

async fn request_manifest_resolution(
    backend: &ResourceBackend,
    namespace: &str,
    kind: &str,
    name: &str,
    action: flotilla_protocol::ManifestResolution,
    requested_by: &str,
) -> Result<ResourceJsonResponse, String> {
    let object = get_resource_kind(backend, namespace, kind, name).await.map_err(|error| error.to_string())?;
    let annotations = object
        .value
        .get("metadata")
        .and_then(|metadata| metadata.get("annotations"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{kind}/{name} has no manifest provenance"))?;
    let root = annotations
        .get("flotilla.work/manifest-reconciler-root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{kind}/{name} has no manifest root"))?;
    let path = annotations
        .get("flotilla.work/manifest-path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{kind}/{name} has no manifest path"))?;
    let key = DocumentKey { path: path.to_string(), kind: object.kind, namespace: namespace.to_string(), name: name.to_string() };
    let roots = backend.using::<ManifestRoot>(namespace);
    // One-roll compatibility: older provenance named the host rather than the
    // materialized ManifestRoot. Resolve that host together with its source.
    // Host config declares one source today; remove this fallback after one roll.
    let root_name = match roots.get(root).await {
        Ok(_) => root.to_string(),
        Err(ResourceError::NotFound { .. }) => {
            let source = annotations
                .get("flotilla.work/manifest-source")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("{kind}/{name} has no manifest source"))?;
            let matching = roots
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|candidate| candidate.spec.host == root && candidate.spec.source == source)
                .map(|candidate| candidate.metadata.name)
                .collect::<Vec<_>>();
            match matching.as_slice() {
                [name] => name.clone(),
                [] => return Err(format!("ManifestRoot for {kind}/{name} is no longer declared")),
                _ => return Err(format!("ManifestRoot for {kind}/{name} is ambiguous")),
            }
        }
        Err(error) => return Err(error.to_string()),
    };
    let action = match action {
        flotilla_protocol::ManifestResolution::Sync => ResolutionAction::Sync,
        flotilla_protocol::ManifestResolution::Adopt => ResolutionAction::Adopt,
    };
    for _ in 0..3 {
        let existing = roots.get(&root_name).await.map_err(|error| error.to_string())?;
        let mut spec = existing.spec;
        spec.resolutions.insert(key.clone(), Resolution {
            action,
            token: uuid::Uuid::new_v4().to_string(),
            requested_by: if requested_by.is_empty() { "unknown".to_string() } else { requested_by.to_string() },
        });
        match roots.update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &spec).await {
            Ok(updated) => {
                return Ok(ResourceJsonResponse {
                    kind: "ManifestRoot".to_string(),
                    plural: "manifestroots".to_string(),
                    namespace: namespace.to_string(),
                    value: serde_json::to_value(updated).map_err(|error| error.to_string())?,
                    replica_origin: None,
                })
            }
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("ManifestRoot spec conflict retry budget exhausted".to_string())
}
