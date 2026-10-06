//! Convoy admission state and transaction boundary.

use super::*;
use crate::{
    agent_adapter::{minimum_harness_version, CrewAssignment, CrewBriefTemplateResolver},
    branch_lookup_observer::BranchLookupObserver,
    providers::discovery::{detectors::git::remote_assertion, FORGEJO_AUTH_PROVIDER},
};

/// Routing uses replicated observations as hints; only admission may refuse on
/// destination-owned facts, credentials, or the installed skill catalog.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PlacementPurpose {
    Routing,
    Admission,
}

#[derive(Clone, Copy, bon::Builder)]
struct PlacementContext<'a> {
    namespace: &'a str,
    project_ref: &'a str,
    repositories: &'a [ConvoyRepositorySpec],
    intent: &'a flotilla_protocol::ConvoyStartIntent,
    purpose: PlacementPurpose,
}

#[derive(Clone, Copy, bon::Builder)]
struct PolicyPlacementContext<'a> {
    namespace: &'a str,
    project_ref: Option<&'a str>,
    repositories: &'a [ConvoyRepositorySpec],
    placement_policy: Option<&'a str>,
    allow_unready: bool,
    purpose: PlacementPurpose,
}

#[derive(bon::Builder)]
pub(super) struct ConvoyCreateAdmission<'a> {
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
}

#[derive(bon::Builder)]
pub(super) struct ConvoyAdmission {
    backend: ResourceBackend,
    observed_backend: ResourceBackend,
    observed_checkout_reconciliation: Arc<Mutex<()>>,
    config: Arc<ConfigStore>,
    discovery: Arc<DiscoveryRuntime>,
    environment_manager: Arc<EnvironmentManager>,
    local_environment_id: EnvironmentId,
    provisioning_namespace: Arc<std::sync::RwLock<String>>,
    pub(super) repository_change_requests: Arc<RwLock<HashMap<RepositoryKey, RepositoryChangeRequestProvider>>>,
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
    fulfilment_decider: Arc<dyn FulfilmentDecider>,
    #[builder(default)]
    pub(super) image_build_inputs: RwLock<Option<Arc<dyn crate::image_build::ImageBuildInputResolver>>>,
    /// Serializes the identity selector check with Convoy creation on the owner host.
    #[builder(default)]
    guard: Mutex<()>,
    #[builder(default)]
    pending_starts: Mutex<HashSet<ConvoyStartKey>>,
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
    pub(super) fn set_free_space_path(&self, path: PathBuf) {
        *self.admission_free_space_path.write().expect("admission free-space path lock poisoned") = path;
    }

    pub(super) async fn free_space_bytes(&self) -> Result<Option<u64>, String> {
        let path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let probe = Arc::clone(&self.discovery.available_space_probe);
        crate::probe::blocking("available disk space", crate::probe::PROBE_TIMEOUT, move || Ok(probe.measure(&path))).await
    }

    async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.environment_manager
            .host_id_for_environment(&self.local_environment_id)
            .map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    async fn snapshot_project_repositories(
        &self,
        namespace: &str,
        project_ref: &str,
        selected: Option<&[RepositoryKey]>,
    ) -> Result<Vec<ConvoyRepositorySpec>, String> {
        project_ops::snapshot_project_repositories_with_backend(&self.backend, namespace, project_ref, selected).await
    }

    async fn fetch_issue_by_ref(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.issue_port.fetch_issue_by_ref(reference).await
    }

    pub(super) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.guard.lock().await
    }

    pub(super) async fn mark_pending(&self, key: ConvoyStartKey) -> bool {
        self.pending_starts.lock().await.insert(key)
    }

    pub(super) async fn clear_pending(&self, key: &ConvoyStartKey) {
        self.pending_starts.lock().await.remove(key);
    }
}

impl ConvoyAdmission {
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

    pub(super) async fn resolve_convoy_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        let (candidates, setup_failures) = self.repository_change_request_candidates(repository_keys).await;
        let mut failures = setup_failures
            .into_iter()
            .map(|error| AdmissionLookupFailure { context: None, error: ObservationError::Forge(error) })
            .collect::<Vec<_>>();
        let consulted = candidates.iter().map(|(_, scope, _)| scope.clone()).collect::<Vec<_>>();

        let mut matches = Vec::new();
        let mut matched_repositories = Vec::new();
        for (repository, scope, provider) in candidates {
            match provider.get_change_request_for_admission(requested_id).await {
                Ok(admission) => {
                    let Some(base_ref) = admission.base_ref else {
                        failures.push(AdmissionLookupFailure {
                            context: Some(format!("repository {scope}")),
                            error: ObservationError::Forge(format!("change request {} did not report a base ref", admission.id)),
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

    pub(super) async fn repository_change_request_candidates(
        &self,
        repository_keys: &[RepositoryKey],
    ) -> (Vec<(RepositoryKey, String, Arc<dyn ChangeRequestTracker>)>, Vec<String>) {
        let namespace = self.provisioning_namespace().await;
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
        self.change_request_port.discover_repository_change_request(namespace, repository).await
    }

    pub(super) async fn resolve_convoy_change_request(
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
        if let Some(change_request) = self.resolve_observed_convoy_change_request(repository_keys, Some(id)).await? {
            return Ok(Some(change_request));
        }
        let namespace = self.provisioning_namespace().await;
        let repositories = self.backend.including_replicas::<Repository>(&namespace);
        let mut failures = Vec::new();
        for repository_key in repository_keys {
            let repository = match repositories.get(&repository_key.to_string()).await {
                Ok(repository) => repository,
                Err(error) => {
                    failures.push(ObservationError::Forge(error.to_string()));
                    continue;
                }
            };
            let Some(remote) = repository.object.spec.live_remote() else { continue };
            let address = match change_request_address(remote, id) {
                Ok(address) => address,
                Err(error) => {
                    failures.push(ObservationError::Forge(error));
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
        // Preserve classification until the final admission diagnostic, including
        // classified limits with no retry deadline. Display wording is not policy.
        if let Some(error) = failures.iter().find(|error| matches!(error, ObservationError::RateLimited { .. })) {
            return Err(error.to_string());
        }
        failures.into_iter().next().map_or(Ok(None), |error| Err(error.to_string()))
    }

    pub(super) async fn refresh_convoy_branch(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        binding: Option<&flotilla_resources::BoundChangeRequest>,
    ) -> crate::convoy_branch_refresh::ConvoyBranchRefresh {
        let bound = match binding {
            Some(binding) => {
                Some(self.resolve_bound_convoy_change_request(std::slice::from_ref(&binding.repository_ref), &binding.id).await)
            }
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
                bound.as_ref().expect("binding has a lookup").clone().map_err(ObservationError::Forge)
            } else {
                let (candidates, setup_failures) = self.repository_change_request_candidates(std::slice::from_ref(key)).await;
                if let Some((repository, _, provider)) = candidates.into_iter().next() {
                    self.branch_lookup_observer.find(&repository.to_string(), &provider, branch).await.map(|found| {
                        found.map(|(id, request)| ConvoyChangeRequest { id, status: request.status, repository_key: repository })
                    })
                } else {
                    // Setup reports repository-read, forge-identity, or provider-discovery
                    // failures. The generic text defends against an empty diagnostic set.
                    Err(ObservationError::Forge(setup_failures.into_iter().next().unwrap_or_else(|| "no repository provider".into())))
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
        crate::convoy_branch_refresh::ConvoyBranchRefresh { primary, repositories: results }
    }

    pub(super) async fn resolve_observed_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let Some(change_request_id) = change_request_id else { return Ok(None) };
        let Ok(number) = change_request_id.parse::<u64>() else { return Ok(None) };
        let namespace = self.provisioning_namespace().await;
        let repositories = self.backend.clone().including_replicas::<Repository>(&namespace);
        let change_requests = self.backend.clone().including_replicas::<ResourceChangeRequest>(&namespace);

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
}

impl ConvoyAdmission {
    pub(super) async fn resolve_convoy_issue_snapshot(
        &self,
        reference: &flotilla_protocol::IssueRef,
    ) -> Result<flotilla_protocol::Issue, String> {
        let issue = self.fetch_issue_by_ref(reference).await?;
        if issue_snapshot_is_fresh(&issue) {
            Ok(issue)
        } else {
            Err(format!("issue {} snapshot is too stale to admit", reference.id))
        }
    }

    pub(super) async fn admission_ai_utility(&self) -> Option<Arc<dyn AiUtility>> {
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

    pub(super) async fn resolve_convoy_issue(
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

    pub(super) async fn start_placement_host(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        let (namespace, intent) = normalize_convoy_start_intent(namespace, intent)?;
        let project_ref = &intent.project_ref;
        let placement = if intent.placement_policy.is_some() {
            self.decide_capability_placement(
                &PlacementContext::builder()
                    .namespace(&namespace)
                    .project_ref(project_ref)
                    .repositories(&[])
                    .intent(&intent)
                    .purpose(PlacementPurpose::Routing)
                    .build(),
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
            let repositories = self.snapshot_project_repositories(&namespace, project_ref, None).await?;
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
                &PlacementContext::builder()
                    .namespace(&namespace)
                    .project_ref(project_ref)
                    .repositories(&repositories)
                    .intent(&intent)
                    .purpose(PlacementPurpose::Routing)
                    .build(),
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
        Ok((self.canonical_local_host_id().as_ref() != Some(&actuator))
            .then(|| flotilla_protocol::qualified_path::HostId::new(actuator.as_str())))
    }

    pub(super) async fn prepare_convoy_admission(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<PreparedConvoyAdmission, String> {
        self.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, None).await
    }

    pub(super) async fn resolve_convoy_admission_workflow(
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
            let scoped = crate::ops_entry::materialized_workflow_name(owner, &workflow_ref);
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
            self.resolve_convoy_skills(&cascade, intent, &mut workflow.spec).await?;
            validate_fork_workflow_admission(&self.backend, namespace, repositories, &workflow_ref, &workflow.spec).await?;
        }
        workflow.spec.cascade = Some(Box::new(cascade));
        Ok((workflow_ref, workflow.spec))
    }

    pub(super) async fn compose_convoy_needs(
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
        namespace: &str,
        project: &ProjectSpec,
        issues: &[ConvoyIssue],
        intent: &flotilla_protocol::ConvoyStartIntent,
        workflow: &mut WorkflowTemplateSpec,
        purpose: PlacementPurpose,
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
        let hosts = self.backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
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
                                .backend
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

    #[cfg(test)]
    pub(super) async fn resolve_capability_placement(
        &self,
        namespace: &str,
        project_ref: &str,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        needs: &BTreeSet<CapabilityNeed>,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<(PlacementResolution, Vec<String>), String> {
        self.decide_capability_placement(
            &PlacementContext::builder()
                .namespace(namespace)
                .project_ref(project_ref)
                .repositories(repositories)
                .intent(intent)
                .purpose(PlacementPurpose::Admission)
                .build(),
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
        let PlacementContext { namespace, project_ref, repositories, intent, purpose } = *context;
        if purpose == PlacementPurpose::Routing && intent.placement_policy.is_some() {
            return self
                .decide_placement(
                    PolicyPlacementContext::builder()
                        .namespace(namespace)
                        .maybe_project_ref(Some(project_ref))
                        .repositories(repositories)
                        .maybe_placement_policy(intent.placement_policy.as_deref())
                        .allow_unready(true)
                        .purpose(purpose)
                        .build(),
                    workflow,
                )
                .await
                .map(|placement| (placement, Vec::new()));
        }
        let pin = intent.placement_policy.as_deref();
        let escalation_reason = intent.escalation_reason.as_deref();
        let kinds = home_copy_wins_by_name(
            self.backend.including_replicas::<FulfilmentKind>(namespace).list().await.map_err(|error| error.to_string())?.items,
        );
        if kinds.is_empty() {
            if purpose == PlacementPurpose::Admission {
                if let Some(need) = needs.iter().next() {
                    return Err(format!("no fulfilment kind covers role need `{need}`"));
                }
            }
            return self
                .decide_placement(
                    PolicyPlacementContext::builder()
                        .namespace(namespace)
                        .maybe_project_ref(Some(project_ref))
                        .repositories(repositories)
                        .maybe_placement_policy(pin)
                        .allow_unready(false)
                        .purpose(purpose)
                        .build(),
                    workflow,
                )
                .await
                .map(|placement| (placement, Vec::new()));
        }
        let hosts = home_copy_wins_by_name(
            self.backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?.items,
        );
        if let Some(pin) = pin {
            if !kinds.iter().any(|kind| kind.metadata.name == pin) {
                return Err(format!("fulfilment kind `{pin}` does not exist"));
            }
        }
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
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
            let policy_name = kind.metadata.name.clone();
            let refusal = |reason: String| PlacementRefusal { policy_name: policy_name.clone(), target_host: target_host.clone(), reason };
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
            let facts = host.and_then(|host| host.status.as_ref()).and_then(|status| status.fulfilment_facts.get(&kind.metadata.name));
            let image_needs = needs.iter().filter(|need| need.is_image_need()).map(CapabilityNeed::capability_string).collect();
            let composed = match &kind.spec.realisation {
                flotilla_resources::FulfilmentRealisation::DockerPerVessel { image } => {
                    match freeze_admission_image(&self.backend, namespace, image, &image_needs).await {
                        Ok(flotilla_resources::DockerImageSource::Composition { composition }) => Some(composition),
                        Ok(_) => None,
                        Err(error) => {
                            rejected.push(refusal(error));
                            continue;
                        }
                    }
                }
                _ => None,
            };
            let image_covers = |need: &CapabilityNeed| {
                composed.as_ref().is_some_and(|composition| {
                    need.is_image_need()
                        && composition
                            .layers
                            .iter()
                            .flat_map(|layer| &layer.spec.provides)
                            .any(|provided| flotilla_resources::capability_satisfies(provided, &need.capability_string()))
                })
            };
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
            match self
                .decide_placement(
                    PolicyPlacementContext::builder()
                        .namespace(namespace)
                        .maybe_project_ref(Some(project_ref))
                        .repositories(repositories)
                        .maybe_placement_policy(Some(&kind.metadata.name))
                        .allow_unready(true)
                        .purpose(purpose)
                        .build(),
                    workflow,
                )
                .await
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
                            rejected.push(refusal(error));
                            continue;
                        }
                        None => None,
                    };
                    let image_matches = purpose == PlacementPurpose::Routing || composed.as_ref().is_none_or(|expected| {
                        policy.spec.docker_per_vessel.as_ref().is_some_and(|docker| {
                            matches!(&docker.image, flotilla_resources::DockerImageSource::Composition { composition } if composition == expected)
                        })
                    });
                    if !image_matches {
                        rejected.push(refusal("fulfilment kind and placement policy disagree on image composition".into()));
                        continue;
                    }
                    let realization_matches = policy_host_ref.is_some() && policy_host == canonical_kind_host;
                    if !realization_matches {
                        rejected.push(refusal("fulfilment kind and placement policy disagree on host or realisation".into()));
                        continue;
                    }
                    let free_slots = facts.and_then(|facts| facts.free_vessel_slots);
                    let host_ready = host.and_then(|host| host.status.as_ref()).is_some_and(|status| {
                        let mut status = status.clone();
                        status.apply_heartbeat_readiness(self.clock.now());
                        status.ready
                    });
                    let sleeping_until = host.and_then(|host| host.status.as_ref()).and_then(|status| status.sleeping_until);
                    // A sleeping host may queue work for its wake-up time. A host
                    // that is simply not ready cannot be admitted, even when it
                    // is the only fulfilment that covers the requested needs.
                    if purpose == PlacementPurpose::Admission {
                        if let Some(reason) = unready_placement_refusal(
                            &policy.metadata.name,
                            &kind.spec.host_ref,
                            host.and_then(|host| host.status.as_ref()),
                            self.clock.now(),
                        ) {
                            rejected.push(refusal(reason));
                            continue;
                        }
                    }
                    candidates.push(KindCandidate { kind, placement, free_slots, host_ready, sleeping_until });
                }
                Err(error) => rejected.push(refusal(error)),
            }
        }
        if candidates.is_empty() {
            let role_needs = workflow
                .vessels
                .iter()
                .flat_map(|vessel| vessel.crew.iter())
                .flat_map(|crew| crew.needs.iter().map(move |need| format!("role {} need `{need}`", crew.role)))
                .collect::<Vec<_>>();
            return Err(format!(
                "no fulfilment kind covers {}; candidates: {}",
                role_needs.join(", "),
                rejected.iter().map(|refusal| format!("{}: {}", refusal.policy_name, refusal.reason)).collect::<Vec<_>>().join("; ")
            ));
        }
        let placement_tiebreak = self.fulfilment_decider.for_admission(needs, self.clock.now());
        // Hold scarce platform capacity only when unreserved capacity also covers the needs.
        let has_unreserved = candidates.iter().any(|candidate| !placement_tiebreak.reserved(candidate));
        let mut reserved = Vec::new();
        if pin.is_none() && has_unreserved {
            (candidates, reserved) = candidates.into_iter().partition(|candidate| !placement_tiebreak.reserved(candidate));
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
            Some(pin) => candidates.iter().position(|candidate| candidate.kind.metadata.name == pin).ok_or_else(|| {
                format!(
                    "pinned fulfilment `{pin}` cannot cover vessel needs; candidates: {}",
                    rejected.iter().map(|refusal| format!("{}: {}", refusal.policy_name, refusal.reason)).collect::<Vec<_>>().join("; ")
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
                policy_name: candidate.kind.metadata.name.clone(),
                target_host,
                reason: if minimal.contains(&candidate.kind.metadata.name) { "minimal alternative" } else { "grants a strict superset" }
                    .to_string(),
            });
        }
        selected.placement.refused_candidates.extend(rejected);
        selected.placement.allocation = Some(allocation);
        Ok((selected.placement, alternatives))
    }

    async fn join_placement_builds(&self, namespace: &str, placement: &mut PlacementResolution) -> Result<(), String> {
        if let Some(docker) = placement.selected.as_mut().and_then(|policy| policy.spec.docker_per_vessel.as_mut()) {
            if let flotilla_resources::DockerImageSource::Composition { composition } = &mut docker.image {
                if composition.baseline_image.is_none() && composition.identity.is_none() {
                    let inputs = self.image_build_inputs.read().await.clone().ok_or("image build input resolver is unavailable")?;
                    composition.build_refs = crate::image_build::ImageBuildAdmission::new(self.backend.clone(), namespace, inputs)
                        .join(&docker.host_ref, composition)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn decide_vessel_placements(
        &self,
        context: &PlacementContext<'_>,
        workflow: &mut WorkflowTemplateSpec,
    ) -> Result<(PlacementResolution, Vec<String>, BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>), String> {
        let PlacementContext { namespace, project_ref, repositories, intent, purpose } = *context;
        let mut vessel_placements = BTreeMap::new();
        let has_kinds =
            !self.backend.including_replicas::<FulfilmentKind>(namespace).list().await.map_err(|error| error.to_string())?.items.is_empty();
        let (placement, minimal_alternatives) = if has_kinds && !workflow.vessels.is_empty() {
            let mut first = None;
            let mut index = 0;
            while index < workflow.vessels.len() {
                let vessel = workflow.vessels[index].clone();
                let needs = vessel.crew.iter().flat_map(|crew| crew.needs.iter().cloned()).collect::<BTreeSet<_>>();
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
                            vessel.crew[0].role,
                            needs.iter().map(ToString::to_string).collect::<Vec<_>>().join(" + ")
                        ));
                    }
                };
                if purpose == PlacementPurpose::Admission {
                    resolve_and_validate_workflow_credentials_for_capability_admission(
                        &self.backend,
                        namespace,
                        Some(project_ref),
                        repositories,
                        resolution.selected.as_ref(),
                        &mut one,
                    )
                    .await?;
                }
                if purpose == PlacementPurpose::Admission {
                    self.join_placement_builds(namespace, &mut resolution).await?;
                }
                workflow.vessels[index] = one.vessels.remove(0);
                if let Some(selected) = resolution.selected.as_ref() {
                    let decision = PlacementDecision {
                        minimal_alternatives: alternatives.clone(),
                        escalation_reason: intent.escalation_reason.clone(),
                        policy_name: selected.metadata.name.clone(),
                        target_host: placement_target_host(&self.backend, namespace, selected).await?,
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
            let mut result = self.decide_capability_placement(context, workflow, &needs).await?;
            if purpose == PlacementPurpose::Admission {
                if has_kinds {
                    resolve_and_validate_workflow_credentials_for_capability_admission(
                        &self.backend,
                        namespace,
                        Some(project_ref),
                        repositories,
                        result.0.selected.as_ref(),
                        workflow,
                    )
                    .await?;
                } else {
                    resolve_and_validate_workflow_credentials(
                        &self.backend,
                        namespace,
                        Some(project_ref),
                        repositories,
                        result.0.selected.as_ref(),
                        workflow,
                    )
                    .await?;
                }
            }
            if purpose == PlacementPurpose::Admission {
                self.join_placement_builds(namespace, &mut result.0).await?;
            }
            result
        };
        Ok((placement, minimal_alternatives, vessel_placements))
    }

    pub(super) async fn prepare_convoy_admission_with_preferences(
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
        let (placement, minimal_alternatives, vessel_placements) = self
            .decide_vessel_placements(
                &PlacementContext::builder()
                    .namespace(namespace)
                    .project_ref(project_ref)
                    .repositories(&repositories_snapshot)
                    .intent(intent)
                    .purpose(PlacementPurpose::Admission)
                    .build(),
                &mut workflow,
            )
            .await?;
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
                target_host: placement_target_host(&self.backend, namespace, selected).await?,
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
        Ok(PreparedConvoyAdmission::builder()
            .name(String::new())
            .spec(spec)
            .workflow(workflow)
            .maybe_placement_policy(placement.selected.map(|placement| placement.spec))
            .maybe_placement_decision(placement_decision)
            .vessel_placements(vessel_placements)
            .build())
    }

    pub(super) async fn admit_convoy_start(
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
        let _admission_guard = self.lock().await;
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation(&self.backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role).await?;
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

    pub(super) async fn check_local_free_space_floor(&self) -> Result<(), String> {
        let config = Arc::clone(&self.config);
        let available_space_probe = Arc::clone(&self.discovery.available_space_probe);
        let admission_free_space_path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let host_name = self.host_name.to_string();
        crate::probe::blocking("free-space admission", crate::probe::PROBE_TIMEOUT, move || {
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

    pub(super) fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        let floor_gib = self.config.load_daemon_config()?.admission.free_space_floor_gib;
        crate::admission::free_space_floor_bytes(floor_gib)
    }

    pub(super) async fn check_remote_placement_free_space_floor(
        &self,
        namespace: &str,
        placement: Option<&PlacementDecision>,
    ) -> Result<(), String> {
        let Some(placement) = placement else {
            return Ok(());
        };
        let target_host = &placement.target_host;

        let sources = self.backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        let matching_sources =
            sources.items.into_iter().filter(|source| source.object.metadata.name == target_host.reference.as_str()).collect::<Vec<_>>();
        let has_replica = matching_sources.iter().any(|source| matches!(source.provenance, ResourceProvenance::Replica { .. }));
        let is_host_targeted_placement = self
            .backend
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

    /// Placement policies are home-bound, so identical content on different
    /// origins must have distinct names. Always author an origin's own snapshot:
    /// reusing a remote snapshot would let its GC race references in flight.
    fn placement_snapshot_name(&self, spec: &serde_json::Value) -> Result<String, String> {
        let root = self.backend.local_root().map_err(|error| error.to_string())?;
        prepared_snapshot_name("placement", &serde_json::json!({ "origin": root, "spec": spec }))
    }

    pub(super) async fn create_convoy_with_workflow_snapshot(
        &self,
        namespace: &str,
        name: &str,
        bundle: ConvoySnapshotBundle<'_>,
        dispatch_regard: ConvoyDispatchRegard,
    ) -> Result<(), String> {
        let ConvoySnapshotBundle { spec, workflow, placement, placement_decision, vessel_placements } = bundle;
        let workflow_value = serde_json::to_value(workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.backend, namespace, &workflow_name, workflow).await?;
        let mut annotations = BTreeMap::from([(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name)]);
        if self.write_admission_briefs(namespace, name, spec, workflow).await? {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = placement {
            if let Some(docker) = &placement.docker_per_vessel {
                if let flotilla_resources::DockerImageSource::Composition { composition } = &docker.image {
                    let mut frozen = flotilla_resources::FrozenImageLayers::default();
                    frozen.include(composition)?;
                    annotations.insert(
                        flotilla_resources::IMAGE_LAYERS_ANNOTATION.to_string(),
                        serde_json::to_string(&frozen).map_err(|error| error.to_string())?,
                    );
                }
            }
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = self.placement_snapshot_name(&placement_value)?;
            ensure_prepared_placement_snapshot(&self.backend, namespace, &placement_name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        if let Some(vessel_placements) = vessel_placements {
            annotations.extend(self.prepare_vessel_placement_annotations(namespace, vessel_placements).await?);
        }
        self.create_convoy_with_annotations(namespace, name, spec, placement_decision, dispatch_regard, annotations).await
    }

    pub(super) async fn write_admission_briefs(
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
        let templates = CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path());
        let repositories = self.backend.clone().using::<Repository>(namespace);
        let checkouts = self.backend.clone().using::<ResourceCheckout>(namespace);
        let local_checkouts = crate::repository_addressing::local_checkouts(
            &self.backend,
            &self.observed_backend,
            namespace,
            self.environment_manager.local_host_id().as_str(),
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
                    .filter_map(|checkout| super::checkout_path(checkout).map(PathBuf::from))
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
                .map(|(index, member)| crate::agent_adapter::CrewBriefMember {
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
                writer.put_brief(namespace, name, &process.role, name, brief.content.as_bytes(), options.charter_commit.as_deref()).await?;
            }
        }
        Ok(true)
    }

    pub(super) async fn prepare_vessel_placement_annotations(
        &self,
        namespace: &str,
        placements: &BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>,
    ) -> Result<BTreeMap<String, String>, String> {
        if placements.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut pins = BTreeMap::new();
        let mut frozen_images = flotilla_resources::FrozenImageLayers::default();
        for (vessel, (policy, decision)) in placements {
            if let Some(docker) = &policy.docker_per_vessel {
                if let flotilla_resources::DockerImageSource::Composition { composition } = &docker.image {
                    frozen_images.include(composition)?;
                }
            }
            let value = serde_json::to_value(policy).map_err(|error| error.to_string())?;
            let name = self.placement_snapshot_name(&value)?;
            ensure_prepared_placement_snapshot(&self.backend, namespace, &name, policy).await?;
            pins.insert(vessel.clone(), flotilla_resources::VesselPlacementPin { policy_ref: name, decision: decision.clone() });
        }
        let mut annotations = BTreeMap::from([(
            flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION.to_string(),
            serde_json::to_string(&pins).map_err(|error| error.to_string())?,
        )]);
        if !frozen_images.layers.is_empty() {
            annotations.insert(
                flotilla_resources::IMAGE_LAYERS_ANNOTATION.to_string(),
                serde_json::to_string(&frozen_images).map_err(|error| error.to_string())?,
            );
        }
        Ok(annotations)
    }

    pub(super) async fn create_convoy_with_annotations(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        placement_decision: Option<PlacementDecision>,
        dispatch_regard: ConvoyDispatchRegard,
        annotations: BTreeMap<String, String>,
    ) -> Result<(), String> {
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

    pub(super) async fn emit_implicit_convoy_regard(
        &self,
        namespace: &str,
        name: &str,
        principal_ref: &PrincipalRef,
    ) -> Result<(), String> {
        let target = ResourceRef::new(api_version(ResourceConvoy::API_PATHS), ResourceConvoy::API_PATHS.kind, namespace, name);
        self.regard_lifecycle.emit_implicit(principal_ref, &target, "convoy-dispatch").await
    }

    pub(super) async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        let target = binding.resource_ref().ok_or_else(|| "resolved attach target has no resource identity".to_string())?;
        match self.regard_lifecycle.emit_expressed_for_surface(surface_id, &target).await? {
            SurfaceGestureOutcome::Handled => Ok(()),
            SurfaceGestureOutcome::UnknownSurface => {
                self.regard_lifecycle.emit_expressed(&PrincipalRef::implicit_for_namespace(&binding.namespace), &target).await
            }
        }
    }

    pub(super) async fn resolve_convoy_placement(
        &self,
        namespace: &str,
        project_ref: Option<&str>,
        repositories: &[ConvoyRepositorySpec],
        workflow: &WorkflowTemplateSpec,
        placement_policy: Option<&str>,
        allow_unready: bool,
    ) -> Result<PlacementResolution, String> {
        self.decide_placement(
            PolicyPlacementContext::builder()
                .namespace(namespace)
                .maybe_project_ref(project_ref)
                .repositories(repositories)
                .maybe_placement_policy(placement_policy)
                .allow_unready(allow_unready)
                .purpose(PlacementPurpose::Admission)
                .build(),
            workflow,
        )
        .await
    }

    async fn decide_placement(
        &self,
        context: PolicyPlacementContext<'_>,
        workflow: &WorkflowTemplateSpec,
    ) -> Result<PlacementResolution, String> {
        let PolicyPlacementContext { namespace, project_ref, repositories, placement_policy, allow_unready, purpose } = context;
        let mut placement = match placement_policy {
            Some(policy) => {
                let policy = required_admission_value(policy, "placement policy")?;
                let resolved = self
                    .backend
                    .clone()
                    .including_replicas::<PlacementPolicy>(namespace)
                    .get(policy)
                    .await
                    .map(|source| source.object)
                    .map_err(|error| format!("placement policy {policy}: {error}"))?;
                if purpose == PlacementPurpose::Admission {
                    validate_docker_placement_host(&self.backend, namespace, &resolved).await?;
                }
                PlacementResolution {
                    selected: Some(resolved),
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                }
            }
            None => {
                let local_host_id = self.canonical_local_host_id();
                let placement = decide_default_placement(
                    &self.backend,
                    namespace,
                    project_ref,
                    repositories,
                    workflow,
                    local_host_id.as_ref(),
                    purpose,
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
        if purpose == PlacementPurpose::Admission {
            if let Some(docker) = placement.selected.as_mut().and_then(|policy| policy.spec.docker_per_vessel.as_mut()) {
                let needs = workflow
                    .vessels
                    .iter()
                    .flat_map(|vessel| &vessel.crew)
                    .flat_map(|crew| &crew.needs)
                    .filter(|need| need.is_image_need())
                    .map(CapabilityNeed::capability_string)
                    .collect();
                docker.image = freeze_admission_image(&self.backend, namespace, &docker.image, &needs).await?;
            }
            validate_workflow_agent_adapters(&self.backend, namespace, workflow, placement.selected.as_ref(), allow_unready).await?;
        }
        Ok(placement)
    }
}

impl ConvoyAdmission {
    pub(super) async fn admit_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        mut admission: PreparedConvoyAdmission,
        mut annotations: BTreeMap<String, String>,
    ) -> Result<String, String> {
        let _admission_guard = self.lock().await;
        let existing = self
            .backend
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
            allocate_convoy_generation(&self.backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role).await?;
        let workflow_value = serde_json::to_value(&admission.workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.backend, namespace, &workflow_name, &admission.workflow).await?;
        annotations.insert(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name);
        if self.write_admission_briefs(namespace, &admission.name, &admission.spec, &admission.workflow).await? {
            annotations.insert(BRIEF_ARTIFACTS_ANNOTATION.to_string(), "true".to_string());
        }
        if let Some(placement) = &admission.placement_policy {
            if let Some(docker) = &placement.docker_per_vessel {
                if let flotilla_resources::DockerImageSource::Composition { composition } = &docker.image {
                    let mut frozen = flotilla_resources::FrozenImageLayers::default();
                    frozen.include(composition)?;
                    annotations.insert(
                        flotilla_resources::IMAGE_LAYERS_ANNOTATION.to_string(),
                        serde_json::to_string(&frozen).map_err(|error| error.to_string())?,
                    );
                }
            }
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = self.placement_snapshot_name(&placement_value)?;
            ensure_prepared_placement_snapshot(&self.backend, namespace, &placement_name, placement).await?;
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
    pub(super) async fn admit_created_convoy(
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
        } = request;
        let address = convoy_address(role, project_ref.as_deref());
        let generation = match allocate_convoy_generation(&self.backend, namespace, project_ref.as_deref(), role).await {
            Ok(generation) => generation,
            Err(message) => {
                if let Some(checkout_ref) = adopted_checkout_ref_to_cleanup {
                    let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                    let cleanup = async {
                        self.backend.clone().using::<ResourceCheckout>(namespace).delete(&checkout_ref).await?;
                        crate::observed_resources::delete_stale_adopted_checkouts(&self.backend, &self.observed_backend, namespace).await
                    }
                    .await;
                    if let Err(error) = cleanup {
                        warn!(%error, %checkout_ref, "failed to clean up adopted checkout after convoy identity conflict");
                    }
                }
                return CommandValue::Error { message };
            }
        };
        let spec = ConvoySpec {
            workflow_ref: workflow_ref.to_string(),
            role: role.to_string(),
            generation,
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
        };
        match self
            .create_convoy_with_workflow_snapshot(
                namespace,
                name,
                ConvoySnapshotBundle::builder()
                    .spec(&spec)
                    .workflow(workflow)
                    .maybe_placement(placement.selected.as_ref().map(|placement| &placement.spec))
                    .maybe_placement_decision(placement_decision)
                    .build(),
                ConvoyDispatchRegard::Emit,
            )
            .await
        {
            Ok(()) => CommandValue::ConvoyCreated { name: address },
            Err(message) => CommandValue::Error { message },
        }
    }
}

#[derive(bon::Builder)]
pub(super) struct ConvoyStartTask {
    pub(super) command_id: u64,
    pub(super) intent: flotilla_protocol::ConvoyStartIntent,
    pub(super) key: ConvoyStartKey,
    pub(super) dispatching_principal_ref: PrincipalRef,
}

#[derive(bon::Builder)]
pub struct PreparedConvoyAdmission {
    pub(super) name: String,
    pub(super) spec: ConvoySpec,
    pub(super) workflow: WorkflowTemplateSpec,
    pub(super) placement_policy: Option<PlacementPolicySpec>,
    pub(super) placement_decision: Option<PlacementDecision>,
    #[builder(default)]
    pub(super) vessel_placements: BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>,
}

#[derive(Clone, Debug)]
pub(super) struct AllocationRole {
    pub(super) crew: CrewSpec,
    pub(super) hint: String,
    pub(super) repository_refs: Option<Vec<RepositoryKey>>,
    pub(super) depends_on: Vec<String>,
    pub(super) credential_signature: String,
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

pub(super) fn expand_allocation_roles(workflow: &mut WorkflowTemplateSpec, project: &ProjectSpec) -> Result<Vec<AllocationRole>, String> {
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

pub(super) fn allocate_roles(workflow: &mut WorkflowTemplateSpec, roles: &[AllocationRole]) -> Result<(), String> {
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

pub(super) fn refresh_crossed_handoffs(workflow: &mut WorkflowTemplateSpec) {
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

pub(super) async fn allocation_credential_grants(
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

pub(super) fn convoy_record_name() -> String {
    format!("convoy-{}", uuid::Uuid::new_v4().simple())
}

pub(super) fn convoy_ensure_name(project: &str, role: &str) -> String {
    let digest = Sha256::digest(format!("{project}\0{role}").as_bytes());
    format!("ensure-{digest:x}")
}

pub(super) fn convoy_address(role: &str, project: Option<&str>) -> String {
    project.map_or_else(|| role.to_string(), |project| format!("{role}@{project}"))
}

pub(super) fn convoy_disambiguation_address(role: &str, project: Option<&str>) -> String {
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

pub(super) async fn allocate_convoy_generation(
    backend: &ResourceBackend,
    namespace: &str,
    project: Option<&str>,
    role: &str,
) -> Result<u64, String> {
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

pub(super) fn parse_role_address(value: &str) -> Result<(&str, Option<&str>), String> {
    match value.split_once('@') {
        Some((role, project)) if !role.is_empty() && !project.contains('@') => Ok((role, Some(project))),
        Some(_) => Err(format!("invalid convoy address `{value}`: expected role@project")),
        None if value.is_empty() => Err("convoy role cannot be empty".to_string()),
        None => Ok((value, None)),
    }
}

pub(super) struct ConvoyAddressIdentity<'a> {
    pub(super) record_name: &'a str,
    pub(super) role: Option<&'a str>,
    pub(super) project: Option<&'a str>,
    pub(super) terminal: bool,
}

pub(super) fn resolve_convoy_candidate_indices(identities: &[ConvoyAddressIdentity<'_>], address: &str) -> Result<Vec<usize>, String> {
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

#[derive(bon::Builder)]
pub(super) struct ConvoySnapshotBundle<'a> {
    pub(super) spec: &'a ConvoySpec,
    pub(super) workflow: &'a WorkflowTemplateSpec,
    pub(super) placement: Option<&'a PlacementPolicySpec>,
    pub(super) placement_decision: Option<PlacementDecision>,
    pub(super) vessel_placements: Option<&'a BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>>,
}

/// An issue body is the crew's contract, so admission may only reuse a
/// recently observed snapshot. Keep this deliberately fixed until an
/// operational need establishes that it should be configurable.
const ISSUE_SNAPSHOT_FRESHNESS: ChronoDuration = ChronoDuration::minutes(5);
pub(super) fn issue_snapshot_is_fresh(issue: &flotilla_protocol::Issue) -> bool {
    let Some(observed_at) = issue.observed_at else { return false };
    let age = Utc::now().signed_duration_since(observed_at);
    (ChronoDuration::zero()..=ISSUE_SNAPSHOT_FRESHNESS).contains(&age)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ConvoyStartKey {
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
    pub(super) fn new(namespace: String, intent: &flotilla_protocol::ConvoyStartIntent) -> Self {
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

pub(super) struct ResolvedConvoyChangeRequestAdmission {
    pub(super) binding: BoundChangeRequest,
    pub(super) branch: String,
    pub(super) base_ref: String,
}

pub(super) struct RepositoryChangeRequestProvider {
    pub(super) service_url: String,
    pub(super) repository: String,
    pub(super) provider: Arc<dyn ChangeRequestTracker>,
}

pub(super) async fn discover_repository_change_request_with(
    resource_backend: &ResourceBackend,
    config: &ConfigStore,
    discovery: &DiscoveryRuntime,
    environment_manager: &EnvironmentManager,
    local_environment_id: &EnvironmentId,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<Arc<dyn ChangeRequestTracker>, String> {
    let identity = repository.forge().ok_or("no forge identity")?;
    let bag = repository_provider_bag(resource_backend, config, environment_manager, local_environment_id, namespace, repository).await?;
    let probe_root = ExecutionEnvironmentPath::new(config.base_path().as_ref());
    let mut unmet = Vec::new();
    for factory in &discovery.factories.change_requests {
        match factory.probe(&bag, config, &probe_root, Arc::clone(&discovery.runner)).await {
            Ok(provider) => return Ok(provider),
            Err(requirements) => unmet.extend(requirements.into_iter().map(|requirement| format!("{requirement:?}"))),
        }
    }
    Err(format!("change request provider unavailable for {} ({})", identity.service_url, unmet.join(", ")))
}

pub(super) async fn repository_provider_bag(
    resource_backend: &ResourceBackend,
    config: &ConfigStore,
    environment_manager: &EnvironmentManager,
    local_environment_id: &EnvironmentId,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<EnvironmentBag, String> {
    let host_bag = environment_manager.environment_bag(local_environment_id).ok_or("local discovery environment unavailable")?;
    // Host capabilities cannot contribute an ambient checkout's forge target.
    let host_bag = host_bag
        .assertions()
        .iter()
        .filter(|assertion| {
            !matches!(
                assertion,
                EnvironmentAssertion::AuthFileExists { provider, .. } if provider == FORGEJO_AUTH_PROVIDER
            ) && !matches!(
                assertion,
                EnvironmentAssertion::RemoteHost { .. }
                    | EnvironmentAssertion::OriginForge { .. }
                    | EnvironmentAssertion::VcsCheckoutDetected { .. }
            )
        })
        .fold(EnvironmentBag::new(), |bag, assertion| bag.with(assertion.clone()));
    let Some(identity) = repository.forge() else {
        return Ok(host_bag);
    };
    let remote = format!("{}/{}", identity.service_url.trim_end_matches('/'), identity.repository);
    let forge = match repository.identity() {
        RepositoryIdentity::Forge { forge_ref, .. } => Some(
            resource_backend
                .including_replicas::<Forge>(namespace)
                .get(forge_ref)
                .await
                .map_err(|error| format!("Forge {forge_ref}: {error}"))?
                .object
                .spec,
        ),
        _ => forge_for_remote(resource_backend, namespace, &remote).await?,
    };
    let remote_assertion = remote_assertion(&remote, "origin").ok_or_else(|| format!("invalid repository remote {remote}"))?;
    let mut bag = host_bag.with(remote_assertion);
    if let Some(forge) = &forge {
        bag = bag.with(EnvironmentAssertion::origin_forge(forge.clone()));
        if forge.kind == ForgeKind::Forgejo {
            // Read current intent on each request: provider leases compare the
            // resolved bag so a changed identity cannot reuse a stale credential.
            if let Some(name) = config.load_daemon_config()?.credentials.forgejo.get(&forge.forge_id) {
                let credential = resource_backend
                    .definitions::<CredentialSpec>(namespace)
                    .get(name)
                    .await
                    .map_err(|error| format!("daemon Forgejo credential {name}: {error}"))?;
                match (&credential.spec.consumer, &credential.spec.source) {
                    (CredentialConsumer::Forgejo { forge_ref, .. }, CredentialSource::File { path }) if forge_ref == &forge.forge_id => {
                        bag = bag.with(EnvironmentAssertion::auth_file(FORGEJO_AUTH_PROVIDER, path));
                    }
                    _ => {
                        return Err(format!("daemon Forgejo credential {name} must use a file source and target Forge {}", forge.forge_id))
                    }
                }
            }
        }
    }
    Ok(bag)
}

#[derive(Debug)]
pub(super) struct PlacementResolution {
    pub(super) selected: Option<ResourceObject<PlacementPolicy>>,
    pub(super) refused_candidates: Vec<PlacementRefusal>,
    pub(super) viable_not_selected: Vec<PlacementViableCandidate>,
    pub(super) allocation: Option<FulfilmentAllocation>,
}

pub(super) struct KindCandidate {
    pub(super) kind: ResourceObject<FulfilmentKind>,
    pub(super) placement: PlacementResolution,
    pub(super) free_slots: Option<u32>,
    pub(super) host_ready: bool,
    pub(super) sleeping_until: Option<DateTime<Utc>>,
}

pub(super) trait PlacementCandidateOrder: Send + Sync {
    fn compare(&self, left: &KindCandidate, right: &KindCandidate) -> std::cmp::Ordering;
    fn reserved(&self, candidate: &KindCandidate) -> bool;
    fn available(&self, candidate: &KindCandidate) -> bool;
}

pub(super) trait FulfilmentDecider: Send + Sync {
    fn for_admission<'a>(&'a self, needs: &'a BTreeSet<CapabilityNeed>, now: DateTime<Utc>) -> Box<dyn PlacementCandidateOrder + 'a>;
}

pub(super) struct StaticFulfilmentDecider;

impl FulfilmentDecider for StaticFulfilmentDecider {
    fn for_admission<'a>(&'a self, needs: &'a BTreeSet<CapabilityNeed>, now: DateTime<Utc>) -> Box<dyn PlacementCandidateOrder + 'a> {
        Box::new(PlacementTieBreak { needs, now })
    }
}

pub(super) struct PlacementTieBreak<'a> {
    pub(super) needs: &'a BTreeSet<CapabilityNeed>,
    pub(super) now: DateTime<Utc>,
}

impl PlacementTieBreak<'_> {
    pub(super) fn reserved(&self, candidate: &KindCandidate) -> bool {
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

#[cfg(test)]
pub(super) async fn default_convoy_placement_policy(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
    local_host_ref: Option<&CanonicalHostId>,
) -> Result<PlacementResolution, String> {
    decide_default_placement(backend, namespace, project_ref, repositories, workflow, local_host_ref, PlacementPurpose::Admission).await
}

async fn decide_default_placement(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
    local_host_ref: Option<&CanonicalHostId>,
    purpose: PlacementPurpose,
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
        let agentless_unready = if purpose == PlacementPurpose::Admission && agentless_ssh {
            placement_agent_adapters(backend, namespace, &policy, false).await.err()
        } else {
            None
        };
        let refusal = if purpose == PlacementPurpose::Routing {
            None
        } else if let Err(reason) = validate_docker_placement_host(backend, namespace, &policy).await {
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

pub(super) fn required_admission_value<'a>(value: &'a str, field: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value)
    }
}

pub(super) fn resolve_project_ref(default_namespace: &str, value: &str) -> Result<(String, String), String> {
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

pub(super) fn normalize_convoy_start_intent(
    default_namespace: &str,
    intent: &flotilla_protocol::ConvoyStartIntent,
) -> Result<(String, flotilla_protocol::ConvoyStartIntent), String> {
    let (namespace, project_ref) = resolve_project_ref(default_namespace, &intent.project_ref)?;
    let mut intent = intent.clone();
    intent.namespace = Some(namespace.clone());
    intent.project_ref = project_ref;
    Ok((namespace, intent))
}

pub(super) fn project_not_ready_error(namespace: &str, project_ref: &str, error: ResourceError) -> String {
    match error {
        ResourceError::NotFound { name } => {
            format!("project {namespace}/{project_ref} is not ready: resource not found: {name} (tried {namespace}/{project_ref})")
        }
        error => format!("project {project_ref} is not ready: {error}"),
    }
}

pub(super) fn workflow_has_in_crew_review(workflow: &WorkflowTemplateSpec) -> bool {
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

pub(super) async fn validate_fork_workflow_admission(
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

pub(super) async fn validate_workflow_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    allow_unready: bool,
) -> Result<(), String> {
    let required_adapters = required_workflow_agent_adapters(workflow)?;
    // Validate each candidate's image or composition once, even for tool-only workflows.
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

pub(super) async fn resolve_workflow_credentials(
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

pub(super) async fn resolve_and_validate_workflow_credentials(
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

pub(super) async fn resolve_and_validate_workflow_credentials_for_capability_admission(
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

pub(super) async fn validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
) -> Result<(), String> {
    validate_workflow_credentials_with_capabilities(backend, namespace, workflow, placement, &CapabilityTable::seeded()).await
}

pub(super) async fn validate_workflow_credentials_with_capabilities(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    capabilities: &CapabilityTable,
) -> Result<(), String> {
    validate_workflow_credentials_with_capabilities_for_admission(backend, namespace, workflow, placement, capabilities, false).await
}

pub(super) async fn validate_workflow_credentials_with_capabilities_for_admission(
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
pub(super) fn ambient_credential_dependent_vessels<'workflow>(
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
pub(super) fn valid_agent_override_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

pub(super) fn apply_agent_overrides(
    workflow: &mut WorkflowTemplateSpec,
    overrides: &[flotilla_protocol::AgentOverride],
) -> Result<(), String> {
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

pub(super) fn required_workflow_agent_adapters(workflow: &WorkflowTemplateSpec) -> Result<BTreeSet<String>, String> {
    required_agent_adapters(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
}

/// Generation 1 never replaces the running baseline with an unbuilt image.
/// Selection is structural; revisions are frozen in the placement snapshot.
async fn freeze_admission_image(
    backend: &ResourceBackend,
    namespace: &str,
    image: &flotilla_resources::DockerImageSource,
    needs: &BTreeSet<String>,
) -> Result<flotilla_resources::DockerImageSource, String> {
    use flotilla_resources::{compose_image, DockerImageSource, FrozenImageLayers, ImageLayer};
    let (selection, baseline_image, mut frozen) = match image {
        DockerImageSource::Baseline { image_baseline_ref } => {
            let baseline = flotilla_resources::CrewImageBaseline::resolve(image_baseline_ref, &backend.definitions(namespace)).await?;
            let baseline_image = baseline.image;
            let Some(selection) = baseline.layers else { return Ok(baseline_image.into()) };
            (selection, Some(baseline_image), FrozenImageLayers::default())
        }
        DockerImageSource::Composition { composition } => (composition.selection.clone(), composition.baseline_image.clone(), {
            let mut frozen = FrozenImageLayers::default();
            frozen.include(composition)?;
            frozen
        }),
        DockerImageSource::Literal(_) => return image.resolve(&backend.definitions(namespace)).await.map(Into::into),
    };
    let catalogue = backend
        .definitions::<ImageLayer>(namespace)
        .list()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|layer| {
            layer.metadata.deletion_timestamp.is_none() && layer.metadata.merge.as_ref().is_none_or(|merge| merge.conflicts.is_empty())
        })
        .map(|layer| (layer.metadata.name, layer.spec))
        .collect();
    let mut composition = compose_image(&selection, needs, &catalogue, &mut frozen)?;
    composition.baseline_image = baseline_image;
    if let DockerImageSource::Composition { composition: previous } = image {
        if composition.layers == previous.layers {
            composition.build_refs = previous.build_refs.clone();
            if let Some(identity) = &previous.identity {
                composition.bind(identity.clone())?;
            }
        }
    }
    Ok(DockerImageSource::Composition { composition: Box::new(composition) })
}

pub(super) async fn placement_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    placement: &ResourceObject<PlacementPolicy>,
    allow_unready: bool,
) -> Result<(BTreeSet<String>, String), String> {
    if let Some(docker) = &placement.spec.docker_per_vessel {
        // Adapter testimony is structural. Unbound compositions acquire their
        // image identity after admission, through ImageBuild and verification.
        let detail = match &docker.image {
            flotilla_resources::DockerImageSource::Composition { composition }
                if composition.identity.is_none() && composition.baseline_image.is_none() =>
            {
                format!("image composition based on `{}`", composition.selection.base)
            }
            _ => format!("image `{}`", docker.image.resolve(&backend.definitions(namespace)).await?),
        };
        Ok((docker.agent_adapters.clone(), detail))
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

pub(super) fn convoy_fallback_slug(title: &str, id: &str) -> String {
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

pub(super) fn convoy_issues_fallback_slug(issues: &[ConvoyIssue], project_display_name: &str, project_ref: &str) -> String {
    match issues {
        [] => convoy_fallback_slug(project_display_name, project_ref),
        [issue] => convoy_fallback_slug(&issue.snapshot.title, &issue.reference.id),
        issues => {
            let issue_ids = issues.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>().join("-");
            convoy_fallback_slug("batch-issues", &issue_ids)
        }
    }
}

pub(super) fn convoy_issue_name_context(issue: &ConvoyIssue) -> String {
    format!("Issue {}: {}\n{}", issue.reference.id, issue.snapshot.title, issue.snapshot.body.as_deref().unwrap_or_default())
}

pub(super) fn validate_convoy_name(name: &str) -> Result<(), String> {
    if name.len() > 63
        || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !name.bytes().last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("convoy name `{name}` must be a lowercase DNS label of at most 63 characters"));
    }
    Ok(())
}

pub(super) fn validate_convoy_branch(branch: &str) -> Result<(), String> {
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

pub(super) fn parse_ad_hoc_capability_need(value: &str) -> Result<CapabilityNeed, String> {
    let need = value.parse::<CapabilityNeed>()?;
    if matches!(&need, CapabilityNeed::Platform(platform) if platform == Platform::MATRIX_PLACEHOLDER) {
        return Err("platform:$matrix is only valid on workflow roles or Project role needs".to_string());
    }
    Ok(need)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_resources::{
        FulfilmentCostClass, FulfilmentKindSpec, FulfilmentRealisation, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec,
    };

    use super::*;
    use crate::providers::{
        discovery::test_support::{fake_discovery, FakeChangeRequest},
        types::ChangeRequest,
    };

    // #2701: Projects with either retired builtin reference admit using the
    // current rules even when the stale builtin still exists, or has been deleted.
    #[tokio::test]
    async fn retired_project_workflow_admits_current_turn_rules() {
        let temp = tempfile::tempdir().expect("config");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"retired-workflow-test\"\n").expect("machine identity");
        let daemon =
            InProcessDaemon::new(Vec::new(), Arc::new(ConfigStore::with_base(temp.path())), fake_discovery(false), HostName::local()).await;
        let templates = daemon.resource_backend().definitions::<WorkflowTemplate>("flotilla");
        let current = flotilla_resources::single_agent_workflow_spec();
        templates.create(&InputMeta::builder().name("single-agent".into()).build(), &current).await.expect("current workflow");
        for retired in ["single-agent-contained", "single-agent-trusted"] {
            let mut stale = current.clone();
            stale.turn_delivery.shift_remove("checks-settled");
            templates.create(&InputMeta::builder().name(retired.into()).build(), &stale).await.expect("stale workflow");
            let project = ProjectSpec::builder().display_name("Flotilla".into()).default_workflow_ref(retired.into()).build();
            let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("flotilla".into()).build();
            for deleted in [false, true] {
                if deleted {
                    templates.delete(retired).await.expect("retire stored builtin");
                }
                let (name, frozen) = daemon
                    .convoy_admission
                    .resolve_convoy_admission_workflow("flotilla", "flotilla", &project, &[], &intent)
                    .await
                    .expect("admit retired Project reference");
                flotilla_resources::validate(&frozen).expect("admitted workflow must remain valid after resolution");
                assert_eq!(name, "single-agent");
                assert!(frozen.turn_delivery.contains_key("checks-settled"));
                assert!(frozen.turn_delivery.contains_key("merged-unclaimed"));
            }
        }
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

    #[tokio::test]
    async fn admission_freezes_role_selections_and_refuses_missing_imports() {
        // Intended: admission resolves declarations once into the workflow that
        // will be persisted. Later defaults cannot alter that admitted snapshot.
        use flotilla_resources::{CrewDefaults, CrewDefaultsSpec, Selector, SkillCatalogEntry};

        use crate::providers::discovery::test_support::TestEnvVars;
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"admission-skills-test\"\n").expect("config");
        let catalog = ["research", "testing", "implement", "wayfinder", "review"]
            .into_iter()
            .map(|name| SkillCatalogEntry {
                source: "source".into(),
                repository: "owner/repo".into(),
                revision: "1".repeat(40),
                name: name.into(),
                path: format!("skills/{name}"),
            })
            .collect::<Vec<_>>();
        std::fs::write(temp.path().join(".flotilla-skill-catalog.json"), serde_json::to_string(&catalog).expect("catalog"))
            .expect("catalog file");
        std::fs::write(temp.path().join(".flotilla-sources.json"), serde_json::json!({"schema_version":5,"sources":[{"name":"source", "repository":"https://github.com/owner/repo.git", "revision":"1".repeat(40)}]}).to_string()).expect("source manifest");
        let mut discovery = fake_discovery(false);
        discovery.env = Arc::new(TestEnvVars::new([("FLOTILLA_SKILLS_DIR", temp.path().display().to_string())]));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            discovery,
            HostName::new("test-host"),
            backend.clone(),
        )
        .await;
        let defaults = CrewDefaultsSpec {
            project_ref: None,
            default_workflow_ref: None,
            roles: BTreeMap::new(),
            skills: BTreeMap::from([
                ("*".into(), vec!["research".into()]),
                ("coder".into(), vec!["testing".into(), "implement".into()]),
                ("governor".into(), vec!["wayfinder".into()]),
            ]),
        };
        let meta = InputMeta::builder().name("fleet".to_string()).build();
        backend.definitions::<CrewDefaults>("flotilla").apply(&meta, &defaults).await.expect("defaults");
        let crew = |role: &str| {
            CrewSpec::builder()
                .role(role.into())
                .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                .build()
        };
        let template = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder().name("work".into()).crew(vec![crew("coder"), crew("governor")]).build()])
            .build();
        backend
            .definitions::<WorkflowTemplate>("flotilla")
            .apply(&InputMeta::builder().name("work".into()).build(), &template)
            .await
            .expect("template");
        let project = ProjectSpec::builder()
            .display_name("Example".into())
            .default_workflow_ref("work".into())
            .skills(BTreeMap::from([("coder".into(), vec!["-testing".into()])]))
            .build();
        let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).skills(vec!["review".into()]).build();
        let (_, frozen) = daemon
            .convoy_admission
            .resolve_convoy_admission_workflow("flotilla", "example", &project, &[], &intent)
            .await
            .expect("admission");
        assert_eq!(frozen.vessels[0].crew[0].skills.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), [
            "implement",
            "research",
            "review"
        ]);
        assert_eq!(frozen.vessels[0].crew[1].skills.selected.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>(), [
            "research",
            "review",
            "wayfinder"
        ]);
        backend.definitions::<CrewDefaults>("flotilla").apply(&meta, &CrewDefaultsSpec::default()).await.expect("change defaults");
        assert_eq!(frozen.vessels[0].crew[0].skills.selected.len(), 3);
        let mut invalid = intent;
        invalid.skills = vec!["missing".into()];
        let error = daemon
            .convoy_admission
            .resolve_convoy_admission_workflow("flotilla", "example", &project, &[], &invalid)
            .await
            .expect_err("missing import refuses admission");
        assert!(error.contains("missing") && error.contains("dispatch") && error.contains("owner/repo"), "{error}");
    }

    // #2719: real admission applies fleet/parent/project defaults before
    // convoy selectors and dispatch; explain provenance names the winner.
    #[tokio::test]
    async fn admission_cascade_precedence_and_dispatch_are_frozen() {
        use flotilla_resources::{FleetDesignation, FleetDesignationSpec, RoleDefinition};
        let temp = tempfile::tempdir().expect("config");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"cascade-test\"\n").expect("machine identity");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("cascade"),
            backend.clone(),
        )
        .await;
        let projects = backend.definitions::<Project>("flotilla");
        let shape = |model: &str| {
            BTreeMap::from([("coder".into(), RoleDefinition {
                agent: Some("claude-code".into()),
                model: Some(model.into()),
                ..Default::default()
            })])
        };
        let fleet = ProjectSpec::builder()
            .display_name("Fleet".into())
            .default_workflow_ref("shared-work".into())
            .role_definitions(shape("fleet"))
            .build();
        projects.apply(&InputMeta::builder().name("fleet".into()).build(), &fleet).await.expect("fleet");
        backend
            .definitions::<FleetDesignation>("flotilla")
            .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec { project: "fleet".into() })
            .await
            .expect("designation");
        let templates = backend.definitions::<WorkflowTemplate>("flotilla");
        let mut template = flotilla_resources::single_agent_workflow_spec();
        let meta = InputMeta::builder()
            .name("fleet--shared-work".into())
            .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.into(), "fleet".into())]))
            .build();
        templates.apply(&meta, &template).await.expect("inherited workflow");
        for position in 0..5 {
            let parent = ProjectSpec::builder()
                .display_name("Parent".into())
                .role_definitions(if position >= 1 { shape("parent") } else { BTreeMap::new() })
                .build();
            projects.apply(&InputMeta::builder().name("parent".into()).build(), &parent).await.expect("parent");
            let child = ProjectSpec::builder()
                .display_name("Child".into())
                .parent("parent".into())
                .role_definitions(if position >= 2 { shape("project") } else { BTreeMap::new() })
                .build();
            projects.apply(&InputMeta::builder().name("child".into()).build(), &child).await.expect("child");
            if let CrewSource::Agent { selector, .. } = &mut template.vessels[0].crew[0].source {
                selector.model = (position >= 3).then(|| "convoy".into());
            }
            templates.apply(&meta, &template).await.expect("convoy layer");
            let intent = flotilla_protocol::ConvoyStartIntent::builder()
                .project_ref("child".into())
                .agent_overrides(if position == 4 { vec!["code=codex:dispatch".parse().expect("override")] } else { Vec::new() })
                .build();
            let (_, frozen) = daemon
                .convoy_admission
                .resolve_convoy_admission_workflow("flotilla", "child", &child, &[], &intent)
                .await
                .expect("admission");
            let expected = ["fleet", "parent", "project", "convoy", "dispatch"][position];
            let layer = ["project:fleet", "project:parent", "project:child", "convoy:workflow", "dispatch"][position];
            let CrewSource::Agent { selector, .. } = &frozen.vessels[0].crew[0].source else { panic!("agent") };
            assert_eq!(selector.model.as_deref(), Some(expected));
            assert_eq!(frozen.cascade.as_ref().expect("provenance").settings["roles.coder.model"].layer, layer);
            assert_eq!(selector.adapter.as_deref(), Some(if position == 4 { "codex" } else { "claude-code" }));
        }
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
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let repository = RepositorySpec::remote("https://github.com/acme/repo").expect("repository");
        let key = repository.key();
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(key.to_string()).build(), &repository)
            .await
            .expect("repository declaration");
        let tracker = Arc::new(FakeChangeRequest::new());
        tracker
            .add_change_requests(vec![("7".to_string(), ChangeRequest {
                title: "Ready".to_string(),
                branch: "feature".to_string(),
                status: flotilla_protocol::ChangeRequestStatus::Open,
                body: None,
                provider_name: "fake".to_string(),
                provider_display_name: "Fake".to_string(),
            })])
            .await;
        let port = Arc::new(FakeQueryPort { backend, tracker, discoveries: AtomicUsize::new(0) });
        let source = ProviderChangeRequestObservationSource::new(port.backend.clone(), port.clone());
        let subject = ChangeRequestRef {
            namespace: "flotilla".to_string(),
            service: "github.com".to_string(),
            scope: "acme/repo".to_string(),
            number: 7,
        };

        let status = source.observe(&subject).await.expect("observed change request");
        assert_eq!(status.state.value, Some(ObservedChangeRequestState::Open));
        assert_eq!(port.discoveries.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn need_composition_unites_project_role_and_dispatch_needs() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"admission-needs-test\"\n").expect("daemon config");
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            ResourceBackend::InMemory(InMemoryBackend::default()),
        )
        .await;
        let mut project = ProjectSpec::builder().display_name("Example".to_string()).default_workflow_ref("work".to_string()).build();
        project.role_needs.insert("coder".to_string(), BTreeSet::from([CapabilityNeed::Platform("linux".to_string())]));
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".to_string())
                .crew(vec![CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "true".to_string() }).build()])
                .build()])
            .build();
        let mut intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".to_string()).build();
        intent.needs.push("gui_session".to_string());

        let needs =
            daemon.convoy_admission.compose_convoy_needs("flotilla", &project, &[], &intent, &mut workflow).await.expect("composed needs");
        assert_eq!(needs, BTreeSet::from([CapabilityNeed::Platform("linux".to_string()), CapabilityNeed::GuiSession]));
        assert_eq!(workflow.vessels[0].crew[0].needs, needs);
    }
    #[tokio::test]
    async fn codex_adapter_floor_is_composed_without_a_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"codex-floor-test\"\n").expect("daemon config");
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("test-host"),
            ResourceBackend::InMemory(InMemoryBackend::default()),
        )
        .await;
        let project = ProjectSpec::builder().display_name("Example".into()).default_workflow_ref("work".into()).build();
        let mut workflow = WorkflowTemplateSpec::builder()
            .vessels(vec![VesselRequirement::builder()
                .name("work".into())
                .crew(vec![CrewSpec::builder()
                    .role("coder".into())
                    .source(CrewSource::Agent {
                        selector: flotilla_resources::Selector::for_capability("code"),
                        prompt: None,
                        brief_template: None,
                    })
                    .build()])
                .build()])
            .build();
        let intent = flotilla_protocol::ConvoyStartIntent::builder().project_ref("example".into()).build();
        let needs = daemon.convoy_admission.compose_convoy_needs("flotilla", &project, &[], &intent, &mut workflow).await.expect("needs");
        assert!(needs.contains(&CapabilityNeed::Harness { adapter: "codex".into(), minimum_version: "0.160.0".into() }));
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
            let policy = ResourceObject::<PlacementPolicy> {
                metadata: metadata.clone(),
                spec: PlacementPolicySpec::builder()
                    .pool("test".to_string())
                    .priority(0)
                    .host_direct(HostDirectPlacementPolicySpec {
                        host_ref: "host".to_string(),
                        checkout: HostDirectPlacementPolicyCheckout::Worktree,
                    })
                    .build(),
                status: None,
            };
            KindCandidate {
                kind: ResourceObject::<FulfilmentKind> {
                    metadata,
                    spec: FulfilmentKindSpec::builder()
                        .host_ref("host".to_string())
                        .pool("test".to_string())
                        .cost_class(cost_class)
                        .realisation(FulfilmentRealisation::HostDirect)
                        .build(),
                    status: None,
                },
                placement: PlacementResolution {
                    selected: Some(policy),
                    refused_candidates: Vec::new(),
                    viable_not_selected: Vec::new(),
                    allocation: None,
                },
                free_slots: slots,
                host_ready: ready,
                sleeping_until,
            }
        };
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
            })
            .collect::<Vec<_>>();
        let rule = flotilla_resources::TurnDeliveryRule::builder()
            .on("$cr.mergeable == conflicting".parse().expect("leaf"))
            .to(flotilla_resources::TurnDeliveryTarget::builder().vessel("verify".to_string()).role("verify".to_string()).build())
            .brief("Verify the result".to_string())
            .hold(HoldAct::ChangeRequestComment { body: "Verification needed".to_string() })
            .build();
        let mut workflow = WorkflowTemplateSpec::builder().turn_delivery(indexmap::IndexMap::from([("verify".to_string(), rule)])).build();
        let error = allocate_roles(&mut workflow, &roles).expect_err("ambiguous delivery must refuse");
        assert!(error.contains("multiple vessels"), "{error}");
        workflow.turn_delivery["verify"].to.vessel = "verify[macos]".to_string();
        allocate_roles(&mut workflow, &roles).expect("explicit concrete target");
        assert_eq!(workflow.turn_delivery["verify"].to.vessel, "verify[macos]");
    }
}
