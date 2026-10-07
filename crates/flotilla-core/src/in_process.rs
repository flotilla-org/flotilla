//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

#[path = "attach.rs"]
mod attach;
mod checkout_providers;
mod cleat_roll;
mod crew_ops;
pub(crate) use crew_ops::convoy_message_address;
pub use crew_ops::{ConvoyResumeOutcome, CrewRoutingContext};
use crew_ops::{CrewService, CrewSupervisionRequest, CrewTurnDeliveryActuator};

// Exercise the real controller in the existing private daemon scenario harness
// without adding a production dependency from core back to controllers.
#[path = "in_process/convoy_admission.rs"]
mod convoy_admission;
#[cfg(test)]
#[path = "../../flotilla-controllers/src/reconcilers/convoy_ensure.rs"]
mod ensure_controller_under_test;
mod project_ops;
use checkout_providers::{CheckoutProvider, CheckoutProviders};
mod repository_operations;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use attach::AttachResolver;
pub use attach::ResolvedAttach;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use convoy_admission::{
    allocate_convoy_generation, convoy_address, convoy_ensure_name, convoy_record_name, discover_repository_change_request_with,
    normalize_convoy_start_intent, project_not_ready_error, resolve_and_validate_workflow_credentials, resolve_convoy_candidate_indices,
    validate_convoy_name, ConvoyAddressIdentity, ConvoyAdmission, ConvoyCreateAdmission, ConvoyStartKey, ConvoyStartTask,
    PlacementResolution, StaticFulfilmentDecider,
};
pub use convoy_admission::{PreparedConvoyAdmission, RoleAddress};
#[cfg(test)]
use flotilla_protocol::CheckoutArchiveStatus;
use flotilla_protocol::{
    commands::{AttachMode, RepositoryIdentityChange},
    qualified_path::QualifiedPath,
    result_set::{ConvoyChangeRequest, Rows},
    AttachBinding, CanonicalHostId, Change, CheckoutArchiveOutcome, CliListKind, CliListResponse, CliListRow, Command, CommandAction,
    CommandValue, ConvoyDispatchRegard, ConvoyExplanation, CrewCommandContext, CrewListResponse, DaemonEvent, DispatchQueueResponse,
    EntryOp, EnvironmentId, FleetHealthResponse, FleetListResponse, FulfilmentAllocation, FulfilmentAllocationCandidate,
    FulfilmentListResponse, HostListResponse, HostName, HostProviderStatus, HostProvidersResponse, HostStatusResponse, HostSummary,
    LeafAddress, ManagedTerminal, NodeId, NodeInfo, PeerConnectionState, PlacementDecision, PlacementRefusal, PlacementTargetHost,
    PlacementViableCandidate, PrincipalRef, ProjectListResponse, ProviderData, ProviderInfo, QueryCursor, RepoDelta, RepoIdentity,
    RepoInfo, RepoProvidersResponse, RepoSummary, ResolvedAttachPlan, ResourceCursor, ResourceJsonResponse, ResourceRecordType,
    ResourceRef, StatusResponse, StreamKey, SurfaceDeclaration, TopologyResponse, TopologyRoute, ViewAddress,
    AGENT_ADAPTER_PROVIDER_CATEGORY, TERMINAL_POOL_PROVIDER_CATEGORY,
};
#[cfg(test)]
use flotilla_resources::CrewMessageSender;
use flotilla_resources::{
    active_change_request_subjects, api_version, apply_resource_document, apply_status_patch as apply_resource_status_patch,
    apply_status_patch_checked as apply_resource_status_patch_checked, capped_github_app_permissions, change_request_address,
    change_request_address_with_forges, change_request_record_name, current_resource_kind_position,
    external_patches as convoy_external_patches, get_resource_kind, get_resource_kind_including_replicas, host_direct_environment_name,
    list_resource_kind, list_resource_kind_including_replicas, normalize_issue_source, normalize_project_spec,
    observed_change_request_subjects, resolve_project_issue_sources, AllocationDecision, BoundChangeRequest, CapabilityNeed,
    ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout, CheckoutPhase as ResourceCheckoutPhase,
    CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus, Clock, Convoy as ResourceConvoy, ConvoyEnsure,
    ConvoyIssue, ConvoyProvisioningState, ConvoyRepositorySpec, ConvoySpec, ConvoyStatusPatch, CredentialConsumer, CredentialGrant,
    CredentialSource, CredentialSpec, CrewCompletionPending, CrewSource, CrewSpec, DocumentKey, Environment as ResourceEnvironment,
    EnvironmentPhase, EventRecorder, EventRegarding, Forge, ForgeKind, FulfilmentGrant, FulfilmentKind, Host as ResourceHost,
    HostStatus as ResourceHostStatus, InMemoryBackend, InputMeta, InputValue, IssueSnapshot, IssueSourceResolution, IssueSourceUnavailable,
    LandingCredentialScope, LifecycleAuthority, ManifestRoot, ObjectMeta, ObservedChangeRequestState,
    ObservedCheckoutSpec as ResourceObservedCheckoutSpec, PlacementPolicy, PlacementPolicySpec, Platform, Project, ProjectSpec,
    ReadResourceObject, Repository, RepositoryIdentity, RepositoryKey, RepositorySpec, RepositoryTrust, Resolution, ResolutionAction,
    Resource, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, RoleHandoff, SupervisionTarget, SystemClock,
    TerminalCrewContext, TurnDeliveryRung, VesselRequirement, WatchEvent, WatchStart, WorkPhase as ResourceWorkPhase, WorkflowTemplate,
    WorkflowTemplateSpec, WriterIdentity, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL, GENERATION_LABEL, PROJECT_LABEL, ROLE_LABEL,
};
#[cfg(test)]
use flotilla_resources::{
    CheckoutIntegrationStatus, ConditionValue, ConvoyEnsureHoldReason, ConvoyEnsureSpec, ConvoyPhase, CrewCompletionRefusalCause,
    Demand as ResourceDemand, DemandKind, DemandSpec, HoldAct, IntegrationCondition, Presentation as ResourcePresentation,
    TerminalSessionIdentity, Vessel, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_SCOPES_ANNOTATION, DRIVER_ADMISSION_CONDITION_TYPE,
};
use futures::{FutureExt, StreamExt};
use project_ops::{is_declaration_backed_project, validate_project_name};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub use crate::convoy_ensure::{ConvoyEnsureAdmission, ConvoyEnsureReconciler, StandingConvoyBackingInspector};
#[cfg(test)]
use crate::ops_entry::{PRESENTS_AS_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION};
use crate::{
    agent_adapter::{required_agent_adapters, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    change_request_observer::{ChangeRequestObservationSource, ChangeRequestRef},
    checkout_integration::{
        checkout_path_from_status_and_spec, convoy_change_request_id_for_checkout, inspect_checkout_integration,
        inspect_convoy_checkout_integration,
    },
    config::{ConfigStore, StaticEnvironmentConfig},
    daemon::{DaemonHandle, QuerySubscription},
    environment_manager::{EnvironmentManager, ResolvedEnvironment},
    event_sink::{BroadcastEventSink, EventSink},
    executor,
    executor::checkout::CheckoutResolutionScope,
    fleet::FleetService,
    host_identity::{
        resolve_local_environment_state_dir, resolve_local_host_id, resolve_local_node_id, resolve_or_create_environment_id,
        resolve_or_create_remote_environment_id, resolve_or_create_remote_host_id,
    },
    host_registry::{HostCounts, HostQueryDetails},
    host_resolution::canonical_placement_host_ref_from_sources,
    leaf_engine::LeafSubscriptionTable,
    model::{provider_names_from_registry, repo_name, RepoModel},
    ops_entry::{ENSURED_FROM_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION},
    path_context::{canonical_or_original, DaemonHostPath, ExecutionEnvironmentPath},
    providers::{
        ai_utility::{AiUtility, ConvoyNames},
        change_request::{BoundObservations, ChangeRequestTracker, ObservationError},
        discovery::{
            discover_checkout_with_host_scoped, run_host_detectors, DiscoveryResult, DiscoveryRuntime, EnvVars, EnvironmentAssertion,
            EnvironmentBag,
        },
        environment::EnvironmentHandle,
        issue_tracker::IssueProvider,
        registry::ProviderRegistry,
        ssh_runner::SshCommandRunner,
        types::RepoCriteria,
        vcs::git_worktree::GitWorktreeStrategy,
        ChannelLabel, CommandRunner,
    },
    regard_lifecycle::{RegardLifecycle, SurfaceGestureOutcome, DEFAULT_REGARD_DECAY_SECONDS, DEFAULT_REGARD_REFRESH_SECONDS},
    repo_state::{RepoRootState, RepoState},
    repository_inspection::{GitRepositoryInspector, RepositoryContinuity, RepositoryInspection, RepositoryInspector},
    resource_explain::{resource_read_envelope, resource_record, run_resource_watch_command, ResourceWatchCommandContext},
    step::{
        run_step_plan_with_remote_executor, RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, StepOutcome, StepResolver,
    },
};

type ObservationScope = (String, String, String);
const OBSERVATION_CACHE_FALLBACK_DELAY: Duration = Duration::from_secs(9);

fn forge_service_matches(service_url: &str, service: &str) -> bool {
    service_url.split_once("://").is_some_and(|(_, authority)| authority.trim_end_matches('/').eq_ignore_ascii_case(service))
}

#[derive(bon::Builder)]
struct CachedObservation {
    expires_at: tokio::time::Instant,
    queried: BTreeSet<u64>,
    next_history_start: usize,
    result: Result<BoundObservations, ObservationError>,
}

impl CachedObservation {
    // A classified quota can affect every subject even when encountered in
    // one history page. This repository's cache pauses all its subjects,
    // including successful entries in an Ok batch; other scopes are independent.
    // Cached hard errors keep their per-subject refusal during this cooldown.
    fn rate_limit_error(&self) -> Option<&ObservationError> {
        observation_rate_limit_error(&self.result)
    }
}

// Header deadlines use UTC; translate their remaining duration once into a
// monotonic cache TTL. Completion and Landing still compare the original UTC
// deadline. Expired headers retain the ordinary short cache TTL to avoid a burst
// of simultaneous fresh claims, without advertising a fabricated forge reset.
fn observation_cache_delay(retry_at: Option<chrono::DateTime<Utc>>, now: chrono::DateTime<Utc>) -> Duration {
    retry_at
        .and_then(|retry_at| retry_at.signed_duration_since(now).to_std().ok())
        .filter(|delay| !delay.is_zero())
        .unwrap_or(OBSERVATION_CACHE_FALLBACK_DELAY)
}

fn observation_rate_limit_error(result: &Result<BoundObservations, ObservationError>) -> Option<&ObservationError> {
    match result {
        Err(error) => error.retry_at().map(|_| error),
        // Latest reset controls the entire scope. Lowest subject number breaks ties,
        // keeping diagnostics stable across HashMap insertion/iteration orders.
        Ok(statuses) => statuses
            .iter()
            .filter_map(|(number, status)| {
                let error = status.as_ref().err()?;
                Some((error.retry_at()?, std::cmp::Reverse(*number), error))
            })
            .max_by_key(|(deadline, number, _)| (*deadline, *number))
            .map(|(_, _, error)| error),
    }
}

// Timed observations (and successes) inherit the controlling scope deadline on
// both the initial read and cache hits, so Completion and Landing cannot shorten
// the wait. Hard refusals and untimed limits keep their original diagnostics.
fn observation_during_cooldown(
    result: &Result<BoundObservations, ObservationError>,
    number: u64,
    controlling: &ObservationError,
) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
    if let Ok(statuses) = result {
        if let Some(Err(error)) = statuses.get(&number) {
            if error.retry_at().is_none() {
                return Err(error.clone());
            }
        }
    }
    Err(controlling.clone())
}

struct ProviderChangeRequestObservationSource {
    backend: ResourceBackend,
    query_port: Arc<dyn ChangeRequestQueryPort>,
    cache: Mutex<HashMap<ObservationScope, Arc<Mutex<Option<CachedObservation>>>>>,
    warned_missing_identity: Mutex<HashSet<(String, String)>>,
    warned_missing_snapshot: Mutex<HashSet<(String, String)>>,
}

#[async_trait]
trait ChangeRequestQueryPort: Send + Sync {
    async fn discover_repository_change_request(
        &self,
        namespace: &str,
        repository: &RepositorySpec,
    ) -> Result<Arc<dyn ChangeRequestTracker>, String>;
}

struct ProviderChangeRequestQueryPort {
    resource_backend: ResourceBackend,
    config: Arc<ConfigStore>,
    discovery: Arc<DiscoveryRuntime>,
    environment_manager: Arc<EnvironmentManager>,
    local_environment_id: EnvironmentId,
}

struct ProviderIssueObservationSource {
    backend: ResourceBackend,
    query_port: Arc<dyn IssueQueryPort>,
}

#[async_trait]
trait IssueQueryPort: Send + Sync {
    async fn provider_for_source(&self, source: &flotilla_protocol::IssueSource) -> Result<Arc<dyn IssueProvider>, String>;

    async fn fetch_issue_by_ref(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.provider_for_source(&reference.source).await?.fetch_by_id(reference).await
    }
}

#[derive(bon::Builder)]
struct HostIssueProviderLease {
    bag: EnvironmentBag,
    runner: Arc<dyn CommandRunner>,
    config: serde_json::Value,
    provider: Weak<dyn IssueProvider>,
}

struct ProviderIssueQueryPort {
    forge_reads: crate::forge_observation::ForgeReads,
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
        let namespace = self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone();
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
                convoy_admission::repository_provider_bag(
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
                        convoy_admission::repository_provider_bag(
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
                    let provider: Arc<dyn IssueProvider> = Arc::new(crate::forge_observation::ObservedIssueProvider {
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
        let forges = self
            .backend
            .definitions::<Forge>(&subject.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let fallback = flotilla_protocol::IssueRef { source: issue_source_for_subject(subject, &forges)?, id: subject.number.to_string() };
        let reference = self
            .backend
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
        let issue = self.query_port.fetch_issue_by_ref(&reference).await?;
        let observed_at = chrono::Utc::now();
        Ok(flotilla_resources::IssueStatus {
            title: flotilla_resources::Observation::known(issue.title, observed_at),
            assignees: flotilla_resources::Observation::known(issue.assignees, observed_at),
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

struct BoundConvoyCredentialRefs {
    numbers: BTreeSet<u64>,
    credentials_by_number: BTreeMap<u64, BTreeSet<String>>,
}

fn convoy_change_request_credential_refs(
    convoy: &ResourceObject<ResourceConvoy>,
    requested: &ChangeRequestRef,
) -> Result<BoundConvoyCredentialRefs, String> {
    let bound_numbers = active_change_request_subjects(convoy)?
        .into_iter()
        .filter(|bound| {
            bound.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && bound.source.service == requested.service
                && bound.source.scope == requested.scope
        })
        .filter_map(|bound| bound.id.parse().ok())
        .collect::<BTreeSet<_>>();
    let refs = convoy
        .status
        .as_ref()
        .and_then(|status| status.workflow_snapshot.as_ref())
        .map(|snapshot| snapshot.vessels.iter().flat_map(|vessel| &vessel.credential_refs).cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let by_number = bound_numbers.iter().map(|number| (*number, refs.clone())).collect();
    Ok(BoundConvoyCredentialRefs { numbers: bound_numbers, credentials_by_number: by_number })
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

impl ProviderChangeRequestObservationSource {
    fn new(backend: ResourceBackend, query_port: Arc<dyn ChangeRequestQueryPort>) -> Self {
        Self {
            backend,
            query_port,
            cache: Mutex::new(HashMap::new()),
            warned_missing_identity: Mutex::new(HashSet::new()),
            warned_missing_snapshot: Mutex::new(HashSet::new()),
        }
    }

    async fn query(
        &self,
        subjects: &[ChangeRequestRef],
        subject: &ChangeRequestRef,
        fresh: bool,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
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
        // A completion read and a newly admitted subject must respect a forge
        // cooldown too. Check before discovery and regardless of the cached batch.
        if let Some(entry) = cache.as_ref().filter(|entry| tokio::time::Instant::now() < entry.expires_at) {
            if let Some(error) = entry.rate_limit_error() {
                return observation_during_cooldown(&entry.result, subject.number, error);
            }
        }

        let repositories =
            self.backend.including_replicas::<Repository>(&subject.namespace).list().await.map_err(|error| error.to_string())?;
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
        let mut credential_refs_by_number = BTreeMap::<u64, BTreeSet<String>>::new();
        let convoys =
            self.backend.including_replicas::<ResourceConvoy>(&subject.namespace).list().await.map_err(|error| error.to_string())?.items;
        let missing_snapshots = convoys
            .iter()
            .filter(|convoy| {
                !convoy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal())
                    && convoy.object.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_none()
            })
            .map(|convoy| convoy.object.metadata.name.as_str())
            .collect::<HashSet<_>>();
        self.warned_missing_snapshot
            .lock()
            .await
            .retain(|(namespace, name)| namespace != &subject.namespace || missing_snapshots.contains(name.as_str()));
        for convoy in convoys {
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
            let bound = match convoy_change_request_credential_refs(&convoy.object, subject) {
                Ok(bound) => bound,
                Err(error) => {
                    tracing::warn!(convoy = %convoy.object.metadata.name, %error, "could not resolve active change request subjects for crew identity");
                    continue;
                }
            };
            if !bound.numbers.is_empty() {
                numbers.extend(&bound.numbers);
                if convoy.object.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_some() {
                    for (number, refs) in bound.credentials_by_number {
                        credential_refs_by_number.entry(number).or_default().extend(refs);
                    }
                } else if self.warned_missing_snapshot.lock().await.insert((subject.namespace.clone(), convoy.object.metadata.name.clone()))
                {
                    tracing::warn!(convoy = %convoy.object.metadata.name, "bound change request has no frozen workflow for crew credential identity");
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
                        .unwrap_or_else(|| Err(format!("change request {} was not found", subject.number).into()));
                }
            }
        }
        let mut numbers = queried.iter().copied().collect::<Vec<_>>();
        // The provider is rediscovered per observation. Keep fair history
        // scheduling with this source's cache rather than on that provider.
        let history_start = cache.as_ref().map_or(0, |cached| cached.next_history_start) % numbers.len();
        numbers.rotate_left(history_start);
        let mut crew_logins = BTreeMap::<u64, BTreeSet<String>>::new();
        let credentials = if credential_refs_by_number.is_empty() {
            Vec::new()
        } else {
            match self.backend.including_replicas::<CredentialSpec>(&subject.namespace).list().await {
                Ok(credentials) => credentials.items,
                Err(error) => {
                    tracing::warn!(%error, "could not list crew credential declarations; change request markers remain unverified");
                    Vec::new()
                }
            }
        };
        let missing_actors = credentials
            .iter()
            .filter(|credential| {
                matches!(credential.object.spec.consumer, CredentialConsumer::GithubApp { .. })
                    && credential.object.spec.consumer.github_actor_login().is_none()
            })
            .map(|credential| credential.object.metadata.name.as_str())
            .collect::<HashSet<_>>();
        self.warned_missing_identity
            .lock()
            .await
            .retain(|(namespace, name)| namespace != &subject.namespace || missing_actors.contains(name.as_str()));
        for credential in credentials {
            for (number, refs) in &credential_refs_by_number {
                if refs.contains(&credential.object.metadata.name) {
                    if let Some(login) = credential.object.spec.consumer.github_graphql_actor_login() {
                        crew_logins.entry(*number).or_default().insert(login.to_string());
                    } else if matches!(credential.object.spec.consumer, CredentialConsumer::GithubApp { .. })
                        && self
                            .warned_missing_identity
                            .lock()
                            .await
                            .insert((subject.namespace.clone(), credential.object.metadata.name.clone()))
                    {
                        tracing::warn!(credential = %credential.object.metadata.name, "granted GitHub App credential has no actor_login; crew address markers cannot be recognized");
                    }
                }
            }
        }
        let provider = self.query_port.discover_repository_change_request(&subject.namespace, &repository.object.spec).await?;
        let crew_logins = crew_logins.into_iter().map(|(number, logins)| (number, logins.into_iter().collect())).collect();
        let result = provider.observe_bound(&numbers, &crew_logins).await;
        let delay = observation_cache_delay(observation_rate_limit_error(&result).and_then(ObservationError::retry_at), Utc::now());
        let status = if let Some(error) = observation_rate_limit_error(&result) {
            observation_during_cooldown(&result, subject.number, error)
        } else {
            result.as_ref().map_err(Clone::clone).and_then(|statuses| {
                statuses
                    .get(&subject.number)
                    .cloned()
                    .unwrap_or_else(|| Err(format!("change request {} was not found", subject.number).into()))
            })
        };
        *cache = Some(
            CachedObservation::builder()
                .expires_at(tokio::time::Instant::now() + delay)
                .queried(queried)
                .next_history_start((history_start + 1) % numbers.len())
                .result(result)
                .build(),
        );
        status
    }
}

#[async_trait]
impl ChangeRequestObservationSource for ProviderChangeRequestObservationSource {
    async fn observe(&self, subject: &ChangeRequestRef) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.query(std::slice::from_ref(subject), subject, false).await
    }

    async fn observe_group(
        &self,
        subjects: &[ChangeRequestRef],
        subject: &ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
        self.query(subjects, subject, false).await
    }

    async fn observe_for_completion(
        &self,
        subject: &ChangeRequestRef,
    ) -> Result<flotilla_resources::ChangeRequestStatus, ObservationError> {
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

mod dispatch_board;
mod read_projections;
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
    let path = crate::probe::canonicalize(checkout_path).await?;
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

fn unready_placement_refusal(
    placement_name: &str,
    host_label: &str,
    status: Option<&ResourceHostStatus>,
    now: DateTime<Utc>,
) -> Option<String> {
    let Some(status) = status else {
        return Some(format!("placement `{placement_name}` host `{host_label}` is not ready: status is unavailable"));
    };
    if status.sleeping_until.is_some_and(|until| until > now) {
        return None;
    }
    let mut observed = status.clone();
    observed.apply_heartbeat_readiness(now);
    if observed.ready {
        return None;
    }
    let mut reason = placement_host_not_ready_reason(placement_name, host_label, host_generation(Some(status)), &observed);
    if !observed.readiness_blocked() && !status.heartbeat_is_fresh(now) {
        match observed.heartbeat_at {
            None => reason.push_str(": heartbeat is unavailable"),
            Some(_) => reason.push_str(": heartbeat is stale"),
        }
    }
    Some(reason)
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
    if capabilities.and_then(|capabilities| capabilities.get("os")).and_then(serde_json::Value::as_str) != Some(Platform::Linux.as_str()) {
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
async fn discover_repo_for_environment(
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
            convoy_admission::repository_provider_bag(resource_backend, config, environment_manager, environment_id, namespace, spec)
                .await?;
    }
    let ee_path = ExecutionEnvironmentPath::new(repo_path);
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };

    let host_scoped = discovery
        .host_scoped_providers
        .discover_for_environment(environment_id, &host_bag, &discovery.factories, config, &ee_path, Arc::clone(&runner))
        .await;
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

pub struct InProcessDaemon {
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
    host_registry: crate::host_registry::HostRegistry,
    local_environment_id: EnvironmentId,
    environment_manager: Arc<EnvironmentManager>,
    cleat_roll_report: Mutex<Option<crate::cleat_roll::RollReport>>,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: Arc<DiscoveryRuntime>,
    issue_query_port: Arc<dyn IssueQueryPort>,
    forge_budgets: crate::forge_budget::ForgeBudgets,
    pub(crate) forge_demand_refresh: Mutex<()>,
    dispatch_board_cache: dispatch_board::DispatchBoardCache,
    /// VCS capabilities are selected once for each checkout in its execution environment.
    checkout_providers: Arc<CheckoutProviders>,
    repository_providers: Mutex<HashMap<(String, RepositoryKey), Arc<repository_operations::RepositoryProviderLease>>>,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    self_weak: Weak<InProcessDaemon>,
    convoy_admission: ConvoyAdmission,
    convoy_ensure_reconciler: RwLock<Option<Arc<dyn ConvoyEnsureReconciler>>>,
    crew_ops: Arc<CrewService>,
    brief_artifact_writer: Arc<RwLock<Option<Arc<dyn BriefArtifactWriter>>>>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: crate::agents::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
    resource_backend: ResourceBackend,
    message_inboxes: Arc<Mutex<HashMap<String, flotilla_resources::MessageInbox>>>,
    clock: Arc<dyn Clock>,
    regard_lifecycle: Arc<RegardLifecycle>,
    observed_resource_backend: ResourceBackend,
    /// Serializes observed Checkout publication with repository removal so a
    /// refresh captured before untracking cannot recreate deleted resources.
    /// Admission cleanup and resource deletion share this lock with adoption.
    observed_checkout_reconciliation: Arc<Mutex<()>>,
    aggregator_projection_state: AggregatorProjectionState,
    /// Provisioning namespace used by daemon-side resource operations (e.g.
    /// looking up the Convoy whose task is being marked complete). Set by the
    /// daemon runtime at startup; defaults to [`DEFAULT_PROVISIONING_NAMESPACE`].
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    checkout_namespace_changes: tokio::sync::watch::Sender<()>,
    fleet: FleetService,
    repository_inspector: RwLock<Option<Arc<dyn RepositoryInspector>>>,
    operator_reconciler: RwLock<Option<Arc<dyn OperatorReconciler>>>,
    /// Process-lifetime host discovery, independent of tracked-root membership.
    /// Heartbeats publish these observations into Host status.
    local_provider_statuses: Vec<HostProviderStatus>,
    local_placement_provider_statuses: RwLock<Vec<HostProviderStatus>>,
    /// Last terminal state published per repository, used to emit field-scoped
    /// deltas without disturbing unrelated provider snapshot state.
    managed_terminals_by_repo: RwLock<HashMap<RepoIdentity, HashMap<flotilla_protocol::AttachableId, ManagedTerminal>>>,
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

pub const BRIEF_ARTIFACTS_ANNOTATION: &str = "flotilla.work/brief-artifacts";

#[async_trait]
pub trait BriefArtifactWriter: Send + Sync {
    async fn put_brief(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        subject: &str,
        content: &[u8],
        charter_commit: Option<&str>,
    ) -> Result<String, String>;
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
        self.convoy_admission.admit_ensured_convoy(namespace, ensure, admission, annotations).await
    }
    async fn abandon(&self, namespace: &str, name: &str, reason: &str, principal_ref: Option<&PrincipalRef>) -> Result<(), String> {
        self.abandon_convoy_internal(namespace, name, reason, principal_ref).await.map(|_| ())
    }
    async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.reap_convoy_internal(namespace, name, force).await
    }
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

    async fn ledger_delivery_environment(&self, _namespace: &str, _environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        Err("credential delivery record is unavailable".to_string())
    }
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

        let mut discovery = discovery;
        let forge_budgets = crate::forge_budget::ForgeBudgets::default();
        discovery.runner = Arc::new(crate::forge_budget::BudgetedRunner { inner: discovery.runner, budgets: forge_budgets.clone() });
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
        let observed = if matches!(&resource_backend, ResourceBackend::Sqlite(_)) {
            let path = config.state_dir().as_path().join("observation-replicas.sqlite");
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.expect("create observation replica directory");
            }
            let replicas = flotilla_resources::SqliteBackend::open_async(&path).await.expect("open durable observation replicas");
            InMemoryBackend::observed_with_durable_replicas(replicas)
        } else {
            InMemoryBackend::observed()
        };
        let observed_resource_backend = ResourceBackend::InMemory(observed).with_local_root(local_node_id.clone());
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
                        Arc::new(crate::vcs::FixedVcsResolver(Arc::clone(&vcs.vcs))),
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
                .map(|inspection| repository_operations::repository_event_identity(&inspection.spec, None))
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
                        flotilla_resources::ensure_repository(
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
                        crate::observed_resources::reconcile_checkouts(
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
        let local_host_summary = crate::host_summary::build_local_host_summary(
            &local_node_id,
            &host_name,
            EnvironmentId::host(environment_manager.local_host_id().clone()),
            &environment_manager,
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
        let change_request_refresher = crate::change_request_observer::ChangeRequestRefresher::new(
            host_name.to_string(),
            resource_backend.clone(),
            local_node_id.to_string(),
            observation_source.clone(),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        if let Err(error) = change_request_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned change request observations at startup failed");
        }
        let issue_query_port: Arc<dyn IssueQueryPort> = Arc::new(ProviderIssueQueryPort {
            forge_reads: crate::forge_observation::ForgeReads::new(resource_backend.clone(), DEFAULT_PROVISIONING_NAMESPACE.into()),
            host_providers: Mutex::new(HashMap::new()),
            backend: resource_backend.clone(),
            config: Arc::clone(&config),
            discovery: Arc::clone(&discovery),
            environment_manager: Arc::clone(&environment_manager),
            local_environment_id: local_environment_id.clone(),
            provisioning_namespace: Arc::clone(&provisioning_namespace),
        });
        let issue_refresher = crate::issue_observer::IssueRefresher::new(
            resource_backend.clone(),
            local_node_id.to_string(),
            Arc::new(ProviderIssueObservationSource { backend: resource_backend.clone(), query_port: Arc::clone(&issue_query_port) }),
            crate::issue_observer::IssueRefreshCadence::default(),
        );
        if let Err(error) = issue_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned issue observations at startup failed");
        }
        let leaf_subscriptions =
            LeafSubscriptionTable::with_issues(resource_backend.clone(), event_sink.clone(), change_request_refresher, issue_refresher);
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
        let checkout_providers = Arc::new(
            CheckoutProviders::builder()
                .resource_backend(resource_backend.clone())
                .observed_resource_backend(observed_resource_backend.clone())
                .config(Arc::clone(&config))
                .discovery(Arc::clone(&discovery))
                .environment_manager(Arc::clone(&environment_manager))
                .local_environment_id(local_environment_id.clone())
                .provisioning_namespace(Arc::clone(&provisioning_namespace))
                .build(),
        );
        let message_inboxes = Arc::new(Mutex::new(HashMap::new()));
        let crew_ops = Arc::new(
            CrewService::builder()
                .message_inboxes(Arc::clone(&message_inboxes))
                .resource_backend(resource_backend.clone())
                .leaf_subscriptions(leaf_subscriptions.clone())
                .clock(Arc::clone(&clock))
                .provisioning_namespace(Arc::clone(&provisioning_namespace))
                .config(Arc::clone(&config))
                .host_name(host_name.clone())
                .brief_artifact_writer(Arc::clone(&brief_artifact_writer))
                .environment_manager(Arc::clone(&environment_manager))
                .checkout_providers(Arc::clone(&checkout_providers))
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
            host_registry: crate::host_registry::HostRegistry::new(
                NodeInfo::new(local_node_id.clone(), host_name.to_string()),
                local_host_summary,
            ),
            local_environment_id: local_environment_id.clone(),
            environment_manager: Arc::clone(&environment_manager),
            cleat_roll_report: Mutex::new(None),
            discovery: Arc::clone(&discovery),
            issue_query_port: Arc::clone(&issue_query_port),
            forge_budgets,
            forge_demand_refresh: Mutex::new(()),
            checkout_providers: Arc::clone(&checkout_providers),
            checkout_namespace_changes: tokio::sync::watch::channel(()).0,
            repository_providers: Mutex::new(HashMap::new()),
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            self_weak: self_weak.clone(),
            convoy_admission: ConvoyAdmission::builder()
                .backend(resource_backend.clone())
                .observed_backend(observed_resource_backend.clone())
                .observed_checkout_reconciliation(Arc::clone(&observed_checkout_reconciliation))
                .config(Arc::clone(&config))
                .discovery(Arc::clone(&discovery))
                .environment_manager(Arc::clone(&environment_manager))
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

            crew_ops: Arc::clone(&crew_ops),
            brief_artifact_writer: Arc::clone(&brief_artifact_writer),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
            clock: Arc::clone(&clock),
            regard_lifecycle: Arc::clone(&regard_lifecycle),
            resource_backend: resource_backend.clone(),
            message_inboxes,
            observed_resource_backend: observed_resource_backend.clone(),
            observed_checkout_reconciliation: Arc::clone(&observed_checkout_reconciliation),
            aggregator_projection_state: aggregator_projection_state.clone(),
            provisioning_namespace: Arc::clone(&provisioning_namespace),
            fleet: FleetService::new(
                resource_backend.clone(),
                aggregator_projection_state.clone(),
                host_name.clone(),
                Some(CanonicalHostId::resolved(environment_manager.local_host_id().as_str())),
            ),
            repository_inspector: RwLock::new(None),
            operator_reconciler: RwLock::new(None),
            local_provider_statuses,
            local_placement_provider_statuses: RwLock::new(Vec::new()),
            managed_terminals_by_repo: RwLock::new(HashMap::new()),
        });
        crew_ops.set_turn_delivery_actuator(Arc::new(CrewTurnDeliveryActuator { crew: Arc::downgrade(&crew_ops) })).await;

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
                daemon.convoy_admission.repository_change_requests.write().await.retain(|key, _| live.contains(key.to_string().as_str()));
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
                    daemon.convoy_admission.repository_change_requests.write().await.retain(|key, _| key.to_string() != name);
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

        #[cfg(test)]
        daemon
            .install_convoy_ensure_reconciler(Arc::new(
                ensure_controller_under_test::EnsureReconciler::builder()
                    .resource_backend(daemon.resource_backend.clone())
                    .clock(Arc::clone(&daemon.clock))
                    .build(),
            ))
            .await;

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
        self.convoy_admission.set_free_space_path(path);
    }

    pub async fn set_brief_artifact_writer(&self, writer: Arc<dyn BriefArtifactWriter>) {
        *self.brief_artifact_writer.write().await = Some(writer);
    }

    pub async fn admission_free_space_bytes(&self) -> Result<Option<u64>, String> {
        self.convoy_admission.free_space_bytes().await
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
        self.crew_ops.set_work_credential_reconciler(reconciler).await;
    }

    pub async fn ledger_delivery_environment(&self, namespace: &str, environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        self.crew_ops.ledger_delivery_environment(namespace, environment_ref).await
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
        let checkouts = crate::repository_addressing::local_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
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
                let path = crate::probe::canonicalize(path).await?;
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
    pub fn local_charter_vcs(&self) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
        let runner = self.local_command_runner().ok_or("local charter runner unavailable")?;
        Ok(Arc::new(crate::vcs::FlotillaVcs::new(
            ExecutionEnvironmentPath::new("/"),
            runner.clone(),
            crate::vcs::GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new("unused".into(), runner))),
        )))
    }

    pub async fn local_vcs_for_checkout(&self, checkout: &Path) -> Result<Arc<dyn crate::vcs::Vcs>, String> {
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
    pub fn agentless_ssh_environments(&self) -> Vec<(EnvironmentId, crate::environment_manager::DirectEnvironmentState)> {
        self.environment_manager
            .managed_environments()
            .into_iter()
            .filter_map(|(id, managed)| match managed {
                crate::environment_manager::ManagedEnvironmentKind::Direct(state)
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
        self.checkout_namespace_changes.send_replace(());
    }

    pub async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity {
        let repo_identity = empty_repo_identity();
        self.event_sink.emit(DaemonEvent::CommandStarted {
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
        self.event_sink.emit(DaemonEvent::CommandFinished { command_id, node_id: self.node_id.clone(), repo_identity, repo: None, result });
    }

    pub async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.clone()
    }

    pub async fn set_image_build_input_resolver(&self, resolver: Arc<dyn crate::image_build::ImageBuildInputResolver>) {
        *self.convoy_admission.image_build_inputs.write().await = Some(resolver);
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
        self.crew_ops.subscribe_wait(connection_id, request).await
    }

    pub async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.crew_ops.unsubscribe_waits(connection_id).await;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn flotilla_resources::controller::SecondaryWatch<Primary = flotilla_resources::Convoy>> {
        self.crew_ops.reconciler_wake_watch()
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.crew_ops.change_request_stale_after()
    }

    pub async fn refresh_change_request_hint(&self, hint: &flotilla_relay_protocol::Subject) -> Result<(), String> {
        self.crew_ops.refresh_change_request_hint(hint).await?;
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
        self.crew_ops.refresh_demanded_owned_change_requests().await
    }

    pub fn set_change_request_relay_healthy(&self, healthy: bool) {
        self.crew_ops.set_change_request_relay_healthy(healthy);
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
        let forges = self
            .resource_backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
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
                let change_request_id = convoy_change_request_id_for_checkout(convoy, &checkout, &forges);
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
        handle: EnvironmentHandle,
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
        let repository = match self.repository_key_for_path(repo_path).await {
            Some(key) => self
                .resource_backend
                .including_replicas::<Repository>(&self.provisioning_namespace().await)
                .get(&key.to_string())
                .await
                .ok()
                .map(|source| source.object),
            None => None,
        };
        discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.resource_backend,
            &self.provisioning_namespace().await,
            &self.local_environment_id,
            environment_id,
            repo_path,
            repository.as_ref().map(|repository| &repository.spec),
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
        use flotilla_protocol::qualified_path::HostId;

        // Serialize list-and-project passes so a slower old read cannot replace a
        // newer description published by the watch or a concurrent query.
        let _projection = self.host_registry.description_projection.lock().await;
        let namespace = self.provisioning_namespace().await;
        let statuses = self.read_projections().host_statuses(&namespace).await?;
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
        self.convoy_admission.start_placement_host(namespace, intent).await
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
        let repo_path = canonical_or_original(repo_path);
        self.repos.read().await.values().find(|state| state.contains_path(&repo_path)).map(|state| state.identity().clone())
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        match self.inspect_repository_path(repo_path, None).await {
            Ok(inspection) => repository_operations::repository_event_identity(&inspection.spec, None),
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
        crate::repository_addressing::resolve_repository(
            &self.resource_backend,
            &self.observed_resource_backend,
            &self.provisioning_namespace().await,
            self.environment_manager.local_host_id().as_str(),
            selector,
        )
        .await
    }

    async fn resolve_repo_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        let repository = self.repository_for_selector(selector).await?;
        let key = repository.spec.key();
        self.local_checkout_for_repository(&key).await?.ok_or_else(|| format!("Repository {key} has no available checkout on this host"))
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
        let checkouts = crate::repository_addressing::local_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
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
    pub async fn validate_new_checkout_branch(&self, checkout: &ResourceObject<ResourceCheckout>) -> Result<Option<String>, String> {
        if !self.checkout_has_network_forge(checkout).await? {
            return Ok(None);
        }
        let (candidates, failures) =
            self.convoy_admission.repository_change_request_candidates(std::slice::from_ref(checkout.spec.repo_ref())).await;
        if let Some(error) = failures.into_iter().next() {
            return Err(error);
        }
        for (_, _, provider) in candidates {
            if let Some((id, request)) =
                provider.find_change_request_by_branch_for_admission(checkout.spec.branch()).await.map_err(|error| error.to_string())?
            {
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
        vcs: &dyn crate::vcs::Vcs,
        path: &Path,
    ) -> Result<Option<String>, String> {
        if !self.checkout_has_network_forge(checkout).await? {
            return Ok(None);
        }
        let branch = vcs.read_repository(path, crate::vcs::RepositoryRead::CurrentBranch).await?;
        let mut branches = vec![branch.trim().to_string()];
        if let Ok(upstream) = vcs.read_repository(path, crate::vcs::RepositoryRead::UpstreamOf("@{upstream}")).await {
            let remote = vcs.read_repository(path, crate::vcs::RepositoryRead::TrackedRemote(branch.trim())).await?;
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
    ) -> crate::convoy_branch_refresh::ConvoyBranchRefresh {
        self.convoy_admission.refresh_convoy_branch(repository_keys, branch, binding).await
    }

    pub async fn discover_convoy_branch_subjects_with_resolution(
        &self,
        namespace: &str,
        convoy_name: &str,
        branch: &str,
        resolution: Option<&crate::convoy_branch_refresh::ConvoyBranchRefresh>,
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
            .collect::<BTreeSet<_>>();
        let should_discover = |subject: &flotilla_protocol::Subject| {
            !adopted.contains(subject)
                && !convoy.status.as_ref().is_some_and(|status| status.unlinked_subjects.contains(subject) || status.produces(subject))
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
                        let address = change_request_address_with_forges(&repository.url, &request.id, &forges)?;
                        if let Some(subject) = flotilla_protocol::Subject::from_leaf(&address) {
                            if should_discover(&subject) {
                                subjects.push((subject, flotilla_protocol::Relationship::Produces));
                            }
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
                    let address = change_request_address_with_forges(&repository.url, &request.id, &forges)?;
                    if let Some(subject) = flotilla_protocol::Subject::from_leaf(&address) {
                        if should_discover(&subject) {
                            subjects.push((subject, flotilla_protocol::Relationship::Produces));
                        }
                    }
                }
                Ok(_) => {}
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
        if errors.is_empty()
            && convoy
                .status
                .as_ref()
                .is_some_and(|status| status.branch_subject_scan_at.is_none() || status.branch_subject_scan_error.is_some())
        {
            apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::RecordBranchSubjectScan { at: self.clock.now() })
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(error) = errors.into_iter().next() {
            if convoy.status.as_ref().and_then(|status| status.branch_subject_scan_error.as_deref()) != Some(error.as_str()) {
                apply_resource_status_patch(&convoys, convoy_name, &ConvoyStatusPatch::RecordBranchSubjectScanFailure {
                    error: error.clone(),
                })
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
        self.convoy_admission.resolve_convoy_change_request(repository_keys, branch, change_request_id).await
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
                RepoState::new(identity.clone(), RepoRootState {
                    path: synthetic_path.clone(),
                    model,
                    slug: None,
                    unmet: Vec::new(),
                    is_local: false,
                }),
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

impl InProcessDaemon {
    #[cfg(test)]
    async fn resolve_convoy_admission_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        self.convoy_admission.resolve_convoy_admission_workflow(namespace, project_ref, project, repositories, intent).await
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
        self.convoy_admission.resolve_capability_placement(namespace, project_ref, repositories, workflow, needs, intent).await
    }

    async fn prepare_convoy_admission_with_preferences(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
        repositories: Option<&[RepositoryKey]>,
    ) -> Result<PreparedConvoyAdmission, String> {
        self.convoy_admission.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, repositories).await
    }

    async fn admit_convoy_start(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<(String, String), String> {
        self.convoy_admission.admit_convoy_start(namespace, intent, dispatching_principal_ref).await
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
    #[cfg(test)]
    async fn ensure_admission_dependency_hash(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        ensure_controller_under_test::EnsureReconciler::builder()
            .resource_backend(self.resource_backend.clone())
            .clock(Arc::clone(&self.clock))
            .build()
            .ensure_admission_dependency_hash(self, namespace, ensure)
            .await
    }
    #[cfg(test)]
    async fn start_ensured_convoy(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        ensure_controller_under_test::EnsureReconciler::builder()
            .resource_backend(self.resource_backend.clone())
            .clock(Arc::clone(&self.clock))
            .build()
            .start_ensured_convoy(self, namespace, ensure)
            .await
    }
    async fn prepare_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String> {
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
            .standing_role(ensure.spec.role.clone())
            .maybe_workflow_ref((!ensure.spec.workflow_ref.is_empty()).then(|| ensure.spec.workflow_ref.clone()))
            .maybe_placement_policy(ensure.spec.placement_policy.clone())
            .maybe_escalation_reason(ensure.spec.escalation_reason.clone())
            .agent_overrides(ensure.spec.agent_overrides.clone())
            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
            .build();
        let admission = self
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
        if !ensure.metadata.annotations.contains_key(SOURCE_COMMIT_ANNOTATION) {
            return Err("materialized ensure has no source commit provenance".into());
        }
        Ok((ensure, admission))
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
    async fn reap_convoy_internal(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.crew_ops.reap(namespace, name, force).await
    }

    async fn check_local_free_space_floor(&self) -> Result<(), String> {
        self.convoy_admission.check_local_free_space_floor().await
    }

    pub fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        self.convoy_admission.admission_free_space_floor_bytes()
    }

    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String> {
        self.convoy_admission.check_remote_placement_free_space_floor(namespace, placement).await
    }

    #[cfg(test)]
    async fn write_admission_briefs(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<bool, String> {
        self.convoy_admission.write_admission_briefs(namespace, name, spec, workflow).await
    }

    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        self.convoy_admission.emit_attach_regard(binding, surface_id).await
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
        self.convoy_admission
            .resolve_convoy_placement(namespace, project_ref, repositories, workflow, placement_policy, allow_unready)
            .await
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
        self.convoy_admission.clear_pending(&key).await;
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
            repository_index: project_ops::RepositoryIndex {
                backend: &self.resource_backend,
                observed: &self.observed_resource_backend,
                namespace: &self.provisioning_namespace,
                host: self.environment_manager.local_host_id().as_str(),
            },
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

    pub async fn reconcile_bound_project_charters(&self) -> Result<(), String> {
        self.project_service().reconcile_bound_charters().await
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
            crate::observed_resources::delete_observed_checkout_at_path(
                &self.observed_resource_backend,
                &namespace,
                previous,
                &inspection.checkout.path,
            )
            .await
            .map_err(|error| error.to_string())?;
        }

        let other_tracked_keys = crate::repository_addressing::local_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
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
                self.event_sink.emit(DaemonEvent::RepoDelta(Box::new(RepoDelta {
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
                let canonical_root = crate::probe::canonicalize(&repo_root_raw).await.unwrap_or(repo_root_raw);
                let canonical_path = crate::probe::canonicalize(path).await.unwrap_or_else(|_| path.to_path_buf());
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
        let identity = repository_operations::repository_event_identity(&repository_inspection.spec, None);
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
                if let Err(error) =
                    crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, &repository_key).await
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
            backend: &self.resource_backend,
            config: &self.config,
            host_registry: &self.host_registry,
            environment_manager: &self.environment_manager,
            host_name: &self.host_name,
            node_id: &self.node_id,
            clock: &self.clock,
            leaf_subscriptions: self.crew_ops.subscription_diagnostics(),
            fleet: &self.fleet,
        }
    }

    pub async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        self.refresh_resource_host_summaries().await?;
        self.read_projections().list_hosts(&self.local_host_counts().await).await
    }

    pub async fn dispatch_board_internal(&self, project_filter: Option<&str>) -> Result<flotilla_protocol::DispatchBoardResponse, String> {
        let readiness = self.dispatch_queue_internal(project_filter).await?;
        let repositories = self.dispatch_board_repositories_internal(project_filter).await?;
        Ok(flotilla_protocol::DispatchBoardResponse { readiness, repositories })
    }

    /// Schedule tracker observations without waiting for the forge or coupling
    /// board freshness to the availability of dispatch readiness evidence.
    pub(crate) async fn service_change_request_demand(
        &self,
        namespace: &str,
        spec: &flotilla_resources::ForgeReadSpec,
    ) -> Result<(), String> {
        use flotilla_resources::ForgeReadRequest;
        let repository = RepositorySpec::remote(format!("{}/{}", spec.source.service.trim_end_matches('/'), spec.source.scope))?;
        let provider = discover_repository_change_request_with(
            &self.resource_backend,
            &self.config,
            &self.discovery,
            &self.environment_manager,
            &self.local_environment_id,
            namespace,
            &repository,
        )
        .await?;
        let provider = provider.for_background_refresh().unwrap_or(provider);
        match &spec.request {
            ForgeReadRequest::Branch { branch } => {
                provider.find_change_request_by_branch(branch).await.map(|_| ()).map_err(|e| e.to_string())
            }
            ForgeReadRequest::ChangeRequests { limit } => provider.list_change_requests(*limit).await.map(|_| ()),
            ForgeReadRequest::ChangeRequest { id } => provider.get_change_request(id).await.map(|_| ()),
            ForgeReadRequest::MergedBranches { limit } => provider.list_merged_branch_names(*limit).await.map(|_| ()),
            _ => Ok(()),
        }
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

    pub async fn dispatch_board_repositories_internal(
        &self,
        project_filter: Option<&str>,
    ) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String> {
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let mut sources = std::collections::BTreeSet::new();
        let mut mission_issues = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        for project in projects {
            if let Some(policy) = &project.spec.dispatch_policy {
                mission_issues.extend(policy.missions.iter().filter_map(|mission| mission.issue.clone()));
            }
            if project_filter.is_some_and(|name| name != project.metadata.name) {
                continue;
            }
            let scope = flotilla_protocol::QueryScope::new(&project.metadata.namespace, &project.metadata.name);
            match self.resolve_issue_source_bindings(&scope).await {
                Ok(bindings) => sources.extend(bindings.into_iter().map(|binding| binding.source)),
                Err(error) => errors.push(error),
            }
        }
        if project_filter.is_none() && errors.is_empty() {
            self.dispatch_board_cache.retain_sources(&sources).await;
        }
        let mut repositories = Vec::new();
        for source in sources {
            let daemon = self.self_weak.clone();
            let tracker_source = source.clone();
            let mission_issues = mission_issues.clone();
            match self
                .dispatch_board_cache
                .read(&source, move || async move {
                    let daemon = daemon.upgrade().ok_or("daemon stopped")?;
                    let provider = daemon.issue_provider_for_source(&tracker_source).await?;
                    let mut board = provider.dispatch_board(&tracker_source).await?;
                    for issue in &mut board.issues {
                        let reference = flotilla_protocol::IssueRef { source: tracker_source.clone(), id: issue.id.clone() };
                        if mission_issues.iter().any(|mission| {
                            mission.id == reference.id
                                && mission.source.scope.eq_ignore_ascii_case(&reference.source.scope)
                                && matches!(mission.source.service.trim_end_matches('/'), "github" | "github.com" | "https://github.com")
                        }) || issue.issue_type.as_ref().is_some_and(|kind| kind.eq_ignore_ascii_case("map"))
                            || issue
                                .labels
                                .iter()
                                .any(|label| label.rsplit(':').next().is_some_and(|kind| kind.eq_ignore_ascii_case("map")))
                        {
                            issue.mission_fields = provider.mission_fields(&reference).await?;
                        }
                    }
                    Ok(board)
                })
                .await
            {
                Ok(board) => repositories.push(board),
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(repositories)
        } else {
            Err(errors.join("; "))
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

    pub fn forge_budget_rows(&self) -> Vec<flotilla_protocol::ForgeBudgetRow> {
        self.forge_budgets.rows(self.host_name().as_str())
    }

    pub async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String> {
        let now = Utc::now();
        let namespace = self.provisioning_namespace().await;
        let host_list = self.list_hosts_internal().await?;
        let rows = self.fleet.rows(&namespace, &self.host_registry).await?;
        let mut response =
            self.read_projections().fleet_health(&namespace, host_list, rows, self.local_host_id().map(|id| id.to_string()), now).await?;
        for host in self.resource_backend.including_replicas::<ResourceHost>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            if let Some(value) = host.object.status.and_then(|status| status.capabilities.get("forge_budgets").cloned()) {
                if let Ok(rows) = serde_json::from_value::<Vec<flotilla_protocol::ForgeBudgetRow>>(value) {
                    response.forge_budgets.extend(rows);
                }
            }
        }
        response.forge_budgets.retain(|row| row.host != self.host_name().as_str());
        response.forge_budgets.extend(self.forge_budget_rows());
        Ok(response)
    }

    /// Raw bound fleet manifests at the current source head, for candidate-side
    /// validation. Returning raw text lets the candidate use its own decoder.
    pub async fn charter_input_inventory(&self, namespace: &str) -> Result<Vec<crate::ops_entry::OperationalEntryFile>, String> {
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
            let snapshot = crate::charter_store::read_charter_source(&source, Path::new(&root.spec.path), Some(&*vcs)).await?;
            files.extend(
                snapshot
                    .files
                    .into_iter()
                    .filter(|(path, _)| {
                        Path::new(path).extension().and_then(|ext| ext.to_str()).is_some_and(|ext| matches!(ext, "yaml" | "yml" | "json"))
                    })
                    .map(|(path, contents)| crate::ops_entry::OperationalEntryFile {
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
    ) -> Result<crate::repository_inspection::OperationalEntryInventory, String> {
        let projects = self.resource_backend.definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        if !projects.iter().any(|project| {
            project.spec.repositories.iter().any(|member| member.roles.contains(&flotilla_resources::ProjectRepositoryRole::Ops))
        }) {
            return Ok(Default::default());
        }
        let mut paths = BTreeMap::<RepositoryKey, Vec<PathBuf>>::new();
        for checkout in crate::repository_addressing::local_checkouts(
            &self.resource_backend,
            &self.observed_resource_backend,
            namespace,
            self.environment_manager.local_host_id().as_str(),
        )
        .await?
        {
            let key = checkout.spec.repo_ref().clone();
            if let Some(path) = checkout_path(&checkout) {
                paths.entry(key).or_default().push(PathBuf::from(path));
            }
        }
        let inspector = self.repository_inspector().await?;
        crate::repository_inspection::inspect_project_ops_entries(&projects, &paths, &*inspector).await
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
                    items.insert((String::new(), reference.clone()), CliListRow {
                        repo: Some(name.clone()),
                        reference,
                        name,
                        status: "declared".into(),
                        provider: None,
                    });
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
                for checkout in durable.items.into_iter().map(|source| source.object).chain(observed.items) {
                    if checkout
                        .status
                        .as_ref()
                        .is_some_and(|status| matches!(status.phase, ResourceCheckoutPhase::Terminating | ResourceCheckoutPhase::Gone))
                    {
                        continue;
                    }
                    let reference = checkout.metadata.name;
                    items.entry((String::new(), reference.clone())).or_insert(CliListRow {
                        repo: Some(checkout.spec.repo_ref().to_string()),
                        reference,
                        name: checkout.spec.branch().to_string(),
                        status: checkout.status.map_or_else(
                            || "observed".to_string(),
                            |status| {
                                match status.phase {
                                    ResourceCheckoutPhase::Pending => "pending",
                                    ResourceCheckoutPhase::Preparing => "preparing",
                                    ResourceCheckoutPhase::Ready => "ready",
                                    ResourceCheckoutPhase::Terminating => "terminating",
                                    ResourceCheckoutPhase::Failed => "failed",
                                    ResourceCheckoutPhase::Gone => "gone",
                                }
                                .to_string()
                            },
                        ),
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
                    items.insert((String::new(), reference.clone()), CliListRow {
                        repo: Some(change_request.spec.scope),
                        reference,
                        name: status.title.value.unwrap_or_else(|| format!("#{}", change_request.spec.number)),
                        status: state.into(),
                        provider: Some(change_request.spec.service),
                    });
                }
            }
            CliListKind::Agent | CliListKind::Workspace => return self.list_provider_cli_items(kind).await,
        }

        Ok(CliListResponse { list_kind: kind, items: items.into_values().collect() })
    }

    async fn list_provider_cli_items(&self, kind: CliListKind) -> Result<CliListResponse, String> {
        let mut items = BTreeMap::new();
        let mut repositories =
            self.repos.read().await.values().map(|state| (state.identity().path.clone(), state.registry())).collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.0.cmp(&right.0));
        let mut seen_workspace_providers = BTreeSet::new();
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
            } else {
                for (descriptor, provider) in registry.presentation_managers.iter() {
                    let provider_name = descriptor.display_name.clone();
                    if seen_workspace_providers.contains(&provider_name) {
                        continue;
                    }
                    let workspaces = match provider.list_workspaces().await {
                        Ok(workspaces) => workspaces,
                        Err(error) => {
                            warn!(provider = %provider_name, %error, "failed to list workspaces");
                            continue;
                        }
                    };
                    seen_workspace_providers.insert(provider_name.clone());
                    for (reference, workspace) in workspaces {
                        let provider_name = descriptor.display_name.clone();
                        items.entry((provider_name.clone(), reference.clone())).or_insert(CliListRow {
                            repo: None,
                            reference,
                            name: workspace.name,
                            status: "active".to_string(),
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
        let rows = self.fleet.rows(&namespace, &self.host_registry).await?;
        self.read_projections().fleet_list(&namespace, rows, Utc::now()).await
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

    pub async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        self.crew_ops.resolve_crew_routing_context(requested).await
    }

    pub async fn mark_crew_completion_pending(
        &self,
        namespace: &str,
        session_name: &str,
        pending: CrewCompletionPending,
    ) -> Result<(), String> {
        self.crew_ops.mark_crew_completion_pending(namespace, session_name, pending).await
    }

    pub async fn clear_crew_completion_pending(&self, namespace: &str, session_name: &str) -> Result<(), String> {
        self.crew_ops.clear_crew_completion_pending(namespace, session_name).await
    }

    pub async fn pending_crew_completions(&self) -> Result<Vec<(String, CrewCompletionPending, CrewCommandContext)>, String> {
        self.crew_ops.pending_crew_completions().await
    }

    pub async fn set_session_capability_source(&self, source: Arc<dyn crate::crew_capabilities::SessionCapabilitySource>) {
        *self.crew_ops.capability_source.write().await = Some(source);
    }

    pub async fn refresh_capability_cards(&self, namespace: &str) -> Result<(), String> {
        let source = self.crew_ops.capability_source.read().await.clone().ok_or("session capability source unavailable")?;
        crate::crew_capabilities::refresh_cards(&self.resource_backend, namespace, &*source).await
    }

    pub async fn crew_capabilities_internal(&self, requested: &CrewCommandContext) -> Result<String, String> {
        self.crew_ops.crew_capabilities_internal(requested).await
    }

    pub async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        self.crew_ops.crew_list_internal(requested).await
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
        self.crew_ops.complete(requested, message, disposition, decision_ledger_ref, force, principal).await
    }

    pub async fn crew_fail_internal(
        &self,
        requested: &CrewCommandContext,
        message: String,
        force: bool,
        principal: Option<&PrincipalRef>,
    ) -> Result<(), String> {
        self.crew_ops.fail(requested, message, force, principal).await
    }

    pub async fn crew_stall_internal(
        &self,
        requested: &CrewCommandContext,
        reason: flotilla_protocol::StallReason,
        proposed_disposition: Option<flotilla_protocol::StallProposedDisposition>,
        message: String,
    ) -> Result<(), String> {
        self.crew_ops.stall(requested, reason, proposed_disposition, message).await
    }

    async fn crew_supervise_internal(&self, request: CrewSupervisionRequest<'_>) -> Result<(), String> {
        self.crew_ops.supervise(request).await
    }

    async fn runner_for_resource_checkout(&self, _checkout: &ResourceObject<ResourceCheckout>) -> Result<Arc<dyn CommandRunner>, String> {
        self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())
    }

    pub async fn verify_convoy_teardown_gate(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.crew_ops.teardown(namespace, name, force).await
    }

    pub async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[ResourceObject<ResourceCheckout>],
        force: bool,
    ) -> Result<(), String> {
        self.crew_ops.verify_convoy_teardown_gate_for_checkouts(convoy, checkout_list, force).await
    }

    async fn abandon_convoy_internal(
        &self,
        namespace: &str,
        name: &str,
        reason: &str,
        principal_ref: Option<&PrincipalRef>,
    ) -> Result<Vec<CheckoutArchiveOutcome>, String> {
        self.crew_ops.abandon(namespace, name, reason, principal_ref).await
    }

    #[cfg(test)]
    async fn abandon_convoy_internal_with_hook<F, Fut>(
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
        self.crew_ops.abandon_convoy_internal_with_hook(namespace, name, reason, principal_ref, before_update).await
    }

    async fn record_lifecycle_mutation_best_effort(
        &self,
        namespace: &str,
        name: &str,
        action: &str,
        caller: Option<&flotilla_protocol::CommandCaller>,
        missing_expected: bool,
    ) {
        self.crew_ops.record_lifecycle_mutation_best_effort(namespace, name, action, caller, missing_expected).await
    }

    pub async fn crew_handoff_internal(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        self.crew_ops.handoff(requested, target, message).await
    }

    pub async fn convoy_resume_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
    ) -> Result<ConvoyResumeOutcome, String> {
        self.crew_ops.resume(namespace, name, prompt, requested_vessel, requested_role).await
    }

    pub async fn convoy_withdraw_pending_brief_internal(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        self.crew_ops.withdraw_pending_brief(namespace, name).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn reconcile_crew_stalls_once(&self, namespace: &str) -> Result<(), String> {
        self.crew_ops.reconcile_crew_stalls_once(namespace).await
    }

    pub fn set_resource_intent_publisher(&self, publisher: Weak<dyn crate::leaf_engine::ResourceIntentPublisher>) {
        self.crew_ops.set_resource_intent_publisher(publisher);
    }

    pub async fn deliver_standing_turn(&self, request: &crate::leaf_engine::CrewTurnIntent) -> Result<TurnDeliveryRung, String> {
        self.crew_ops.deliver_turn(request).await.map(|admission| admission.rung)
    }

    pub async fn reconcile_pending_supervisor_turns_once(&self, namespace: &str) -> Result<(), String> {
        self.crew_ops.reconcile_pending_supervisor_turns_once(namespace).await
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
            CommandAction::MessageFailBatch { namespace, name, .. } => Some((namespace.as_str(), "Message", name.as_str())),
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

    async fn message_creation_origin(&self, namespace: &str, document: &serde_json::Value) -> Result<Option<NodeId>, String> {
        let namespace = document.pointer("/metadata/namespace").and_then(serde_json::Value::as_str).unwrap_or(namespace);
        let spec = flotilla_resources::qualify_message_spec(
            serde_json::from_value(document.get("spec").cloned().unwrap_or_default()).map_err(|error| format!("Message spec: {error}"))?,
        )
        .map_err(|error| error.to_string())?;
        let receiver = spec.receiver.as_str();
        if receiver.starts_with("system:") {
            let original_id = spec.in_reply_to.as_deref().ok_or("a system receiver requires a correlated reply")?;
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
        if let Some(holder) = flotilla_resources::resolve_message_receiver(&self.resource_backend, namespace, receiver)
            .await
            .map_err(|error| error.to_string())?
        {
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
            if *project != "fleet" {
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
                    let home = match &self.resource_backend {
                        ResourceBackend::Http(_) => home,
                        _ => {
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
                        }
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

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
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
        let summary = crate::host_summary::build_local_host_summary(
            &self.node_id,
            &self.host_name,
            EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            &self.environment_manager,
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

    async fn execute_action_artifact_reserve_ledger_comment(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::ArtifactReserveLedgerComment { namespace, name, address } = &command.action else {
            return Err("ledger reservation selected the wrong handler".into());
        };
        let identity = self.start_context_free_command(id, command.description().to_string());
        let result = match flotilla_resources::reserve_ledger_comment_creation(&self.resource_backend, namespace, name, address).await {
            Ok(granted) => CommandValue::LedgerCommentCreationReserved { granted },
            Err(error) => CommandValue::Error { message: error.to_string() },
        };
        self.finish_context_free_command(id, identity, result);
        Ok(id)
    }

    pub async fn message_inbox(&self, namespace: &str) -> flotilla_resources::MessageInbox {
        self.message_inboxes
            .lock()
            .await
            .entry(namespace.to_string())
            .or_insert_with(|| {
                let (change_request, issue) = self.crew_ops.message_observation_staleness();
                flotilla_resources::MessageInbox::new(self.resource_backend.clone(), namespace)
                    .with_observation_staleness(change_request, issue)
                    .with_audit_retention_days(self.config.load_daemon_config().unwrap_or_default().message_audit_retention_days)
            })
            .clone()
    }

    /// Returns the canonical admitted record. Suppression before creation does
    /// not create the successor ID. A recovered partial creation stays as a
    /// superseded audit record and durably forwards admission to its predecessor.
    async fn apply_intent_document(
        &self,
        namespace: &str,
        document: serde_json::Value,
    ) -> Result<flotilla_resources::DynamicResourceObject, ResourceError> {
        use flotilla_resources::{get_resource_kind, MessageAdmission, MessageSpec};
        if document.get("kind").and_then(serde_json::Value::as_str) != Some("Message") {
            return apply_resource_document(&self.resource_backend, namespace, document).await;
        }
        flotilla_resources::validate_resource_document(&document)?;
        let namespace = document.pointer("/metadata/namespace").and_then(serde_json::Value::as_str).unwrap_or(namespace);
        let meta: InputMeta = serde_json::from_value(document.get("metadata").cloned().unwrap_or_default())
            .map_err(|error| ResourceError::decode(format!("message metadata: {error}")))?;
        let spec: MessageSpec = serde_json::from_value(document.get("spec").cloned().unwrap_or_default())
            .map_err(|error| ResourceError::decode(format!("message spec: {error}")))?;
        let spec = flotilla_resources::qualify_message_spec(spec)?;
        let admission = self.message_inbox(namespace).await.accept(&meta, &spec, self.clock.now()).await?;
        let record = match admission {
            MessageAdmission::Accepted(record) => record,
            MessageAdmission::Suppressed { predecessor } => predecessor,
        };
        get_resource_kind(&self.resource_backend, namespace, "Message", &record.metadata.name).await
    }

    async fn execute_action_resource_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ResourceApply { namespace, document } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            // Artifact reservations and Message admission can race status writers.
            // Both mutations are replay-safe; retain a bounded conflict budget.
            let kind = document.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
            let applied = retry_resource_apply(kind, || self.apply_intent_document(namespace, document.clone())).await;
            let result = match applied {
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
        Err("ResourceApply action selected the wrong handler".to_string())
    }

    async fn execute_action_repository_remote_remove(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("RepositoryRemoteRemove action selected the wrong handler".to_string())
    }

    async fn execute_action_resource_manifest_resolve(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ResourceManifestResolve action selected the wrong handler".to_string())
    }

    async fn execute_action_ensure_roll(&self, id: u64, command: &Command) -> Result<u64, String> {
        let CommandAction::ConvoyEnsureRoll { namespace, name } = &command.action else {
            return Err("ensure roll selected the wrong handler".into());
        };
        let identity = self.start_context_free_command(id, command.description().to_string());
        let result = match self.roll_convoy_ensure(namespace, name).await {
            Ok(message) => CommandValue::ResourceReconciled { resource_kind: "ConvoyEnsure".into(), name: name.clone(), message },
            Err(message) => CommandValue::Error { message },
        };
        self.finish_context_free_command(id, identity, result);
        Ok(id)
    }

    async fn execute_action_resource_reconcile_now(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ResourceReconcileNow action selected the wrong handler".to_string())
    }

    async fn execute_action_resource_status_patch(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ResourceStatusPatch { namespace, kind, name, status, expected_resource_version } =
            &command.action
        {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let patched = match expected_resource_version {
                Some(expected) => {
                    flotilla_resources::patch_resource_status_if_version(
                        &self.resource_backend,
                        namespace,
                        kind,
                        name,
                        status.clone(),
                        expected,
                    )
                    .await
                }
                None => flotilla_resources::patch_resource_status(&self.resource_backend, namespace, kind, name, status.clone()).await,
            };
            let result = match patched {
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
        Err("ResourceStatusPatch action selected the wrong handler".to_string())
    }

    async fn execute_action_resource_delete(&self, id: u64, command: &Command) -> Result<u64, String> {
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
                // Serialize deletion and cleanup with adopted checkout writes.
                let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                let deleted = async {
                    let deleted = flotilla_resources::delete_resource_kind(&self.resource_backend, namespace, kind, name).await?;
                    if deleted.object.kind == ResourceCheckout::API_PATHS.kind {
                        crate::observed_resources::delete_stale_adopted_checkouts(
                            &self.resource_backend,
                            &self.observed_resource_backend,
                            namespace,
                        )
                        .await?;
                    }
                    Ok::<_, ResourceError>(deleted)
                }
                .await;
                match deleted {
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
        Err("ResourceDelete action selected the wrong handler".to_string())
    }

    async fn execute_action_resource_watch(&self, id: u64, command: &Command, command_node_id: &NodeId) -> Result<u64, String> {
        let command_node_id = command_node_id.clone();
        if let flotilla_protocol::CommandAction::ResourceWatch { namespace, kind, name, include_replicas, replica_sources, cursor } =
            command.action.clone()
        {
            let repo_identity = empty_repo_identity();
            let description = format!("watch resource {namespace}/{kind}");
            let token = CancellationToken::new();
            {
                let mut guard = self.active_commands.lock().await;
                guard.insert(id, token.clone());
            }
            self.event_sink.emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: command_node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });

            let (backend, kind) = match kind.strip_prefix("observed/") {
                Some(kind) => (self.observed_resource_backend.clone(), kind.to_string()),
                None => (self.resource_backend.clone(), kind),
            };
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
                        .event_sink(event_sink.clone())
                        .token(token)
                        .build(),
                )
                .await;
                active_ref.lock().await.remove(&id);
                event_sink.emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: command_node_id,
                    repo_identity,
                    repo: None,
                    result,
                });
            });
            return Ok(id);
        }
        Err("ResourceWatch action selected the wrong handler".to_string())
    }

    async fn execute_action_refresh_all(&self, id: u64, command: &Command) -> Result<u64, String> {
        if matches!(command.action, flotilla_protocol::CommandAction::Refresh { repo: None }) {
            let repositories = self
                .resource_backend
                .including_replicas::<Repository>(&self.provisioning_namespace().await)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items;
            let repo_identity = empty_repo_identity();
            let description = command.description().to_string();
            self.event_sink.emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });
            let mut refreshed = Vec::new();
            let mut identity_changes = Vec::new();
            let result = match async {
                for repository in &repositories {
                    let key = repository.object.spec.key();
                    if let Some(change) = self.refresh(&flotilla_protocol::RepoSelector::Repository(key.clone())).await? {
                        identity_changes.push(change);
                    }
                    if let Some(path) = self.local_checkout_for_repository(&key).await? {
                        refreshed.push(path);
                    }
                }
                Ok::<(), String>(())
            }
            .await
            {
                Ok(()) => {
                    flotilla_protocol::CommandValue::Refreshed { repos: refreshed, repository_count: repositories.len(), identity_changes }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("Refresh action selected the wrong handler".to_string())
    }

    async fn execute_action_crew_handoff(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::CrewHandoff { context, target, message, carries } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match Box::pin(self.crew_ops.handoff_with_carries(context, target, message, carries.clone())).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewHandoff action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_resume(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::ConvoyResume { namespace, name, prompt, vessel, role } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match Box::pin(self.crew_ops.convoy_resume_with_sender_internal(
                        &namespace,
                        &record_name,
                        prompt,
                        vessel.as_deref(),
                        role.as_deref(),
                        crew_ops::MessageAttribution::operator(dispatching_principal_ref.as_ref()),
                    ))
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
        Err("ConvoyResume action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_withdraw_pending_brief(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ConvoyWithdrawPendingBrief action selected the wrong handler".to_string())
    }

    async fn execute_action_crew_complete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
        if let flotilla_protocol::CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref, force } =
            &command.action
        {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let routing = Box::pin(self.resolve_crew_routing_context(context)).await.ok();
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
        Err("CrewComplete action selected the wrong handler".to_string())
    }

    async fn execute_action_crew_fail(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        if let flotilla_protocol::CommandAction::CrewFail { context, message, force } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let operator =
                caller.as_ref().filter(|caller| caller.crew.is_none() && context.crew_id.is_none()).map(|caller| &caller.principal_ref);
            let result = match self.crew_fail_internal(context, message.clone(), *force, operator).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewFail action selected the wrong handler".to_string())
    }

    async fn execute_action_crew_stall(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::CrewStall { context, reason, proposed_disposition, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_stall_internal(context, *reason, *proposed_disposition, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }
        Err("CrewStall action selected the wrong handler".to_string())
    }

    async fn execute_action_crew_supervise(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
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
        Err("CrewSupervise action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_link(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ConvoyLink action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_unlink(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ConvoyUnlink action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_delete(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
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
        Err("ConvoyDelete action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_abandon(
        &self,
        id: u64,
        command: &Command,
        caller: &Option<flotilla_protocol::CommandCaller>,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let caller = caller.clone();
        let dispatching_principal_ref = dispatching_principal_ref.clone();
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
        Err("ConvoyAbandon action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_work_force_complete(&self, id: u64, command: &Command) -> Result<u64, String> {
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
        Err("ConvoyWorkForceComplete action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_start(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = dispatching_principal_ref.clone();
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
            if !self.convoy_admission.mark_pending(key.clone()).await {
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
                self.convoy_admission.clear_pending(&key).await;
                self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error {
                    message: "convoy start worker is unavailable".to_string(),
                });
            }
            return Ok(id);
        }
        Err("ConvoyStart action selected the wrong handler".to_string())
    }

    async fn execute_action_convoy_create(
        &self,
        id: u64,
        command: &Command,
        dispatching_principal_ref: &Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let dispatching_principal_ref = dispatching_principal_ref.clone();
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
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
                self.event_sink.emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            if let Err(message) = self.check_local_free_space_floor().await {
                let result = flotilla_protocol::CommandValue::Error { message };
                self.event_sink.emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            // Use the admission transaction before checking identity or writing
            // adopted checkout resources. A duplicate must have no side effects.
            let admission_guard = self.convoy_admission.lock().await;
            if let Err(message) = allocate_convoy_generation(&self.resource_backend, &namespace, project_identity, &role).await {
                let result = flotilla_protocol::CommandValue::Error { message };
                self.event_sink.emit(DaemonEvent::CommandFinished {
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
                    self.event_sink.emit(DaemonEvent::CommandFinished {
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
                        self.event_sink.emit(DaemonEvent::CommandFinished {
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
                self.event_sink.emit(DaemonEvent::CommandFinished {
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
                            self.event_sink.emit(DaemonEvent::CommandFinished {
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
                    let canonical_url = self.project_service().repository_transport_url(&namespace, &repository_spec).await?;
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
                        self.event_sink.emit(DaemonEvent::CommandFinished {
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
                    self.event_sink.emit(DaemonEvent::CommandFinished {
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
                    self.event_sink.emit(DaemonEvent::CommandFinished {
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
                self.event_sink.emit(DaemonEvent::CommandFinished {
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
                        refused_candidates: placement.refused_candidates.clone(),
                        viable_not_selected: placement.viable_not_selected.clone(),
                        allocation: placement.allocation.clone(),
                    }),
                    Err(message) => {
                        self.event_sink.emit(DaemonEvent::CommandFinished {
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
                self.event_sink.emit(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let result = self
                .convoy_admission
                .admit_created_convoy(
                    ConvoyCreateAdmission::builder()
                        .namespace(&namespace)
                        .name(name)
                        .role(&role)
                        .workflow_ref(workflow_ref)
                        .workflow(&workflow.spec)
                        .placement(placement)
                        .maybe_placement_decision(placement_decision)
                        .inputs(inputs)
                        .repositories(repositories)
                        .maybe_source_ref(r#ref)
                        .maybe_project_ref(project_ref.clone())
                        .adopted_checkout_refs(adopted_checkout_refs)
                        .maybe_adopted_checkout_ref_to_cleanup(adopted_checkout_ref_to_cleanup)
                        .maybe_dispatching_principal_ref(dispatching_principal_ref)
                        .build(),
                    admission_guard,
                )
                .await;
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ConvoyCreate action selected the wrong handler".to_string())
    }

    async fn execute_action_workflow_template_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::WorkflowTemplateApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("WorkflowTemplateApply action selected the wrong handler".to_string())
    }

    async fn execute_action_project_add(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectAdd { target, name, display_name, remote } = &command.action {
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectAdd action selected the wrong handler".to_string())
    }

    async fn execute_action_project_apply(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectApply action selected the wrong handler".to_string())
    }

    async fn execute_action_project_register(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectRegister { target } = &command.action {
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectRegister action selected the wrong handler".to_string())
    }

    async fn execute_action_project_refresh(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::ProjectRefresh { name } = &command.action {
            let empty_identity = empty_repo_identity();
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }
        Err("ProjectRefresh action selected the wrong handler".to_string())
    }

    async fn execute_action_track_repo_path(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::TrackRepoPath { path } = &command.action {
            let description = command.description().to_string();
            let repo_path = path.clone();
            let repo_identity = self.detect_repo_identity(path).await;
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: self.tracked_repo_identity_for_path(path).await.unwrap_or(repo_identity),
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }
        Err("TrackRepoPath action selected the wrong handler".to_string())
    }

    async fn execute_action_untrack_repo(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::UntrackRepo { repo } = &command.action {
            let repo_path = match self.resolve_repo_selector(repo).await {
                Ok(path) => path,
                Err(tracked_error) => self.resolve_observation_root_selector(repo).map_err(|_| tracked_error)?,
            };
            let description = command.description().to_string();
            let repo_identity = self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
            self.event_sink.emit(DaemonEvent::CommandStarted {
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
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }
        Err("UntrackRepo action selected the wrong handler".to_string())
    }

    async fn execute_action_refresh_repo(&self, id: u64, command: &Command) -> Result<u64, String> {
        if let flotilla_protocol::CommandAction::Refresh { repo: Some(selector) } = &command.action {
            let repository = self.repository_for_selector(selector).await?;
            let repo_path = self.local_checkout_for_repository(&repository.spec.key()).await?;
            let description = command.description().to_string();
            let repo_identity = repository_operations::repository_event_identity(&repository.spec, repo_path.as_deref());
            self.event_sink.emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: repo_path.clone(),
                description,
            });
            let result = match self.refresh(selector).await {
                Ok(identity_change) => flotilla_protocol::CommandValue::Refreshed {
                    repository_count: 1,
                    repos: repo_path.clone().into_iter().collect(),
                    identity_changes: identity_change.into_iter().collect(),
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.event_sink.emit(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: repo_path,
                result,
            });
            return Ok(id);
        }
        Err("Refresh action selected the wrong handler".to_string())
    }

    // This executor has many async arms. Box substantial nested futures below
    // so their combined state fits on the default Tokio test-thread stack.
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
            self.event_sink.emit(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = flotilla_protocol::CommandValue::Error { message: "query commands should use execute_query, not execute".into() };
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
            ($future:expr) => {
                Box::pin(async { Box::pin($future).await }).await
            };
        }

        if let CommandAction::FleetPostInstall { cleat_bin, generation, diagnostics_dir } = &command.action {
            let identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.post_install_cleat(cleat_bin, generation, diagnostics_dir).await {
                Ok(report) => CommandValue::FleetPostInstall {
                    failed: report.failed(),
                    report: serde_json::to_value(report).map_err(|error| error.to_string())?,
                },
                Err(message) => CommandValue::Error { message },
            };
            self.finish_context_free_command(id, identity, result);
            return Ok(id);
        }

        match &command.action {
            CommandAction::ArtifactReserveLedgerComment { .. } => {
                return boxed_action!(self.execute_action_artifact_reserve_ledger_comment(id, &command))
            }
            flotilla_protocol::CommandAction::ResourceApply { .. } => {
                return boxed_action!(self.execute_action_resource_apply(id, &command))
            }
            flotilla_protocol::CommandAction::RepositoryRemoteRemove { .. } => {
                return boxed_action!(self.execute_action_repository_remote_remove(id, &command))
            }
            flotilla_protocol::CommandAction::ResourceManifestResolve { .. } => {
                return boxed_action!(self.execute_action_resource_manifest_resolve(id, &command))
            }
            flotilla_protocol::CommandAction::ConvoyEnsureRoll { .. } => {
                return boxed_action!(self.execute_action_ensure_roll(id, &command))
            }
            flotilla_protocol::CommandAction::ResourceReconcileNow { .. } => {
                return boxed_action!(self.execute_action_resource_reconcile_now(id, &command))
            }
            flotilla_protocol::CommandAction::MessageFailBatch { namespace, name, reason } => {
                let empty_identity = self.start_context_free_command(id, command.description().to_string());
                let result = match self.message_inbox(namespace).await.fail_batch(name, reason, self.clock.now()).await {
                    Ok(()) => CommandValue::Ok,
                    Err(error) => CommandValue::Error { message: error.to_string() },
                };
                self.finish_context_free_command(id, empty_identity, result);
                return Ok(id);
            }
            flotilla_protocol::CommandAction::ResourceStatusPatch { .. } => {
                return boxed_action!(self.execute_action_resource_status_patch(id, &command))
            }
            flotilla_protocol::CommandAction::ResourceDelete { .. } => {
                return boxed_action!(self.execute_action_resource_delete(id, &command))
            }
            flotilla_protocol::CommandAction::ResourceWatch { .. } => {
                return boxed_action!(self.execute_action_resource_watch(id, &command, &command_node_id))
            }
            flotilla_protocol::CommandAction::Refresh { repo: None } => return boxed_action!(self.execute_action_refresh_all(id, &command)),
            flotilla_protocol::CommandAction::CrewHandoff { .. } => return boxed_action!(self.execute_action_crew_handoff(id, &command)),
            flotilla_protocol::CommandAction::ConvoyResume { .. } => {
                return boxed_action!(self.execute_action_convoy_resume(id, &command, &caller, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { .. } => {
                return boxed_action!(self.execute_action_convoy_withdraw_pending_brief(id, &command))
            }
            flotilla_protocol::CommandAction::CrewComplete { .. } => {
                return boxed_action!(self.execute_action_crew_complete(id, &command, &caller, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::CrewFail { .. } => return boxed_action!(self.execute_action_crew_fail(id, &command, &caller)),
            flotilla_protocol::CommandAction::CrewStall { .. } => return boxed_action!(self.execute_action_crew_stall(id, &command)),
            flotilla_protocol::CommandAction::CrewSupervise { .. } => {
                return boxed_action!(self.execute_action_crew_supervise(id, &command, &caller, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::ConvoyLink { .. } => return boxed_action!(self.execute_action_convoy_link(id, &command)),
            flotilla_protocol::CommandAction::ConvoyUnlink { .. } => return boxed_action!(self.execute_action_convoy_unlink(id, &command)),
            flotilla_protocol::CommandAction::ConvoyDelete { .. } => {
                return boxed_action!(self.execute_action_convoy_delete(id, &command, &caller))
            }
            flotilla_protocol::CommandAction::ConvoyAbandon { .. } => {
                return boxed_action!(self.execute_action_convoy_abandon(id, &command, &caller, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::ConvoyWorkForceComplete { .. } => {
                return boxed_action!(self.execute_action_convoy_work_force_complete(id, &command))
            }
            flotilla_protocol::CommandAction::ConvoyStart { .. } => {
                return boxed_action!(self.execute_action_convoy_start(id, &command, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::ConvoyCreate { .. } => {
                return boxed_action!(self.execute_action_convoy_create(id, &command, &dispatching_principal_ref))
            }
            flotilla_protocol::CommandAction::WorkflowTemplateApply { .. } => {
                return boxed_action!(self.execute_action_workflow_template_apply(id, &command))
            }
            flotilla_protocol::CommandAction::ProjectAdd { .. } => return boxed_action!(self.execute_action_project_add(id, &command)),
            flotilla_protocol::CommandAction::ProjectApply { .. } => return boxed_action!(self.execute_action_project_apply(id, &command)),
            flotilla_protocol::CommandAction::ProjectRegister { .. } => {
                return boxed_action!(self.execute_action_project_register(id, &command))
            }
            flotilla_protocol::CommandAction::ProjectRefresh { .. } => {
                return boxed_action!(self.execute_action_project_refresh(id, &command))
            }
            flotilla_protocol::CommandAction::TrackRepoPath { .. } => {
                return boxed_action!(self.execute_action_track_repo_path(id, &command))
            }
            flotilla_protocol::CommandAction::UntrackRepo { .. } => return boxed_action!(self.execute_action_untrack_repo(id, &command)),
            flotilla_protocol::CommandAction::OpenChangeRequest { .. }
            | flotilla_protocol::CommandAction::CloseChangeRequest { .. }
            | flotilla_protocol::CommandAction::MergeChangeRequest { .. }
            | flotilla_protocol::CommandAction::OpenIssue { .. }
            | flotilla_protocol::CommandAction::LinkIssuesToChangeRequest { .. } => {
                return boxed_action!(self.execute_action_repository_forge(id, &command))
            }
            flotilla_protocol::CommandAction::Refresh { repo: Some(_) } => {
                return boxed_action!(self.execute_action_refresh_repo(id, &command))
            }
            _ => {}
        }

        // Gather what the spawned task needs — validate repo before broadcasting
        let repo = self.resolve_repo_for_command(&command).await?;
        let runner = Arc::clone(&self.discovery.runner);
        let env = Arc::clone(&self.discovery.env);
        let event_sink = self.event_sink.clone();
        let repository = self.repository_for_selector(&flotilla_protocol::RepoSelector::Path(repo.clone())).await?;
        let repo_identity = repository_operations::repository_event_identity(&repository.spec, None);
        let registry = self.execution_registry(&repository, &repo).await?;
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

        self.event_sink.emit(DaemonEvent::CommandStarted {
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
            .await
            .map_err(executor::PlannerRefusal::into_command_value);

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
                    event_sink.emit(DaemonEvent::CommandFinished {
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
                        event_sink.clone(),
                        &resolver,
                        remote_executor.as_ref(),
                    )
                    .await;
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
                    event_sink.emit(DaemonEvent::CommandFinished {
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
    async fn explain_project_internal(&self, name: &str) -> Result<serde_json::Value, String> {
        let namespace = self.provisioning_namespace().await;
        let project = self.resource_backend.definitions::<Project>(&namespace).get(name).await.map_err(|error| error.to_string())?;
        let cascade = flotilla_resources::ResolvedCascade::load(&self.resource_backend, &namespace, name, &project.spec)
            .await
            .map_err(|error| error.to_string())?;
        Ok(serde_json::json!({ "namespace": namespace, "project": name, "cascade": cascade }))
    }

    async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String> {
        let namespace = requested_namespace.map(ToOwned::to_owned).unwrap_or(self.provisioning_namespace().await);
        self.read_projections().explain_convoy(&namespace, name).await
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_source.subscribe()
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
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false, None).await
    }

    async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        use flotilla_protocol::CommandAction;
        match &command.action {
            CommandAction::QueryResolveRepository { repo } => {
                let key = self.resolve_repository_selector(repo).await?;
                Ok(CommandValue::RepositoryResolved { key })
            }
            CommandAction::QueryRepoProviders { repo } => match self.get_repo_providers_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoProviders(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostList {} => match self.list_hosts_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryExplainProject { name } => match self.explain_project_internal(name).await {
                Ok(explanation) => Ok(CommandValue::ProjectExplanation(explanation)),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryProjectList {} => match self.list_projects_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::ProjectList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryCliList { kind } => match self.list_cli_items_internal(*kind).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CliList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDispatchBoard { project } => match self.dispatch_board_internal(project.as_deref()).await {
                Ok(board) => Ok(CommandValue::DispatchBoard(Box::new(board))),
                Err(error) => Ok(CommandValue::Error { message: error }),
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
            CommandAction::QueryCrewStalls { full } => {
                match read_projections::ReadProjections::crew_stalls(&self.resource_backend, *full, self.clock.now()).await {
                    Ok(value) => Ok(CommandValue::CrewStalls(Box::new(value))),
                    Err(message) => Ok(CommandValue::Error { message }),
                }
            }
            CommandAction::QueryCrewCapabilities { context } => match self.crew_capabilities_internal(context).await {
                Ok(card) => Ok(CommandValue::CrewCapabilities { card }),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryCrewList { context } => match self.crew_list_internal(context).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CrewList(Box::new(v))),
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
            CommandAction::QueryResourceDigest { namespace, kind, query } => {
                let (kind, backend) = match kind.strip_prefix("observed/") {
                    Some(kind) => (kind, self.observed_resource_backend()),
                    None => (kind.as_str(), self.resource_backend()),
                };
                let result = flotilla_resources::digest_resource_kind(&backend, namespace, kind, query).await;
                match result {
                    Ok(digest) => Ok(CommandValue::ResourceDigest(Box::new(digest.into()))),
                    Err(error) => Ok(CommandValue::Error { message: error.to_string() }),
                }
            }
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
                let position = match current_resource_kind_position(&self.resource_backend, namespace, kind).await {
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
                let resource_version = position.resource_version;
                let generation = position.generation;
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
                let (provider, source) = self.get_issue_provider_for_repository(repo).await?;
                let page = provider.query(&source, params, *page, *count).await?;
                Ok(flotilla_protocol::CommandValue::IssuePage(page))
            }
            CommandAction::QueryIssueFetchByIds { repo, ids } => {
                let (provider, source) = self.get_issue_provider_for_repository(repo).await?;
                let items = provider.fetch_by_ids(&source, ids).await?;
                Ok(flotilla_protocol::CommandValue::IssuesByIds { items })
            }
            CommandAction::QueryIssueOpenInBrowser { repo, id } => {
                let (provider, source) = self.get_issue_provider_for_repository(repo).await?;
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

/// Retry only replay-safe mutations and only optimistic conflicts.
async fn retry_resource_apply<T, F, Fut>(kind: &str, mut apply: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ResourceError>>,
{
    let attempts = if matches!(kind, "Artifact" | "Message") { 16 } else { 1 };
    for attempt in 0..attempts {
        let result = apply().await;
        if attempt + 1 == attempts || !matches!(result, Err(ResourceError::Conflict { .. })) {
            return result;
        }
    }
    unreachable!("retry budget always makes an attempt")
}
