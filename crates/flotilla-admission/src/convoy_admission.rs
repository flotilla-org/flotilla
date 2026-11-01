//! Convoy admission state and transaction boundary.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use flotilla_agent::adapter::{
    minimum_harness_version, required_agent_adapters, CapabilityTable, CrewAssignment, CrewBriefTemplateResolver,
};
use flotilla_agent::brief_artifact::BriefArtifactWriter;
use flotilla_ai_utility::{AiUtility, ConvoyNames};
use flotilla_change_request::branch_lookup_observer::BranchLookupObserver;
use flotilla_change_request::observation::{ChangeRequestKey, ChangeRequestObservationSource};
use flotilla_change_request::ChangeRequestTracker;
use flotilla_charter::ops_entry::{ENSURED_FROM_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION};
use flotilla_config::ConfigStore;
use flotilla_discovery::{environments::Environments, AiUtilityFactory, DiscoveryRuntime};
use flotilla_discovery_api::{provider_config::ProviderConfigView, EnvironmentBag};
use flotilla_observation::sources::{ChangeRequestQueryPort, IssueQueryPort, ProviderChangeRequestObservationSource};
#[cfg(test)]
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::result_set::ConvoyChangeRequest;
use flotilla_protocol::{
    AttachBinding, CanonicalHostId, CommandValue, ConvoyDispatchRegard, ConvoyNaming, EnvironmentId, FulfilmentAllocation,
    FulfilmentAllocationCandidate, HostName, PlacementDecision, PlacementRefusal, PlacementTargetHost, PlacementViableCandidate,
    PrincipalRef, ResourceRef, ViewAddress,
};
use flotilla_provider_runtime::{CommandRunner, ObservationError};
use flotilla_resources::address::{record_name, remote_change_request_address};
use flotilla_resources::checkout_path::checkout_path;
use flotilla_resources::convoy_address::{convoy_address, convoy_record_name};
use flotilla_resources::{
    api_version, capped_github_app_permissions, is_prepared_snapshot, normalize_issue_source, AllocationDecision, BoundChangeRequest,
    CapabilityNeed, ChangeRequest as ResourceChangeRequest, Checkout as ResourceCheckout, Clock, Convoy as ResourceConvoy, ConvoyEnsure,
    ConvoyIssue, ConvoyRepositorySpec, ConvoySpec, ConvoyStatusPatch, CredentialConsumer, CredentialGrant, CredentialSpec, CrewSource,
    CrewSpec, Forge, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, Host as ResourceHost, InputMeta,
    InputValue, IssueSnapshot, IssueSourceResolution, IssueSourceUnavailable, ObjectMeta, ObservedChangeRequestState, Platform, Project,
    ProjectSpec, ReadResourceObject, Repository, RepositoryKey, RepositoryTrust, Resource, ResourceError, ResourceObject,
    ResourceProvenance, RoleHandoff, SupervisionTarget, TerminalCrewContext, VesselRequirement, WorkflowTemplate, WorkflowTemplateSpec,
    BRIEF_ARTIFACTS_ANNOTATION, CODEX_CENTRAL_LOGIN_CAPABILITY, GENERATION_LABEL, PLACEMENT_SNAPSHOT_KIND, PROJECT_LABEL, ROLE_LABEL,
};
#[cfg(test)]
use flotilla_resources::{HoldAct, RepositorySpec, SystemClock};
use flotilla_store::{
    apply_status_patch as apply_resource_status_patch, resolve_project_issue_sources, DockerImageSourceStoreExt, ResolvedCascadeStoreExt,
    ResourceBackend, StoreQuery,
};
#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::project_ops;
use crate::regard_lifecycle::{RegardLifecycle, SurfaceGestureOutcome};
use crate::support::{
    authoritative_placement_host, check_placement_capacity, ensure_prepared_placement_snapshot, ensure_prepared_workflow_snapshot,
    home_copy_wins_by_name, host_generation, host_occupancies, host_occupancy, is_ssh_host, placement_actuator_host_ref,
    placement_host_not_ready_reason, placement_target_host, prepared_snapshot_name, resolve_placement_provider, unready_placement_refusal,
    SSH_HOST_PIN_ONLY,
};

/// Routing uses replicated observations as hints; only admission may refuse on
/// destination-owned facts, credentials, or the installed skill catalog.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlacementPurpose {
    Routing,
    Admission,
}

#[derive(Clone, Copy)]
struct PlacementContext<'a> {
    namespace: &'a str,
    project_ref: Option<&'a str>,
    repositories: &'a [ConvoyRepositorySpec],
    intent: &'a flotilla_protocol::ConvoyStartIntent,
    purpose: PlacementPurpose,
}

#[derive(bon::Builder)]
pub struct ConvoyCreateAdmission<'a> {
    namespace: &'a str,
    name: &'a str,
    role: &'a str,
    workflow_ref: &'a str,
    workflow: &'a WorkflowTemplateSpec,
    placement: PlacementResolution,
    placement_decision: Option<PlacementDecision>,
    inputs: &'a [(String, String)],
    repositories: Vec<ConvoyRepositorySpec>,
    source_ref: Option<String>,
    project_ref: Option<String>,
    adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
    adopted_checkout_ref_to_cleanup: Option<String>,
    dispatching_principal_ref: Option<PrincipalRef>,
    /// Per-vessel composed-image pins when vessels resolve to different images.
    #[builder(default)]
    vessel_placements: BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>,
    /// Builds for the composed images, created with the convoy.
    #[builder(default)]
    image_builds: Vec<crate::image_build::PlannedImageBuild>,
}

#[derive(bon::Builder)]
pub struct ConvoyAdmission {
    backend: ResourceBackend,
    observed_backend: ResourceBackend,
    #[builder(default = Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads))]
    lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    /// The account kinds' records: the old store's, or the engine's behind `[primitives]`.
    accounts: Arc<dyn flotilla_resources::account_access::AccountRecords>,
    observed_checkout_reconciliation: Arc<Mutex<()>>,
    config: Arc<ConfigStore>,
    discovery: Arc<DiscoveryRuntime>,
    environment_manager: Arc<dyn Environments>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    repository_change_requests: Arc<RwLock<HashMap<RepositoryKey, RepositoryChangeRequestProvider>>>,
    change_request_port: Arc<dyn ChangeRequestQueryPort>,
    #[builder(default)]
    branch_lookup_observer: BranchLookupObserver,
    issue_port: Arc<dyn IssueQueryPort>,
    change_request_observation_source: Arc<ProviderChangeRequestObservationSource>,
    brief_artifact_writer: Arc<RwLock<Option<Arc<dyn BriefArtifactWriter>>>>,
    admission_free_space_path: Arc<std::sync::RwLock<PathBuf>>,
    regard_lifecycle: Arc<RegardLifecycle>,
    host_name: HostName,
    clock: Arc<dyn Clock>,
    /// The draw that selects a requester's account (#3884); tests fix the seed.
    #[builder(default = std::sync::RwLock::new(Arc::new(crate::random::SeededRandom::from_entropy())))]
    random: std::sync::RwLock<Arc<dyn crate::random::RandomSource>>,
    fulfilment_decider: Arc<dyn FulfilmentDecider>,
    /// Serializes the identity selector check with Convoy creation on the owner host.
    #[builder(default)]
    guard: Mutex<()>,
    #[builder(default)]
    pending_starts: Mutex<HashSet<ConvoyStartKey>>,
}

/// Decisions use one explicit inventory per participating kind. Runtime
/// observations and selected build joins are separate, explicit dependencies.
struct AdmissionReadContext<'a> {
    runtime: AdmissionRuntime<'a>,
    backend: ResourceBackend,
    /// Capped SSH hosts' occupancy, counted from the live store when a
    /// request snapshot (which copies no convoys or vessels) is loaded.
    occupancy: BTreeMap<String, flotilla_resources::HostOccupancy>,
    /// Vessels this admission has already placed on each capped host: a
    /// convoy's vessels are placed one at a time and must fit together.
    placed_here: std::sync::Mutex<BTreeMap<String, u32>>,
}

/// External observations and actions available during planning. No admission
/// owner, mutation store, identity guard or pending-start registry is exposed.
struct AdmissionRuntime<'a> {
    lifecycle_reads: &'a Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    config: &'a ConfigStore,
    discovery: &'a DiscoveryRuntime,
    environment_manager: &'a dyn Environments,
    local_environment_id: &'a EnvironmentId,
    local_host: Option<CanonicalHostId>,
    namespace: String,
    repository_change_requests: &'a RwLock<HashMap<RepositoryKey, RepositoryChangeRequestProvider>>,
    change_request_port: &'a dyn ChangeRequestQueryPort,
    issue_port: &'a dyn IssueQueryPort,
    clock: &'a dyn Clock,
    fulfilment_decider: &'a dyn FulfilmentDecider,
    builds: PlacementBuildJoiner<'a>,
}

/// The only store-writing action allowed during planning: get-or-creating the
/// ImageBuild for each selected vessel's composed image.
struct PlacementBuildJoiner<'a> {
    backend: &'a ResourceBackend,
}

struct AdmissionLookupFailure {
    context: Option<String>,
    error: ObservationError,
}

impl AdmissionLookupFailure {
    fn diagnostic(self) -> String {
        match self.context {
            Some(context) => format!("{context}: {}", self.error),
            None => self.error.to_string(),
        }
    }
}

impl ConvoyAdmission {
    pub fn repository_change_requests(&self) -> &Arc<RwLock<HashMap<RepositoryKey, RepositoryChangeRequestProvider>>> {
        &self.repository_change_requests
    }

    fn reads(&self, backend: ResourceBackend) -> AdmissionReadContext<'_> {
        AdmissionReadContext {
            backend,
            occupancy: BTreeMap::new(),
            placed_here: Default::default(),
            runtime: AdmissionRuntime {
                lifecycle_reads: &self.lifecycle_reads,
                config: &self.config,
                discovery: &self.discovery,
                environment_manager: self.environment_manager.as_ref(),
                local_environment_id: &self.local_environment_id,
                local_host: self.canonical_local_host_id(),
                namespace: self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone(),
                repository_change_requests: &self.repository_change_requests,
                change_request_port: self.change_request_port.as_ref(),
                issue_port: self.issue_port.as_ref(),
                clock: self.clock.as_ref(),
                fulfilment_decider: self.fulfilment_decider.as_ref(),
                builds: PlacementBuildJoiner { backend: &self.backend },
            },
        }
    }

    pub fn set_free_space_path(&self, path: PathBuf) {
        *self.admission_free_space_path.write().expect("admission free-space path lock poisoned") = path;
    }

    pub async fn free_space_bytes(&self) -> Result<Option<u64>, String> {
        let path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let probe = Arc::clone(&self.discovery.available_space_probe);
        self.discovery
            .admission_probe
            .run("available disk space", flotilla_provider_runtime::probe::PROBE_TIMEOUT, move || Ok(probe.measure(&path)))
            .await
    }

    async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.environment_manager
            .host_id_for_environment(&self.local_environment_id)
            .map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.guard.lock().await
    }

    pub async fn mark_pending(&self, key: ConvoyStartKey) -> bool {
        self.pending_starts.lock().await.insert(key)
    }

    pub async fn clear_pending(&self, key: &ConvoyStartKey) {
        self.pending_starts.lock().await.remove(key);
    }
}

impl ConvoyAdmission {
    /// Resolve explicit continuation with fresh forge facts. Absence is allowed
    /// for branch-only work; terminal requests are never silently adopted.
    #[doc(hidden)]
    pub async fn resolve_continuation(
        &self,
        repositories: &[ConvoyRepositorySpec],
        continuation: &flotilla_protocol::ConvoyContinuation,
    ) -> Result<(RepositoryKey, String, Option<ResolvedConvoyChangeRequestAdmission>), String> {
        self.reads(self.backend.clone()).resolve_continuation(repositories, continuation).await
    }

    fn refuse_live_continuation_holder(spec: &ConvoySpec, inventory: &[ReadResourceObject<ResourceConvoy>]) -> Result<(), String> {
        let Some(repository) = &spec.continuation else { return Ok(()) };
        let requested_subjects = spec
            .declared_subjects()?
            .into_iter()
            .filter(|entry| entry.relationship == flotilla_protocol::Relationship::Produces)
            .map(|entry| entry.subject)
            .collect::<BTreeSet<_>>();
        for source in inventory {
            let other = &source.object;
            if other.metadata.deletion_timestamp.is_some() || other.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                continue;
            }
            let same_branch = other.spec.r#ref == spec.r#ref && other.spec.repositories.iter().any(|repo| &repo.repo_ref == repository);
            let same_pr =
                flotilla_resources::active_change_request_subjects(other)?.iter().any(|subject| requested_subjects.contains(subject));
            if same_branch || same_pr {
                return Err(format!("continuation is held by live convoy {}; release it before continuing", other.metadata.name));
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub async fn resolve_convoy_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        self.reads(self.backend.clone()).resolve_convoy_change_request_admission(repository_keys, requested_id).await
    }

    pub async fn repository_change_request_candidates(
        &self,
        repository_keys: &[RepositoryKey],
    ) -> (Vec<(RepositoryKey, String, Arc<dyn ChangeRequestTracker>)>, Vec<String>) {
        let namespace = self.provisioning_namespace().await;
        let repositories = match self
            .backend
            .query_declared::<Repository>(&namespace, &StoreQuery::names(repository_keys.iter().map(ToString::to_string), true))
            .await
        {
            Ok(repositories) => repositories,
            Err(error) => return (Vec::new(), vec![error.to_string()]),
        };
        let mut snapshot = flotilla_store::ViewSnapshot::new(self.backend.local_root().ok());
        if let Err(error) = snapshot.insert::<Repository>(&namespace, repositories) {
            return (Vec::new(), vec![error.to_string()]);
        }
        self.reads(ResourceBackend::new(snapshot)).repository_change_request_candidates(repository_keys).await
    }

    pub async fn resolve_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        match change_request_id {
            Some(id) => self.resolve_bound_convoy_change_request(repository_keys, id).await,
            None => self.refresh_convoy_branch(repository_keys, branch, None).await.primary,
        }
    }

    async fn resolve_bound_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        id: &str,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let namespace = self.provisioning_namespace().await;
        let reads = self.change_request_read_inventory(&namespace, repository_keys, id).await?;
        self.resolve_bound_convoy_change_request_from_inventory(&reads, &namespace, repository_keys, id).await
    }

    async fn resolve_bound_convoy_change_request_from_inventory(
        &self,
        reads: &ResourceBackend,
        namespace: &str,
        repository_keys: &[RepositoryKey],
        id: &str,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        if let Some(change_request) =
            self.resolve_observed_convoy_change_request_from_inventory(reads, namespace, repository_keys, id).await?
        {
            return Ok(Some(change_request));
        }
        let repositories = reads.including_replicas::<Repository>(namespace);
        let forges = reads.definitions::<Forge>(namespace).list().await.map_err(|error| error.to_string())?;
        let forges = forges.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
        let mut failures = Vec::new();
        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository,
                Err(error) => {
                    failures.push(ObservationError::Failed(error.to_string()));
                    continue;
                }
            };
            let Some(remote) = repository.object.spec.live_remote() else { continue };
            let subject = id
                .parse::<u64>()
                .map_err(|_| format!("change request id `{id}` is not a numeric forge number"))
                .and_then(|number| remote_change_request_address(remote, number, &forges))
                .and_then(|address| ChangeRequestKey::new(namespace, address));
            let subject = match subject {
                Ok(subject) => subject,
                Err(error) => {
                    failures.push(ObservationError::Failed(error));
                    continue;
                }
            };
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
        // Preserve classification until the final admission diagnostic, including
        // classified limits with no retry deadline. Display wording is not policy.
        if let Some(error) = failures.iter().find(|error| matches!(error, ObservationError::RateLimited { .. })) {
            return Err(error.to_string());
        }
        failures.into_iter().next().map_or(Ok(None), |error| Err(error.to_string()))
    }

    pub async fn refresh_convoy_branch(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        binding: Option<&flotilla_resources::BoundChangeRequest>,
    ) -> flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh {
        let namespace = self.provisioning_namespace().await;
        let mut keys = repository_keys.to_vec();
        if let Some(binding) = binding {
            keys.push(binding.repository_ref.clone());
        }
        let reads = match self.change_request_read_inventory(&namespace, &keys, binding.map_or("", |binding| binding.id.as_str())).await {
            Ok(reads) => reads,
            Err(error) => {
                return flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh {
                    primary: Err(error.clone()),
                    repositories: repository_keys.iter().cloned().map(|key| (key, Err(error.clone()))).collect(),
                }
            }
        };
        let context = self.reads(reads.clone());
        let bound = match binding {
            Some(binding) => Some(
                self.resolve_bound_convoy_change_request_from_inventory(
                    &reads,
                    &namespace,
                    std::slice::from_ref(&binding.repository_ref),
                    &binding.id,
                )
                .await,
            ),
            None => None,
        };
        let mut results = Vec::new();
        let mut failures = Vec::new();
        let mut primary = None;
        let mut seen = BTreeSet::new();
        for key in repository_keys {
            if !seen.insert(key.clone()) {
                continue;
            }
            let result = if binding.is_some_and(|binding| binding.repository_ref == *key) {
                // The admitted ID remains authoritative for this repository;
                // reuse its lookup for discovery just as for the primary row.
                bound.as_ref().expect("binding has a lookup").clone().map_err(ObservationError::Failed)
            } else {
                let (candidates, setup_failures) = context.repository_change_request_candidates(std::slice::from_ref(key)).await;
                if let Some((repository, _, provider)) = candidates.into_iter().next() {
                    self.branch_lookup_observer.find(&repository.to_string(), &provider, branch).await.map(|found| {
                        found.map(|(id, request)| ConvoyChangeRequest { id, status: request.status, repository_key: repository })
                    })
                } else {
                    // Setup reports repository-read, forge-identity, or provider-discovery
                    // failures. The generic text defends against an empty diagnostic set.
                    Err(ObservationError::Failed(setup_failures.into_iter().next().unwrap_or_else(|| "no repository provider".into())))
                }
            };
            match &result {
                Ok(Some(request)) if primary.is_none() => primary = Some(request.clone()),
                Err(error) => failures.push(error.clone()),
                _ => {}
            }
            results.push((key.clone(), result.map_err(|error| error.to_string())));
        }
        // An admitted ID remains authoritative even on absence or failure;
        // matches in other repositories still contribute discovery results.
        let primary = if let Some(bound) = bound {
            bound
        } else if primary.is_some() {
            Ok(primary)
        } else {
            let error = failures.iter().find(|error| matches!(error, ObservationError::RateLimited { .. })).or_else(|| failures.first());
            error.map_or(Ok(None), |error| Err(error.to_string()))
        };
        flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh { primary, repositories: results }
    }

    #[doc(hidden)]
    pub async fn resolve_observed_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let Some(change_request_id) = change_request_id else { return Ok(None) };
        if change_request_id.parse::<u64>().is_err() {
            return Ok(None);
        }
        let namespace = self.provisioning_namespace().await;
        let reads = self.change_request_read_inventory(&namespace, repository_keys, change_request_id).await?;
        self.resolve_observed_convoy_change_request_from_inventory(&reads, &namespace, repository_keys, change_request_id).await
    }

    async fn change_request_read_inventory(
        &self,
        namespace: &str,
        repository_keys: &[RepositoryKey],
        id: &str,
    ) -> Result<ResourceBackend, String> {
        let repositories = self
            .backend
            .query_declared::<Repository>(namespace, &StoreQuery::names(repository_keys.iter().map(ToString::to_string), true))
            .await
            .map_err(|error| error.to_string())?;
        // One merged read: a remote's host resolves through its declared Forge.
        let forges =
            self.backend.definitions::<Forge>(namespace).list_selected(StoreQuery::default()).await.map_err(|error| error.to_string())?;
        let forge_specs = forges.iter().map(|forge| forge.spec.clone()).collect::<Vec<_>>();
        let mut names = BTreeSet::new();
        if let Ok(number) = id.parse::<u64>() {
            for source in &repositories.items {
                names.extend(
                    source
                        .object
                        .spec
                        .live_remote()
                        .and_then(|remote| remote_change_request_address(remote, number, &forge_specs).ok())
                        .and_then(|address| record_name(&address)),
                );
            }
        }
        let observations = self
            .backend
            .query_declared::<ResourceChangeRequest>(namespace, &StoreQuery::names(names, true))
            .await
            .map_err(|error| error.to_string())?;
        let mut snapshot = flotilla_store::RequestSnapshot::new(&self.backend, namespace)?;
        snapshot.insert::<Repository>(repositories)?;
        snapshot.insert::<ResourceChangeRequest>(observations)?;
        snapshot.insert_definitions::<Forge>(forges)?;
        Ok(snapshot.finish())
    }

    async fn resolve_observed_convoy_change_request_from_inventory(
        &self,
        reads: &ResourceBackend,
        namespace: &str,
        repository_keys: &[RepositoryKey],
        change_request_id: &str,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let Ok(number) = change_request_id.parse::<u64>() else { return Ok(None) };
        let repositories = reads.including_replicas::<Repository>(namespace);
        let change_requests = reads.including_replicas::<ResourceChangeRequest>(namespace);
        let forges = reads.definitions::<Forge>(namespace).list().await.map_err(|error| error.to_string())?;
        let forges = forges.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();

        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            };
            let Some(live_remote) = repository.object.spec.live_remote() else {
                continue;
            };
            // A remote no Forge declares names no change request record.
            let Some(name) = remote_change_request_address(live_remote, number, &forges).ok().and_then(|address| record_name(&address))
            else {
                continue;
            };
            let observation = match change_requests.get(&name).await {
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
}

impl ConvoyAdmission {
    async fn record_admission_naming(&self, namespace: &str, name: &str, naming: Option<ConvoyNaming>) {
        if let Some(naming) = naming {
            finish_naming_write(namespace, name, async {
                apply_resource_status_patch(
                    &self.backend.using::<ResourceConvoy>(namespace),
                    name,
                    &ConvoyStatusPatch::RecordNaming { naming },
                )
                .await
                .map(|_| ())
            })
            .await;
        }
    }

    pub async fn start_placement_host(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        let (namespace, intent) = normalize_convoy_start_intent(namespace, intent)?;
        let reads = if intent.placement_policy.is_some() {
            let mut snapshot = flotilla_store::RequestSnapshot::new(&self.backend, &namespace)?;
            // Every candidate participates in routing, including pin refusals.
            snapshot.copy::<FulfilmentKind>(&self.backend).await?;
            snapshot.copy::<ResourceHost>(&self.backend).await?;
            self.reads(snapshot.finish())
        } else {
            AdmissionReadContext::load(self, &namespace, &intent).await?
        };
        reads.start_placement_host(&namespace, &intent).await
    }

    pub async fn prepare_convoy_admission(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<PreparedConvoyAdmission, String> {
        self.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, None).await
    }

    /// The harness adapters the project's default workflow's agent roles
    /// resolve to, through the same cascade and workflow lookup admission
    /// uses (`flotilla image check` without `--agent`).
    pub async fn default_workflow_harness_adapters(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
    ) -> Result<BTreeSet<String>, String> {
        let intent =
            flotilla_protocol::ConvoyStartIntent::builder().project_ref(project_ref.to_string()).namespace(namespace.to_string()).build();
        let (workflow_ref, workflow) = self
            .reads(self.backend.clone())
            .load_placement_workflow(namespace, project_ref, project, &[], &intent, PlacementPurpose::Routing)
            .await?;
        flotilla_agent::adapter::required_agent_adapters(
            workflow
                .roles
                .iter()
                .chain(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
                .filter(|crew| matches!(crew.source, CrewSource::Agent { .. })),
        )
        .map_err(|error| format!("workflow `{workflow_ref}`: {error}"))
    }

    #[doc(hidden)]
    pub async fn resolve_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        self.reads(self.backend.clone()).resolve_workflow(namespace, project_ref, project, repositories, intent).await
    }

    #[doc(hidden)]
    pub async fn compose_convoy_needs(
        &self,
        namespace: &str,
        project: &ProjectSpec,
        issues: &[ConvoyIssue],
        intent: &flotilla_protocol::ConvoyStartIntent,
        workflow: &mut WorkflowTemplateSpec,
    ) -> Result<BTreeSet<CapabilityNeed>, String> {
        self.reads(self.backend.clone()).compose_convoy_needs(namespace, project, issues, intent, workflow).await
    }

    #[doc(hidden)]
    pub async fn resolve_capability_placement(
        &self,
        namespace: &str,
        project_ref: &str,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        self.decide_capability_placement(
            &PlacementContext { namespace, project_ref: Some(project_ref), repositories, intent, purpose: PlacementPurpose::Admission },
            workflow,
            needs,
        )
        .await
    }

    async fn decide_capability_placement(
        &self,
        context: &PlacementContext<'_>,
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        self.reads(self.backend.clone()).decide_capability_placement(context, workflow, needs).await
    }

    pub async fn prepare_convoy_admission_with_preferences(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
        repositories: Option<&[RepositoryKey]>,
    ) -> Result<PreparedConvoyAdmission, String> {
        let reads = AdmissionReadContext::load(self, namespace, intent).await?;
        reads.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, repositories).await
    }

    pub async fn admit_convoy_start(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<(String, String, Option<ConvoyNaming>, Vec<String>), String> {
        let mut admission = self.prepare_convoy_admission(namespace, intent, dispatching_principal_ref).await?;
        self.check_prepared_placement_floors(namespace, &admission).await?;
        let _admission_guard = self.lock().await;
        let inventory = convoy_admission_inventory(
            &self.backend,
            namespace,
            &admission.spec,
            None,
            self.prepared_brief_query(namespace, &admission).await?,
        )
        .await?;
        Self::refuse_live_continuation_holder(&admission.spec, &inventory)?;
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation_from_inventory(&inventory, admission.spec.project_ref.as_deref(), &admission.spec.role)?;
        let bindings = self
            .choose_account_bindings(
                namespace,
                &admission.name,
                &admission.spec,
                &admission.workflow,
                AdmittedPlacements {
                    vessels: &admission.vessel_placements,
                    convoy: admission.placement_kind.as_ref().zip(admission.placement_decision.as_ref()),
                },
                &intent.account_pins,
            )
            .await?;
        self.create_convoy_with_workflow_snapshot(
            namespace,
            &admission.name,
            ConvoySnapshotBundle {
                evidence: BriefEvidence { definitions: &admission.evidence, convoys: &inventory },
                spec: &admission.spec,
                workflow: &admission.workflow,
                placement: admission.placement_kind.as_ref(),
                placement_decision: admission.placement_decision,
                vessel_placements: Some(&admission.vessel_placements),
                image_builds: &admission.image_builds,
            },
            intent.auto_attach.into(),
        )
        .await?;
        self.accounts.open(namespace, bindings).await?;
        self.record_admission_naming(namespace, &admission.name, admission.naming.clone()).await;
        let address = convoy_address(&admission.spec.role, admission.spec.project_ref.as_deref());
        let warnings = admission.spec.issues.iter().flat_map(|issue| issue.placement_needs().1).collect();
        Ok((admission.name, address, admission.naming, warnings))
    }

    pub async fn check_local_free_space_floor(&self) -> Result<(), String> {
        let config = Arc::clone(&self.config);
        let available_space_probe = Arc::clone(&self.discovery.available_space_probe);
        let admission_free_space_path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let host_name = self.host_name.to_string();
        self.discovery
            .admission_probe
            .run("free-space admission", flotilla_provider_runtime::probe::PROBE_TIMEOUT, move || {
                let daemon_config = config.load_daemon_config()?;
                crate::admission::check_free_space_floor(
                    &*available_space_probe,
                    &host_name,
                    &admission_free_space_path,
                    daemon_config.admission.free_space_floor_gib,
                )
            })
            .await
            .map_err(|error| format!("free-space check failed on host `{}`: {error}", self.host_name))
    }

    /// The admitting daemon's own floor gates only work that lands on it; a
    /// remote target is judged by the floor it publishes. Work with no
    /// placement decision, or an unknown local identity, counts as local.
    pub async fn check_admitting_host_floor<'a>(&self, decisions: impl IntoIterator<Item = &'a PlacementDecision>) -> Result<(), String> {
        let local = self.canonical_local_host_id();
        let mut decisions = decisions.into_iter().peekable();
        let lands_here = decisions.peek().is_none()
            || decisions.any(|decision| local.as_ref().is_none_or(|local| local == &decision.target_host.reference));
        if lands_here {
            self.check_local_free_space_floor().await?;
        }
        Ok(())
    }

    fn prepared_placement_decisions(admission: &PreparedConvoyAdmission) -> Vec<&PlacementDecision> {
        if admission.vessel_placements.is_empty() {
            admission.placement_decision.iter().collect()
        } else {
            admission.vessel_placements.values().map(|(_, decision)| decision).collect()
        }
    }

    pub fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        let floor_gib = self.config.load_daemon_config()?.admission.free_space_floor_gib;
        crate::admission::free_space_floor_bytes(floor_gib)
    }

    pub async fn check_remote_placement_free_space_floor(
        &self,
        namespace: &str,
        placement: Option<&PlacementDecision>,
    ) -> Result<(), String> {
        self.reads(self.backend.clone()).check_remote_placement_free_space_floor(namespace, placement).await
    }

    pub async fn check_prepared_placement_floors(&self, namespace: &str, admission: &PreparedConvoyAdmission) -> Result<(), String> {
        self.check_admitting_host_floor(Self::prepared_placement_decisions(admission)).await?;
        let reads = self.reads(admission.evidence.clone());
        if admission.vessel_placements.is_empty() {
            reads.check_remote_placement_free_space_floor(namespace, admission.placement_decision.as_ref()).await?;
        }
        for (_, decision) in admission.vessel_placements.values() {
            reads.check_remote_placement_free_space_floor(namespace, Some(decision)).await?;
        }
        Ok(())
    }

    /// A convoy's own snapshot, at its own origin: reusing another's would let its GC race
    /// references in flight, and its change names the one Convoy it was frozen for.
    fn placement_snapshot_name(&self, convoy: &str, spec: &FulfilmentKindSpec) -> Result<String, String> {
        let root = self.backend.local_root().map_err(|error| error.to_string())?;
        let spec = serde_json::to_value(spec).map_err(|error| error.to_string())?;
        prepared_snapshot_name("placement", &serde_json::json!({ "origin": root, "convoy": convoy, "spec": spec }))
    }

    async fn write_placement_snapshot(&self, namespace: &str, convoy: &str, spec: &FulfilmentKindSpec) -> Result<String, String> {
        let name = self.placement_snapshot_name(convoy, spec)?;
        ensure_prepared_placement_snapshot(&self.backend, namespace, &name, convoy, spec).await?;
        Ok(name)
    }

    async fn create_convoy_with_workflow_snapshot(
        &self,
        namespace: &str,
        name: &str,
        bundle: ConvoySnapshotBundle<'_>,
        dispatch_regard: ConvoyDispatchRegard,
    ) -> Result<(), String> {
        let ConvoySnapshotBundle { spec, workflow, placement, placement_decision, vessel_placements, image_builds, evidence } = bundle;
        let workflow_value = serde_json::to_value(workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.backend, namespace, &workflow_name, workflow).await?;
        let mut annotations = BTreeMap::from([(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name)]);
        if self.write_admission_briefs_from_inventory(namespace, name, spec, workflow, evidence).await? {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = placement {
            let placement_name = self.write_placement_snapshot(namespace, name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        if let Some(vessel_placements) = vessel_placements {
            annotations.extend(self.prepare_vessel_placement_annotations(namespace, name, vessel_placements).await?);
        }
        let created = crate::image_build::create_planned_builds(&self.backend, namespace, image_builds).await?;
        let result = self.create_convoy_with_annotations(namespace, name, spec, placement_decision, dispatch_regard, annotations).await;
        if result.is_err() {
            crate::image_build::remove_created_builds(&self.backend, namespace, &created).await;
        }
        result
    }

    async fn prepared_brief_query(&self, namespace: &str, admission: &PreparedConvoyAdmission) -> Result<Option<StoreQuery>, String> {
        self.brief_query(namespace, &admission.name, &admission.spec, &admission.workflow, &admission.evidence).await
    }

    async fn brief_query(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
        reads: &ResourceBackend,
    ) -> Result<Option<StoreQuery>, String> {
        use flotilla_resources::{CrewDefaults, FleetDesignation, FLEET_DESIGNATION_NAME};
        if self.brief_artifact_writer.read().await.is_none()
            || !workflow.vessels.iter().flat_map(|vessel| &vessel.crew).any(|crew| matches!(crew.source, CrewSource::Agent { .. }))
        {
            return Ok(None);
        }
        let Some(project) = spec.project_ref.as_deref() else { return Ok(None) };
        let projects = flotilla_store::read_definition_catalog::<Project>(reads, namespace).await?;
        let defaults = flotilla_store::read_definition_catalog::<CrewDefaults>(reads, namespace).await?;
        let fleet = reads
            .definitions::<FleetDesignation>(namespace)
            .list_names([FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(brief_convoy_query(project, name, workflow, &projects, &defaults, &fleet)?.0))
    }

    async fn load_brief_definitions(
        &self,
        namespace: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<ResourceBackend, String> {
        use flotilla_resources::{CrewDefaults, FleetDesignation, FLEET_DESIGNATION_NAME};
        if self.brief_artifact_writer.read().await.is_none()
            || !workflow.vessels.iter().flat_map(|vessel| &vessel.crew).any(|crew| matches!(crew.source, CrewSource::Agent { .. }))
        {
            return Ok(ResourceBackend::new(flotilla_store::ViewSnapshot::default()));
        }
        let mut snapshot = flotilla_store::RequestSnapshot::new(&self.backend, namespace)?;
        let names = spec
            .repositories
            .iter()
            .map(|repository| repository.repo_ref.to_string())
            .chain(workflow.vessels.iter().flat_map(|vessel| vessel.repository_refs.iter().flatten().map(ToString::to_string)));
        snapshot.insert::<Repository>(
            self.backend
                .query_declared::<Repository>(namespace, &StoreQuery::names(names, false))
                .await
                .map_err(|error| error.to_string())?,
        )?;
        if spec.project_ref.is_some() {
            // Every graph/default layer participates in cascade/topic refusal.
            snapshot.insert_definitions::<Project>(flotilla_store::read_definition_catalog::<Project>(&self.backend, namespace).await?)?;
            snapshot.insert_definitions::<CrewDefaults>(
                flotilla_store::read_definition_catalog::<CrewDefaults>(&self.backend, namespace).await?,
            )?;
            snapshot.insert_definitions::<FleetDesignation>(
                self.backend
                    .definitions::<FleetDesignation>(namespace)
                    .list_names([FLEET_DESIGNATION_NAME.into()])
                    .await
                    .map_err(|error| error.to_string())?,
            )?;
        }
        Ok(snapshot.finish())
    }

    async fn brief_read_inventory(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
        evidence: BriefEvidence<'_>,
    ) -> Result<(ResourceBackend, ResourceBackend), String> {
        use flotilla_resources::TerminalSession as ResourceTerminalSession;
        use flotilla_resources::{CrewDefaults, Environment, FleetDesignation, ReadResourceList, FLEET_DESIGNATION_NAME};
        let read_source = evidence.definitions;
        let repository_names = spec
            .repositories
            .iter()
            .map(|repository| repository.repo_ref.to_string())
            .chain(workflow.vessels.iter().flat_map(|vessel| vessel.repository_refs.iter().flatten().map(ToString::to_string)))
            .collect::<BTreeSet<_>>();
        let repositories = read_source
            .query_declared::<Repository>(namespace, &StoreQuery::names(repository_names.clone(), false))
            .await
            .map_err(|error| error.to_string())?;
        let local_host = self.environment_manager.local_host_id().as_str();
        let local_environment_query = host_direct_environment_query(local_host)?;
        let environments = self
            .backend
            .query_declared::<Environment>(namespace, &StoreQuery { include_replicas: true, ..local_environment_query.clone() })
            .await
            .map_err(|error| error.to_string())?;
        let local_checkout_query = StoreQuery {
            field_values: BTreeMap::from([("spec.repo_ref".into(), repository_names.into_iter().collect())]),
            any_of: Some(vec![
                StoreQuery { fields: BTreeMap::from([("spec.host_ref".into(), local_host.into())]), ..Default::default() },
                StoreQuery::related_kind("spec.env_ref", "Environment", "metadata.name", local_environment_query),
            ]),
            ..Default::default()
        };
        // The observed partition is an in-memory cache; durable rows override
        // all selected cache names even when their ready/local predicates changed.
        let observed = self
            .observed_backend
            .query_declared::<ResourceCheckout>(namespace, &local_checkout_query)
            .await
            .map_err(|error| error.to_string())?;
        let checkouts = self
            .backend
            .query_declared::<ResourceCheckout>(
                namespace,
                &StoreQuery {
                    any_of: Some(vec![
                        local_checkout_query,
                        StoreQuery::names(
                            observed
                                .items
                                .iter()
                                .map(|source| source.object.metadata.name.clone())
                                .chain(spec.adopted_checkout_refs.values().cloned()),
                            true,
                        ),
                    ]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut snapshot = flotilla_store::RequestSnapshot::new(&self.backend, namespace)?;
        snapshot.insert::<Repository>(repositories)?;
        snapshot.insert::<Environment>(environments)?;
        snapshot.insert::<ResourceCheckout>(checkouts)?;
        if let Some(project) = spec.project_ref.as_deref() {
            // Every graph/default layer participates in topic and cascade refusals.
            let projects = flotilla_store::read_definition_catalog::<Project>(read_source, namespace).await?;
            let defaults = flotilla_store::read_definition_catalog::<CrewDefaults>(read_source, namespace).await?;
            let fleet = read_source
                .definitions::<FleetDesignation>(namespace)
                .list_names([FLEET_DESIGNATION_NAME.into()])
                .await
                .map_err(|error| error.to_string())?;
            let (convoy_query, visible) = brief_convoy_query(project, name, workflow, &projects, &defaults, &fleet)?;
            let convoys = ReadResourceList { items: evidence.convoys.to_vec() };
            let terminal_query = StoreQuery {
                any_of: Some(
                    std::iter::once(StoreQuery::related_kind(
                        "metadata.labels.\"flotilla.work/convoy\"",
                        "Convoy",
                        "metadata.name",
                        convoy_query,
                    ))
                    .chain(visible.iter().map(|project| StoreQuery {
                        field_prefixes: BTreeMap::from([(
                            "metadata.annotations.\"flotilla.work/role-address\"".into(),
                            format!("{project}/"),
                        )]),
                        ..Default::default()
                    }))
                    .collect(),
                ),
                include_replicas: true,
                ..Default::default()
            };
            let terminals = self
                .backend
                .query_declared::<ResourceTerminalSession>(namespace, &terminal_query)
                .await
                .map_err(|error| error.to_string())?;
            let ensures = self
                .backend
                .query_declared::<ConvoyEnsure>(
                    namespace,
                    &StoreQuery {
                        field_values: BTreeMap::from([("spec.project_ref".into(), visible.into_iter().collect())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            snapshot.insert_definitions::<Project>(projects)?;
            snapshot.insert_definitions::<CrewDefaults>(defaults)?;
            snapshot.insert_definitions::<FleetDesignation>(fleet)?;
            snapshot.insert::<ResourceConvoy>(convoys)?;
            let terminals = flotilla_resources::ReadResourceList {
                items: flotilla_store::read_terminal_sources(Arc::clone(&self.lifecycle_reads), terminals.items)
                    .await
                    .map_err(|error| error.to_string())?,
            };
            snapshot.insert::<ResourceTerminalSession>(terminals)?;
            snapshot.insert::<ConvoyEnsure>(ensures)?;
        }
        let mut observed_snapshot = flotilla_store::RequestSnapshot::new(&self.observed_backend, namespace)?;
        observed_snapshot.insert::<ResourceCheckout>(observed)?;
        Ok((snapshot.finish(), observed_snapshot.finish()))
    }

    pub async fn write_admission_briefs(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<bool, String> {
        self.write_briefs(namespace, name, spec, workflow, BriefReadSource::Load).await
    }

    async fn write_admission_briefs_from_inventory(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
        evidence: BriefEvidence<'_>,
    ) -> Result<bool, String> {
        self.write_briefs(namespace, name, spec, workflow, BriefReadSource::Prepared(evidence)).await
    }

    async fn write_briefs(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
        source: BriefReadSource<'_>,
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
        if agent_roles.is_empty() {
            return Ok(true);
        }
        let definitions;
        let convoys;
        let evidence = match source {
            BriefReadSource::Prepared(evidence) => evidence,
            BriefReadSource::Load => {
                definitions = self.load_brief_definitions(namespace, spec, workflow).await?;
                convoys = match self.brief_query(namespace, name, spec, workflow, &definitions).await? {
                    Some(query) => {
                        self.backend.query_declared::<ResourceConvoy>(namespace, &query).await.map_err(|error| error.to_string())?.items
                    }
                    None => Vec::new(),
                };
                BriefEvidence { definitions: &definitions, convoys: &convoys }
            }
        };
        let (reads, observed) = self.brief_read_inventory(namespace, name, spec, workflow, evidence).await?;
        let mut annotations = BTreeMap::new();
        if workflow.exit.is_none() {
            annotations.insert(flotilla_charter::ops_entry::ENSURED_FROM_ANNOTATION.to_string(), "standing".to_string());
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
        let templates = CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path());
        let repositories = reads.clone().using::<Repository>(namespace);
        let checkouts = reads.clone().using::<ResourceCheckout>(namespace);
        let local_checkouts = crate::repository_addressing::local_checkouts_with_lifecycle_reads(
            &reads,
            &observed,
            namespace,
            self.environment_manager.local_host_id().as_str(),
            self.lifecycle_reads.as_ref(),
        )
        .await?;
        for requirement in &workflow.vessels {
            let repository_refs = requirement
                .repository_refs
                .clone()
                .unwrap_or_else(|| spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>());
            let mut fork_stance = false;
            let mut roots = Vec::new();
            for repository_ref in &repository_refs {
                let mut source_roots = local_checkouts
                    .iter()
                    .filter(|checkout| checkout.spec.repo_ref() == repository_ref)
                    .filter_map(|checkout| checkout_path(checkout).map(PathBuf::from))
                    .collect::<Vec<_>>();
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
                .map(|(index, member)| flotilla_agent::adapter::CrewBriefMember {
                    role: member.role.clone(),
                    state: if requirement.starts_eagerly(index) { "active" } else { "latent" }.to_string(),
                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                })
                .collect::<Vec<_>>();
            for process in &requirement.crew {
                let CrewSource::Agent { prompt, brief_template, .. } = &process.source else { continue };
                let assignment = match prompt.as_deref() {
                    Some(prompt) => CrewAssignment::Prompt(prompt),
                    None if !spec.issues.is_empty() => CrewAssignment::CarriedIssue,
                    None if spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
                    None => CrewAssignment::Unassigned,
                };
                let mut options = templates.render_options_with_fork_stance(
                    brief_template.as_deref(),
                    spec.project_ref.as_deref(),
                    roots.clone(),
                    fork_stance,
                );
                options.apply_cascade(workflow.cascade.as_deref(), &process.role);
                options.has_credential_scope = !requirement.credential_scopes.is_empty();
                let context = TerminalCrewContext {
                    namespace: namespace.to_string(),
                    convoy: name.to_string(),
                    vessel_ref: format!("{name}-{}", requirement.name),
                };
                let mut brief = flotilla_agent::adapter::build_convoy_crew_brief_with_options(
                    &convoy,
                    &context,
                    &requirement.name,
                    &process.role,
                    assignment,
                    &members,
                    &options,
                )?;
                flotilla_agent::adapter::append_convoy_work_context(
                    &mut brief.content,
                    &convoy,
                    &repository_refs,
                    &requirement.credential_scopes,
                    None,
                );
                if let Some(project) = spec.project_ref.as_deref() {
                    let address = format!("{project}/{name}/{}/{}", requirement.name, process.role);
                    let mut book =
                        flotilla_messaging::crew_address_book(&reads, namespace, &address).await.map_err(|error| error.to_string())?;
                    for peer_vessel in &workflow.vessels {
                        for peer in &peer_vessel.crew {
                            let peer_address = format!("{project}/{name}/{}/{}", peer_vessel.name, peer.role);
                            if peer_address != address {
                                book.contacts.push(flotilla_messaging::RoleContact {
                                    address: peer_address,
                                    relation: flotilla_resources::MessageRelation::Peer,
                                    project: project.into(),
                                    terminal: None,
                                });
                            }
                        }
                    }
                    book.contacts.sort_by(|a, b| a.address.cmp(&b.address));
                    book.contacts.dedup_by(|a, b| a.address == b.address);
                    brief.content.push('\n');
                    brief.content.push_str(&book.render());
                }
                writer.put_brief(namespace, name, &process.role, name, brief.content.as_bytes(), options.charter_commit.as_deref()).await?;
            }
        }
        Ok(true)
    }

    pub async fn prepare_vessel_placement_annotations(
        &self,
        namespace: &str,
        convoy: &str,
        placements: &BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>,
    ) -> Result<BTreeMap<String, String>, String> {
        if placements.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut pins = BTreeMap::new();
        for (vessel, (kind, decision)) in placements {
            let name = self.write_placement_snapshot(namespace, convoy, kind).await?;
            pins.insert(vessel.clone(), flotilla_resources::VesselPlacementPin { kind_ref: name, decision: decision.clone() });
        }
        Ok(BTreeMap::from([(
            flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION.to_string(),
            serde_json::to_string(&pins).map_err(|error| error.to_string())?,
        )]))
    }

    pub async fn create_convoy_with_annotations(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        placement_decision: Option<PlacementDecision>,
        dispatch_regard: ConvoyDispatchRegard,
        mut annotations: BTreeMap<String, String>,
    ) -> Result<(), String> {
        // Publish admission inputs and initial status in one create. Publishing
        // first and patching status later races finalization/bootstrap/roll-up.
        if let Some(placement_decision) = placement_decision {
            annotations.insert(
                flotilla_resources::convoy::ADMISSION_PLACEMENT_ANNOTATION.to_string(),
                serde_json::to_string(&placement_decision).map_err(|error| error.to_string())?,
            );
        }
        let convoys = self.backend.clone().using::<ResourceConvoy>(namespace);
        let labels = BTreeMap::from([
            (PROJECT_LABEL.to_string(), spec.project_ref.clone().unwrap_or_default()),
            (ROLE_LABEL.to_string(), spec.role.clone()),
            (GENERATION_LABEL.to_string(), spec.generation.to_string()),
        ]);
        convoys
            .create(&InputMeta::builder().name(name.to_string()).labels(labels).annotations(annotations).build(), spec)
            .await
            .map_err(|error| error.to_string())?;
        if dispatch_regard == ConvoyDispatchRegard::Emit {
            if let Err(error) = self.emit_implicit_convoy_regard(namespace, name, &spec.dispatching_principal_ref).await {
                warn!(%error, %namespace, %name, "failed to emit convoy dispatch regard");
            }
        }
        Ok(())
    }

    pub async fn emit_implicit_convoy_regard(&self, namespace: &str, name: &str, principal_ref: &PrincipalRef) -> Result<(), String> {
        let target = ResourceRef::new(api_version(ResourceConvoy::API_PATHS), ResourceConvoy::API_PATHS.kind, namespace, name);
        self.regard_lifecycle.emit_implicit(principal_ref, &target, "convoy-dispatch").await
    }

    pub async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        let target = binding.resource_ref().ok_or_else(|| "resolved attach target has no resource identity".to_string())?;
        match self.regard_lifecycle.emit_expressed_for_surface(surface_id, &target).await? {
            SurfaceGestureOutcome::Handled => Ok(()),
            SurfaceGestureOutcome::UnknownSurface => {
                self.regard_lifecycle.emit_expressed(&PrincipalRef::implicit_for_namespace(&binding.namespace), &target).await
            }
        }
    }

    /// Freeze each vessel's composed image for a ConvoyCreate placement. The
    /// returned per-vessel pins are non-empty only when vessels resolve to
    /// different images.
    pub async fn join_convoy_placement_builds(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        workflow: &WorkflowTemplateSpec,
        placement: &mut PlacementResolution,
        decision: Option<&PlacementDecision>,
    ) -> Result<(BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>, Vec<crate::image_build::PlannedImageBuild>), String> {
        let joiner = PlacementBuildJoiner { backend: &self.backend };
        let (images, builds) = joiner.compose_placement_images(namespace, project_ref, workflow, placement).await?;
        Ok((apply_vessel_images(placement, images, decision)?, builds))
    }

    /// `convoy create`'s placement: the capability path over the workflow's own needs. Its pin
    /// is the operator's explicit choice, so it needs no further escalation reason.
    pub async fn resolve_created_convoy_placement(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        pin: Option<&str>,
    ) -> Result<PlacementResolution, String> {
        let intent = flotilla_protocol::ConvoyStartIntent::builder()
            .project_ref(project_ref.unwrap_or_default().to_string())
            .namespace(namespace.to_string())
            .maybe_placement_policy(pin.map(str::to_owned))
            .maybe_escalation_reason(pin.map(|pin| format!("convoy create pins `{pin}`")))
            .build();
        let needs = workflow
            .vessels
            .iter()
            .flat_map(|vessel| vessel.needs.iter().chain(vessel.crew.iter().flat_map(|crew| &crew.needs)))
            .cloned()
            .collect();
        let context = PlacementContext { namespace, project_ref, repositories, intent: &intent, purpose: PlacementPurpose::Admission };
        self.decide_capability_placement(&context, workflow, &needs).await.map(|(placement, _)| placement)
    }
}

impl ConvoyAdmission {
    pub async fn admit_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        mut admission: PreparedConvoyAdmission,
        mut annotations: BTreeMap<String, String>,
    ) -> Result<String, String> {
        let _admission_guard = self.lock().await;
        let inventory = convoy_admission_inventory(
            &self.backend,
            namespace,
            &admission.spec,
            Some(&ensure.metadata.name),
            self.prepared_brief_query(namespace, &admission).await?,
        )
        .await?;
        let existing = inventory
            .iter()
            .filter(|source| matches!(source.provenance, ResourceProvenance::Local))
            .map(|source| &source.object)
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
            allocate_convoy_generation_from_inventory(&inventory, admission.spec.project_ref.as_deref(), &admission.spec.role)?;
        let bindings = self
            .choose_account_bindings(
                namespace,
                &admission.name,
                &admission.spec,
                &admission.workflow,
                AdmittedPlacements {
                    vessels: &admission.vessel_placements,
                    convoy: admission.placement_kind.as_ref().zip(admission.placement_decision.as_ref()),
                },
                &[],
            )
            .await?;
        let workflow_value = serde_json::to_value(&admission.workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.backend, namespace, &workflow_name, &admission.workflow).await?;
        annotations.insert(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name);
        if self
            .write_admission_briefs_from_inventory(
                namespace,
                &admission.name,
                &admission.spec,
                &admission.workflow,
                BriefEvidence { definitions: &admission.evidence, convoys: &inventory },
            )
            .await?
        {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = &admission.placement_kind {
            let placement_name = self.write_placement_snapshot(namespace, &admission.name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        annotations.extend(self.prepare_vessel_placement_annotations(namespace, &admission.name, &admission.vessel_placements).await?);
        let created = crate::image_build::create_planned_builds(&self.backend, namespace, &admission.image_builds).await?;
        let result = self
            .create_convoy_with_annotations(
                namespace,
                &admission.name,
                &admission.spec,
                admission.placement_decision,
                ConvoyDispatchRegard::Suppress,
                annotations,
            )
            .await;
        if result.is_err() {
            crate::image_build::remove_created_builds(&self.backend, namespace, &created).await;
        }
        result?;
        self.accounts.open(namespace, bindings).await?;
        self.record_admission_naming(namespace, &admission.name, admission.naming.clone()).await;
        apply_resource_status_patch(
            &self.backend.using::<ResourceConvoy>(namespace),
            &admission.name,
            &ConvoyStatusPatch::RecordEnsureAdmission { config: ensure.spec.clone() },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(admission.name)
    }
}

impl ConvoyAdmission {
    /// The caller holds this guard from the identity check through any adopted
    /// checkout writes, and transfers it here through the final Convoy commit.
    pub async fn admit_created_convoy(
        &self,
        request: ConvoyCreateAdmission<'_>,
        _admission_guard: tokio::sync::MutexGuard<'_, ()>,
    ) -> CommandValue {
        let ConvoyCreateAdmission {
            namespace,
            name,
            role,
            workflow_ref,
            workflow,
            placement,
            placement_decision,
            inputs,
            repositories,
            source_ref,
            project_ref,
            adopted_checkout_refs,
            adopted_checkout_ref_to_cleanup,
            dispatching_principal_ref,
            vessel_placements,
            image_builds,
        } = request;
        let address = convoy_address(role, project_ref.as_deref());
        let mut spec = ConvoySpec {
            continuation: None,
            workflow_ref: workflow_ref.to_string(),
            role: role.to_string(),
            generation: 0,
            dispatching_principal_ref: dispatching_principal_ref.unwrap_or_else(|| PrincipalRef::implicit_for_namespace(namespace)),
            inputs: inputs.iter().map(|(key, value)| (key.clone(), InputValue::String(value.clone()))).collect(),
            placement_policy: placement.selected.as_ref().map(|placement| placement.metadata.name.clone()),
            repositories,
            r#ref: source_ref,
            project_ref,
            adopted_checkout_refs,
            subjects: Vec::new(),
            issues: Vec::new(),
            change_request: None,
            instruction: None,
            promises: Vec::new(),
        };
        let read_snapshot = match self.load_brief_definitions(namespace, &spec, workflow).await {
            Ok(reads) => reads,
            Err(message) => return CommandValue::Error { message },
        };
        let extra = match self.brief_query(namespace, name, &spec, workflow, &read_snapshot).await {
            Ok(query) => query,
            Err(message) => return CommandValue::Error { message },
        };
        let inventory = match convoy_admission_inventory(&self.backend, namespace, &spec, None, extra).await {
            Ok(inventory) => inventory,
            Err(message) => return CommandValue::Error { message },
        };
        spec.generation = match allocate_convoy_generation_from_inventory(&inventory, spec.project_ref.as_deref(), role) {
            Ok(generation) => generation,
            Err(message) => {
                if let Some(checkout_ref) = adopted_checkout_ref_to_cleanup {
                    let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                    let cleanup = async {
                        self.backend.clone().using::<ResourceCheckout>(namespace).delete(&checkout_ref).await?;
                        flotilla_observation::observed_resources::delete_stale_adopted_checkouts(
                            &self.backend,
                            &self.observed_backend,
                            namespace,
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = cleanup {
                        warn!(%error, %checkout_ref, "failed to clean up adopted checkout after convoy identity conflict");
                    }
                }
                return CommandValue::Error { message };
            }
        };
        let bindings = match self
            .choose_account_bindings(
                namespace,
                name,
                &spec,
                workflow,
                AdmittedPlacements {
                    vessels: &vessel_placements,
                    convoy: placement.selected.as_ref().map(|placement| &placement.spec).zip(placement_decision.as_ref()),
                },
                &[],
            )
            .await
        {
            Ok(bindings) => bindings,
            Err(message) => return CommandValue::Error { message },
        };
        match self
            .create_convoy_with_workflow_snapshot(
                namespace,
                name,
                ConvoySnapshotBundle {
                    evidence: BriefEvidence { definitions: &read_snapshot, convoys: &inventory },
                    spec: &spec,
                    workflow,
                    placement: placement.selected.as_ref().map(|placement| &placement.spec),
                    placement_decision,
                    vessel_placements: (!vessel_placements.is_empty()).then_some(&vessel_placements),
                    image_builds: &image_builds,
                },
                ConvoyDispatchRegard::Emit,
            )
            .await
        {
            Ok(()) => match self.accounts.open(namespace, bindings).await {
                Ok(()) => CommandValue::ConvoyCreated { name: address },
                Err(message) => CommandValue::Error { message },
            },
            Err(message) => CommandValue::Error { message },
        }
    }

    /// Choose each requester's account binding at the admitted placements, before the convoy
    /// is written: an unauthorized pin or an unusable requirement refuses admission (#3907).
    async fn choose_account_bindings(
        &self,
        namespace: &str,
        convoy: &str,
        spec: &ConvoySpec,
        workflow: &WorkflowTemplateSpec,
        placements: AdmittedPlacements<'_>,
        pins: &[(String, String)],
    ) -> Result<Vec<flotilla_resources::account_access::PendingBinding>, String> {
        let mut by_role = BTreeMap::new();
        for (role, credential) in pins {
            if by_role.insert(role.clone(), credential.clone()).is_some() {
                return Err(format!("duplicate --account pin for role `{role}`"));
            }
        }
        crate::account_access::choose_bindings(
            &self.backend,
            namespace,
            spec.project_ref.as_deref(),
            &spec.repositories,
            convoy,
            workflow,
            placements.vessels,
            placements.convoy,
            &by_role,
            &*self.account_random(),
            flotilla_schema::Timestamp::from_datetime(&self.clock.now()),
        )
        .await
    }

    fn account_random(&self) -> Arc<dyn crate::random::RandomSource> {
        Arc::clone(&self.random.read().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// Replace the account draw's random source; tests fix its seed (#3884).
    pub fn set_account_random(&self, random: Arc<dyn crate::random::RandomSource>) {
        *self.random.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = random;
    }

    /// The home pass over one convoy's account bindings (#3882 lifecycle). Only binding a
    /// requester that has none is serialised against admission; the rest is per convoy.
    pub async fn reconcile_convoy_accounts(
        &self,
        namespace: &str,
        convoy: &str,
    ) -> Result<Vec<(String, crate::account_access::BindingChange)>, String> {
        let convoy = match self.backend.clone().using::<ResourceConvoy>(namespace).get(convoy).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(format!("read convoy `{convoy}`: {error}")),
        };
        let now = flotilla_schema::Timestamp::from_datetime(&self.clock.now());
        let (mut changes, unbound) =
            crate::account_access::reconcile_convoy_bindings(&self.backend, &*self.accounts, namespace, &convoy, now).await?;
        if !unbound.is_empty() {
            let _admission_guard = self.lock().await;
            // Re-read under the lock: the convoy may have ended since. A requester another path
            // (admission, a rebind) bound meanwhile keeps that binding: `open` keeps an open one.
            let convoy = match self.backend.clone().using::<ResourceConvoy>(namespace).get(&convoy.metadata.name).await {
                Ok(convoy) if !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) => convoy,
                Ok(_) | Err(ResourceError::NotFound { .. }) => return Ok(changes),
                Err(error) => return Err(format!("read convoy `{}`: {error}", convoy.metadata.name)),
            };
            changes.extend(
                crate::account_access::bind_unbound(&*self.accounts, namespace, &convoy, unbound, &*self.account_random(), now).await?,
            );
        }
        Ok(changes)
    }

    /// An explicit rebind of one requester's account (#3882).
    pub async fn rebind_account(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        pin: Option<&str>,
    ) -> Result<flotilla_resources::AccountBindingSpec, String> {
        let _admission_guard = self.lock().await;
        let now = flotilla_schema::Timestamp::from_datetime(&self.clock.now());
        crate::account_access::rebind(&self.backend, &*self.accounts, namespace, convoy, role, pin, &*self.account_random(), now).await
    }
}

/// The placements a convoy was admitted at: per vessel, or one for the whole convoy.
struct AdmittedPlacements<'a> {
    vessels: &'a BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>,
    convoy: Option<(&'a FulfilmentKindSpec, &'a PlacementDecision)>,
}

impl PlacementBuildJoiner<'_> {
    /// Admission only. For a docker-per-vessel kind that omits the image,
    /// resolve every vessel's composition against the replica-inclusive
    /// catalogue, plan its ImageBuild for the placement host's architecture,
    /// and return the kind each vessel runs (`Built`) with the builds to
    /// create once admission commits. A literal image returns no entries.
    /// Nothing is written here.
    async fn compose_placement_images(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        workflow: &WorkflowTemplateSpec,
        placement: &PlacementResolution,
    ) -> Result<(BTreeMap<String, FulfilmentKindSpec>, Vec<crate::image_build::PlannedImageBuild>), String> {
        let Some(kind) = placement.selected.as_ref() else { return Ok(Default::default()) };
        if kind.spec.realisation != (FulfilmentRealisation::DockerPerVessel { image: None }) {
            return Ok(Default::default());
        }
        let resolved = resolve_vessel_images(self.backend, namespace, project_ref, workflow).await?;
        let mut images = BTreeMap::new();
        let mut builds = Vec::new();
        for (vessel, image) in resolved {
            let planned = crate::image_build::plan_image_build(self.backend, namespace, &image, &kind.spec.host_ref).await?;
            let mut spec = kind.spec.clone();
            spec.realisation = FulfilmentRealisation::DockerPerVessel {
                image: Some(flotilla_resources::DockerImageSource::Built {
                    identity: image.identity,
                    image_build_ref: planned.name.clone(),
                }),
            };
            images.insert(vessel, spec);
            builds.push(planned);
        }
        Ok((images, builds))
    }
}

fn host_direct_environment_query(local_host: &str) -> Result<StoreQuery, String> {
    let host_address = flotilla_address::Address::root(flotilla_address::Kind::Host, local_host).map_err(|error| error.to_string())?;
    // Keep admission bounded to this host's direct environments. A full
    // Environment scan grows with every per-vessel cloud/container backing.
    Ok(StoreQuery {
        any_of: Some(vec![
            StoreQuery {
                field_prefixes: BTreeMap::from([("spec.provider_address".into(), format!("{host_address}/provider:"))]),
                present: vec!["spec.host_direct".into()],
                ..Default::default()
            },
            StoreQuery {
                fields: BTreeMap::from([("spec.host_direct.host_ref".into(), local_host.into())]),
                missing_fields: BTreeSet::from(["spec.provider_address".into()]),
                ..Default::default()
            },
        ]),
        ..Default::default()
    })
}

/// Write composed images into the placement: the convoy-level kind carries
/// the first vessel's image, and when vessels differ every vessel gets its own
/// pinned snapshot (identical values share one snapshot by content).
/// The convoy-level kind takes the first vessel's image (map order) only so
/// it holds a valid spec; when vessels' images differ, the returned per-vessel
/// placement pins are authoritative and every vessel reads its own snapshot.
fn apply_vessel_images(
    placement: &mut PlacementResolution,
    images: BTreeMap<String, FulfilmentKindSpec>,
    decision: Option<&PlacementDecision>,
) -> Result<BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>, String> {
    let Some(first) = images.values().next().cloned() else { return Ok(BTreeMap::new()) };
    if let Some(selected) = placement.selected.as_mut() {
        selected.spec = first.clone();
    }
    if images.values().all(|spec| *spec == first) {
        return Ok(BTreeMap::new());
    }
    let decision = decision.ok_or("vessels with different composed images need a placement decision")?;
    Ok(images.into_iter().map(|(vessel, spec)| (vessel, (spec, decision.clone()))).collect())
}

/// Resolve each vessel's composed image (ADR 0062): the project's `image:`
/// (else the cascaded installation defaults), the vessel's image needs and the
/// harness layers its crew roles use. Refusals name the vessel and the need.
async fn resolve_vessel_images(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    workflow: &WorkflowTemplateSpec,
) -> Result<BTreeMap<String, flotilla_resources::ResolvedImage>, String> {
    let selection = match project_ref {
        Some(project) => match backend.including_replicas::<Project>(namespace).get(project).await {
            Ok(project) => project.object.spec.image,
            // A named project that is absent must not silently compose from
            // installation defaults: that would build an image the project
            // never selected.
            Err(ResourceError::NotFound { .. }) => {
                return Err(format!("composed image: project `{project}` is not defined, so its image selection is unknown"))
            }
            Err(error) => return Err(error.to_string()),
        },
        None => None,
    };
    let defaults = match workflow.cascade.as_deref() {
        Some(cascade) => cascade.image_defaults.clone(),
        None => installation_image_defaults(backend, namespace, project_ref).await?,
    };
    let catalogue = crate::image_build::read_image_catalogue(backend, namespace).await?;
    let mut resolved = BTreeMap::new();
    for vessel in &workflow.vessels {
        let request =
            crate::image_build::vessel_image_request(project_ref.unwrap_or_default(), selection.as_ref(), defaults.as_ref(), vessel)?;
        let image = flotilla_resources::resolve_image(&catalogue, &request)
            .map_err(|error| format!("vessel `{}` composed image: {error}", vessel.name))?;
        resolved.insert(vessel.name.clone(), image);
    }
    Ok(resolved)
}

/// Installation image defaults outside a resolved workflow cascade: the
/// project's cascade when it is declared, else the fleet-root CrewDefaults.
pub async fn installation_image_defaults(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
) -> Result<Option<flotilla_resources::ImageSelection>, String> {
    use flotilla_resources::{CrewDefaults, FleetDesignation, FLEET_DESIGNATION_NAME};
    let defaults = flotilla_store::read_definition_catalog::<CrewDefaults>(backend, namespace).await?;
    let projects = flotilla_store::read_definition_catalog::<Project>(backend, namespace).await?;
    if let Some(project) = project_ref.and_then(|name| projects.iter().find(|project| project.metadata.name == name)) {
        let fleet = backend
            .definitions::<FleetDesignation>(namespace)
            .list_names([FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?;
        let hierarchy = flotilla_resources::ProjectHierarchy::from_declared(
            projects.iter().map(|project| (project.metadata.name.clone(), project.spec.parent.clone())).collect(),
            fleet.first().map(|designation| designation.spec.project.clone()),
        );
        let specs = projects.iter().map(|project| (project.metadata.name.clone(), project.spec.clone())).collect();
        let layers = defaults.iter().map(|defaults| (defaults.metadata.name.clone(), defaults.spec.clone())).collect::<Vec<_>>();
        return flotilla_resources::ResolvedCascade::from_specs(&hierarchy, &specs, &layers, &project.metadata.name, &project.spec)
            .map(|cascade| cascade.image_defaults)
            .map_err(|error| error.to_string());
    }
    Ok(defaults.into_iter().filter(|defaults| defaults.spec.project_ref.is_none()).find_map(|defaults| defaults.spec.image))
}

impl AdmissionRuntime<'_> {
    async fn resolve_convoy_skills(
        &self,
        cascade: &flotilla_resources::ResolvedCascade,
        intent: &flotilla_protocol::ConvoyStartIntent,
        workflow: &mut WorkflowTemplateSpec,
    ) -> Result<(), String> {
        use flotilla_resources::{resolve_skills, SkillCatalogEntry};
        let required = cascade.skill_layers.iter().any(|(_, refs)| !refs.is_empty()) || !intent.skills.is_empty();
        let catalog: Vec<SkillCatalogEntry> = if required {
            let bundle =
                self.discovery.env.get("FLOTILLA_SKILLS_DIR").ok_or_else(|| "skill admission requires FLOTILLA_SKILLS_DIR".to_string())?;
            let path = PathBuf::from(bundle).join(".flotilla-skill-catalog.json");
            let contents =
                tokio::fs::read_to_string(&path).await.map_err(|error| format!("read pinned skill catalog {}: {error}", path.display()))?;
            let catalog: Vec<SkillCatalogEntry> =
                serde_json::from_str(&contents).map_err(|error| format!("decode pinned skill catalog: {error}"))?;
            let manifest = tokio::fs::read_to_string(path.with_file_name(".flotilla-sources.json"))
                .await
                .map_err(|error| format!("read pinned skill sources: {error}"))?;
            flotilla_resources::crew_defaults::validate_catalog(
                &catalog,
                &serde_json::from_str(&manifest).map_err(|error| format!("decode pinned skill sources: {error}"))?,
            )?;
            catalog
        } else {
            Vec::new()
        };
        for crew in workflow.roles.iter_mut().chain(workflow.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew)) {
            if matches!(crew.source, CrewSource::Agent { .. }) {
                crew.skills =
                    resolve_skills(&catalog, &cascade.skills_for(&crew.role, &intent.skills)).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    async fn resolve_convoy_issue_snapshot(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        let issue = self.issue_port.fetch_issue_by_ref(reference).await?;
        if issue_snapshot_is_fresh(&issue) {
            Ok(issue)
        } else {
            Err(format!("issue {} snapshot is too stale to admit", reference.id))
        }
    }

    async fn admission_ai_utility(&self) -> Result<ProbedNamingUtility, String> {
        let environment = self.environment_manager.environment_bag(self.local_environment_id);
        let runner = self.environment_manager.environment_runner(self.local_environment_id);
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        probe_admission_ai_utility(&self.discovery.factories.ai_utilities, environment.as_ref(), self.config, &probe_root, runner).await
    }
}

impl AdmissionReadContext<'_> {
    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String> {
        let Some(placement) = placement else {
            return Ok(());
        };
        let target_host = &placement.target_host;

        let sources = self
            .backend
            .query_declared::<ResourceHost>(namespace, &StoreQuery::names([target_host.reference.to_string()], true))
            .await
            .map_err(|error| error.to_string())?;
        let matching_sources =
            sources.items.into_iter().filter(|source| source.object.metadata.name == target_host.reference.as_str()).collect::<Vec<_>>();
        let has_replica = matching_sources.iter().any(|source| matches!(source.provenance, ResourceProvenance::Replica { .. }));
        let is_host_targeted_placement =
            self.backend.clone().including_replicas::<FulfilmentKind>(namespace).get(&placement.kind).await.is_ok();
        if !has_replica && !is_host_targeted_placement {
            return Ok(());
        }

        let owns_target_identity = self.runtime.local_host.as_ref().is_some_and(|host_id| host_id == &target_host.reference);
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

    async fn start_placement_host(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        let (namespace, intent) = normalize_convoy_start_intent(namespace, intent)?;
        let project_ref = &intent.project_ref;
        let placement = if intent.placement_policy.is_some() {
            self.decide_capability_placement(
                &PlacementContext {
                    namespace: &namespace,
                    project_ref: Some(project_ref),
                    repositories: &[],
                    intent: &intent,
                    purpose: PlacementPurpose::Routing,
                },
                &WorkflowTemplateSpec::builder().build(),
                &BTreeSet::new(),
            )
            .await?
            .0
        } else {
            let project = self
                .backend
                .including_replicas::<Project>(&namespace)
                .get(project_ref)
                .await
                .map_err(|error| project_not_ready_error(&namespace, project_ref, error))?
                .object;
            let repositories =
                project_ops::snapshot_project_repositories_with_backend(&self.backend, &namespace, project_ref, None).await?;
            let (_, mut workflow) = self
                .load_placement_workflow(&namespace, project_ref, &project.spec, &repositories, &intent, PlacementPurpose::Routing)
                .await?;
            let mut roles = expand_allocation_roles(&mut workflow, &project.spec)?;
            let mut issues = Vec::new();
            for selector in &intent.issues {
                issues.push(self.resolve_convoy_issue(&namespace, &project, selector).await?);
            }
            self.compose_placement_needs(&namespace, &project.spec, &issues, &intent, &mut workflow, PlacementPurpose::Routing).await?;
            refresh_allocation_role_crews(&workflow, &mut roles)?;
            allocate_roles(&mut workflow, &roles)?;
            self.decide_vessel_placements(
                &PlacementContext {
                    namespace: &namespace,
                    project_ref: Some(project_ref),
                    repositories: &repositories,
                    intent: &intent,
                    purpose: PlacementPurpose::Routing,
                },
                &mut workflow,
            )
            .await?
            .0
        };
        let Some(policy) = placement.selected else {
            return Ok(None);
        };
        let target = placement_target_host(&self.backend, &namespace, &policy).await?;
        let actuator = placement_actuator_host_ref(&self.backend, &namespace, &target).await?;
        Ok((self.runtime.local_host.as_ref() != Some(&actuator)).then(|| flotilla_protocol::qualified_path::HostId::new(actuator.as_str())))
    }

    async fn load<'a>(
        owner: &'a ConvoyAdmission,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<AdmissionReadContext<'a>, String> {
        use flotilla_resources::{CrewDefaults, FleetDesignation, ImageBuild, ImageLayer, FLEET_DESIGNATION_NAME};
        use flotilla_store::StoreQuery;
        let backend = &owner.backend;
        let project_ref = required_admission_value(&intent.project_ref, "project")?;
        // All graph declarations participate in ancestor/cycle validation. Load
        // once and reuse the exact merged catalog for every decision in this request.
        let projects = flotilla_store::read_definition_catalog::<Project>(backend, namespace).await?;
        let project = projects
            .iter()
            .find(|project| project.metadata.name == project_ref)
            .ok_or_else(|| project_not_ready_error(namespace, project_ref, ResourceError::not_found(project_ref)))?;
        let fleet = backend
            .definitions::<FleetDesignation>(namespace)
            .list_names([FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?;
        let defaults = flotilla_store::read_definition_catalog::<CrewDefaults>(backend, namespace).await?;
        let hierarchy = flotilla_resources::ProjectHierarchy::from_declared(
            projects.iter().map(|project| (project.metadata.name.clone(), project.spec.parent.clone())).collect(),
            fleet.first().map(|designation| designation.spec.project.clone()),
        );
        let specs = projects.iter().map(|project| (project.metadata.name.clone(), project.spec.clone())).collect();
        let layers = defaults.iter().map(|defaults| (defaults.metadata.name.clone(), defaults.spec.clone())).collect::<Vec<_>>();
        let cascade = flotilla_resources::ResolvedCascade::from_specs(&hierarchy, &specs, &layers, project_ref, &project.spec)
            .map_err(|error| error.to_string())?;
        let repository_sources = backend
            .query_declared::<Repository>(
                namespace,
                &StoreQuery::names(project.spec.repositories.iter().map(|repository| repository.repo.to_string()), true),
            )
            .await
            .map_err(|error| error.to_string())?;
        let forge_names = repository_sources
            .items
            .iter()
            .filter_map(|source| match source.object.spec.identity() {
                flotilla_resources::RepositoryIdentity::Forge { forge_ref, .. } => Some(forge_ref.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let remote_hosts = repository_sources
            .items
            .iter()
            .filter_map(|source| source.object.spec.forge())
            .filter_map(|identity| {
                flotilla_resources::canonicalize_repo_url(&format!(
                    "{}/{}",
                    identity.service_url.trim_end_matches('/'),
                    identity.repository
                ))
                .ok()
            })
            .filter_map(|remote| remote.split_once("://").and_then(|(_, rest)| rest.split('/').next()).map(str::to_ascii_lowercase))
            .chain(project.spec.issue_source_bindings.iter().filter_map(|binding| {
                binding.source.service.split_once("://").and_then(|(_, rest)| rest.split('/').next()).map(str::to_ascii_lowercase)
            }))
            .collect();
        let forges = backend
            .definitions::<Forge>(namespace)
            .list_selected(StoreQuery {
                any_of: Some(vec![
                    StoreQuery::names(forge_names, true),
                    StoreQuery { field_values: BTreeMap::from([("spec.remote_hosts.*".into(), remote_hosts)]), ..Default::default() },
                ]),
                include_replicas: true,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let inherited = cascade.workflow(intent.standing_role.as_deref()).value.clone();
        let refs = [intent.workflow_ref.clone().unwrap_or(inherited), "single-agent-shepherd".into()];
        let mut workflow_names = BTreeSet::new();
        for reference in refs {
            workflow_names.insert(reference.clone());
            workflow_names.insert(flotilla_resources::current_builtin_workflow_name(&reference).to_string());
            for project in &cascade.project_chain {
                workflow_names.insert(flotilla_charter::ops_entry::materialized_workflow_name(project, &reference));
            }
        }
        let workflows =
            backend.definitions::<WorkflowTemplate>(namespace).list_names(workflow_names).await.map_err(|error| error.to_string())?;
        // Every kind is a placement candidate or a reported refusal;
        // every host supplies model/fulfilment facts for those candidate decisions.
        // Reuse these complete candidate catalogs across roles and vessels.
        let kinds = backend
            .query_declared::<FulfilmentKind>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        let hosts = backend
            .query_declared::<ResourceHost>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        let roles = workflows
            .iter()
            .flat_map(|workflow| workflow.spec.roles.iter().chain(workflow.spec.vessels.iter().flat_map(|vessel| &vessel.crew)))
            .map(|crew| crew.role.clone())
            .collect::<BTreeSet<_>>();
        let mut role_values = roles;
        role_values.insert(String::new());
        let grant_query = credential_grant_query(Some(project_ref), role_values);
        let grants = backend.query_declared::<CredentialGrant>(namespace, &grant_query).await.map_err(|error| error.to_string())?;
        let credential_names = grants
            .items
            .iter()
            .flat_map(|grant| grant.object.spec.credentials.iter().cloned())
            .chain(owner.config.load_daemon_config()?.credentials.forgejo.into_values())
            .collect::<BTreeSet<_>>();
        // Every consumer is selected by the name a grant references (#3907):
        // no adapter list decides which declarations admission reads.
        let credentials = backend
            .query_declared::<CredentialSpec>(namespace, &StoreQuery::names(credential_names, true))
            .await
            .map_err(|error| error.to_string())?;
        // Composed images resolve against every layer definition, and their
        // placement cost reads the per-identity builds.
        let image_layers = flotilla_store::read_definition_catalog::<ImageLayer>(backend, namespace).await?;
        let builds = backend
            .query_declared::<ImageBuild>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        let mut snapshot = flotilla_store::RequestSnapshot::new(backend, namespace)?;
        snapshot.insert_definitions::<Project>(projects)?;
        snapshot.insert_definitions::<FleetDesignation>(fleet)?;
        snapshot.insert_definitions::<CrewDefaults>(defaults)?;
        snapshot.insert_definitions::<Forge>(forges)?;
        snapshot.insert_definitions::<WorkflowTemplate>(workflows)?;
        snapshot.insert_definitions::<ImageLayer>(image_layers)?;
        snapshot.insert::<Repository>(repository_sources)?;
        snapshot.insert::<FulfilmentKind>(kinds)?;
        snapshot.insert::<CredentialGrant>(grants)?;
        snapshot.insert::<CredentialSpec>(credentials)?;
        let capped = hosts
            .items
            .iter()
            .filter(|host| host.object.spec.max_vessels.is_some())
            .map(|host| host.object.metadata.name.clone())
            .collect();
        let occupancy = host_occupancies(backend, namespace, &capped).await?;
        snapshot.insert::<ImageBuild>(builds)?;
        snapshot.insert::<ResourceHost>(hosts)?;
        let mut reads = owner.reads(snapshot.finish());
        reads.occupancy = occupancy;
        Ok(reads)
    }

    fn placed_here(&self, host: &str) -> u32 {
        self.placed_here.lock().expect("admission placement count lock poisoned").get(host).copied().unwrap_or(0)
    }

    /// Record a vessel this admission placed, so its later vessels count it.
    /// Both sides key by the chosen kind's canonical `host_ref`, the field
    /// the candidate loop reads, so they cannot drift apart.
    fn record_placed_vessel(&self, decision: &PlacementDecision) {
        let Some(chosen) = decision.allocation.as_ref().and_then(|allocation| {
            allocation.candidates.iter().find(|candidate| candidate.kind == allocation.chosen_kind && candidate.max_vessels.is_some())
        }) else {
            return;
        };
        *self.placed_here.lock().expect("admission placement count lock poisoned").entry(chosen.host.clone()).or_default() += 1;
    }

    async fn vessel_occupancy(&self, namespace: &str, host: &str) -> Result<flotilla_resources::HostOccupancy, String> {
        match self.occupancy.get(host) {
            Some(occupancy) => Ok(occupancy.clone()),
            None => host_occupancy(&self.backend, namespace, host).await,
        }
    }

    async fn resolve_continuation(
        &self,
        repositories: &[ConvoyRepositorySpec],
        continuation: &flotilla_protocol::ConvoyContinuation,
    ) -> Result<(RepositoryKey, String, Option<ResolvedConvoyChangeRequestAdmission>), String> {
        use flotilla_protocol::ConvoyContinuation;
        let keys = repositories.iter().map(|repo| repo.repo_ref.clone()).collect::<Vec<_>>();
        let (key, branch, id) = match continuation {
            ConvoyContinuation::ChangeRequest(id) => {
                let resolved = self.resolve_change_request_admission(&keys, required_admission_value(id, "change request")?, true).await?;
                return Ok((resolved.binding.repository_ref.clone(), resolved.branch.clone(), Some(resolved)));
            }
            ConvoyContinuation::Branch(branch) => {
                let [repository] = repositories else {
                    return Err(
                        "--continue-branch requires a single-repository project; use --continue-pr for an unambiguous repository".into()
                    );
                };
                validate_convoy_branch(branch)?;
                let (candidates, failures) = self.repository_change_request_candidates(&keys).await;
                if !failures.is_empty() {
                    return Err(failures.join("; "));
                }
                let mut id = None;
                for (_, _, provider) in candidates {
                    if let Some((found, request)) =
                        provider.find_change_request_by_branch(branch).await.map_err(|error| error.to_string())?
                    {
                        require_open_continuation(&request.status, &found)?;
                        id = Some(found);
                        break;
                    }
                }
                (repository.repo_ref.clone(), branch.clone(), id)
            }
        };
        let resolved = match id {
            Some(id) => {
                let resolved = self.resolve_change_request_admission(std::slice::from_ref(&key), &id, true).await?;
                if resolved.branch != branch {
                    return Err("change request head changed during continuation admission; retry".into());
                }
                Some(resolved)
            }
            None => None,
        };
        Ok((key, branch, resolved))
    }

    async fn resolve_convoy_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        self.resolve_change_request_admission(repository_keys, requested_id, false).await
    }

    async fn resolve_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
        require_open: bool,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        let (candidates, setup_failures) = self.repository_change_request_candidates(repository_keys).await;
        let mut failures = setup_failures
            .into_iter()
            .map(|error| AdmissionLookupFailure { context: None, error: ObservationError::Failed(error) })
            .collect::<Vec<_>>();
        let consulted = candidates.iter().map(|(_, scope, _)| scope.clone()).collect::<Vec<_>>();

        let mut matches = Vec::new();
        let mut matched_repositories = Vec::new();
        for (repository, scope, provider) in candidates {
            match provider.get_change_request_for_admission(requested_id).await {
                Ok(admission) => {
                    if require_open {
                        if let Err(error) = require_open_continuation(&admission.change_request.status, &admission.id) {
                            failures.push(AdmissionLookupFailure {
                                context: Some(format!("repository {scope}")),
                                error: ObservationError::Failed(error),
                            });
                            continue;
                        }
                    }
                    let Some(base_ref) = admission.base_ref else {
                        failures.push(AdmissionLookupFailure {
                            context: Some(format!("repository {scope}")),
                            error: ObservationError::Failed(format!("change request {} did not report a base ref", admission.id)),
                        });
                        continue;
                    };
                    matches.push(ResolvedConvoyChangeRequestAdmission {
                        binding: BoundChangeRequest { id: admission.id, repository_ref: repository, title: admission.change_request.title },
                        branch: admission.change_request.branch,
                        base_ref,
                    });
                    matched_repositories.push(scope);
                }
                Err(error) => failures.push(AdmissionLookupFailure { context: Some(format!("repository {scope}")), error }),
            }
        }

        let limited = failures.iter().any(|failure| matches!(failure.error, ObservationError::RateLimited { .. }));
        let failures = failures.into_iter().map(AdmissionLookupFailure::diagnostic).collect::<Vec<_>>();
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 if limited => Err(format!("change request {requested_id} lookup was rate limited: {}", failures.join("; "))),
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
        let namespace = self.runtime.namespace.clone();
        let repositories = self.backend.including_replicas::<Repository>(&namespace);
        let mut candidates = Vec::new();
        let mut failures = Vec::new();
        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository.object,
                Err(error) => {
                    failures.push(format!("repository {repository_key}: {error}"));
                    continue;
                }
            };
            let Some(identity) = repository.spec.forge() else {
                failures.push(format!("repository {repository_key}: no forge identity"));
                continue;
            };
            if let Some(cached) = self.runtime.repository_change_requests.read().await.get(repository_key) {
                if cached.service_url == identity.service_url && cached.repository == identity.repository {
                    candidates.push((repository_key.clone(), identity.repository.clone(), Arc::clone(&cached.provider)));
                    continue;
                }
            }
            let provider = match self.runtime.change_request_port.discover_repository_change_request(&namespace, &repository.spec).await {
                Ok(provider) => provider,
                Err(error) => {
                    failures.push(format!("repository {}: {error}", identity.repository));
                    continue;
                }
            };
            self.runtime.repository_change_requests.write().await.insert(
                repository_key.clone(),
                RepositoryChangeRequestProvider {
                    service_url: identity.service_url.clone(),
                    repository: identity.repository.clone(),
                    provider: Arc::clone(&provider),
                },
            );
            candidates.push((repository_key.clone(), identity.repository.clone(), provider));
        }
        (candidates, failures)
    }

    async fn resolve_convoy_issue(
        &self,
        namespace: &str,
        project: &ResourceObject<Project>,
        selector: &flotilla_protocol::IssueSelector,
    ) -> Result<ConvoyIssue, String> {
        let sources = match resolve_project_issue_sources(&self.backend.including_replicas::<Repository>(namespace), &project.spec).await {
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
                self.runtime
                    .resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef {
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
                self.runtime
                    .resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef { source: binding.source.clone(), id: id.clone() })
                    .await?
            }
            flotilla_protocol::IssueSelector::Id(id) => {
                if sources.len() != 1 {
                    return Err(format!(
                        "issue {id} requires an alias because project {} has {} issue sources",
                        project.metadata.name,
                        sources.len()
                    ));
                }
                self.runtime
                    .resolve_convoy_issue_snapshot(&flotilla_protocol::IssueRef { source: sources[0].source.clone(), id: id.clone() })
                    .await?
            }
        };

        let repositories = self.backend.including_replicas::<Repository>(namespace);
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

    async fn resolve_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        self.load_placement_workflow(namespace, project_ref, project, repositories, intent, PlacementPurpose::Admission).await
    }

    async fn load_placement_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
        purpose: PlacementPurpose,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        let mut cascade = flotilla_resources::ResolvedCascade::load(&self.backend, namespace, project_ref, project)
            .await
            .map_err(|error| error.to_string())?;
        let inherited = cascade.workflow(intent.standing_role.as_deref()).clone();
        let mut workflow_ref = match intent.workflow_ref.as_deref() {
            Some(workflow_ref) => required_admission_value(workflow_ref, "workflow")?.to_string(),
            None if intent.change_request.is_some() => "single-agent-shepherd".to_string(),
            None => inherited.value.clone(),
        };
        let override_layer = if intent.standing_role.is_some() { "convoy:ensure" } else { "dispatch" };
        cascade.set(
            "workflow",
            &workflow_ref,
            if intent.workflow_ref.is_some() {
                override_layer
            } else if intent.change_request.is_some() {
                "change-request"
            } else {
                &inherited.layer
            },
        );
        let templates = self.backend.definitions::<WorkflowTemplate>(namespace);
        let owners = cascade.project_chain.iter().rev();
        let mut found = None;
        for owner in owners {
            let scoped = flotilla_charter::ops_entry::materialized_workflow_name(owner, &workflow_ref);
            match templates.get(&scoped).await {
                Ok(workflow) => {
                    if workflow.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).is_some_and(|declared| declared != owner) {
                        return Err(format!("workflow template {scoped} is materialized by another project"));
                    }
                    found = Some(workflow);
                    break;
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        let mut workflow = match found {
            Some(workflow) => workflow,
            None => {
                workflow_ref = flotilla_resources::current_builtin_workflow_name(&workflow_ref).to_string();
                templates
                    .get(&workflow_ref)
                    .await
                    .map_err(|error| format!("workflow template {workflow_ref} for project {project_ref}: {error}"))?
            }
        };
        if workflow.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).is_some_and(|owner| !cascade.project_chain.contains(owner)) {
            return Err(format!("workflow template {workflow_ref} is materialized by another project"));
        }
        for crew in workflow.spec.roles.iter_mut().chain(workflow.spec.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew)) {
            if let CrewSource::Agent { selector, brief_template, .. } = &mut crew.source {
                let definition = cascade.roles.get(&crew.role).cloned().unwrap_or_default();
                for (field, target, default) in
                    [("agent", &mut selector.adapter, &definition.agent), ("model", &mut selector.model, &definition.model)]
                {
                    if let Some(value) = target.as_ref() {
                        cascade.set(&format!("roles.{}.{field}", crew.role), value, "convoy:workflow");
                    } else {
                        *target = default.clone();
                    }
                }
                if let Some(template) = brief_template {
                    cascade.set(&format!("roles.{}.brief_template", crew.role), template, "convoy:workflow");
                    cascade.roles.entry(crew.role.clone()).or_default().brief_template = None;
                }
            }
        }
        apply_agent_overrides(&mut workflow.spec, &intent.agent_overrides)?;
        for crew in workflow.spec.roles.iter().chain(workflow.spec.vessels.iter().flat_map(|vessel| &vessel.crew)) {
            if let CrewSource::Agent { selector, .. } = &crew.source {
                if intent.agent_overrides.iter().any(|choice| choice.capability == selector.capability) {
                    if let Some(agent) = &selector.adapter {
                        cascade.set(&format!("roles.{}.agent", crew.role), agent, override_layer);
                    }
                    if let Some(model) = &selector.model {
                        cascade.set(&format!("roles.{}.model", crew.role), model, override_layer);
                    } else {
                        cascade.settings.remove(&format!("roles.{}.model", crew.role));
                    }
                }
            }
        }
        if purpose == PlacementPurpose::Admission {
            self.runtime.resolve_convoy_skills(&cascade, intent, &mut workflow.spec).await?;
            validate_fork_workflow_admission(&self.backend, namespace, repositories, &workflow_ref, &workflow.spec).await?;
        }
        workflow.spec.cascade = Some(Box::new(cascade));
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
        self.compose_placement_needs(namespace, project, issues, intent, workflow, PlacementPurpose::Admission).await
    }

    async fn compose_placement_needs(
        &self,
        _namespace: &str,
        project: &ProjectSpec,
        issues: &[ConvoyIssue],
        intent: &flotilla_protocol::ConvoyStartIntent,
        workflow: &mut WorkflowTemplateSpec,
        purpose: PlacementPurpose,
    ) -> Result<BTreeSet<CapabilityNeed>, String> {
        let mut common = BTreeSet::new();
        for issue in issues {
            common.extend(issue.placement_needs().0);
        }
        for value in &intent.needs {
            common.insert(parse_ad_hoc_capability_need(value)?);
        }
        let mut union = BTreeSet::new();
        for vessel in &mut workflow.vessels {
            let mut vessel_needs = BTreeSet::new();
            for crew in &mut vessel.crew {
                crew.needs.extend(common.iter().cloned());
                if let Some(standing) = project.role_needs.get(&crew.role) {
                    crew.needs.extend(standing.iter().filter(|need| **need != CapabilityNeed::matrix_placeholder()).cloned());
                }
                if let CrewSource::Agent { selector, .. } = &crew.source {
                    if purpose == PlacementPurpose::Routing {
                        vessel_needs.extend(crew.needs.iter().cloned());
                        continue;
                    }
                    let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
                    if let Some(minimum) = minimum_harness_version(&requirement.adapter) {
                        crew.needs
                            .insert(CapabilityNeed::Harness { adapter: requirement.adapter.clone(), minimum_version: minimum.to_string() });
                    }
                    // Account/model entitlement is unknown. Host login probes,
                    // including persisted results, cannot authorize or refuse
                    // work using the credential selected by its actual grant.
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

    async fn decide_capability_placement(
        &self,
        context: &PlacementContext<'_>,
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        let PlacementContext { namespace, project_ref, repositories: _, intent, purpose } = *context;
        let pin = intent.placement_policy.as_deref();
        let escalation_reason = intent.escalation_reason.as_deref();
        // Admission's own snapshots are kinds too, frozen for their convoys; none is a candidate.
        let kinds = home_copy_wins_by_name(
            self.backend
                .query_declared::<FulfilmentKind>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
                .await
                .map_err(|error| error.to_string())?
                .items,
        )
        .into_iter()
        .filter(|kind| !is_prepared_snapshot(&kind.metadata.name, &kind.metadata.labels, PLACEMENT_SNAPSHOT_KIND))
        .collect::<Vec<_>>();
        if purpose == PlacementPurpose::Routing {
            if let Some(pin) = pin {
                let kind = kinds.into_iter().find(|kind| kind.metadata.name == pin);
                let kind = kind.ok_or_else(|| format!("fulfilment kind `{pin}` does not exist"))?;
                return Ok((PlacementResolution::selected(kind), Vec::new()));
            }
            if kinds.is_empty() {
                return Ok((PlacementResolution::none(), Vec::new()));
            }
        }
        if kinds.is_empty() {
            return Err(match needs.iter().next() {
                Some(need) => format!("no fulfilment kind covers role need `{need}`"),
                None => "no fulfilment kind is registered".to_string(),
            });
        }
        let hosts = home_copy_wins_by_name(
            self.backend
                .query_declared::<ResourceHost>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
                .await
                .map_err(|error| error.to_string())?
                .items,
        );
        if let Some(pin) = pin {
            if !kinds.iter().any(|kind| kind.metadata.name == pin) {
                return Err(format!("fulfilment kind `{pin}` does not exist"));
            }
        }
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
        let composed_images = tokio::sync::OnceCell::new();
        for mut kind in kinds {
            let canonical_kind_host = flotilla_resources::canonical_host_id(hosts.iter(), &kind.spec.host_ref);
            let target_ref = canonical_kind_host
                .as_ref()
                .ok()
                .and_then(Clone::clone)
                .unwrap_or_else(|| CanonicalHostId::resolved(kind.spec.host_ref.clone()));
            let host = hosts.iter().find(|host| host.metadata.name == target_ref.as_str());
            let target_host = PlacementTargetHost {
                reference: target_ref,
                display_name: host
                    .map(|host| host.spec.display_name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| kind.spec.host_ref.clone()),
            };
            let kind_name = kind.metadata.name.clone();
            let refusal = |reason: String| PlacementRefusal { kind: kind_name.clone(), target_host: target_host.clone(), reason };
            // #3907: a kind where some requester's access has no plan drops out.
            let environment_kind =
                flotilla_agent::account_access::EnvironmentKind::of_realisation(&kind.spec.realisation, host.is_some_and(is_ssh_host));
            if let Some(reason) = crate::account_access::plan_refusal(workflow, environment_kind)? {
                rejected.push(refusal(reason));
                continue;
            }
            let canonical_kind_host = match canonical_kind_host {
                Ok(host) => host,
                Err(error) => {
                    rejected.push(refusal(error));
                    continue;
                }
            };
            if let Some(canonical_kind_host) = &canonical_kind_host {
                kind.spec.host_ref = canonical_kind_host.to_string();
            }
            let vessel_cap = host.and_then(|host| host.spec.vessel_cap(&kind.spec.realisation));
            let single_tenant = host.is_some_and(|host| host.spec.single_tenant);
            if host.is_some_and(is_ssh_host) && vessel_cap.is_none() && pin != Some(kind.metadata.name.as_str()) {
                rejected.push(refusal(SSH_HOST_PIN_ONLY.to_string()));
                continue;
            }
            // A convoy waits until all its vessels on a host fit, so one
            // that needs more vessels there than the cap could never start.
            let placed_here = self.placed_here(&kind.spec.host_ref);
            let needed = workflow.vessels.len().max(1) as u32 + placed_here;
            if let Some(cap) = vessel_cap.filter(|cap| needed > *cap) {
                rejected.push(refusal(format!("convoy needs {needed} vessels here, more than the host's cap of {cap}")));
                continue;
            }
            let facts = host.and_then(|host| host.status.as_ref()).and_then(|status| status.fulfilment_facts.get(&kind.metadata.name));
            // A kind that omits its image realises each vessel's composed
            // image: resolution provides every image need or refuses, naming it.
            let composed = match &kind.spec.realisation {
                FulfilmentRealisation::DockerPerVessel { image: None } => {
                    match composed_images.get_or_init(|| resolve_vessel_images(&self.backend, namespace, project_ref, workflow)).await {
                        Ok(images) => Some(images),
                        Err(error) => {
                            rejected.push(refusal(error.clone()));
                            continue;
                        }
                    }
                }
                _ => None,
            };
            // Remote image testimony is checked inside the actual sandbox before
            // crew launch. Host privileges and other grants are never deferred.
            let cloud_probe = matches!(kind.spec.realisation, FulfilmentRealisation::CloudSandboxPerVessel { .. });
            let image_covers =
                |need: &CapabilityNeed| composed.is_some() && need.is_image_need() || cloud_probe && need.requires_guest_probe();
            let structurally_missing = needs
                .iter()
                .filter(|need| {
                    !image_covers(need)
                        && match need {
                            CapabilityNeed::GuiSession => !kind.spec.grants.contains(&FulfilmentGrant::gui_session()),
                            CapabilityNeed::Toolchain(_) | CapabilityNeed::Harness { .. } => false,
                            _ => !need.covered_by(&kind.spec.grants, None),
                        }
                })
                .collect::<Vec<_>>();
            if !structurally_missing.is_empty() {
                rejected.push(refusal(format!(
                    "uncovered {}",
                    structurally_missing.iter().map(|need| format!("`{need}`")).collect::<Vec<_>>().join(", ")
                )));
                continue;
            }
            if purpose == PlacementPurpose::Admission
                && facts.is_none()
                && composed.is_none()
                && !cloud_probe
                && needs
                    .iter()
                    .any(|need| matches!(need, CapabilityNeed::GuiSession | CapabilityNeed::Toolchain(_) | CapabilityNeed::Harness { .. }))
            {
                rejected.push(refusal(format!("facts not yet observed for kind {}", kind.metadata.name)));
                continue;
            }
            let missing = needs.iter().filter(|need| !image_covers(need) && !need.covered_by(&kind.spec.grants, facts)).collect::<Vec<_>>();
            if purpose == PlacementPurpose::Admission && !missing.is_empty() {
                rejected.push(refusal(format!(
                    "uncovered {}",
                    missing
                        .iter()
                        .map(|need| {
                            match need {
                                CapabilityNeed::Harness { adapter, .. } => {
                                    let observed = facts.and_then(|facts| facts.harnesses.get(adapter));
                                    format!(
                                        "`{need}` (observed {adapter} {})",
                                        observed.map_or("unknown", |harness| harness.version.as_str())
                                    )
                                }
                                _ => format!("`{need}`"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
                continue;
            }
            let mut selected = kind.clone();
            if purpose == PlacementPurpose::Admission {
                let checked = async {
                    resolve_placement_provider(&self.backend, namespace, &mut selected).await?;
                    // A literal image is checked now; composed images freeze per vessel once the
                    // placement is final.
                    if let FulfilmentRealisation::DockerPerVessel {
                        image: Some(image @ flotilla_resources::DockerImageSource::Literal(_)),
                    } = &selected.spec.realisation
                    {
                        image.resolve()?;
                    }
                    validate_workflow_agent_adapters(&self.backend, namespace, workflow, Some(&selected), true).await
                };
                if let Err(reason) = checked.await {
                    rejected.push(refusal(reason));
                    continue;
                }
            }
            // A capped SSH host's slots are counted afresh: the
            // published fact trails the admissions just made (#3793).
            let free_slots = match (vessel_cap, host, purpose) {
                (Some(cap), Some(host), PlacementPurpose::Admission) => Some(
                    self.vessel_occupancy(namespace, &host.metadata.name)
                        .await?
                        .free_slots(cap)
                        .saturating_sub(self.placed_here(&kind.spec.host_ref)),
                ),
                _ => facts.and_then(|facts| facts.free_vessel_slots),
            };
            let host_ready = host.and_then(|host| host.status.as_ref()).is_some_and(|status| {
                let mut status = status.clone();
                status.apply_heartbeat_readiness(self.runtime.clock.now());
                status.ready
            });
            let sleeping_until = host.and_then(|host| host.status.as_ref()).and_then(|status| status.sleeping_until);
            // A sleeping host may queue work for its wake-up time. A host
            // that is simply not ready cannot be admitted, even when it
            // is the only fulfilment that covers the requested needs.
            if purpose == PlacementPurpose::Admission {
                if let Some(reason) = unready_placement_refusal(
                    &kind.metadata.name,
                    &kind.spec.host_ref,
                    host.and_then(|host| host.status.as_ref()),
                    self.runtime.clock.now(),
                ) {
                    rejected.push(refusal(reason));
                    continue;
                }
            }
            let image_cost = image_placement_cost(
                &self.backend,
                namespace,
                host,
                crate::support::provider_instance(&selected),
                &kind.spec.realisation,
                composed.map(|images| images.values().map(|image| image.identity.as_str()).collect::<BTreeSet<_>>()),
                Arc::clone(self.runtime.lifecycle_reads),
            )
            .await?;
            candidates.push(KindCandidate {
                kind,
                placement: PlacementResolution::selected(selected),
                free_slots,
                vessel_cap,
                single_tenant,
                host_ready,
                sleeping_until,
                image_cost,
            });
        }
        if candidates.is_empty() {
            let role_needs = workflow
                .vessels
                .iter()
                .flat_map(|vessel| vessel.crew.iter())
                .flat_map(|crew| crew.needs.iter().map(move |need| format!("role {} need `{need}`", crew.role)))
                .collect::<Vec<_>>();
            let covered = if role_needs.is_empty() { "the workflow".to_string() } else { role_needs.join(", ") };
            return Err(format!(
                "no fulfilment kind covers {covered}; candidates: {}",
                rejected.iter().map(|refusal| format!("{}: {}", refusal.kind, refusal.reason)).collect::<Vec<_>>().join("; ")
            ));
        }
        let placement_tiebreak = self.runtime.fulfilment_decider.for_admission(needs, self.runtime.clock.now());
        // Hold scarce platform capacity only when unreserved capacity also covers the needs.
        let has_unreserved = candidates.iter().any(|candidate| !placement_tiebreak.reserved(candidate));
        let mut reserved = Vec::new();
        if pin.is_none() && has_unreserved {
            (candidates, reserved) = candidates.into_iter().partition(|candidate| !placement_tiebreak.reserved(candidate));
        }
        // A single-tenant host's reach ranks as contained (#4002).
        let ranked =
            |candidate: &KindCandidate| flotilla_resources::least_privilege_grants(&candidate.kind.spec.grants, candidate.single_tenant);
        let minimal = candidates
            .iter()
            .filter(|candidate| {
                let grants = ranked(candidate);
                !candidates.iter().any(|other| {
                    let other_grants = ranked(other);
                    other.kind.metadata.name != candidate.kind.metadata.name && other_grants.is_subset(&grants) && other_grants != grants
                })
            })
            .map(|candidate| candidate.kind.metadata.name.clone())
            .collect::<BTreeSet<_>>();
        candidates.sort_by(|left, right| placement_tiebreak.compare(left, right));
        let index = match pin {
            Some(pin) => candidates.iter().position(|candidate| candidate.kind.metadata.name == pin).ok_or_else(|| {
                format!(
                    "pinned fulfilment `{pin}` cannot cover vessel needs; candidates: {}",
                    rejected.iter().map(|refusal| format!("{}: {}", refusal.kind, refusal.reason)).collect::<Vec<_>>().join("; ")
                )
            })?,
            None => candidates.iter().position(|candidate| minimal.contains(&candidate.kind.metadata.name)).expect("nonempty minimal set"),
        };
        let chosen_kind = candidates[index].kind.metadata.name.clone();
        let selected_reserved = placement_tiebreak.reserved(&candidates[index]);
        let allocation = FulfilmentAllocation {
            chosen_kind,
            reservation_reason: (selected_reserved && !has_unreserved).then(|| "no unreserved capacity covers the needs".to_string()),
            candidates: candidates
                .iter()
                .chain(reserved.iter())
                .map(|candidate| FulfilmentAllocationCandidate {
                    kind: candidate.kind.metadata.name.clone(),
                    host: candidate.kind.spec.host_ref.clone(),
                    cost_class: candidate.kind.spec.cost_class.to_string(),
                    host_ready: candidate.host_ready,
                    sleeping_until: candidate.sleeping_until,
                    free_vessel_slots: candidate.free_slots,
                    max_vessels: candidate.vessel_cap,
                    single_tenant: candidate.single_tenant,
                    reserved_for_platform: placement_tiebreak.reserved(candidate),
                    minimal: minimal.contains(&candidate.kind.metadata.name),
                    available: placement_tiebreak.available(candidate),
                })
                .collect(),
        };
        let mut selected = candidates.remove(index);
        if purpose == PlacementPurpose::Admission
            && selected_reserved
            && has_unreserved
            && escalation_reason.is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(format!(
                "fulfilment `{}` reserves scarce platform capacity; supply --escalation-reason to pin it for work without a platform need",
                selected.kind.metadata.name
            ));
        }
        if purpose == PlacementPurpose::Admission
            && !minimal.contains(&selected.kind.metadata.name)
            && escalation_reason.is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(format!(
                "fulfilment `{}` exceeds minimal alternatives {}; supply --escalation-reason",
                selected.kind.metadata.name,
                minimal.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        let alternatives = minimal.iter().filter(|name| **name != selected.kind.metadata.name).cloned().collect::<Vec<_>>();
        for candidate in candidates {
            let policy = candidate.placement.selected.as_ref().expect("validated candidate has policy");
            let target_host = placement_target_host(&self.backend, namespace, policy).await?;
            selected.placement.viable_not_selected.push(PlacementViableCandidate {
                kind: candidate.kind.metadata.name.clone(),
                target_host,
                reason: if minimal.contains(&candidate.kind.metadata.name) { "minimal alternative" } else { "grants a strict superset" }
                    .to_string(),
            });
        }
        selected.placement.refused_candidates.extend(rejected);
        selected.placement.allocation = Some(allocation);
        Ok((selected.placement, alternatives))
    }

    async fn decide_vessel_placements(
        &self,
        context: &PlacementContext<'_>,
        workflow: &mut WorkflowTemplateSpec,
    ) -> Result<
        (
            PlacementResolution,
            Vec<String>,
            BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>,
            Vec<crate::image_build::PlannedImageBuild>,
        ),
        String,
    > {
        let PlacementContext { namespace, project_ref, repositories, intent, purpose } = *context;
        let mut vessel_placements = BTreeMap::new();
        let mut image_builds = Vec::new();
        let (placement, minimal_alternatives) = if !workflow.vessels.is_empty() {
            let mut first = None;
            let mut index = 0;
            while index < workflow.vessels.len() {
                let vessel = workflow.vessels[index].clone();
                let needs = vessel.needs.iter().chain(vessel.crew.iter().flat_map(|crew| &crew.needs)).cloned().collect::<BTreeSet<_>>();
                let mut one = WorkflowTemplateSpec { vessels: vec![vessel.clone()], ..workflow.clone() };
                let (mut resolution, alternatives) = match self.decide_capability_placement(context, &one, &needs).await {
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
                            if let Some(policy) = workflow.stall_nudges.shift_remove(&format!("{}/{}", vessel.name, part.crew[0].role)) {
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
                            vessel.crew.first().map_or(vessel.name.as_str(), |crew| crew.role.as_str()),
                            needs.iter().map(ToString::to_string).collect::<Vec<_>>().join(" + ")
                        ));
                    }
                };
                if purpose == PlacementPurpose::Admission {
                    resolve_and_validate_workflow_credentials_for_capability_admission(
                        &self.backend,
                        namespace,
                        project_ref,
                        repositories,
                        resolution.selected.as_ref(),
                        &mut one,
                    )
                    .await?;
                    let (images, builds) = self.runtime.builds.compose_placement_images(namespace, project_ref, &one, &resolution).await?;
                    apply_vessel_images(&mut resolution, images, None)?;
                    image_builds.extend(builds);
                }
                workflow.vessels[index] = one.vessels.remove(0);
                if let Some(selected) = resolution.selected.as_ref() {
                    let decision = PlacementDecision {
                        provider_instance: crate::support::provider_instance(selected).map(str::to_owned),
                        provider_workspace_root: crate::support::provider_workspace_root(&self.backend, namespace, selected).await?,
                        minimal_alternatives: alternatives.clone(),
                        escalation_reason: intent.escalation_reason.clone(),
                        kind: selected.metadata.name.clone(),
                        target_host: placement_target_host(&self.backend, namespace, selected).await?,
                        refused_candidates: resolution.refused_candidates.clone(),
                        viable_not_selected: resolution.viable_not_selected.clone(),
                        allocation: resolution.allocation.clone(),
                    };
                    self.record_placed_vessel(&decision);
                    vessel_placements.insert(vessel.name.clone(), (selected.spec.clone(), decision));
                }
                if first.is_none() {
                    first = Some((resolution, alternatives));
                }
                index += 1;
            }
            first.expect("nonempty vessels")
        } else {
            let needs = workflow
                .vessels
                .iter()
                .flat_map(|vessel| vessel.needs.iter().chain(vessel.crew.iter().flat_map(|crew| &crew.needs)))
                .cloned()
                .collect();
            let mut result = self.decide_capability_placement(context, workflow, &needs).await?;
            if purpose == PlacementPurpose::Admission {
                resolve_and_validate_workflow_credentials_for_capability_admission(
                    &self.backend,
                    namespace,
                    project_ref,
                    repositories,
                    result.0.selected.as_ref(),
                    workflow,
                )
                .await?;
                let (images, builds) = self.runtime.builds.compose_placement_images(namespace, project_ref, workflow, &result.0).await?;
                image_builds.extend(builds);
                let decision = match result.0.selected.as_ref() {
                    Some(selected) => Some(PlacementDecision {
                        provider_instance: crate::support::provider_instance(selected).map(str::to_owned),
                        provider_workspace_root: crate::support::provider_workspace_root(&self.backend, namespace, selected).await?,
                        minimal_alternatives: result.1.clone(),
                        escalation_reason: intent.escalation_reason.clone(),
                        kind: selected.metadata.name.clone(),
                        target_host: placement_target_host(&self.backend, namespace, selected).await?,
                        refused_candidates: result.0.refused_candidates.clone(),
                        viable_not_selected: result.0.viable_not_selected.clone(),
                        allocation: result.0.allocation.clone(),
                    }),
                    None => None,
                };
                vessel_placements.extend(apply_vessel_images(&mut result.0, images, decision.as_ref())?);
            }
            result
        };
        Ok((placement, minimal_alternatives, vessel_placements, image_builds))
    }

    async fn prepare_convoy_admission_with_preferences(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
        repositories: Option<&[RepositoryKey]>,
    ) -> Result<PreparedConvoyAdmission, String> {
        let project_ref = required_admission_value(&intent.project_ref, "project")?;
        let project = self
            .backend
            .clone()
            .including_replicas::<Project>(namespace)
            .get(project_ref)
            .await
            .map(|project| project.object)
            .map_err(|error| project_not_ready_error(namespace, project_ref, error))?;
        let mut repositories_snapshot =
            project_ops::snapshot_project_repositories_from_project(&self.backend, namespace, &project, repositories).await?;
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
        let forges = self.backend.definitions::<Forge>(namespace).list().await.map_err(|error| error.to_string())?;
        let forges = forges.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
        let repositories = self
            .backend
            .query_declared::<Repository>(
                namespace,
                &flotilla_store::StoreQuery::names(repositories_snapshot.iter().map(|repository| repository.repo_ref.to_string()), true),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut services = flotilla_discovery::observation_credentials::ObservationCredentials::repository_services(
            repositories.items.iter().map(|repository| &repository.object.spec),
            &forges,
        )?;
        if let IssueSourceResolution::Available { bindings } =
            resolve_project_issue_sources(&self.backend.including_replicas::<Repository>(namespace), &project.spec).await
        {
            services.extend(bindings.into_iter().map(|binding| binding.source.service));
        }
        let overrides = self.runtime.config.load_daemon_config()?.forge_overrides;
        for service in services {
            if !overrides.contains_key(service.trim_end_matches('/'))
                && flotilla_discovery::observation_credentials::ObservationCredentials::github_service(&service, &forges)
            {
                self.runtime.discovery.observation_credentials.require(self.runtime.discovery.runner.as_ref(), &service).await?;
            }
        }
        let mut normalized_intent = intent.clone();
        let mut continued_request = None;
        let continuation = if let Some(continuation) = &intent.continuation {
            if intent.branch.is_some() || intent.change_request.is_some() {
                return Err("continuation cannot be combined with --branch or --pr".into());
            }
            let (repository, branch, request) = self.resolve_continuation(&repositories_snapshot, continuation).await?;
            normalized_intent.branch = request.is_none().then_some(branch);
            normalized_intent.change_request = request.as_ref().map(|request| request.binding.id.clone());
            continued_request = request;
            Some(repository)
        } else {
            None
        };
        let intent = &normalized_intent;
        if intent.change_request.is_some() && intent.branch.is_some() {
            return Err("change request adoption derives the branch from --pr; do not also provide a branch".to_string());
        }
        if intent.change_request.is_some() && !intent.issues.is_empty() {
            return Err("change request adoption is PR-first; do not also provide issues".to_string());
        }
        let change_request = match intent.change_request.as_deref() {
            Some(id) => {
                let id = required_admission_value(id, "change request")?;
                let resolved = match continued_request.take() {
                    Some(resolved) => resolved,
                    None => {
                        self.resolve_convoy_change_request_admission(
                            &repositories_snapshot.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>(),
                            id,
                        )
                        .await?
                    }
                };
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
            self.resolve_workflow(namespace, project_ref, &project.spec, &repositories_snapshot, intent).await?;
        let exclusions = intent
            .promise_exclusions
            .iter()
            .map(|(owner, kind)| Ok(flotilla_resources::TemplatePromise { owner: owner.clone(), kind: kind.parse()? }))
            .collect::<Result<Vec<_>, String>>()?;
        workflow.exclude_template_promises(&exclusions)?;
        // Dispatchers name owners in the workflow they can read; allocation may
        // regroup or rename those vessels below.
        let authored_owners = flotilla_resources::promises::authored_owners(&workflow);
        let mut allocation_roles = expand_allocation_roles(&mut workflow, &project.spec)?;
        self.compose_convoy_needs(namespace, &project.spec, &issues, intent, &mut workflow).await?;
        refresh_allocation_role_crews(&workflow, &mut allocation_roles)?;
        let grant_sets =
            allocation_credential_grants(&self.backend, namespace, project_ref, &repositories_snapshot, &workflow.vessels).await?;
        for ((role, vessel), grant_set) in allocation_roles.iter_mut().zip(&workflow.vessels).zip(grant_sets) {
            let mut one = WorkflowTemplateSpec { vessels: vec![vessel.clone()], ..workflow.clone() };
            resolve_workflow_credentials(&self.backend, namespace, Some(project_ref), &repositories_snapshot, &mut one).await?;
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
            Some(match self.runtime.admission_ai_utility().await {
                Ok(ProbedNamingUtility { name, utility }) => utility
                    .generate_convoy_names(&context)
                    .await
                    .map(|names| (name.clone(), names))
                    .map_err(|error| naming_failure_summary(&name, &error)),
                Err(reason) => Err(reason),
            })
        } else {
            None
        };
        let (generated, naming) = resolve_admission_names(
            generated,
            &fallback_slug,
            intent.name.is_some(),
            if change_request.is_some() {
                "change-request admission uses deterministic naming"
            } else {
                "explicit name and branch supplied"
            },
        );

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
        let (placement, minimal_alternatives, vessel_placements, image_builds) = self
            .decide_vessel_placements(
                &PlacementContext {
                    namespace,
                    project_ref: Some(project_ref),
                    repositories: &repositories_snapshot,
                    intent,
                    purpose: PlacementPurpose::Admission,
                },
                &mut workflow,
            )
            .await?;
        refresh_crossed_handoffs(&mut workflow);
        flotilla_resources::validate(&workflow).map_err(|errors| {
            format!("allocated workflow invalid: {}", errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
        })?;
        // ADR 0061: dispatch promises freeze owners in the final, allocated
        // workflow, which is the snapshot Bootstrap freezes and seeds from.
        let promises = flotilla_resources::promises::resolve_dispatch_promises(&workflow, &authored_owners, &intent.promises)?;
        let placement_policy = placement.selected.as_ref().map(|placement| placement.metadata.name.clone());
        let placement_decision = match placement.selected.as_ref() {
            Some(selected) => Some(PlacementDecision {
                provider_instance: crate::support::provider_instance(selected).map(str::to_owned),
                provider_workspace_root: crate::support::provider_workspace_root(&self.backend, namespace, selected).await?,
                minimal_alternatives,
                escalation_reason: intent.escalation_reason.clone(),
                kind: selected.metadata.name.clone(),
                target_host: placement_target_host(&self.backend, namespace, selected).await?,
                refused_candidates: placement.refused_candidates,
                viable_not_selected: placement.viable_not_selected,
                allocation: placement.allocation,
            }),
            None => None,
        };
        let spec = ConvoySpec {
            continuation,
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
            promises,
        };
        Ok(PreparedConvoyAdmission::builder()
            .evidence(self.backend.clone())
            .name(String::new())
            .maybe_naming(naming)
            .spec(spec)
            .workflow(workflow)
            .maybe_placement_kind(placement.selected.map(|placement| placement.spec))
            .maybe_placement_decision(placement_decision)
            .vessel_placements(vessel_placements)
            .image_builds(image_builds)
            .build())
    }
}

#[derive(bon::Builder)]
pub struct ConvoyStartTask {
    pub command_id: u64,
    pub intent: flotilla_protocol::ConvoyStartIntent,
    pub key: ConvoyStartKey,
    pub dispatching_principal_ref: PrincipalRef,
}

#[derive(bon::Builder)]
pub struct PreparedConvoyAdmission {
    pub naming: Option<ConvoyNaming>,
    // Reuse the exact host/policy evidence of this request through final capacity
    // validation; it is neither stored nor reused by another request.
    evidence: ResourceBackend,

    pub name: String,
    pub spec: ConvoySpec,
    pub workflow: WorkflowTemplateSpec,
    pub placement_kind: Option<FulfilmentKindSpec>,
    pub placement_decision: Option<PlacementDecision>,
    #[builder(default)]
    pub vessel_placements: BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>,
    /// Builds for the composed images, created with the convoy.
    #[builder(default)]
    pub image_builds: Vec<crate::image_build::PlannedImageBuild>,
}

#[derive(Clone, Debug)]
pub struct AllocationRole {
    pub crew: CrewSpec,
    pub hint: String,
    pub repository_refs: Option<Vec<RepositoryKey>>,
    pub depends_on: Vec<String>,
    pub credential_signature: String,
    /// The authored leg's needs (ADR 0062); roles only share a vessel with
    /// roles from legs with equal needs.
    pub leg_needs: BTreeSet<CapabilityNeed>,
}

fn refresh_allocation_role_crews(workflow: &WorkflowTemplateSpec, roles: &mut [AllocationRole]) -> Result<(), String> {
    if roles.len() != workflow.vessels.len() {
        return Err("expanded roles and vessels must have the same count".to_string());
    }
    for (role, vessel) in roles.iter_mut().zip(&workflow.vessels) {
        role.crew = vessel.crew.first().ok_or_else(|| format!("vessel `{}` has no crew", vessel.name))?.clone();
    }
    Ok(())
}

pub fn expand_allocation_roles(workflow: &mut WorkflowTemplateSpec, project: &ProjectSpec) -> Result<Vec<AllocationRole>, String> {
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
            let matrix = crew.needs.contains(&CapabilityNeed::matrix_placeholder())
                || project.role_needs.get(&crew.role).is_some_and(|needs| needs.contains(&CapabilityNeed::matrix_placeholder()));
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
                    expanded.needs.remove(&CapabilityNeed::matrix_placeholder());
                    expanded.needs.insert(need);
                    if let Some(standing) = project.role_needs.get(&crew.role) {
                        expanded.needs.extend(standing.iter().filter(|need| **need != CapabilityNeed::matrix_placeholder()).cloned());
                    }
                    roles.push(AllocationRole {
                        crew: expanded,
                        hint: format!("{}[{platform}]", crew.role),
                        repository_refs: vessel.repository_refs.clone().or_else(|| workflow.repository_refs.clone()),
                        depends_on: vessel.depends_on.clone(),
                        credential_signature: String::new(),
                        leg_needs: vessel.needs.clone(),
                    });
                }
            } else {
                roles.push(AllocationRole {
                    crew,
                    hint: vessel.name.clone(),
                    repository_refs: vessel.repository_refs.clone().or_else(|| workflow.repository_refs.clone()),
                    depends_on: vessel.depends_on.clone(),
                    credential_signature: String::new(),
                    leg_needs: vessel.needs.clone(),
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
                .needs(role.leg_needs.clone())
                .build()
        })
        .collect();
    Ok(roles)
}

pub fn allocate_roles(workflow: &mut WorkflowTemplateSpec, roles: &[AllocationRole]) -> Result<(), String> {
    let mut groups: Vec<Vec<&AllocationRole>> = Vec::new();
    for role in roles {
        let group = groups.iter_mut().find(|group| {
            let first = group[0];
            (first.crew.needs == role.crew.needs
                || (first.hint == role.hint
                    && first.crew.needs.iter().any(|need| role.crew.needs.iter().any(|other| need.conflicts_with(other)))))
                && first.credential_signature == role.credential_signature
                && first.repository_refs == role.repository_refs
                && first.leg_needs == role.leg_needs
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
                .needs(group[0].leg_needs.clone())
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

pub fn refresh_crossed_handoffs(workflow: &mut WorkflowTemplateSpec) {
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

pub async fn allocation_credential_grants(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: &str,
    repositories: &[ConvoyRepositorySpec],
    vessels: &[VesselRequirement],
) -> Result<Vec<BTreeSet<String>>, String> {
    let grants = backend
        .query_declared::<CredentialGrant>(
            namespace,
            &credential_grant_query(
                Some(project_ref),
                vessels.iter().flat_map(|vessel| vessel.crew.iter().map(|crew| crew.role.clone())).chain([String::new()]).collect(),
            ),
        )
        .await
        .map_err(|error| format!("list credential grants: {error}"))?;
    let repository_trust = backend
        .query_declared::<Repository>(
            namespace,
            &StoreQuery::names(
                repositories
                    .iter()
                    .map(|repository| repository.repo_ref.to_string())
                    .chain(vessels.iter().flat_map(|vessel| vessel.repository_refs.iter().flatten().map(ToString::to_string))),
                true,
            ),
        )
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

fn brief_convoy_query(
    project: &str,
    name: &str,
    workflow: &WorkflowTemplateSpec,
    projects: &[ResourceObject<Project>],
    defaults: &[ResourceObject<flotilla_resources::CrewDefaults>],
    fleet: &[ResourceObject<flotilla_resources::FleetDesignation>],
) -> Result<(StoreQuery, BTreeSet<String>), String> {
    let hierarchy = flotilla_resources::ProjectHierarchy::from_declared(
        projects.iter().map(|object| (object.metadata.name.clone(), object.spec.parent.clone())).collect(),
        fleet.first().map(|object| object.spec.project.clone()),
    );
    let cascade = match projects.iter().find(|object| object.metadata.name == project) {
        Some(object) => Some(
            flotilla_resources::ResolvedCascade::from_specs(
                &hierarchy,
                &projects.iter().map(|object| (object.metadata.name.clone(), object.spec.clone())).collect(),
                &defaults.iter().map(|object| (object.metadata.name.clone(), object.spec.clone())).collect::<Vec<_>>(),
                project,
                &object.spec,
            )
            .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    let subtree = hierarchy.fleet() == Some(project)
        || cascade.as_ref().is_some_and(|cascade| {
            workflow.vessels.iter().flat_map(|vessel| &vessel.crew).any(|crew| {
                cascade
                    .roles
                    .get(&crew.role)
                    .is_some_and(|role| role.subscriptions.iter().flatten().any(|subscription| subscription.subtree))
            })
        });
    let mut visible = BTreeSet::from([project.to_string()]);
    if projects.iter().any(|object| object.metadata.name == project) {
        visible.extend(hierarchy.ancestors(project).map_err(|error| error.to_string())?);
        if subtree {
            visible.extend(hierarchy.descendants(project).map_err(|error| error.to_string())?);
        }
    }
    let convoy_scope = if subtree {
        StoreQuery {
            field_values: BTreeMap::from([("spec.project_ref".into(), visible.clone().into_iter().collect())]),
            any_of: Some(vec![
                StoreQuery { missing_fields: ["status.phase".into()].into(), ..Default::default() },
                StoreQuery {
                    field_values: BTreeMap::from([(
                        "status.phase".into(),
                        ["Pending", "Active", "Interrupted", "Landing"].into_iter().map(str::to_string).collect(),
                    )]),
                    ..Default::default()
                },
            ]),
            include_replicas: true,
            ..Default::default()
        }
    } else {
        StoreQuery::names([name.into()], true)
    };
    let convoy_query = StoreQuery {
        any_of: Some(vec![
            convoy_scope.clone(),
            StoreQuery {
                include_replicas: true,
                ..StoreQuery::related_kind(
                    "metadata.name",
                    "ConvoyEnsure",
                    "status.convoy_ref",
                    StoreQuery {
                        field_values: BTreeMap::from([("spec.project_ref".into(), visible.clone().into_iter().collect())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
            },
        ]),
        include_replicas: true,
        ..Default::default()
    };
    Ok((convoy_query, visible))
}

fn convoy_identity_query(project: Option<&str>, role: &str) -> flotilla_store::StoreQuery {
    let mut query = flotilla_store::StoreQuery {
        fields: BTreeMap::from([("spec.role".into(), role.into())]),
        include_replicas: true,
        ..Default::default()
    };
    match project {
        Some(project) => {
            query.fields.insert("spec.project_ref".into(), project.into());
        }
        None => {
            query.missing_fields.insert("spec.project_ref".into());
        }
    }
    query
}

async fn convoy_admission_inventory(
    backend: &ResourceBackend,
    namespace: &str,
    spec: &ConvoySpec,
    ensure: Option<&str>,
    extra: Option<StoreQuery>,
) -> Result<Vec<ReadResourceObject<ResourceConvoy>>, String> {
    use flotilla_store::StoreQuery;
    let mut selections = vec![convoy_identity_query(spec.project_ref.as_deref(), &spec.role)];
    selections.extend(extra);
    if let Some(ensure) = ensure {
        selections.push(StoreQuery {
            fields: BTreeMap::from([("metadata.annotations.\"flotilla.work/ensured-from\"".into(), ensure.into())]),
            ..Default::default()
        });
    }
    if let Some(repository) = &spec.continuation {
        let mut branch =
            StoreQuery { fields: BTreeMap::from([("spec.repositories.*.repo_ref".into(), repository.to_string())]), ..Default::default() };
        match &spec.r#ref {
            Some(reference) => {
                branch.fields.insert("spec.ref".into(), reference.clone());
            }
            None => {
                branch.missing_fields.insert("spec.ref".into());
            }
        }
        selections.push(branch);
        let subjects = spec
            .declared_subjects()?
            .into_iter()
            .filter(|entry| entry.relationship == flotilla_protocol::Relationship::Produces)
            .map(|entry| serde_json::to_value(entry.subject).map(|value| value.to_string()).map_err(|error| error.to_string()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        for path in ["spec.subjects.*.subject", "status.subjects.*.subject"] {
            selections.push(StoreQuery {
                field_values: BTreeMap::from([(path.into(), subjects.clone().into_iter().collect())]),
                ..Default::default()
            });
        }
    }
    backend
        .query_declared::<ResourceConvoy>(
            namespace,
            &StoreQuery { any_of: (!selections.is_empty()).then_some(selections), include_replicas: true, ..Default::default() },
        )
        .await
        .map(|listed| listed.items)
        .map_err(|error| error.to_string())
}

pub async fn allocate_convoy_generation(
    backend: &ResourceBackend,
    namespace: &str,
    project: Option<&str>,
    role: &str,
) -> Result<u64, String> {
    let generations = backend
        .query_declared::<ResourceConvoy>(namespace, &convoy_identity_query(project, role))
        .await
        .map_err(|error| error.to_string())?;
    allocate_convoy_generation_from_inventory(&generations.items, project, role)
}

/// The identity guard and continuation guard share a single selected inventory
/// taken under the admission lock. Neither performs reads in a candidate loop.
fn allocate_convoy_generation_from_inventory(
    generations: &[ReadResourceObject<ResourceConvoy>],
    project: Option<&str>,
    role: &str,
) -> Result<u64, String> {
    let mut maximum = 0;
    for source in
        generations.iter().filter(|source| source.object.spec.project_ref.as_deref() == project && source.object.spec.role == role)
    {
        let convoy = &source.object;
        let generation =
            convoy.metadata.labels.get(GENERATION_LABEL).and_then(|value| value.parse::<u64>().ok()).unwrap_or(convoy.spec.generation);
        maximum = maximum.max(generation);
        let live = convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal());
        if live {
            let provenance = match &source.provenance {
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

#[derive(Clone, Copy)]
struct BriefEvidence<'a> {
    definitions: &'a ResourceBackend,
    convoys: &'a [ReadResourceObject<ResourceConvoy>],
}

enum BriefReadSource<'a> {
    Load,
    Prepared(BriefEvidence<'a>),
}

struct ConvoySnapshotBundle<'a> {
    pub spec: &'a ConvoySpec,
    pub workflow: &'a WorkflowTemplateSpec,
    pub placement: Option<&'a FulfilmentKindSpec>,
    pub placement_decision: Option<PlacementDecision>,
    pub vessel_placements: Option<&'a BTreeMap<String, (FulfilmentKindSpec, PlacementDecision)>>,
    /// Composed images' builds, created only with the convoy (refusals before
    /// this point never leave builds behind).
    pub image_builds: &'a [crate::image_build::PlannedImageBuild],
    evidence: BriefEvidence<'a>,
}

/// The harness adapter whose crews on an SSH host take its owner's central login (#4037).
const CODEX_ADAPTER: &str = "codex";

/// An issue body is the crew's contract, so admission may only reuse a
/// recently observed snapshot. Keep this deliberately fixed until an
/// operational need establishes that it should be configurable.
const ISSUE_SNAPSHOT_FRESHNESS: ChronoDuration = ChronoDuration::minutes(5);
pub fn issue_snapshot_is_fresh(issue: &flotilla_protocol::Issue) -> bool {
    let Some(observed_at) = issue.observed_at else { return false };
    let age = Utc::now().signed_duration_since(observed_at);
    (ChronoDuration::zero()..=ISSUE_SNAPSHOT_FRESHNESS).contains(&age)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConvoyStartKey {
    namespace: String,
    project_ref: String,
    subject: ConvoyStartSubject,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ConvoyStartSubject {
    Continuation(flotilla_protocol::ConvoyContinuation),
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
    pub fn new(namespace: String, intent: &flotilla_protocol::ConvoyStartIntent) -> Self {
        let subject = if let Some(continuation) = &intent.continuation {
            ConvoyStartSubject::Continuation(continuation.clone())
        } else if let Some(change_request) = &intent.change_request {
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

pub struct ResolvedConvoyChangeRequestAdmission {
    pub binding: BoundChangeRequest,
    pub branch: String,
    pub base_ref: String,
}

pub struct RepositoryChangeRequestProvider {
    pub service_url: String,
    pub repository: String,
    pub provider: Arc<dyn ChangeRequestTracker>,
}

#[derive(Debug)]
pub struct PlacementResolution {
    pub selected: Option<ResourceObject<FulfilmentKind>>,
    pub refused_candidates: Vec<PlacementRefusal>,
    pub viable_not_selected: Vec<PlacementViableCandidate>,
    pub allocation: Option<FulfilmentAllocation>,
}

impl PlacementResolution {
    pub fn none() -> Self {
        Self { selected: None, refused_candidates: Vec::new(), viable_not_selected: Vec::new(), allocation: None }
    }

    pub fn selected(kind: ResourceObject<FulfilmentKind>) -> Self {
        Self { selected: Some(kind), ..Self::none() }
    }
}

pub struct KindCandidate {
    pub kind: ResourceObject<FulfilmentKind>,
    pub placement: PlacementResolution,
    pub free_slots: Option<u32>,
    /// The host's declared vessel cap for this kind's realisation (#3793, #4002).
    pub vessel_cap: Option<u32>,
    /// The host is declared dedicated and single-tenant (#4002).
    pub single_tenant: bool,
    pub host_ready: bool,
    pub sleeping_until: Option<DateTime<Utc>>,
    pub image_cost: flotilla_resources::ImageAcquisitionCost,
}

pub trait PlacementCandidateOrder: Send + Sync {
    fn compare(&self, left: &KindCandidate, right: &KindCandidate) -> std::cmp::Ordering;
    fn reserved(&self, candidate: &KindCandidate) -> bool;
    fn available(&self, candidate: &KindCandidate) -> bool;
}

pub trait FulfilmentDecider: Send + Sync {
    fn for_admission<'a>(&'a self, needs: &'a BTreeSet<CapabilityNeed>, now: DateTime<Utc>) -> Box<dyn PlacementCandidateOrder + 'a>;
}

pub struct StaticFulfilmentDecider;

impl FulfilmentDecider for StaticFulfilmentDecider {
    fn for_admission<'a>(&'a self, needs: &'a BTreeSet<CapabilityNeed>, now: DateTime<Utc>) -> Box<dyn PlacementCandidateOrder + 'a> {
        Box::new(PlacementTieBreak { needs, now })
    }
}

pub struct PlacementTieBreak<'a> {
    pub needs: &'a BTreeSet<CapabilityNeed>,
    pub now: DateTime<Utc>,
}

/// Free share of a host's vessel cap; uncapped hosts count as fully free.
#[derive(Clone, Copy, Eq)]
struct FreeFraction {
    free: u64,
    cap: u64,
}

impl FreeFraction {
    fn of(candidate: &KindCandidate) -> Self {
        match candidate.vessel_cap.zip(candidate.free_slots) {
            Some((cap, free)) => Self { free: u64::from(free.min(cap)), cap: u64::from(cap.max(1)) },
            None => Self { free: 1, cap: 1 },
        }
    }
}

impl PartialEq for FreeFraction {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Ord for FreeFraction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.free * other.cap).cmp(&(other.free * self.cap))
    }
}

impl PartialOrd for FreeFraction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PlacementTieBreak<'_> {
    pub fn reserved(&self, candidate: &KindCandidate) -> bool {
        candidate.kind.spec.grants.iter().any(|grant| {
            let Some(platform) = grant.0.strip_prefix("platform:") else { return false };
            platform.parse::<Platform>().is_ok_and(Platform::is_reserved)
                && !self.needs.contains(&CapabilityNeed::Platform(platform.to_string()))
        })
    }

    fn available(&self, candidate: &KindCandidate) -> bool {
        candidate.host_ready
            && candidate.sleeping_until.is_none_or(|until| until <= self.now)
            && candidate.free_slots.is_none_or(|slots| slots > 0)
    }
}

impl PlacementCandidateOrder for PlacementTieBreak<'_> {
    fn reserved(&self, candidate: &KindCandidate) -> bool {
        PlacementTieBreak::reserved(self, candidate)
    }

    fn available(&self, candidate: &KindCandidate) -> bool {
        PlacementTieBreak::available(self, candidate)
    }

    fn compare(&self, left: &KindCandidate, right: &KindCandidate) -> std::cmp::Ordering {
        let key = |candidate: &KindCandidate| {
            (
                !self.available(candidate),
                candidate.image_cost,
                candidate.kind.spec.cost_class,
                Reverse(FreeFraction::of(candidate)),
                candidate.kind.metadata.name.clone(),
            )
        };
        key(left).cmp(&key(right))
    }
}

pub fn required_admission_value<'a>(value: &'a str, field: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value)
    }
}

pub fn resolve_project_ref(default_namespace: &str, value: &str) -> Result<(String, String), String> {
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

pub fn normalize_convoy_start_intent(
    default_namespace: &str,
    intent: &flotilla_protocol::ConvoyStartIntent,
) -> Result<(String, flotilla_protocol::ConvoyStartIntent), String> {
    let (namespace, project_ref) = resolve_project_ref(default_namespace, &intent.project_ref)?;
    let mut intent = intent.clone();
    intent.namespace = Some(namespace.clone());
    intent.project_ref = project_ref;
    Ok((namespace, intent))
}

pub fn project_not_ready_error(namespace: &str, project_ref: &str, error: ResourceError) -> String {
    match error {
        ResourceError::NotFound { name } => {
            format!("project {namespace}/{project_ref} is not ready: resource not found: {name} (tried {namespace}/{project_ref})")
        }
        error => format!("project {project_ref} is not ready: {error}"),
    }
}

pub fn workflow_has_in_crew_review(workflow: &WorkflowTemplateSpec) -> bool {
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

pub async fn validate_fork_workflow_admission(
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

pub async fn validate_workflow_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<FulfilmentKind>>,
    allow_unready: bool,
) -> Result<(), String> {
    let required_adapters = required_workflow_agent_adapters(workflow)?;
    // Cloud images are verified through interior discovery before crew launch.
    // The controller cannot run an image-local probe for a remote sandbox.
    if placement.is_some_and(|kind| matches!(kind.spec.realisation, FulfilmentRealisation::CloudSandboxPerVessel { .. })) {
        return Ok(());
    }
    // A composed image derives its harness layers from these same roles.
    if placement.is_some_and(|kind| {
        matches!(
            kind.spec.realisation,
            FulfilmentRealisation::DockerPerVessel { image: None | Some(flotilla_resources::DockerImageSource::Built { .. }) }
        )
    }) {
        return Ok(());
    }
    // Validate each candidate's literal image once, even for tool-only workflows.
    let capabilities = match placement {
        Some(policy)
            if !required_adapters.is_empty() || matches!(policy.spec.realisation, FulfilmentRealisation::DockerPerVessel { .. }) =>
        {
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

/// One declared candidate query shared by admission and credential resolution.
/// Empty selector sets are wildcards; detailed repository trust remains a join.
pub(crate) fn credential_grant_query(project: Option<&str>, roles: BTreeSet<String>) -> StoreQuery {
    let mut projects = vec![StoreQuery { missing_fields: ["spec.selector.projects.*".into()].into(), ..Default::default() }];
    if let Some(project) = project {
        projects.push(StoreQuery { fields: BTreeMap::from([("spec.selector.projects.*".into(), project.into())]), ..Default::default() });
    }
    let roles = [
        StoreQuery { missing_fields: ["spec.selector.roles.*".into()].into(), ..Default::default() },
        StoreQuery { field_values: BTreeMap::from([("spec.selector.roles.*".into(), roles.into_iter().collect())]), ..Default::default() },
    ];
    let mut alternatives = Vec::new();
    for project in projects {
        for role in &roles {
            let mut query = project.clone();
            query.missing_fields.extend(role.missing_fields.clone());
            query.field_values.extend(role.field_values.clone());
            alternatives.push(query);
        }
    }
    StoreQuery {
        missing_fields: ["spec.selector.host_action".into()].into(),
        any_of: (!alternatives.is_empty()).then_some(alternatives),
        include_replicas: true,
        ..Default::default()
    }
}

pub async fn resolve_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    let grants = backend
        .query_declared::<CredentialGrant>(
            namespace,
            &credential_grant_query(
                project_ref,
                workflow
                    .vessels
                    .iter()
                    .flat_map(|vessel| vessel.crew.iter().map(|crew| crew.role.clone()))
                    .chain([String::new()])
                    .collect(),
            ),
        )
        .await
        .map_err(|error| format!("list credential grants: {error}"))?
        .items;
    let specs = backend
        .query_declared::<CredentialSpec>(
            namespace,
            &StoreQuery::names(
                grants
                    .iter()
                    .flat_map(|grant| grant.object.spec.credentials.iter().cloned())
                    .chain(workflow.vessels.iter().flat_map(|vessel| vessel.credential_refs.iter().cloned())),
                true,
            ),
        )
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .map(|source| (source.object.metadata.name, source.object.spec.consumer))
        .collect::<BTreeMap<_, _>>();
    let all_repositories = repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<BTreeSet<_>>();
    let repository_definitions = backend
        .query_declared::<Repository>(
            namespace,
            &StoreQuery::names(
                repositories
                    .iter()
                    .map(|repository| repository.repo_ref.to_string())
                    .chain(workflow.vessels.iter().flat_map(|vessel| vessel.repository_refs.iter().flatten().map(ToString::to_string))),
                true,
            ),
        )
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
        let matching_grants = grants
            .iter()
            .filter(|source| {
                (vessel.crew.is_empty() && source.object.spec.selector.matches(project_ref, &repository_trust, ""))
                    || vessel.crew.iter().any(|crew| source.object.spec.selector.matches(project_ref, &repository_trust, &crew.role))
            })
            .collect::<Vec<_>>();
        flotilla_resources::validate_matching_grant_permissions(
            matching_grants.iter().map(|grant| (grant.object.metadata.name.as_str(), &grant.object.spec)),
        )
        .map_err(|error| format!("vessel `{}`: {error}", vessel.name))?;
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
        let mut granted = matching_grants.iter().flat_map(|grant| grant.object.spec.credentials.iter().cloned()).collect::<BTreeSet<_>>();
        if let Some(missing) = granted.iter().find(|name| !specs.contains_key(*name)) {
            return Err(format!("credential grant references missing credential `{missing}`"));
        }
        // A harness-bound credential (claude-max, codex) is delivered only to a
        // vessel whose agents use that harness: a grant selects whole projects
        // and roles, and its other vessels' images lack the consumer binary.
        let vessel_adapters =
            flotilla_agent::adapter::required_agent_adapters(&vessel.crew).map_err(|error| format!("vessel `{}`: {error}", vessel.name))?;
        // A credential that supplies a registered account access (a Claude
        // subscription) reaches a requester only through its binding (#3907),
        // never as a vessel-wide reference.
        let account_access = flotilla_agent::account_access::AccountProvisioners::seeded();
        granted.retain(|name| {
            specs.get(name).is_some_and(|consumer| !account_access.supplies_any(consumer))
                && specs.get(name).and_then(CredentialConsumer::harness_adapter).is_none_or(|adapter| vessel_adapters.contains(adapter))
        });
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
            for credential in grant.object.spec.credentials.iter().filter(|credential| granted.contains(*credential)) {
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

pub async fn resolve_and_validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    placement: Option<&ResourceObject<FulfilmentKind>>,
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    resolve_workflow_credentials(backend, namespace, project_ref, repositories, workflow).await?;
    validate_workflow_credentials(backend, namespace, workflow, placement).await?;
    crate::account_access::validate_account_access(backend, namespace, project_ref, repositories, workflow, placement).await.map(|_| ())
}

pub async fn resolve_and_validate_workflow_credentials_for_capability_admission(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    placement: Option<&ResourceObject<FulfilmentKind>>,
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    resolve_workflow_credentials(backend, namespace, project_ref, repositories, workflow).await?;
    validate_workflow_credentials_for_admission(backend, namespace, workflow, placement, true).await?;
    crate::account_access::validate_account_access(backend, namespace, project_ref, repositories, workflow, placement).await.map(|_| ())
}

pub async fn validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<FulfilmentKind>>,
) -> Result<(), String> {
    validate_workflow_credentials_for_admission(backend, namespace, workflow, placement, false).await
}

pub async fn validate_workflow_credentials_for_admission(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<FulfilmentKind>>,
    allow_unready: bool,
) -> Result<(), String> {
    let required = workflow.vessels.iter().flat_map(|vessel| vessel.credential_refs.iter().cloned()).collect::<BTreeSet<_>>();
    let Some(first) = required.first() else { return Ok(()) };
    let Some(placement) = placement else {
        return Err(format!("workflow requires credential `{first}`, but no placement is available"));
    };
    let target_host = placement_target_host(backend, namespace, placement).await?;
    let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
    let host_label = target_host.display_name;
    let generation = host_generation(host.status.as_ref()).to_string();
    let Some(mut status) = host.status else {
        return Err(format!(
            "placement `{}` host `{host_label}` generation `{generation}` has no observed status",
            placement.metadata.name
        ));
    };
    status.apply_heartbeat_readiness(Utc::now());
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
    Ok(())
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
pub fn valid_agent_override_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

pub fn apply_agent_overrides(workflow: &mut WorkflowTemplateSpec, overrides: &[flotilla_protocol::AgentOverride]) -> Result<(), String> {
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
        for crew in workflow.roles.iter_mut().chain(workflow.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew)) {
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
                .roles
                .iter()
                .chain(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
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

pub fn required_workflow_agent_adapters(workflow: &WorkflowTemplateSpec) -> Result<BTreeSet<String>, String> {
    required_agent_adapters(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
}

pub async fn placement_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    placement: &ResourceObject<FulfilmentKind>,
    allow_unready: bool,
) -> Result<(BTreeSet<String>, String), String> {
    if let FulfilmentRealisation::DockerPerVessel { image } = &placement.spec.realisation {
        // A composed image carries exactly the harness layers its vessels' roles
        // resolve to (resolution refuses otherwise); a literal image's are what
        // its host probed it for.
        match image {
            None | Some(flotilla_resources::DockerImageSource::Built { .. }) => Ok((BTreeSet::new(), "composed image".to_string())),
            Some(image) => {
                let detail = format!("image `{}`", image.resolve()?);
                let target_host = placement_target_host(backend, namespace, placement).await?;
                let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
                let probed = host
                    .status
                    .as_ref()
                    .and_then(|status| status.fulfilment_facts.get(&placement.metadata.name))
                    .map(|facts| facts.harnesses.keys().cloned().collect())
                    .unwrap_or_default();
                Ok((probed, detail))
            }
        }
    } else if placement.spec.realisation == FulfilmentRealisation::HostDirect {
        let target_host = placement_target_host(backend, namespace, placement).await?;
        let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
        let host_label = target_host.display_name;
        let ssh_host = is_ssh_host(&host);
        let generation = host_generation(host.status.as_ref()).to_string();
        let mut status = host.status.ok_or_else(|| {
            format!("placement `{}` host `{host_label}` generation `{generation}` has no observed status", placement.metadata.name)
        })?;
        status.apply_heartbeat_readiness(Utc::now());
        if !status.ready && !allow_unready {
            return Err(placement_host_not_ready_reason(&placement.metadata.name, &host_label, &generation, &status));
        }
        let mut available_adapters = status.agent_adapters().map_err(|error| {
            format!(
                "placement `{}` host `{}` generation `{generation}` has invalid agent adapter capabilities: {error}",
                placement.metadata.name, host_label
            )
        })?;
        // #4093: an SSH host's crews run the pinned harness delivered to them, so the host
        // offers each pinned adapter whatever its PATH holds.
        if ssh_host {
            available_adapters.extend(flotilla_resources::stdlib::stdlib_harness_pins().keys().cloned());
        }
        // #4037: an SSH host's Codex crews take a copy of its owning daemon's central
        // login, so the host offers Codex only while the owner publishes one.
        let central_login = status.capabilities.get(CODEX_CENTRAL_LOGIN_CAPABILITY).and_then(serde_json::Value::as_bool) == Some(true);
        if ssh_host && !central_login && available_adapters.remove(CODEX_ADAPTER) {
            return Ok((available_adapters, format!("host `{host_label}`, whose owning daemon has no central Codex login")));
        }
        Ok((available_adapters, format!("host `{host_label}`")))
    } else {
        Ok((BTreeSet::new(), "unknown target environment".to_string()))
    }
}

pub fn convoy_fallback_slug(title: &str, id: &str) -> String {
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

pub fn convoy_issues_fallback_slug(issues: &[ConvoyIssue], project_display_name: &str, project_ref: &str) -> String {
    match issues {
        [] => convoy_fallback_slug(project_display_name, project_ref),
        [issue] => convoy_fallback_slug(&issue.snapshot.title, &issue.reference.id),
        issues => {
            let issue_ids = issues.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>().join("-");
            convoy_fallback_slug("batch-issues", &issue_ids)
        }
    }
}

pub fn convoy_issue_name_context(issue: &ConvoyIssue) -> String {
    format!("Issue {}: {}\n{}", issue.reference.id, issue.snapshot.title, issue.snapshot.body.as_deref().unwrap_or_default())
}

pub fn validate_convoy_name(name: &str) -> Result<(), String> {
    if name.len() > 63
        || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !name.bytes().last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("convoy name `{name}` must be a lowercase DNS label of at most 63 characters"));
    }
    Ok(())
}

pub fn validate_convoy_branch(branch: &str) -> Result<(), String> {
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

pub fn parse_ad_hoc_capability_need(value: &str) -> Result<CapabilityNeed, String> {
    CapabilityNeed::parse_ad_hoc(value)
}

/// Image acquisition is a placement cost, after host liveness and before other
/// economic tie breaks. Host inventories are hints; delivery verifies identity.
/// A composed placement costs its most expensive vessel image: Held when the
/// host's inventory holds the Built digest, RegistryPull when the build was
/// pushed to the registry, otherwise Build.
async fn image_placement_cost(
    backend: &ResourceBackend,
    namespace: &str,
    host: Option<&ResourceObject<ResourceHost>>,
    provider_instance: Option<&str>,
    realisation: &flotilla_resources::FulfilmentRealisation,
    composed: Option<BTreeSet<&str>>,
    lifecycle_reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
) -> Result<flotilla_resources::ImageAcquisitionCost, String> {
    use flotilla_resources::{
        DockerImageSource, FleetDesignation, FulfilmentRealisation, ImageAcquisitionCost, ImageBuildPhase, FLEET_DESIGNATION_NAME,
        IMAGE_DIGESTS_CAPABILITY,
    };
    let image = match realisation {
        FulfilmentRealisation::HostDirect | FulfilmentRealisation::CloudSandboxPerVessel { .. } => return Ok(ImageAcquisitionCost::Held),
        FulfilmentRealisation::DockerPerVessel { image } => image,
    };
    let held = host
        .and_then(|host| host.status.as_ref())
        .and_then(|status| status.capabilities.get(IMAGE_DIGESTS_CAPABILITY))
        .and_then(|value| serde_json::from_value::<flotilla_resources::LocalImageInventories>(value.clone()).ok())
        .unwrap_or_default();
    let held = held.held(provider_instance);
    let is_held = |image: &str| {
        held.is_some_and(|held| held.contains(image) || image.rsplit_once('@').is_some_and(|(_, digest)| held.contains(digest)))
    };
    if let Some(identities) = composed {
        let architecture = host.and_then(flotilla_resources::image_architecture::host_architecture);
        let image_cache = crate::image_build::fleet_image_cache(backend, namespace).await?;
        let mut cost = ImageAcquisitionCost::Held;
        for identity in identities {
            let digest = match (&architecture, host) {
                (Some(architecture), Some(host)) => {
                    let name = crate::image_build::expected_build_name(identity, architecture, image_cache.as_ref(), &host.metadata.name);
                    match flotilla_store::read_image_build(backend, namespace, &name, Arc::clone(&lifecycle_reads)).await {
                        Ok(reading) => match reading.lifecycle {
                            Some(flotilla_protocol::LifecycleReading::Ready { identity }) => Some(identity),
                            Some(_) => None,
                            None => reading
                                .build
                                .object
                                .status
                                .filter(|status| status.phase == ImageBuildPhase::Built)
                                .and_then(|status| status.digest),
                        },
                        Err(ResourceError::NotFound { .. }) => None,
                        Err(error) => return Err(error.to_string()),
                    }
                }
                _ => None,
            };
            let vessel = match digest {
                Some(digest) if is_held(&digest) => ImageAcquisitionCost::Held,
                Some(digest) if digest.contains('@') => ImageAcquisitionCost::RegistryPull,
                _ => ImageAcquisitionCost::Build,
            };
            cost = cost.max(vessel);
        }
        return Ok(cost);
    }
    let image = match image {
        Some(DockerImageSource::Literal(image)) => Some(image.as_str()),
        _ => None,
    };
    if image.is_some_and(is_held) {
        return Ok(ImageAcquisitionCost::Held);
    }
    let cache = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(fleet) => fleet.spec.image_cache,
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    let registry = cache.as_ref().is_some_and(|cache| {
        image.is_some_and(|reference| {
            reference
                .rsplit_once('@')
                .is_some_and(|(repository, digest)| repository == cache.repository && flotilla_resources::is_image_digest(digest))
        })
    });
    Ok(if registry { ImageAcquisitionCost::RegistryPull } else { ImageAcquisitionCost::Build })
}

fn require_open_continuation(status: &flotilla_protocol::ChangeRequestStatus, id: &str) -> Result<(), String> {
    if matches!(status, flotilla_protocol::ChangeRequestStatus::Open | flotilla_protocol::ChangeRequestStatus::Draft) {
        Ok(())
    } else {
        Err(format!("cannot continue {status:?} change request #{id}; reopen it explicitly first"))
    }
}

#[cfg(test)]
pub async fn handler_contract_fixture() -> HandlerContractFixture {
    use flotilla_discovery_testkit::fake_discovery;

    // Discovery stands in for host/subprocess I/O; orchestration and stores are real.
    let temp = tempfile::tempdir().expect("config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let discovery = Arc::new(fake_discovery(false));
    let local_environment_id = EnvironmentId::new("local");
    let environment_manager: Arc<dyn Environments> = Arc::new(FixtureEnvironment {
        host: flotilla_protocol::qualified_path::HostId::new("host"),
        runner: discovery.runner.clone(),
        bag: flotilla_discovery::run_host_detectors(&discovery.host_detectors, &*discovery.runner, &*discovery.env).await,
    });
    let resource_backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let observed_resource_backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    let provisioning_namespace = Arc::new(std::sync::RwLock::new("flotilla".into()));
    let query_port: Arc<dyn ChangeRequestQueryPort> = Arc::new(FixtureQueries);
    let issue_query_port: Arc<dyn IssueQueryPort> = Arc::new(FixtureQueries);
    let observation_source = Arc::new(ProviderChangeRequestObservationSource::new(resource_backend.clone(), query_port.clone()));
    let observed_checkout_reconciliation = Arc::new(Mutex::new(()));
    let repository_change_requests = Arc::new(RwLock::new(HashMap::new()));
    let brief_artifact_writer = Arc::new(RwLock::new(None));
    let admission_free_space_path = Arc::new(std::sync::RwLock::new(temp.path().to_path_buf()));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let regard_lifecycle = Arc::new(RegardLifecycle::new(resource_backend.clone(), clock.clone(), chrono::Duration::hours(1)));
    let host_name = HostName::new("host");
    let admission = ConvoyAdmission::builder()
        .backend(resource_backend.clone())
        .accounts(Arc::new(flotilla_store::OldStoreAccounts::new(resource_backend.clone())))
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
        .build();
    HandlerContractFixture {
        owner: admission,
        backend: resource_backend,
        observed: observed_resource_backend,
        reconciliation: observed_checkout_reconciliation,
        namespace: provisioning_namespace,
        temp,
    }
}

/// Registers a ready host advertising one host-direct provider and the
/// host-direct kind admission places onto it.
#[cfg(test)]
pub async fn register_host_direct_kind(backend: &ResourceBackend, host: &str) {
    use flotilla_protocol::provider::{
        CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderCapabilities, ProviderKind, ProviderReadiness,
    };
    let hosts = backend.using::<ResourceHost>("flotilla");
    let created = hosts
        .create(
            &InputMeta::builder().name(host.into()).build(),
            &flotilla_resources::HostSpec { display_name: host.into(), ..Default::default() },
        )
        .await
        .expect("host");
    let providers = vec![ProviderAdvertisement {
        instance: format!("{host}-direct"),
        kind: Some(ProviderKind::new(EnvironmentClass::HostDirect, "fixture")),
        readiness: ProviderReadiness::Ready,
        capabilities: ProviderCapabilities {
            material_modes: vec![MaterialMode::Shared],
            checkout_strategies: vec![CheckoutStrategy::HostWorktree],
            ..Default::default()
        },
    }];
    let status = flotilla_resources::HostStatus {
        capabilities: [(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers))].into_iter().collect(),
        heartbeat_at: Some(chrono::Utc::now()),
        ready: true,
        disk_free_bytes: Some(u64::MAX),
        admission_free_space_floor_bytes: Some(0),
        ..Default::default()
    };
    hosts.update_status(host, &created.metadata.resource_version, &status).await.expect("host status");
    backend
        .using::<FulfilmentKind>("flotilla")
        .create(
            &InputMeta::builder().name(format!("host-direct-{host}")).build(),
            &FulfilmentKindSpec::builder()
                .host_ref(host.into())
                .pool("passthrough".into())
                .realisation(flotilla_resources::FulfilmentRealisation::HostDirect)
                .build(),
        )
        .await
        .expect("kind");
}

#[cfg(test)]
pub struct HandlerContractFixture {
    pub owner: ConvoyAdmission,
    pub backend: ResourceBackend,
    pub observed: ResourceBackend,
    pub reconciliation: Arc<Mutex<()>>,
    pub namespace: Arc<std::sync::RwLock<String>>,
    pub temp: tempfile::TempDir,
}

struct ProbedNamingUtility {
    name: String,
    utility: Arc<dyn AiUtility>,
}

/// Diagnostic persistence must never turn an already-created convoy into an
/// admission error that invites a duplicate retry.
async fn finish_naming_write(namespace: &str, name: &str, write: impl std::future::Future<Output = Result<(), ResourceError>>) {
    if let Err(error) = write.await {
        warn!(%namespace, %name, %error,
            "convoy admitted; unable to persist naming provenance");
    }
}

/// Stderr and API error bodies are untrusted and may contain credentials.
/// Persist only fixed diagnostic categories, never arbitrary utility output.
fn naming_failure_summary(utility: &str, error: &str) -> String {
    let lower = error.chars().take(1024).collect::<String>().to_ascii_lowercase();
    // Keep identifier separators inside tokens: request_id=req-401 must not
    // resemble the HTTP status phrase "HTTP 401".
    let words =
        lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-').filter(|word| !word.is_empty()).collect::<Vec<_>>();
    let has = |phrase: &[&str]| words.windows(phrase.len()).any(|window| window == phrase);
    let summary = if has(&["invalid", "convoy", "names", "response"]) {
        "invalid convoy names response"
    } else if has(&["empty", "convoy", "or", "branch", "name"]) {
        "empty convoy or branch name"
    } else if has(&["oauth", "session", "expired"]) {
        "authentication failed: OAuth session expired and could not be refreshed"
    } else if has(&["authenticate"])
        || has(&["authentication"])
        || has(&["unauthorized"])
        || has(&["http", "401"])
        || has(&["status", "401"])
    {
        "authentication failed"
    } else if has(&["timed", "out"]) || has(&["timeout"]) {
        "name generation timed out"
    } else if has(&["offline"])
        || has(&["connection", "refused"])
        || has(&["connection", "reset"])
        || has(&["could", "not", "connect"])
        || has(&["failed", "to", "connect"])
    {
        "utility unavailable"
    } else if has(&["permission", "denied"]) {
        "utility permission denied"
    } else if has(&["no", "such", "file"]) || has(&["not", "found"]) {
        "utility command unavailable"
    } else if has(&["http", "429"]) || has(&["status", "429"]) || has(&["rate", "limit"]) {
        "utility rate limited"
    } else {
        "utility command or API request failed (unrecognized diagnostic)"
    };
    // Factory identifiers are configuration-owned, not stderr. Bound them too.
    let utility = utility.chars().take(80).collect::<String>();
    format!("{utility}: {summary}")
}

fn resolve_admission_names(
    generated: Option<Result<(String, ConvoyNames), String>>,
    fallback: &str,
    explicit: bool,
    not_attempted_reason: &str,
) -> (ConvoyNames, Option<ConvoyNaming>) {
    let (names, naming) = match generated {
        Some(Ok((utility, names))) => (names, ConvoyNaming::AiUtility { utility }),
        None => (
            ConvoyNames { name: fallback.to_string(), branch: fallback.to_string() },
            ConvoyNaming::DeterministicFallback { reason: not_attempted_reason.to_string() },
        ),
        Some(Err(reason)) => {
            warn!(%reason, "convoy AI naming unavailable; deterministic fallback used");
            (ConvoyNames { name: fallback.to_string(), branch: fallback.to_string() }, ConvoyNaming::DeterministicFallback { reason })
        }
    };
    (names, Some(if explicit { ConvoyNaming::Explicit } else { naming }))
}

async fn probe_admission_ai_utility(
    factories: &[Box<AiUtilityFactory>],
    environment: Option<&EnvironmentBag>,
    config: &dyn ProviderConfigView,
    probe_root: &ExecutionEnvironmentPath,
    runner: Option<Arc<dyn CommandRunner>>,
) -> Result<ProbedNamingUtility, String> {
    let environment = environment.ok_or_else(|| "AI utilities: admitting environment unavailable".to_string())?;
    let runner = runner.ok_or_else(|| "AI utilities: admitting runner unavailable".to_string())?;
    let mut failures = Vec::new();
    for factory in factories {
        let utility_name = factory.descriptor().display_name.chars().take(80).collect::<String>();
        match factory.probe(environment, config, probe_root, Arc::clone(&runner)).await {
            Ok(utility) => return Ok(ProbedNamingUtility { name: utility_name, utility }),
            Err(unmet) => failures.push(format!(
                "{utility_name}: unmet probe requirements: {}",
                unmet.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            )),
        }
    }
    Err(if failures.is_empty() { "no AI utility factories configured".to_string() } else { failures.join("; ") })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_resources::{FulfilmentCostClass, FulfilmentKindSpec, FulfilmentRealisation};

    use super::*;
    use flotilla_store_testkit::cr_name;

    use flotilla_discovery_testkit::FakeChangeRequest;
    use flotilla_protocol::ChangeRequest;

    // WORKLOAD.md: admission must select only this host's direct environments,
    // independently of the number of unrelated per-vessel environments.
    #[hegel::test]
    fn direct_environment_query_covers_only_local_current_and_legacy_records(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Escaped/case-sensitive hosts, both stored generations, missing direct
        // inputs, and a conflicting legacy host alongside an explicit address.
        let host = tc.draw(gs::text().alphabet("aZ09 /:%é").min_size(1).max_size(16));
        let address = flotilla_address::Address::root(flotilla_address::Kind::Host, &host).expect("host address");
        let mut index = flotilla_store::QueryIndex::default();
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(24));
        let mut expected = BTreeSet::new();
        for row in 0..count {
            let local = tc.draw(gs::booleans());
            let legacy = tc.draw(gs::booleans());
            let direct = tc.draw(gs::booleans());
            let target = if local { host.clone() } else { format!("{host}-other") };
            let mut spec = serde_json::json!({});
            if !legacy {
                let place = flotilla_address::Address::root(flotilla_address::Kind::Host, &target).expect("place");
                spec["provider_address"] = format!("{place}/provider:custom").into();
            }
            if direct {
                spec["host_direct"] =
                    serde_json::json!({"host_ref": if legacy { target.clone() } else { host.clone() }, "repo_default_dir":"/work"});
            } else {
                spec["docker"] = serde_json::json!({"host_ref": target, "image":"image"});
            }
            let name = format!("environment-{row}");
            index.update(&name, Some(&serde_json::json!({"kind":"Environment", "metadata":{"name":name}, "spec":spec})));
            if local && direct {
                expected.insert(name);
            }
            assert_eq!(
                index.names(&host_direct_environment_query(&host).expect("query")),
                expected,
                "candidate cover after every insert at {address}"
            );
        }
    }

    // Artifact storage is the process boundary; read inventories and rendering
    // use real collaborators. Multiple vessels must not reread a kind.
    #[derive(Default)]
    struct CapturedBriefs(RwLock<Vec<String>>);
    #[async_trait]
    impl BriefArtifactWriter for CapturedBriefs {
        async fn put_brief(
            &self,
            _namespace: &str,
            _convoy: &str,
            _role: &str,
            _subject: &str,
            content: &[u8],
            _charter_commit: Option<&str>,
        ) -> Result<String, String> {
            self.0.write().await.push(String::from_utf8(content.to_vec()).unwrap());
            Ok("digest".into())
        }
    }

    // Repository and cached request joins are one inventory read each; a missing
    // observation on the first repository must not cause point reads per member.
    #[tokio::test]
    async fn observed_request_reads_are_bounded_across_repositories() {
        use flotilla_resources::{ChangeRequestSpec, Message, MessageRelation, MessageSpec, RepositorySpec};
        use flotilla_store_testkit::ReadCountsBackendExt;
        let mut outcomes = Vec::new();
        for n in [0, 200] {
            let mut fixture = handler_contract_fixture().await;
            let observed = fixture.backend.clone().with_read_counts();
            fixture.owner.backend = observed.clone();
            let mut keys = Vec::new();
            for name in ["first", "second"] {
                let spec = RepositorySpec::remote(format!("https://github.com/acme/{name}")).unwrap();
                keys.push(spec.key());
                fixture
                    .backend
                    .using::<Repository>("flotilla")
                    .create(&InputMeta::builder().name(spec.key().to_string()).build(), &spec)
                    .await
                    .unwrap();
            }
            let record = ChangeRequestSpec::builder()
                .service("github.com".into())
                .scope("acme/second".into())
                .number(42)
                .observing_authority("root".into())
                .build();
            fixture
                .backend
                .using::<ResourceChangeRequest>("flotilla")
                .create(&InputMeta::builder().name(cr_name(&record.service, &record.scope, record.number)).build(), &record)
                .await
                .unwrap();
            for index in 0..n {
                fixture
                    .backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("unrelated-{index}")).build(),
                        &ConvoySpec::builder().workflow_ref("scratch".into()).build(),
                    )
                    .await
                    .unwrap();
                fixture
                    .backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("message-{index}")).build(),
                        &MessageSpec::builder()
                            .sender("system:test".into())
                            .receiver("other/unrelated/work/coder".into())
                            .relation(MessageRelation::System)
                            .body("unrelated".into())
                            .build(),
                    )
                    .await
                    .unwrap();
            }
            let before = observed.read_snapshot();
            assert!(fixture.owner.resolve_observed_convoy_change_request(&keys, Some("42")).await.unwrap().is_none());
            let delta = observed.read_snapshot().since(&before);
            let (reads, decoded) = (delta.calls_by_kind(), delta.objects_by_kind());
            for (kind, count) in &reads {
                assert!(*count <= 1, "{kind}: {count} reads");
            }
            assert_eq!(reads.get("Repository"), Some(&1));
            assert_eq!(reads.get("ChangeRequest"), Some(&1));
            assert_eq!(decoded.get("Repository"), Some(&2));
            assert_eq!(decoded.get("ChangeRequest"), Some(&1));
            outcomes.push((reads, decoded));
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }

    #[tokio::test]
    async fn admission_brief_reads_are_bounded_across_vessels() {
        use flotilla_resources::{Message, MessageRelation, MessageSpec};
        use flotilla_store_testkit::ReadCountsBackendExt;
        let mut outcomes = Vec::new();
        for (reuse, n) in [(false, 0), (false, 200), (true, 0), (true, 200)] {
            let mut fixture = handler_contract_fixture().await;
            let observed = fixture.backend.clone().with_read_counts();
            fixture.owner.backend = observed.clone();
            let writer = Arc::new(CapturedBriefs::default());
            *fixture.owner.brief_artifact_writer.write().await = Some(writer.clone());
            fixture
                .backend
                .definitions::<Project>("flotilla")
                .create(
                    &InputMeta::builder().name("project".into()).build(),
                    &ProjectSpec::builder().display_name("Project".into()).build(),
                )
                .await
                .unwrap();
            for index in 0..n {
                fixture
                    .backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("unrelated-{index}")).build(),
                        &ConvoySpec::builder().workflow_ref("scratch".into()).project_ref("project".into()).build(),
                    )
                    .await
                    .unwrap();
                fixture
                    .backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("message-{index}")).build(),
                        &MessageSpec::builder()
                            .sender("system:test".into())
                            .receiver(format!("other/convoy-{index}/work/coder"))
                            .relation(MessageRelation::System)
                            .body("unrelated".into())
                            .build(),
                    )
                    .await
                    .unwrap();
            }
            let workflow = WorkflowTemplateSpec::builder()
                .vessels(
                    ["coder", "tester"]
                        .into_iter()
                        .map(|role| {
                            VesselRequirement::builder()
                                .name(role.into())
                                .crew(vec![CrewSpec::builder()
                                    .role(role.into())
                                    .source(CrewSource::Agent {
                                        selector: flotilla_resources::Selector::for_capability("coding"),
                                        prompt: Some("test".into()),
                                        brief_template: None,
                                    })
                                    .build()])
                                .build()
                        })
                        .collect(),
                )
                .build();
            let spec = ConvoySpec::builder().workflow_ref("scratch".into()).project_ref("project".into()).build();
            let before = observed.read_snapshot();
            if reuse {
                let snapshot = fixture.owner.load_brief_definitions("flotilla", &spec, &workflow).await.unwrap();
                let query = fixture.owner.brief_query("flotilla", "target", &spec, &workflow, &snapshot).await.unwrap().unwrap();
                let inventory = observed.query_declared::<ResourceConvoy>("flotilla", &query).await.unwrap().items;
                // Incomplete prepared evidence must refuse, never fall back to native reads.
                let missing = ResourceBackend::new(flotilla_store::ViewSnapshot::default());
                let error = fixture
                    .owner
                    .write_admission_briefs_from_inventory(
                        "flotilla",
                        "target",
                        &spec,
                        &workflow,
                        BriefEvidence { definitions: &missing, convoys: &inventory },
                    )
                    .await
                    .expect_err("missing prepared evidence");
                assert!(error.contains("kind Repository missing from view snapshot"), "{error}");
                assert!(fixture
                    .owner
                    .write_admission_briefs_from_inventory(
                        "flotilla",
                        "target",
                        &spec,
                        &workflow,
                        BriefEvidence { definitions: &snapshot, convoys: &inventory }
                    )
                    .await
                    .expect("render reused briefs"));
            } else {
                assert!(fixture.owner.write_admission_briefs("flotilla", "target", &spec, &workflow).await.expect("render briefs"));
            }
            assert_eq!(writer.0.read().await.len(), 2);
            let delta = observed.read_snapshot().since(&before);
            let (reads, decoded) = (delta.calls_by_kind(), delta.objects_by_kind());
            for (kind, count) in &reads {
                assert!(*count <= 1, "{kind}: {count} reads");
            }
            assert_eq!(decoded.get("Convoy"), Some(&0));
            outcomes.push((reads, decoded));
        }
        assert_eq!(outcomes[0], outcomes[1]);
        assert_eq!(outcomes[2], outcomes[3]);
    }

    // Preparation captures capacity evidence; later native writes cannot silently
    // replace that evidence or add reads during the per-vessel capacity check.
    #[tokio::test]
    async fn prepared_capacity_uses_captured_evidence() {
        use flotilla_resources::{HostSpec, HostStatus};
        use flotilla_store_testkit::ReadCountsBackendExt;
        for (captured, current) in [(100, 0), (0, 100)] {
            let mut fixture = handler_contract_fixture().await;
            let counted = fixture.backend.clone().with_read_counts();
            fixture.owner.backend = counted.clone();
            let hosts = fixture.backend.using::<ResourceHost>("flotilla");
            let host = hosts.create(&InputMeta::builder().name("remote".into()).build(), &HostSpec::default()).await.unwrap();
            let capacity =
                |bytes| HostStatus { disk_free_bytes: Some(bytes), admission_free_space_floor_bytes: Some(20), ..Default::default() };
            let host = hosts.update_status("remote", &host.metadata.resource_version, &capacity(captured)).await.unwrap();
            fixture
                .backend
                .using::<FulfilmentKind>("flotilla")
                .create(
                    &InputMeta::builder().name("remote-policy".into()).build(),
                    &FulfilmentKindSpec::builder()
                        .host_ref("remote".into())
                        .pool("passthrough".into())
                        .realisation(FulfilmentRealisation::HostDirect)
                        .build(),
                )
                .await
                .unwrap();
            let mut evidence = flotilla_store::RequestSnapshot::new(&counted, "flotilla").unwrap();
            evidence.copy::<ResourceHost>(&counted).await.unwrap();
            evidence.copy::<FulfilmentKind>(&counted).await.unwrap();
            let admission = PreparedConvoyAdmission::builder()
                .evidence(evidence.finish())
                .name("prepared".into())
                .spec(ConvoySpec::builder().workflow_ref("scratch".into()).build())
                .workflow(WorkflowTemplateSpec::builder().build())
                .placement_decision(PlacementDecision {
                    provider_instance: None,
                    provider_workspace_root: None,
                    kind: "remote-policy".into(),
                    target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("remote"), display_name: "remote".into() },
                    minimal_alternatives: Vec::new(),
                    escalation_reason: None,
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                })
                .build();
            hosts.update_status("remote", &host.metadata.resource_version, &capacity(current)).await.unwrap();
            let before = counted.read_snapshot();
            let result = fixture.owner.check_prepared_placement_floors("flotilla", &admission).await;
            assert_eq!(result.is_ok(), captured >= 20, "captured={captured}, current={current}: {result:?}");
            let reads = counted.read_snapshot().since(&before);
            assert!(reads.is_empty(), "capacity validation must not reread the native store\n{reads}");
        }
    }

    // Admission's frozen snapshot kinds live beside the live kinds but are never
    // candidates, even when one would be the minimal choice, nor pinnable.
    #[tokio::test]
    async fn placement_snapshot_kinds_are_never_candidates() {
        let fixture = handler_contract_fixture().await;
        register_host_direct_kind(&fixture.backend, "host").await;
        let kinds = fixture.backend.using::<FulfilmentKind>("flotilla");
        let mut live = kinds.get("host-direct-host").await.unwrap();
        live.spec.grants = BTreeSet::from([flotilla_resources::FulfilmentGrant::network("host".into())]);
        kinds.update(&InputMeta::from(&live.metadata), &live.metadata.resource_version, &live.spec).await.unwrap();
        let minimal = FulfilmentKindSpec::builder()
            .host_ref("host".into())
            .pool("passthrough".into())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let labelled = BTreeMap::from([(flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(), "placement".to_string())]);
        for (name, labels) in [("frozen-by-label", labelled), ("placement-snapshot-0123456789ab", BTreeMap::new())] {
            kinds.create(&InputMeta::builder().name(name.into()).labels(labels).build(), &minimal).await.unwrap();
        }
        let workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder().role("tool".into()).source(CrewSource::Tool { command: "true".into() }).build()])
                .build()])
            .build();
        let placement =
            fixture.owner.resolve_created_convoy_placement("flotilla", None, &[], &workflow, None).await.expect("live kind places");
        assert_eq!(placement.selected.as_ref().map(|kind| kind.metadata.name.as_str()), Some("host-direct-host"));
        let considered = placement
            .refused_candidates
            .iter()
            .map(|refusal| refusal.kind.as_str())
            .chain(placement.viable_not_selected.iter().map(|viable| viable.kind.as_str()))
            .chain(placement.allocation.iter().flat_map(|allocation| allocation.candidates.iter().map(|candidate| candidate.kind.as_str())))
            .collect::<Vec<_>>();
        assert!(
            !considered.iter().any(|kind| ["frozen-by-label", "placement-snapshot-0123456789ab"].contains(kind)),
            "snapshots must not be considered: {considered:?}"
        );
        for pin in ["frozen-by-label", "placement-snapshot-0123456789ab"] {
            let error = fixture.owner.resolve_created_convoy_placement("flotilla", None, &[], &workflow, Some(pin)).await.err();
            assert_eq!(error, Some(format!("fulfilment kind `{pin}` does not exist")));
        }
    }

    // A host whose Docker provider is not ready names why in the refusal of its Docker kinds.
    #[tokio::test]
    async fn an_unready_docker_provider_refuses_its_kinds_with_the_reason() {
        use flotilla_protocol::provider::{
            CheckoutStrategy, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderCapabilities, ProviderKind, ProviderReadiness,
        };
        let fixture = handler_contract_fixture().await;
        register_host_direct_kind(&fixture.backend, "host").await;
        let kinds = fixture.backend.using::<FulfilmentKind>("flotilla");
        kinds.delete("host-direct-host").await.unwrap();
        let docker = FulfilmentKindSpec::builder()
            .host_ref("host".into())
            .pool("cleat".into())
            .grants(BTreeSet::from([flotilla_resources::FulfilmentGrant::network("scoped".into())]))
            .realisation(FulfilmentRealisation::DockerPerVessel {
                image: Some(flotilla_resources::DockerImageSource::Literal("ubuntu:24.04".into())),
            })
            .build();
        kinds.create(&InputMeta::builder().name("docker-on-host".into()).build(), &docker).await.unwrap();
        let workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder().role("tool".into()).source(CrewSource::Tool { command: "true".into() }).build()])
                .build()])
            .build();
        let hosts = fixture.backend.using::<ResourceHost>("flotilla");
        let reason = "[docker_vessels] is not set in daemon.toml, so nothing bounds a Docker vessel's memory";
        for readiness in [ProviderReadiness::MissingTool { reason: reason.into() }, ProviderReadiness::Ready] {
            let host = hosts.get("host").await.unwrap();
            let mut status = host.status.clone().unwrap();
            let providers = vec![ProviderAdvertisement {
                instance: "docker".into(),
                kind: Some(ProviderKind::new(EnvironmentClass::Container, "docker")),
                readiness: readiness.clone(),
                capabilities: ProviderCapabilities {
                    material_modes: vec![MaterialMode::Shared],
                    checkout_strategies: vec![CheckoutStrategy::HostWorktree],
                    ..Default::default()
                },
            }];
            status.capabilities.insert(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers));
            hosts.update_status("host", &host.metadata.resource_version, &status).await.unwrap();
            let placement = fixture.owner.resolve_created_convoy_placement("flotilla", None, &[], &workflow, None).await;
            match readiness {
                ProviderReadiness::Ready => {
                    let placement = placement.expect("a ready Docker provider places");
                    assert_eq!(placement.selected.as_ref().map(|kind| kind.metadata.name.as_str()), Some("docker-on-host"));
                }
                _ => {
                    let error = placement.expect_err("refused");
                    assert!(error.contains("docker-on-host") && error.contains(reason), "{error}");
                }
            }
        }
    }

    // A public, grant-free GitHub project must be refused before creating work
    // when its controller has no observation credential.
    #[tokio::test(start_paused = true)]
    async fn github_observation_credential_gates_grant_free_admission() {
        let mut fixture = handler_contract_fixture().await;
        let runner = flotilla_discovery_testkit::DiscoveryMockRunner::builder()
            .on_run("gh", &["auth", "status", "--hostname", "github.com"], Err("please run gh auth login; secret diagnostic".into()))
            .on_run("gh", &["auth", "status", "--hostname", "github.com"], Ok("authenticated".into()))
            .build();
        Arc::get_mut(&mut fixture.owner.discovery).expect("fixture discovery").runner = Arc::new(runner);
        Arc::get_mut(&mut fixture.owner.discovery).expect("fixture discovery").observation_credentials = Default::default();
        register_host_direct_kind(&fixture.backend, "host").await;
        let repository = flotilla_resources::RepositorySpec::remote("https://github.com/example/public").expect("repository");
        let key = repository.key();
        fixture
            .backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
            .await
            .expect("repository");
        let workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder().role("tool".into()).source(CrewSource::Tool { command: "true".into() }).build()])
                .build()])
            .build();
        fixture
            .backend
            .definitions::<WorkflowTemplate>("flotilla")
            .create(&InputMeta::builder().name("work".into()).build(), &workflow)
            .await
            .expect("workflow");
        fixture
            .backend
            .definitions::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("public".into()).build(),
                &ProjectSpec::builder()
                    .display_name("Public".into())
                    .default_workflow_ref("work".into())
                    .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder()
                        .repo(key)
                        .default_branch("main".into())
                        .build()])
                    .build(),
            )
            .await
            .expect("project");
        let intent = flotilla_protocol::ConvoyStartIntent::builder()
            .project_ref("public".into())
            .name("public-work".into())
            .branch("public-work".into())
            .build();
        let result = fixture.owner.prepare_convoy_admission("flotilla", &intent, &PrincipalRef::implicit_for_namespace("flotilla")).await;
        let error = result.err().expect("missing controller credential must refuse admission");
        assert!(error.contains("GitHub observation credential"), "{error}");
        assert!(error.contains("https://github.com"), "{error}");
        assert!(!error.contains("secret diagnostic"), "subprocess diagnostics must not leak");
        assert!(fixture.backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items.is_empty());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(fixture
            .owner
            .prepare_convoy_admission("flotilla", &intent, &PrincipalRef::implicit_for_namespace("flotilla"))
            .await
            .is_ok());
        assert!(fixture.owner.discovery.observation_credentials.failures().is_empty());
    }

    // Admission prepares multiple vessels from one selected inventory per kind;
    // unrelated convoys/messages cannot add reads or decoded objects.
    #[tokio::test]
    async fn admission_reads_and_decodes_stay_bounded() {
        use flotilla_resources::{Message, MessageRelation, MessageSpec, ProjectRepositoryRole, ProjectRepositorySpec, RepositorySpec};
        use flotilla_store_testkit::ReadCountsBackendExt;
        let mut outcomes = Vec::new();
        for n in [0, 200] {
            let mut fixture = handler_contract_fixture().await;
            let observed = fixture.backend.clone().with_read_counts();
            fixture.owner.backend = observed.clone();
            register_host_direct_kind(&fixture.backend, "host").await;
            let repository = RepositorySpec::remote("https://example.com/owner/repo").expect("remote identity");
            let key = repository.key();
            fixture
                .backend
                .using::<Repository>("flotilla")
                .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
                .await
                .unwrap();
            let workflow = WorkflowTemplateSpec::builder()
                .vessels(
                    ["first", "second"]
                        .map(|name| {
                            VesselRequirement::builder()
                                .name(name.into())
                                .crew(vec![CrewSpec::builder()
                                    .role("tool".into())
                                    .source(CrewSource::Tool { command: "true".into() })
                                    .build()])
                                .build()
                        })
                        .to_vec(),
                )
                .build();
            fixture
                .backend
                .definitions::<WorkflowTemplate>("flotilla")
                .create(&InputMeta::builder().name("bounded".into()).build(), &workflow)
                .await
                .unwrap();
            fixture
                .backend
                .definitions::<Project>("flotilla")
                .create(
                    &InputMeta::builder().name("project".into()).build(),
                    &ProjectSpec::builder()
                        .display_name("Project".into())
                        .default_workflow_ref("bounded".into())
                        .repositories(vec![ProjectRepositorySpec::builder()
                            .repo(key)
                            .roles([ProjectRepositoryRole::Code].into())
                            .default_branch("main".into())
                            .build()])
                        .build(),
                )
                .await
                .unwrap();
            for index in 0..n {
                fixture
                    .backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("unrelated-{index}")).build(),
                        &ConvoySpec::builder().workflow_ref("unrelated".into()).build(),
                    )
                    .await
                    .unwrap();
                fixture
                    .backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("message-{index}")).build(),
                        &MessageSpec::builder()
                            .sender("system:test".into())
                            .receiver(format!("other/convoy-{index}/work/tool"))
                            .relation(MessageRelation::System)
                            .body("unrelated".into())
                            .build(),
                    )
                    .await
                    .unwrap();
            }
            let intent = flotilla_protocol::ConvoyStartIntent::builder()
                .project_ref("project".into())
                .name("bounded".into())
                .branch("bounded".into())
                .build();
            let before = observed.read_snapshot();
            let prepared = fixture
                .owner
                .prepare_convoy_admission("flotilla", &intent, &PrincipalRef::implicit_for_namespace("flotilla"))
                .await
                .expect("prepare");
            assert_eq!(prepared.workflow.vessels.len(), 2);
            let delta = observed.read_snapshot().since(&before);
            let (reads, decoded) = (delta.calls_by_kind(), delta.objects_by_kind());
            for (kind, count) in &reads {
                assert!(*count <= 1, "{kind}: {count} admission reads");
            }
            assert_eq!(reads.get("Repository"), Some(&1));
            assert_eq!(decoded.get("Repository"), Some(&1));
            assert!(!reads.contains_key("Convoy"));
            assert!(!reads.contains_key("Message"));
            outcomes.push((reads, decoded));
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }

    #[test]
    fn role_refresh_preserves_composed_needs_and_refuses_missing_crew() {
        // Behaviour: routing and admission must allocate the composed crew needs;
        // a missing expanded crew is an error rather than a panic. This is glue:
        // one workflow exercises the copy, empty-crew, and count-mismatch cases.
        let mut workflow = flotilla_resources::single_agent_workflow_spec();
        let project = ProjectSpec::builder().display_name("example".into()).default_workflow_ref("single-agent".into()).build();
        let mut roles = expand_allocation_roles(&mut workflow, &project).expect("expand one role");
        workflow.vessels[0].crew[0].needs.insert(CapabilityNeed::Gpu);
        refresh_allocation_role_crews(&workflow, &mut roles).expect("refresh composed crew");
        assert_eq!(roles[0].crew, workflow.vessels[0].crew[0]);
        workflow.vessels[0].crew.clear();
        assert_eq!(refresh_allocation_role_crews(&workflow, &mut roles).expect_err("empty crew must refuse"), "vessel `work` has no crew");
        workflow.vessels.clear();
        assert_eq!(
            refresh_allocation_role_crews(&workflow, &mut roles).expect_err("unpaired roles must refuse"),
            "expanded roles and vessels must have the same count"
        );
        roles.clear();
        refresh_allocation_role_crews(&workflow, &mut roles).expect("empty workflow has no roles to refresh");
    }

    struct FakeQueryPort {
        backend: ResourceBackend,
        tracker: Arc<dyn ChangeRequestTracker>,
        discoveries: AtomicUsize,
    }

    #[async_trait]
    impl ChangeRequestQueryPort for FakeQueryPort {
        async fn discover_repository_change_request(
            &self,
            _namespace: &str,
            _repository: &RepositorySpec,
        ) -> Result<Arc<dyn ChangeRequestTracker>, String> {
            self.discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::clone(&self.tracker))
        }
    }

    #[tokio::test]
    async fn observed_change_request_uses_injected_repository_query() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let repository = RepositorySpec::remote("https://github.com/acme/repo").expect("repository");
        let key = repository.key();
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
            .await
            .expect("repository declaration");
        let tracker = Arc::new(FakeChangeRequest::new());
        tracker
            .add_change_requests(vec![(
                "7".to_string(),
                ChangeRequest {
                    title: "Ready".to_string(),
                    branch: "feature".to_string(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    body: None,
                    provider_name: "fake".to_string(),
                    provider_display_name: "Fake".to_string(),
                },
            )])
            .await;
        let port = Arc::new(FakeQueryPort { backend, tracker, discoveries: AtomicUsize::new(0) });
        let source = ProviderChangeRequestObservationSource::new(port.backend.clone(), port.clone());
        let subject = ChangeRequestKey::new(
            "flotilla",
            flotilla_resources::address::stored_change_request_address("github.com", "acme/repo", 7, &[]).expect("address"),
        )
        .expect("key");

        let status = source.observe(&subject).await.expect("observed change request");
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Open));
        assert_eq!(port.discoveries.load(Ordering::SeqCst), 1);
    }

    // #2972: frozen provider-instance labels must not borrow legacy host-only
    // evidence or another instance's digest on the same host.
    #[hegel::test]
    fn image_placement_cost_respects_frozen_instance(tc: hegel::TestCase) {
        // Covers both stored generations and present/missing selected instances.
        let legacy = tc.draw(hegel::generators::booleans());
        let other_instance = tc.draw(hegel::generators::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let hosts = backend.using::<ResourceHost>("test");
            let host = hosts
                .create(&InputMeta::builder().name("host".into()).build(), &flotilla_resources::HostSpec::default())
                .await
                .expect("host");
            let digest = format!("sha256:{}", "1".repeat(64));
            let inventory = if legacy { serde_json::json!([digest]) } else { serde_json::json!({"cache-a": [digest]}) };
            let mut status = flotilla_resources::HostStatus::default();
            status.capabilities.insert(flotilla_resources::IMAGE_DIGESTS_CAPABILITY.into(), inventory);
            hosts.update_status("host", &host.metadata.resource_version, &status).await.expect("inventory");
            let host = hosts.get("host").await.expect("host");
            let instance = if other_instance { "cache-b" } else { "cache-a" };
            let realisation = flotilla_resources::FulfilmentRealisation::DockerPerVessel {
                image: Some(flotilla_resources::DockerImageSource::Literal(digest)),
            };
            let cost = image_placement_cost(
                &backend,
                "test",
                Some(&host),
                Some(instance),
                &realisation,
                None,
                Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
            )
            .await
            .expect("cost");
            assert_eq!(
                cost,
                if !legacy && !other_instance {
                    flotilla_resources::ImageAcquisitionCost::Held
                } else {
                    flotilla_resources::ImageAcquisitionCost::Build
                }
            );
        });
    }

    // ADR 0062: a composed placement costs Held when the host inventory holds
    // the identity's Built digest, RegistryPull when the build was pushed,
    // and Build while the build is absent or unfinished.
    #[tokio::test]
    async fn composed_image_cost_follows_the_identity_build() {
        use flotilla_resources::{
            ImageAcquisitionCost, ImageBuild, ImageBuildStatus, ImageDelivery, ImageLayerSource, ImageLayerSpec, ImageLayerTier,
        };
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let hosts = backend.using::<ResourceHost>("test");
        let host =
            hosts.create(&InputMeta::builder().name("host".into()).build(), &flotilla_resources::HostSpec::default()).await.expect("host");
        let local_id = format!("sha256:{}", "1".repeat(64));
        let mut status = flotilla_resources::HostStatus {
            description: Some(
                flotilla_protocol::HostSummary::builder()
                    .environment_id(flotilla_protocol::EnvironmentId::new("host"))
                    .node(flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new("host"), "host".to_string()))
                    .system(flotilla_protocol::SystemInfo { arch: Some("x86_64".into()), ..Default::default() })
                    .build(),
            ),
            ..Default::default()
        };
        status.capabilities.insert(flotilla_resources::IMAGE_DIGESTS_CAPABILITY.into(), serde_json::json!({"cache": [local_id]}));
        hosts.update_status("host", &host.metadata.resource_version, &status).await.expect("inventory");
        let host = hosts.get("host").await.expect("host");
        let layers = vec![ImageLayerSpec::builder()
            .name("base".into())
            .version("1".into())
            .tier(ImageLayerTier::Base)
            .source(ImageLayerSource::Stdlib)
            .from(format!("ubuntu@sha256:{}", "a".repeat(64)))
            .fragment("FROM ${BASE}\n".into())
            .probe("true".into())
            .build()];
        let identity = flotilla_resources::image_identity(&layers);
        let name = flotilla_resources::image_build_name(&identity, "amd64", &ImageDelivery::Local, "host");
        let realisation = flotilla_resources::FulfilmentRealisation::DockerPerVessel { image: None };
        let cost = || {
            image_placement_cost(
                &backend,
                "test",
                Some(&host),
                Some("cache"),
                &realisation,
                Some(BTreeSet::from([identity.as_str()])),
                Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
            )
        };
        assert_eq!(cost().await.expect("cost"), ImageAcquisitionCost::Build, "no build yet");
        let builds = backend.using::<ImageBuild>("test");
        let spec = flotilla_resources::ImageBuildSpec::builder()
            .identity(identity.clone())
            .architecture("amd64".into())
            .layers(layers)
            .host_ref("host".into())
            .delivery(ImageDelivery::Local)
            .reservation(flotilla_resources::ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .build();
        let created = builds.create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await.expect("build");
        assert_eq!(cost().await.expect("cost"), ImageAcquisitionCost::Build, "queued build");
        let built = |digest: &str| ImageBuildStatus {
            phase: flotilla_resources::ImageBuildPhase::Built,
            digest: Some(digest.to_string()),
            ..Default::default()
        };
        let manifest = format!("sha256:{}", "2".repeat(64));
        builds
            .update_status(&name, &created.metadata.resource_version, &built(&format!("registry.test/crew@{manifest}")))
            .await
            .expect("pushed");
        assert_eq!(cost().await.expect("cost"), ImageAcquisitionCost::RegistryPull);
        status.capabilities.insert(flotilla_resources::IMAGE_DIGESTS_CAPABILITY.into(), serde_json::json!({"cache": [local_id, manifest]}));
        hosts.update_status("host", &host.metadata.resource_version, &status).await.expect("pulled inventory");
        let host = hosts.get("host").await.expect("host");
        let held = image_placement_cost(
            &backend,
            "test",
            Some(&host),
            Some("cache"),
            &realisation,
            Some(BTreeSet::from([identity.as_str()])),
            Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads),
        );
        assert_eq!(held.await.expect("cost"), ImageAcquisitionCost::Held);

        // A held engine judgement replaces the contradictory Built digest in placement costs.
        use flotilla_provisioning::engine::image_build::ImageBuildBinding;
        use flotilla_provisioning_testkit::{Ending, LifecycleFixture};
        let build = builds.get(&name).await.expect("build");
        let fixture = LifecycleFixture::<ImageBuildBinding>::new(false);
        fixture.request(&build);
        let cost = || {
            image_placement_cost(
                &backend,
                "test",
                Some(&host),
                Some("cache"),
                &realisation,
                Some(BTreeSet::from([identity.as_str()])),
                fixture.reads.clone(),
            )
        };
        assert_eq!(cost().await.expect("requested"), ImageAcquisitionCost::Build);
        fixture.finish(&build, Ending::Ready(&manifest));
        assert_eq!(cost().await.expect("ready"), ImageAcquisitionCost::Held);
        fixture.finish(&build, Ending::Lost(&manifest));
        assert_eq!(cost().await.expect("lost"), ImageAcquisitionCost::Build);
        fixture.poison();
        assert!(cost().await.expect_err("checked read").contains("poisoned"));
    }

    #[test]
    fn placement_tiebreak_orders_live_minimal_candidates_by_availability_then_cost() {
        let now = chrono::Utc::now();
        let needs = BTreeSet::new();
        let placement_tiebreak = PlacementTieBreak { needs: &needs, now };
        let candidate = |name: &str, cost_class, ready, sleeping_until, slots| {
            let metadata = flotilla_resources::ObjectMeta {
                name: name.to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            };
            let kind = ResourceObject::<FulfilmentKind> {
                metadata,
                spec: FulfilmentKindSpec::builder()
                    .host_ref("host".to_string())
                    .pool("test".to_string())
                    .cost_class(cost_class)
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
                status: None,
            };
            KindCandidate {
                image_cost: flotilla_resources::ImageAcquisitionCost::Held,
                kind: kind.clone(),
                placement: PlacementResolution::selected(kind),
                free_slots: slots,
                vessel_cap: None,
                single_tenant: false,
                host_ready: ready,
                sleeping_until,
            }
        };
        // #2729: exact-held beats pull, and pull beats build/transfer even
        // when another economic class would prefer the more expensive image.
        for (left_cost, right_cost) in [
            (flotilla_resources::ImageAcquisitionCost::Held, flotilla_resources::ImageAcquisitionCost::RegistryPull),
            (flotilla_resources::ImageAcquisitionCost::RegistryPull, flotilla_resources::ImageAcquisitionCost::Build),
        ] {
            let mut left = candidate("left", FulfilmentCostClass::Metered, true, None, Some(1));
            let mut right = candidate("right", FulfilmentCostClass::OwnedIdle, true, None, Some(1));
            left.image_cost = left_cost;
            right.image_cost = right_cost;
            assert!(placement_tiebreak.compare(&left, &right).is_lt());
            left.host_ready = false;
            assert!(placement_tiebreak.compare(&left, &right).is_gt(), "host liveness comes before image cost");
        }
        let cases = [
            (
                "owned available beats metered",
                (true, None, Some(1), FulfilmentCostClass::OwnedIdle),
                (true, None, Some(1), FulfilmentCostClass::Metered),
                true,
            ),
            (
                "available metered beats full owned",
                (true, None, Some(0), FulfilmentCostClass::OwnedIdle),
                (true, None, Some(1), FulfilmentCostClass::Metered),
                false,
            ),
            (
                "awake beats sleeping",
                (true, Some(now + ChronoDuration::hours(1)), Some(1), FulfilmentCostClass::OwnedIdle),
                (true, None, Some(1), FulfilmentCostClass::Metered),
                false,
            ),
            (
                "ready beats unready",
                (false, None, Some(1), FulfilmentCostClass::OwnedIdle),
                (true, None, Some(1), FulfilmentCostClass::SubscriptionIncluded),
                false,
            ),
            (
                "subscription beats metered",
                (true, None, None, FulfilmentCostClass::SubscriptionIncluded),
                (true, None, None, FulfilmentCostClass::Metered),
                true,
            ),
        ];
        for (label, left, right, left_wins) in cases {
            let left = candidate("left", left.3, left.0, left.1, left.2);
            let right = candidate("right", right.3, right.0, right.1, right.2);
            assert_eq!(placement_tiebreak.compare(&left, &right).is_lt(), left_wins, "{label}");
        }
        let mut fuller = candidate("a-fuller", FulfilmentCostClass::OwnedIdle, true, None, Some(1));
        let mut emptier = candidate("b-emptier", FulfilmentCostClass::OwnedIdle, true, None, Some(3));
        assert!(placement_tiebreak.compare(&fuller, &emptier).is_lt(), "uncapped kinds are not spread by published slots");
        fuller.vessel_cap = Some(4);
        emptier.vessel_cap = Some(4);
        assert!(placement_tiebreak.compare(&fuller, &emptier).is_gt(), "capped hosts spread to the most free slots");

        let mut idle_box = candidate("b-idle-box", FulfilmentCostClass::OwnedIdle, true, None, Some(1));
        let mut big_host = candidate("a-big-host", FulfilmentCostClass::OwnedIdle, true, None, Some(14));
        idle_box.vessel_cap = Some(1);
        big_host.vessel_cap = Some(20);
        assert!(
            placement_tiebreak.compare(&idle_box, &big_host).is_lt(),
            "an idle single-tenant host beats a larger host with a smaller free fraction"
        );
        big_host.vessel_cap = Some(14);
        assert!(placement_tiebreak.compare(&big_host, &idle_box).is_lt(), "equal fractions fall back to name");
        let mut over_reported = candidate("c-over", FulfilmentCostClass::OwnedIdle, true, None, Some(9));
        over_reported.vessel_cap = Some(2);
        big_host.vessel_cap = Some(20);
        assert!(placement_tiebreak.compare(&over_reported, &big_host).is_lt(), "free slots above the cap count as fully free");
        let mut zero_cap = candidate("a-zero", FulfilmentCostClass::OwnedIdle, true, None, Some(0));
        zero_cap.vessel_cap = Some(0);
        assert!(placement_tiebreak.compare(&zero_cap, &big_host).is_gt(), "a zero cap ranks last");
        let uncapped = candidate("c-uncapped", FulfilmentCostClass::OwnedIdle, true, None, Some(2));
        big_host.vessel_cap = Some(20);
        assert!(placement_tiebreak.compare(&uncapped, &big_host).is_lt(), "uncapped ranks fully free, ahead of a partly used capped host");
    }

    // Issue #3645: tracker labels are untrusted hints; malformed needs must
    // leave placement unchanged, while the same explicit --need is refused.
    #[tokio::test]
    async fn invalid_issue_need_does_not_refuse_admission() {
        let fixture = handler_contract_fixture().await;
        let project = ProjectSpec::builder().display_name("Example".into()).build();
        let mut intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).build();
        let workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder()
                    .role("coder".into())
                    .source(CrewSource::Tool { command: "true".into() })
                    .needs(BTreeSet::from([CapabilityNeed::Gpu]))
                    .build()])
                .build()])
            .build();
        let mut issue = ConvoyIssue {
            reference: flotilla_protocol::IssueRef {
                source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
                id: "3397".into(),
            },
            repository_ref: None,
            snapshot: flotilla_resources::IssueSnapshot {
                title: "Example".into(),
                body: None,
                state: flotilla_protocol::IssueState::Open,
                labels: vec!["needs:multi-host".into()],
                as_of: chrono::Utc::now(),
            },
        };
        let mut composed = workflow.clone();
        let needs = fixture
            .owner
            .compose_convoy_needs("flotilla", &project, &[issue.clone()], &intent, &mut composed)
            .await
            .expect("tracker label cannot refuse admission");
        assert_eq!(needs, BTreeSet::from([CapabilityNeed::Gpu]));
        assert_eq!(composed, workflow);
        issue.snapshot.labels = vec!["needs:platform:windows".into()];
        let needs = fixture.owner.compose_convoy_needs("flotilla", &project, &[issue], &intent, &mut composed).await.expect("valid label");
        assert_eq!(needs, BTreeSet::from([CapabilityNeed::Gpu, CapabilityNeed::Platform("windows".into())]));
        intent.needs.push("multi-host".into());
        assert!(fixture
            .owner
            .compose_convoy_needs("flotilla", &project, &[], &intent, &mut composed)
            .await
            .unwrap_err()
            .contains("invalid namespaced capability `multi-host`"));
    }

    // Placement matches every vessel's crew needs and policy independently.
    // Reversing vessel order cannot lend one vessel's provider to another.
    #[hegel::test]
    fn distinct_vessel_needs_freeze_distinct_provider_instances(tc: hegel::TestCase) {
        use flotilla_protocol::provider::{
            CheckoutStrategy, EgressEnforcement, EnvironmentClass, MaterialMode, ProviderAdvertisement, ProviderCapabilities, ProviderKind,
            ProviderReadiness,
        };
        use flotilla_resources::{HostSpec, HostStatus};
        let reverse = tc.draw(hegel::generators::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let fixture = handler_contract_fixture().await;
            for platform in ["linux", "macos"] {
                let host = fixture
                    .backend
                    .using::<ResourceHost>("flotilla")
                    .create(
                        &InputMeta::builder().name(platform.into()).build(),
                        &HostSpec { display_name: platform.into(), ..Default::default() },
                    )
                    .await
                    .unwrap();
                let providers = vec![ProviderAdvertisement {
                    instance: format!("{platform}-instance"),
                    kind: Some(ProviderKind::new(EnvironmentClass::HostDirect, "fixture")),
                    readiness: ProviderReadiness::Ready,
                    capabilities: ProviderCapabilities {
                        material_modes: vec![MaterialMode::Shared],
                        checkout_strategies: vec![CheckoutStrategy::HostWorktree],
                        egress_enforcement: EgressEnforcement::Enforced,
                        ..Default::default()
                    },
                }];
                fixture
                    .backend
                    .using::<ResourceHost>("flotilla")
                    .update_status(
                        platform,
                        &host.metadata.resource_version,
                        &HostStatus {
                            capabilities: [(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers))]
                                .into_iter()
                                .collect(),
                            heartbeat_at: Some(Utc::now()),
                            ready: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                fixture
                    .backend
                    .using::<FulfilmentKind>("flotilla")
                    .create(
                        &InputMeta::builder().name(platform.into()).build(),
                        &FulfilmentKindSpec::builder()
                            .host_ref(platform.into())
                            .pool("passthrough".into())
                            .grants(BTreeSet::from([flotilla_resources::FulfilmentGrant::platform(platform.to_string())]))
                            .realisation(FulfilmentRealisation::HostDirect)
                            .build(),
                    )
                    .await
                    .unwrap();
            }
            let mut workflow = WorkflowTemplateSpec::builder()
                .vessels(
                    ["linux", "macos"]
                        .into_iter()
                        .map(|platform| {
                            VesselRequirement::builder()
                                .name(platform.into())
                                .crew(vec![CrewSpec::builder()
                                    .role(platform.into())
                                    .source(CrewSource::Tool { command: "true".into() })
                                    .needs(BTreeSet::from([CapabilityNeed::Platform(platform.into())]))
                                    .build()])
                                .build()
                        })
                        .collect(),
                )
                .build();
            if reverse {
                workflow.vessels.reverse();
            }
            let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("project".into()).build();
            let context = PlacementContext {
                namespace: "flotilla",
                project_ref: Some("project"),
                repositories: &[],
                intent: &intent,
                purpose: PlacementPurpose::Admission,
            };
            let reads = fixture.owner.reads(fixture.backend.clone());
            let (_, _, placements, _) = reads.decide_vessel_placements(&context, &mut workflow).await.unwrap();
            for platform in ["linux", "macos"] {
                assert_eq!(placements[platform].1.provider_instance.as_deref(), Some(format!("{platform}-instance").as_str()));
                assert_eq!(placements[platform].1.target_host.reference.as_str(), platform);
            }
        });
    }

    // ADR 0062: a workflow template leg's `needs` survive role expansion and
    // allocation onto the allocated vessel, and roles from legs with different
    // needs never share a vessel.
    #[test]
    fn leg_needs_survive_allocation() {
        let crew = |role: &str| CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "true".into() }).build();
        let display: BTreeSet<CapabilityNeed> = BTreeSet::from(["display:x11".parse().expect("need")]);
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![
                VesselRequirement::builder().name("demo".to_string()).crew(vec![crew("coder")]).needs(display.clone()).build(),
                VesselRequirement::builder().name("review".to_string()).crew(vec![crew("reviewer")]).build(),
            ])
            .build();
        let project = ProjectSpec::builder().display_name("flotilla".into()).build();
        let roles = expand_allocation_roles(&mut workflow, &project).expect("expand");
        assert_eq!(workflow.vessels[0].needs, display, "expanded vessels keep their leg needs");
        allocate_roles(&mut workflow, &roles).expect("allocate");
        assert_eq!(workflow.vessels.len(), 2, "different leg needs keep roles apart");
        let demo = workflow.vessels.iter().find(|vessel| vessel.crew[0].role == "coder").expect("coder vessel");
        assert_eq!(demo.needs, display);
        let review = workflow.vessels.iter().find(|vessel| vessel.crew[0].role == "reviewer").expect("reviewer vessel");
        assert!(review.needs.is_empty());
    }

    #[test]
    fn allocation_groups_by_needs_and_credential_environment() {
        let cases = [
            (vec![("coder", vec!["platform:linux"], "write"), ("reviewer", vec!["platform:linux"], "write")], 1),
            (vec![("coder", vec!["platform:linux"], "write"), ("verifier", vec!["gui_session"], "write")], 2),
            (vec![("coder", vec![], "write"), ("reviewer", vec!["host_account_reach"], "write")], 2),
            (vec![("coder", vec![], "write"), ("reviewer", vec![], "read")], 2),
        ];
        for (case, expected_count) in cases {
            let roles = case
                .into_iter()
                .map(|(name, needs, grants)| AllocationRole {
                    crew: CrewSpec::builder()
                        .role(name.to_string())
                        .needs(needs.into_iter().map(|need| need.parse().expect("valid need")).collect())
                        .source(CrewSource::Tool { command: "true".to_string() })
                        .build(),
                    hint: name.to_string(),
                    repository_refs: None,
                    depends_on: Vec::new(),
                    credential_signature: grants.to_string(),
                    leg_needs: BTreeSet::new(),
                })
                .collect::<Vec<_>>();
            let mut workflow = WorkflowTemplateSpec::builder()
                .handoffs(vec![RoleHandoff { from: "coder".to_string(), to: roles[1].crew.role.clone() }])
                .build();
            allocate_roles(&mut workflow, &roles).expect("allocation");
            assert_eq!(workflow.vessels.len(), expected_count);
            assert_eq!(workflow.allocation.len(), expected_count);
            if expected_count == 1 {
                assert!(workflow.allocation[0].crossed_handoffs.is_empty());
            } else {
                assert!(workflow.allocation.iter().any(|decision| !decision.crossed_handoffs.is_empty()));
            }
        }
    }

    #[test]
    fn platform_matrix_expands_into_named_vessels() {
        let project = ProjectSpec::builder()
            .display_name("example".to_string())
            .default_workflow_ref("verify".to_string())
            .platform_matrix(vec!["macos".to_string(), "windows".to_string()])
            .build();
        let verifier = CrewSpec::builder()
            .role("verify".to_string())
            .needs(BTreeSet::from(["platform:$matrix".parse().expect("matrix need")]))
            .source(CrewSource::Tool { command: "true".to_string() })
            .build();
        let mut workflow = WorkflowTemplateSpec::builder().roles(vec![verifier]).build();
        workflow.repository_refs = Some(vec![RepositoryKey("scoped-repository".to_string())]);
        let roles = expand_allocation_roles(&mut workflow, &project).expect("expand matrix");
        assert_eq!(roles.iter().map(|role| role.hint.as_str()).collect::<Vec<_>>(), ["verify[macos]", "verify[windows]"]);
        assert_eq!(roles[0].crew.needs.iter().map(ToString::to_string).collect::<Vec<_>>(), ["platform:macos"]);
        assert_eq!(roles[0].repository_refs, workflow.repository_refs);
    }

    #[test]
    fn dual_authored_template_refuses_a_role_only_in_the_vessel_hint() {
        let project = ProjectSpec::builder().display_name("example".to_string()).default_workflow_ref("work".to_string()).build();
        let crew = |role: &str| CrewSpec::builder().role(role.to_string()).source(CrewSource::Tool { command: "true".to_string() }).build();
        let mut workflow = WorkflowTemplateSpec::builder()
            .roles(vec![crew("coder")])
            .vessels(vec![VesselRequirement::builder().name("work".to_string()).crew(vec![crew("reviewer")]).build()])
            .build();
        let error = expand_allocation_roles(&mut workflow, &project).expect_err("missing role must refuse");
        assert!(error.contains("reviewer"), "{error}");
    }

    #[test]
    fn matrix_turn_delivery_requires_a_concrete_vessel() {
        let roles = ["macos", "windows"]
            .into_iter()
            .map(|platform| AllocationRole {
                crew: CrewSpec::builder()
                    .role("verify".to_string())
                    .needs(BTreeSet::from([CapabilityNeed::Platform(platform.to_string())]))
                    .source(CrewSource::Tool { command: "true".to_string() })
                    .build(),
                hint: format!("verify[{platform}]"),
                repository_refs: None,
                depends_on: Vec::new(),
                credential_signature: String::new(),
                leg_needs: BTreeSet::new(),
            })
            .collect::<Vec<_>>();
        let rule = flotilla_resources::TurnDeliveryRule::builder()
            .on(flotilla_resources::parse_leaf_template("$cr.mergeable == conflicting").expect("leaf"))
            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("verify".to_string()).role("verify".to_string()).build())
            .brief("Verify the result".to_string())
            .hold(HoldAct::State)
            .build();
        let mut workflow = WorkflowTemplateSpec::builder().turn_delivery(indexmap::IndexMap::from([("verify".to_string(), rule)])).build();
        let error = allocate_roles(&mut workflow, &roles).expect_err("ambiguous delivery must refuse");
        assert!(error.contains("multiple vessels"), "{error}");
        workflow.turn_delivery["verify"].to.vessel = "verify[macos]".to_string();
        allocate_roles(&mut workflow, &roles).expect("explicit concrete target");
        assert_eq!(workflow.turn_delivery["verify"].to.vessel, "verify[macos]");
    }
}

#[cfg(test)]
mod naming_tests {
    use super::*;
    use flotilla_ai_utility::factories::claude::{ClaudeApiAiUtilityFactory, ClaudeCliAiUtilityFactory};
    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_provider_runtime::{ChannelLabel, CommandOutput};
    use flotilla_replay_testkit::testing::capture_logs;
    use flotilla_resources::StatusPatch;

    // Double at the Claude subprocess boundary; all probes and parsing are real.
    struct NamingRunner(Result<String, String>);
    #[async_trait::async_trait]
    impl CommandRunner for NamingRunner {
        async fn run(&self, _: &str, _: &[&str], _: &std::path::Path, _: &ChannelLabel) -> Result<String, String> {
            self.0.clone()
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &std::path::Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            self.0.clone().map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    async fn naming(environment: EnvironmentBag, response: Result<String, String>, explicit: bool) -> (ConvoyNames, Option<ConvoyNaming>) {
        let config = ConfigStore::new(DaemonHostPath::new("/unused"), DaemonHostPath::new("/unused-state"));
        let factories: Vec<Box<AiUtilityFactory>> = vec![Box::new(ClaudeApiAiUtilityFactory), Box::new(ClaudeCliAiUtilityFactory)];
        let generated = match probe_admission_ai_utility(
            &factories,
            Some(&environment),
            &config,
            &ExecutionEnvironmentPath::new("/unused"),
            Some(Arc::new(NamingRunner(response))),
        )
        .await
        {
            Ok(ProbedNamingUtility { name: utility, utility: provider }) => provider
                .generate_convoy_names("issue")
                .await
                .map(|names| (utility.clone(), names))
                .map_err(|error| naming_failure_summary(&utility, &error)),
            Err(reason) => Err(reason),
        };
        resolve_admission_names(Some(generated), "repair-widget-3062", explicit, "not attempted")
    }

    // Provenance does not alter fallback names or coupled AI names for any explicit/source combination.
    #[hegel::test]
    fn provenance_preserves_generated_names(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Covers zero/upper-bound IDs and both explicit and generated provenance.
        let id = tc.draw(gs::integers::<usize>().min_value(0).max_value(9999));
        let explicit = tc.draw(gs::booleans());
        let success = tc.draw(gs::booleans());
        let fallback = convoy_fallback_slug("Repair widget", &id.to_string());
        let generated_names = ConvoyNames { name: format!("repair-{id}"), branch: format!("fix/repair-{id}") };
        let generated = if success { Ok(("utility".to_string(), generated_names.clone())) } else { Err("failure".to_string()) };
        let (names, source) = resolve_admission_names(Some(generated), &fallback, explicit, "not attempted");
        assert_eq!(names, if success { generated_names } else { ConvoyNames { name: fallback.clone(), branch: fallback } });
        let expected = if explicit {
            ConvoyNaming::Explicit
        } else if success {
            ConvoyNaming::AiUtility { utility: "utility".into() }
        } else {
            ConvoyNaming::DeterministicFallback { reason: "failure".into() }
        };
        assert_eq!(source, Some(expected));
    }

    // ADR 0047: previous stored statuses decode without naming; new provenance survives serde.
    #[test]
    fn stored_naming_is_optional_and_round_trips() {
        let old = serde_json::to_value(flotilla_resources::ConvoyStatus::default()).expect("status");
        assert!(old.get("naming").is_none());
        let mut status: flotilla_resources::ConvoyStatus = serde_json::from_value(old).expect("old generation");
        assert_eq!(status.naming, None);
        for naming in [
            ConvoyNaming::Explicit,
            ConvoyNaming::AiUtility { utility: "Claude CLI".into() },
            ConvoyNaming::DeterministicFallback { reason: "Claude API: missing key".into() },
        ] {
            ConvoyStatusPatch::RecordNaming { naming: naming.clone() }.apply(&mut status);
            let decoded: flotilla_resources::ConvoyStatus =
                serde_json::from_value(serde_json::to_value(&status).expect("encode")).expect("decode");
            assert_eq!(decoded.naming, Some(naming));
        }
    }

    // A failed diagnostic write cannot fail an admission after the convoy exists.
    #[tokio::test]
    async fn naming_write_failure_is_nonfatal_and_visible() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        convoys
            .create(
                &InputMeta::builder().name("admitted".into()).build(),
                &ConvoySpec::builder().workflow_ref("single-agent".into()).role("admitted".into()).build(),
            )
            .await
            .expect("create convoy");
        // Inject failure at the status-write boundary; the resource store is real.
        let (_, logs) = capture_logs(
            tracing::Level::WARN,
            finish_naming_write("flotilla", "admitted", async { Err(ResourceError::invalid("status write failed")) }),
        )
        .await;
        assert!(logs.contains("WARN") && logs.contains("unable to persist naming provenance"), "{logs}");
        assert_eq!(convoys.list().await.expect("convoys").items.len(), 1);
        assert_eq!(convoys.get("admitted").await.expect("convoy remains admitted").spec.role, "admitted");
    }

    // Explicit refers to the convoy name, even when its missing branch uses the fallback.
    #[tokio::test]
    async fn explicit_name_with_failed_branch_generation_remains_explicit() {
        let environment = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/bin/claude"));
        let ((names, source), logs) =
            capture_logs(tracing::Level::WARN, naming(environment, Err("OAuth session expired token=never-store-this".into()), true)).await;
        assert_eq!(source, Some(ConvoyNaming::Explicit));
        assert_eq!(names.branch, "repair-widget-3062");
        assert!(logs.contains("OAuth session expired") && !logs.contains("never-store-this"), "{logs}");
    }

    // Stderr/API bodies may contain secrets: only bounded fixed summaries reach durable evidence.
    #[test]
    fn diagnostics_do_not_copy_raw_utility_output() {
        let secret = "sk-ant-private-token";
        for error in [
            format!("OAuth session expired token={secret}"),
            format!("invalid convoy names response: {secret}"),
            format!("arbitrary sensitive body {secret} {}", "x".repeat(10000)),
            format!("401 api_key={secret}"),
        ] {
            let summary = naming_failure_summary(&"u".repeat(10000), &error);
            assert!(!summary.contains(secret), "{summary}");
            assert!(summary.len() <= 200, "{summary}");
        }
        let (_, naming) = resolve_admission_names(None, "fallback", false, "caller supplied reason");
        assert_eq!(naming, Some(ConvoyNaming::DeterministicFallback { reason: "caller supplied reason".into() }));
    }

    // Identifiers containing status digits or diagnostic fragments are not failure categories.
    #[test]
    fn diagnostic_categories_match_words_and_http_context() {
        for error in ["request_id=req401", "request_id=401", "connecticut", "unknown_not_found", "request-id=req-429"] {
            assert_eq!(
                naming_failure_summary("Claude API", error),
                "Claude API: utility command or API request failed (unrecognized diagnostic)"
            );
        }
        for (error, expected) in [
            ("Anthropic API error (HTTP 401): private body", "authentication failed"),
            ("HTTP 429", "utility rate limited"),
            ("connection refused", "utility unavailable"),
            ("binary not found", "utility command unavailable"),
            ("invalid convoy names response: request_id=401", "invalid convoy names response"),
        ] {
            assert_eq!(naming_failure_summary("Claude API", error), format!("Claude API: {expected}"));
        }
    }

    // Missing admission collaborators report the specific availability reason and use fallback naming.
    #[tokio::test]
    async fn unavailable_environment_and_runner_keep_their_reasons() {
        let environment = EnvironmentBag::new();
        let config = ConfigStore::new(DaemonHostPath::new("/unused"), DaemonHostPath::new("/unused-state"));
        for (bag, runner, expected) in [
            (
                None,
                Some(Arc::new(NamingRunner(Ok(String::new()))) as Arc<dyn CommandRunner>),
                "AI utilities: admitting environment unavailable",
            ),
            (Some(&environment), None, "AI utilities: admitting runner unavailable"),
            (None, None, "AI utilities: admitting environment unavailable"),
        ] {
            let result = probe_admission_ai_utility(&[], bag, &config, &ExecutionEnvironmentPath::new("/unused"), runner).await;
            let reason = result.err().expect("missing collaborator refuses probing");
            assert_eq!(reason, expected);
            let ((names, source), logs) = capture_logs(tracing::Level::WARN, async {
                resolve_admission_names(Some(Err(reason)), "fallback", false, "not attempted")
            })
            .await;
            assert_eq!(names, ConvoyNames { name: "fallback".into(), branch: "fallback".into() });
            assert_eq!(source, Some(ConvoyNaming::DeterministicFallback { reason: expected.into() }));
            assert!(logs.contains(expected) && logs.contains("WARN"), "{logs}");
        }
    }

    // Contract: missing API key and binary retain every factory's unmet requirements.
    #[tokio::test]
    async fn missing_requirements_keep_all_probe_outcomes() {
        let ((names, source), logs) = capture_logs(tracing::Level::WARN, naming(EnvironmentBag::new(), Ok(String::new()), false)).await;
        assert!(logs.contains("WARN") && logs.contains("deterministic fallback used"), "{logs}");
        assert_eq!(names.name, "repair-widget-3062");
        assert_eq!(names.branch, names.name);
        let Some(ConvoyNaming::DeterministicFallback { reason }) = source else { panic!("fallback provenance") };
        for expected in ["Claude API", "ANTHROPIC_API_KEY", "Claude CLI", "claude", "missing binary", "missing environment variable"] {
            assert!(reason.contains(expected), "{reason}");
        }
    }

    // Contract: CLI failures and invalid responses keep the utility and diagnostic; success and explicit names keep their source.
    #[tokio::test]
    async fn cli_generation_records_failures_and_sources() {
        let environment = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/bin/claude"));
        for (response, expected) in [
            (Err("Failed to authenticate: OAuth session expired and could not be refreshed".to_string()), "OAuth session expired"),
            (Ok("not json".to_string()), "invalid convoy names response"),
            (Ok(r#"{"name":"","branch":"fix/widget"}"#.to_string()), "empty convoy or branch name"),
        ] {
            let ((names, source), logs) = capture_logs(tracing::Level::WARN, naming(environment.clone(), response, false)).await;
            assert!(
                logs.contains("WARN")
                    && logs.contains("Claude CLI")
                    && logs.contains(expected)
                    && logs.contains("deterministic fallback used"),
                "{logs}"
            );
            assert_eq!(names.name, "repair-widget-3062");
            assert_eq!(names.branch, names.name);
            let Some(ConvoyNaming::DeterministicFallback { reason }) = source else { panic!("fallback provenance") };
            assert!(reason.contains("Claude CLI"), "{reason}");
            assert!(reason.contains(expected), "{reason}");
        }
        let response = Ok(r#"{"name":"repair-widget","branch":"fix/widget"}"#.to_string());
        let (names, source) = naming(environment.clone(), response.clone(), false).await;
        assert_eq!(names.name, "repair-widget");
        assert_eq!(source, Some(ConvoyNaming::AiUtility { utility: "Claude CLI".to_string() }));
        assert_eq!(naming(environment, response, true).await.1, Some(ConvoyNaming::Explicit));
    }
}

impl ConvoyAdmission {
    pub async fn prepare_ensured_convoy(
        &self,
        repository_port: &dyn crate::project_ops::ProjectOperations,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String> {
        let repositories = self.backend.clone().including_replicas::<Repository>(namespace);
        for key in &ensure.spec.repositories {
            match repositories.get(&key.to_string()).await {
                Ok(repository) => {
                    repository_port
                        .resolve_forge_identity_in(namespace, repository.object.spec)
                        .await
                        .map_err(|error| format!("repository {key}: {error}"))?;
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        // The forge sweep may rewrite both Project membership and this ensure.
        let ensure = match self.backend.clone().including_replicas::<ConvoyEnsure>(namespace).get(&ensure.metadata.name).await {
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
        self.check_prepared_placement_floors(namespace, &admission).await?;
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
}

#[cfg(test)]
struct FixtureEnvironment {
    host: flotilla_protocol::qualified_path::HostId,
    runner: Arc<dyn CommandRunner>,
    bag: EnvironmentBag,
}
#[cfg(test)]
impl Environments for FixtureEnvironment {
    fn local_host_id(&self) -> &flotilla_protocol::qualified_path::HostId {
        &self.host
    }
    fn environment_bag(&self, _: &EnvironmentId) -> Option<EnvironmentBag> {
        Some(self.bag.clone())
    }
    fn environment_runner(&self, _: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        Some(self.runner.clone())
    }
    fn environment_registry(&self, _: &EnvironmentId) -> Option<Arc<flotilla_discovery::registry::ProviderRegistry>> {
        None
    }
    fn host_id_for_environment(&self, _: &EnvironmentId) -> Option<flotilla_protocol::qualified_path::HostId> {
        Some(self.host.clone())
    }
}
#[cfg(test)]
struct FixtureQueries;
#[cfg(test)]
#[async_trait]
impl ChangeRequestQueryPort for FixtureQueries {
    async fn discover_repository_change_request(&self, _: &str, _: &RepositorySpec) -> Result<Arc<dyn ChangeRequestTracker>, String> {
        Ok(Arc::new(flotilla_discovery_testkit::FakeChangeRequest::default()))
    }
}
#[cfg(test)]
#[async_trait]
impl IssueQueryPort for FixtureQueries {
    async fn provider_for_source(
        &self,
        _: &flotilla_protocol::IssueSource,
    ) -> Result<Arc<dyn flotilla_issue_tracker::IssueProvider>, String> {
        Ok(Arc::new(flotilla_discovery_testkit::FakeIssueProvider::default()))
    }
}
