use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    path::PathBuf,
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_core::agent_adapter::{
    append_convoy_work_context, build_crew_brief_with_options, CapabilityTable, CrewAssignment, CrewBriefMember, CrewBriefTemplateResolver,
};
use flotilla_protocol::PlacementDecision;
use flotilla_resources::{
    canonicalize_repo_url, clone_key,
    controller::{
        delete_lifecycle_owned_matching, Actuation, LabelJoinWatch, LabelMappedWatch, ReconcileOutcome, Reconciler, SecondaryWatch,
    },
    repository_workspace_slugs, Checkout, CheckoutPhase, CheckoutSpec, CheckoutWorktreeSpec, Clone, ClonePhase, CloneSpec, Convoy,
    CrewSource, CrewWorkPhase, DockerCheckoutStrategy, DockerEnvironmentSpec, DockerImagePullPolicy, Environment, EnvironmentMount,
    EnvironmentMountMode, EnvironmentPhase, EnvironmentSpec, EnvironmentWaitReason, FreshCloneCheckoutSpec,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta, LifecycleAuthority, OwnerReference, PlacementPolicy,
    PlacementPolicySpec, ReplicaReadResolver, Repository, RepositoryIdentity, RepositoryKey, RepositorySpec, Resource, ResourceBackend,
    ResourceError, ResourceObject, ResourceProvenance, Stance, TerminalSession, TerminalSessionIdentity, TerminalSessionPhase,
    TerminalSessionSpec, TypedResolver, Vessel, VesselPhase, VesselStatusPatch, WorkPhase, ACTUATOR_HOST_REF_ANNOTATION,
    ACTUATOR_SOURCE_ROOT_ANNOTATION, CHANGE_REQUEST_ID_LABEL, CONVOY_LABEL, CREDENTIAL_REFS_ANNOTATION, CREDENTIAL_REFS_ENV,
    VESSEL_REF_LABEL,
};
use tracing::warn;

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
const REPO_LABEL: &str = "flotilla.work/repo";
const ENV_LABEL: &str = "flotilla.work/env";
const VESSEL_PROVISIONING_REQUEUE_AFTER: Duration = Duration::from_secs(5);
const VESSEL_INTERRUPTED_REQUEUE_AFTER: Duration = Duration::from_millis(250);
const VESSEL_PROVISIONING_STUCK_SECONDS: i64 = 2 * 60;

#[derive(bon::Builder)]
pub struct VesselReconciler {
    convoys: TypedResolver<Convoy>,
    repositories: TypedResolver<Repository>,
    placement_policies: TypedResolver<PlacementPolicy>,
    environments: TypedResolver<Environment>,
    clones: TypedResolver<Clone>,
    checkouts: TypedResolver<Checkout>,
    terminal_sessions: TypedResolver<TerminalSession>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    federated_placement_policies: Option<ReplicaReadResolver<PlacementPolicy>>,
    local_host_ref: Option<String>,
    namespace: String,
    brief_templates: CrewBriefTemplateResolver,
}

impl VesselReconciler {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            convoys: backend.clone().using::<Convoy>(namespace),
            repositories: backend.clone().using::<Repository>(namespace),
            placement_policies: backend.clone().using::<PlacementPolicy>(namespace),
            environments: backend.clone().using::<Environment>(namespace),
            clones: backend.clone().using::<Clone>(namespace),
            checkouts: backend.clone().using::<Checkout>(namespace),
            terminal_sessions: backend.using::<TerminalSession>(namespace),
            federated_convoys: None,
            federated_placement_policies: None,
            local_host_ref: None,
            namespace: namespace.to_string(),
            brief_templates: CrewBriefTemplateResolver::default(),
        }
    }

    pub fn new_with_config_dir(backend: ResourceBackend, namespace: &str, config_dir: impl Into<PathBuf>) -> Self {
        Self { brief_templates: CrewBriefTemplateResolver::with_config_dir(config_dir), ..Self::new(backend, namespace) }
    }

    pub fn with_federated_dependencies(mut self, backend: &ResourceBackend, local_host_ref: impl Into<String>) -> Self {
        self.federated_convoys = Some(backend.including_replicas::<Convoy>(&self.namespace));
        self.federated_placement_policies = Some(backend.including_replicas::<PlacementPolicy>(&self.namespace));
        self.local_host_ref = Some(local_host_ref.into());
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Vessel>>> {
        vec![
            Box::new(LabelMappedWatch::<Environment, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Checkout, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
            Box::new(LabelJoinWatch::<Checkout, Vessel> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<TerminalSession, Vessel> { label_key: VESSEL_REF_LABEL, _marker: PhantomData }),
        ]
    }

    async fn dependency<T: Resource>(
        local: &TypedResolver<T>,
        federated: Option<&ReplicaReadResolver<T>>,
        vessel: &ResourceObject<Vessel>,
        name: &str,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let Some(origin) = vessel.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) else {
            return local.get(name).await;
        };
        let Some(federated) = federated else {
            return Err(ResourceError::not_found(name));
        };
        federated
            .list()
            .await?
            .items
            .into_iter()
            .find(|source| {
                source.object.metadata.name == name
                    && matches!(
                        &source.provenance,
                        ResourceProvenance::Replica { origin_root, .. } if origin_root.as_str() == origin
                    )
            })
            .map(|source| source.object)
            .ok_or_else(|| ResourceError::not_found(name))
    }

    fn missing_host_local_environment(&self, environment_ref: &str, placement_host_ref: &str) -> String {
        let reconciler_host_ref = self.local_host_ref.as_deref().unwrap_or("unknown");
        let message = format!(
            "host-local Environment {environment_ref} for placement host {placement_host_ref} was not found in the resource store for reconciler host {reconciler_host_ref}"
        );
        warn!(
            %environment_ref,
            %placement_host_ref,
            %reconciler_host_ref,
            "host-local Environment lookup failed in Vessel reconciliation"
        );
        message
    }
}

#[derive(Debug, Clone)]
enum PlacementStrategy {
    HostDirect {
        host_ref: String,
        pool: String,
    },
    DockerWorktreeOnHostAndMount {
        host_ref: String,
        pool: String,
        image: String,
        pull_policy: DockerImagePullPolicy,
        env: BTreeMap<String, String>,
        mount_path: String,
        default_cwd: Option<String>,
    },
    DockerFreshCloneInContainer {
        host_ref: String,
        pool: String,
        image: String,
        pull_policy: DockerImagePullPolicy,
        env: BTreeMap<String, String>,
        clone_path: String,
        default_cwd: Option<String>,
    },
}

#[derive(Debug, Clone)]
enum PlannedPatch {
    None,
    Provisioning {
        observed_policy_ref: String,
        observed_policy_version: String,
        placement_decision: Option<PlacementDecision>,
        waiting_for: String,
        wait_reason: Option<EnvironmentWaitReason>,
    },
    Ready {
        placement_decision: Option<PlacementDecision>,
        environment_ref: String,
        image: Option<ImageStamp>,
        checkout_refs: BTreeMap<RepositoryKey, String>,
        terminal_session_refs: Vec<String>,
        requested_stance: Stance,
        effective_stance: Stance,
    },
    Interrupted {
        roles: BTreeSet<String>,
        message: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone)]
struct ImageStamp {
    image_ref: String,
    image_digest: String,
}

#[derive(Debug, Clone)]
pub struct VesselDeps {
    patch: PlannedPatch,
    actuations: Vec<Actuation>,
}

impl VesselDeps {
    fn none() -> Self {
        Self { patch: PlannedPatch::None, actuations: Vec::new() }
    }

    fn provisioning(
        placement_policy: &ResourceObject<PlacementPolicy>,
        placement_decision: Option<PlacementDecision>,
        waiting_for: impl Into<String>,
        actuations: Vec<Actuation>,
    ) -> Self {
        let waiting_for = legible_waiting_for(waiting_for.into(), placement_decision.as_ref());
        Self { patch: provisioning_patch(placement_policy, placement_decision, waiting_for, None), actuations }
    }

    fn provisioning_for_environment(
        placement_policy: &ResourceObject<PlacementPolicy>,
        placement_decision: Option<PlacementDecision>,
        environment: &ResourceObject<Environment>,
        fallback: impl Into<String>,
        actuations: Vec<Actuation>,
    ) -> Self {
        let fallback = fallback.into();
        let (waiting_for, wait_reason) = environment
            .status
            .as_ref()
            .map(|status| (status.message.clone().unwrap_or_else(|| fallback.clone()), status.wait_reason.clone()))
            .unwrap_or((fallback, None));
        let waiting_for = legible_waiting_for(waiting_for, placement_decision.as_ref());
        Self { patch: provisioning_patch(placement_policy, placement_decision, waiting_for, wait_reason), actuations }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self { patch: PlannedPatch::Failed { message: message.into() }, actuations: Vec::new() }
    }

    fn interrupted(role: String, terminal_name: &str, restart: bool, mut actuations: Vec<Actuation>) -> Self {
        if restart {
            actuations.push(Actuation::RestartTerminalSession { name: terminal_name.to_string() });
        }
        Self {
            patch: PlannedPatch::Interrupted {
                roles: BTreeSet::from([role]),
                message: format!("crew terminal session {terminal_name} disappeared; relaunching from its durable brief"),
            },
            actuations,
        }
    }
}

impl Reconciler for VesselReconciler {
    type Resource = Vessel;
    type Dependencies = VesselDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
            return Ok(VesselDeps::none());
        }

        let convoy = match Self::dependency(&self.convoys, self.federated_convoys.as_ref(), obj, &obj.spec.convoy_ref).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(VesselDeps::failed(format!("convoy {} not found", obj.spec.convoy_ref))),
            Err(err) => return Err(err),
        };
        let placement_decision = convoy.status.as_ref().and_then(|status| status.placement_decision.clone());
        if self
            .local_host_ref
            .as_ref()
            .zip(placement_decision.as_ref())
            .is_some_and(|(local_host_ref, decision)| decision.target_host.reference != *local_host_ref)
        {
            return Ok(VesselDeps::none());
        }
        let placement_policy = match Self::dependency(
            &self.placement_policies,
            self.federated_placement_policies.as_ref(),
            obj,
            &obj.spec.placement_policy_ref,
        )
        .await
        {
            Ok(policy) => policy,
            Err(ResourceError::NotFound { .. }) => {
                return Ok(VesselDeps::failed(format!("placement policy {} not found", obj.spec.placement_policy_ref)))
            }
            Err(err) => return Err(err),
        };
        let strategy = match placement_strategy(&placement_policy.spec) {
            Ok(strategy) => strategy,
            Err(message) => return Ok(VesselDeps::failed(message)),
        };
        let declared_agent_adapters =
            placement_policy.spec.docker_per_vessel.as_ref().map(|docker| docker.agent_adapters.clone()).unwrap_or_default();

        let (vessel_index, requirement) = match convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == obj.spec.vessel_name))
        {
            Some((vessel_index, requirement)) => (vessel_index, requirement),
            None => return Ok(VesselDeps::failed(format!("vessel {} missing from convoy snapshot", obj.spec.vessel_name))),
        };
        let capabilities = CapabilityTable::seeded();
        let mut required_agent_adapters = BTreeSet::new();
        for process in &requirement.crew {
            if let CrewSource::Agent { selector, .. } = &process.source {
                match capabilities.resolve(&selector.capability) {
                    Ok(agent) => {
                        required_agent_adapters.insert(agent.adapter.clone());
                    }
                    Err(message) => return Ok(VesselDeps::failed(message)),
                }
            }
        }
        let effective_stance = strategy.effective_stance();
        if effective_stance < requirement.stance {
            return Ok(VesselDeps::failed(format!(
                "vessel {} requires {} stance, but placement policy {} uses {} placement with effective {} stance",
                obj.spec.vessel_name,
                requirement.stance,
                placement_policy.metadata.name,
                strategy.description(),
                effective_stance,
            )));
        }

        let repository_refs = requirement
            .repository_refs
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        if repository_refs.is_empty() && requirement.repository_refs.is_some() {
            return Ok(VesselDeps::failed(format!("vessel {} has an empty repository scope", obj.spec.vessel_name)));
        }
        let git_ref = if repository_refs.is_empty() {
            None
        } else {
            match convoy.spec.r#ref.clone() {
                Some(git_ref) => Some(git_ref),
                None => return Ok(VesselDeps::failed("convoy ref is missing".to_string())),
            }
        };
        let checkout_slug = git_ref.as_deref().map(checkout_path_component);
        let convoy_checkout_slug = checkout_path_component(&convoy.metadata.name);
        if repository_refs.len() > 1 && !obj.spec.adopted_checkout_refs.is_empty() {
            return Ok(VesselDeps::failed("adopted checkouts are not supported for multi-repository vessel workspaces".to_string()));
        }
        let mut convoy_repositories = Vec::new();
        for repo_ref in &repository_refs {
            let Some(repository) = convoy.spec.repositories.iter().find(|repository| &repository.repo_ref == repo_ref) else {
                return Ok(VesselDeps::failed(format!(
                    "vessel {} references repository {repo_ref} outside its convoy",
                    obj.spec.vessel_name
                )));
            };
            if convoy_repositories.iter().any(|existing: &&flotilla_resources::ConvoyRepositorySpec| existing.repo_ref == *repo_ref) {
                return Ok(VesselDeps::failed(format!("vessel {} repository scope contains duplicate {repo_ref}", obj.spec.vessel_name)));
            }
            convoy_repositories.push(repository);
        }
        let multi_repository = convoy.spec.repositories.len() > 1;
        let workspace_slugs = convoy_repositories
            .iter()
            .map(|repository| (repository.repo_ref.clone(), repository.workspace_slug.clone()))
            .collect::<BTreeMap<_, _>>();

        let clone_env_ref = host_direct_environment_name(strategy.host_ref());
        let mut actuations = Vec::new();
        let shared_clone_root = if strategy.needs_shared_clone() {
            let clone_env = match self.environments.get(&clone_env_ref).await {
                Ok(environment) => environment,
                Err(ResourceError::NotFound { .. }) => {
                    return Ok(VesselDeps::failed(self.missing_host_local_environment(&clone_env_ref, strategy.host_ref())))
                }
                Err(err) => return Err(err),
            };
            let clone_env_spec = match clone_env.spec.host_direct.as_ref() {
                Some(spec) => spec,
                None => return Ok(VesselDeps::failed(format!("environment {clone_env_ref} is not a host_direct environment"))),
            };
            if clone_env.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                return Ok(VesselDeps::provisioning(
                    &placement_policy,
                    placement_decision.clone(),
                    format!("environment {clone_env_ref} to become ready"),
                    actuations,
                ));
            }
            Some(clone_env_spec.repo_default_dir.clone())
        } else {
            None
        };

        let precreated_environment = match &strategy {
            PlacementStrategy::DockerFreshCloneInContainer { host_ref, image, pull_policy, env, .. } => {
                let env_name = environment_name(&obj.metadata.name);
                let image = match self.environments.get(&env_name).await {
                    Ok(existing) => {
                        if existing.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Failed) {
                            let message = existing
                                .status
                                .as_ref()
                                .and_then(|status| status.message.clone())
                                .unwrap_or_else(|| format!("environment {env_name} failed"));
                            return Ok(VesselDeps::failed(message));
                        }
                        if existing.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                            let waiting_for = existing
                                .status
                                .as_ref()
                                .and_then(|status| status.message.clone())
                                .unwrap_or_else(|| format!("environment {env_name} to become ready"));
                            return Ok(VesselDeps::provisioning_for_environment(
                                &placement_policy,
                                placement_decision.clone(),
                                &existing,
                                waiting_for,
                                actuations,
                            ));
                        }
                        match image_stamp(&existing) {
                            Ok(image) => image,
                            Err(message) => return Ok(VesselDeps::failed(message)),
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        actuations.push(Actuation::CreateEnvironment {
                            meta: owned_child_meta(&env_name, obj, BTreeMap::new()),
                            spec: EnvironmentSpec {
                                host_direct: None,
                                docker: Some(DockerEnvironmentSpec {
                                    host_ref: host_ref.clone(),
                                    image: image.clone(),
                                    declared_agent_adapters: declared_agent_adapters.clone(),
                                    required_agent_adapters: required_agent_adapters.clone(),
                                    pull_policy: *pull_policy,
                                    mounts: Vec::new(),
                                    env: environment_with_credentials(env.clone(), &requirement.credential_refs),
                                }),
                            },
                        });
                        return Ok(VesselDeps::provisioning(
                            &placement_policy,
                            placement_decision.clone(),
                            format!("environment {env_name} to become ready"),
                            actuations,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                Some((env_name, image))
            }
            _ => None,
        };

        let workspace_root = if multi_repository {
            match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                    format!(
                        "{}/{}/{}",
                        shared_clone_root.as_deref().expect("shared-clone placement has a root").trim_end_matches('/'),
                        convoy_checkout_slug,
                        checkout_slug.as_deref().expect("repository workspace requires a branch")
                    )
                }
                PlacementStrategy::DockerFreshCloneInContainer { clone_path, .. } => clone_path.clone(),
            }
        } else {
            String::new()
        };
        let mut checkout_refs = BTreeMap::new();
        let mut checkout_paths = BTreeMap::new();
        let mut contained_worktree_checkouts = Vec::new();
        let mut waiting_for_checkouts = Vec::new();
        let mut fork_stance = false;
        for convoy_repository in convoy_repositories {
            let repository_spec = match self.repositories.get(&convoy_repository.repo_ref.to_string()).await {
                Ok(repository) => repository.spec,
                Err(ResourceError::NotFound { .. }) => {
                    // Convoy URLs are clone transports and may use SCP-like SSH syntax.
                    // Pre-canonicalize them before constructing the identity-bearing spec.
                    let repository_spec = match canonicalize_repo_url(&convoy_repository.url).and_then(RepositorySpec::remote) {
                        Ok(spec) => spec,
                        Err(message) => {
                            return Ok(VesselDeps::failed(format!(
                                "convoy repository {} has invalid URL: {message}",
                                convoy_repository.repo_ref
                            )))
                        }
                    };
                    if let Err(message) = repository_spec.verify_key(&convoy_repository.repo_ref) {
                        return Ok(VesselDeps::failed(message));
                    }
                    actuations.push(Actuation::CreateRepository { key: convoy_repository.repo_ref.clone(), spec: repository_spec.clone() });
                    repository_spec
                }
                Err(err) => return Err(err),
            };
            if let Err(message) = repository_spec.verify_key(&convoy_repository.repo_ref) {
                return Ok(VesselDeps::failed(message));
            }
            fork_stance |= repository_spec.is_fork();
            let canonical_repo = match repository_spec.identity() {
                RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
                RepositoryIdentity::Local { .. } => return Ok(VesselDeps::failed("convoy repository must have a transport remote")),
            };
            let repository_key = convoy_repository.repo_ref.clone();
            let repo_key = repository_key.to_string();
            let adopted_checkout_ref = obj.spec.adopted_checkout_refs.get(&repository_key).cloned();
            let clone_name = if adopted_checkout_ref.is_none() && strategy.needs_shared_clone() {
                let clone_name = format!("clone-{}", clone_key(&canonical_repo, &clone_env_ref));
                match self.clones.get(&clone_name).await {
                    Ok(existing) => {
                        if existing.spec.repo_ref != repository_key || existing.spec.env_ref != clone_env_ref {
                            return Ok(VesselDeps::failed(format!("clone {clone_name} does not match expected repo/env tuple")));
                        }
                        if existing.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Failed) {
                            let message = existing
                                .status
                                .as_ref()
                                .and_then(|status| status.message.clone())
                                .unwrap_or_else(|| format!("clone {clone_name} failed"));
                            return Ok(VesselDeps::failed(message));
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        let repository_catalog = self.repositories.list().await?;
                        let keyed_repositories = repository_catalog
                            .items
                            .iter()
                            .filter(|candidate| candidate.spec.key() != repository_key)
                            .map(|candidate| (candidate.spec.key(), &candidate.spec))
                            .chain(std::iter::once((repository_key.clone(), &repository_spec)))
                            .collect::<Vec<_>>();
                        let clone_directory_slug = repository_workspace_slugs(keyed_repositories.iter().map(|(key, spec)| (key, *spec)))
                            .remove(&repository_key)
                            .expect("the current repository was included in slug allocation");
                        let clone_path = format!(
                            "{}/{}",
                            shared_clone_root.as_deref().expect("shared-clone placement has a root").trim_end_matches('/'),
                            clone_directory_slug
                        );
                        actuations.push(Actuation::CreateClone {
                            meta: InputMeta::builder()
                                .name(clone_name.clone())
                                .labels(BTreeMap::from([
                                    (REPO_KEY_LABEL.to_string(), repo_key.clone()),
                                    (ENV_LABEL.to_string(), clone_env_ref.clone()),
                                    (REPO_LABEL.to_string(), convoy_repository.workspace_slug.clone()),
                                ]))
                                .build(),
                            spec: CloneSpec {
                                repo_ref: repository_key.clone(),
                                url: convoy_repository.url.clone(),
                                env_ref: clone_env_ref.clone(),
                                path: clone_path,
                            },
                        });
                    }
                    Err(err) => return Err(err),
                }
                Some(clone_name)
            } else {
                None
            };
            let checkout_name = adopted_checkout_ref.clone().unwrap_or_else(|| match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                    checkout_name(&convoy.metadata.name, &convoy_repository.workspace_slug, multi_repository)
                }
                PlacementStrategy::DockerFreshCloneInContainer { .. } => {
                    checkout_name(&obj.metadata.name, &convoy_repository.workspace_slug, multi_repository)
                }
            });
            let checkout_target_path = match &strategy {
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                    if multi_repository {
                        format!("{workspace_root}/{}", convoy_repository.workspace_slug)
                    } else {
                        checkout_target_path(
                            shared_clone_root.as_deref().expect("shared-clone placement has a root"),
                            &convoy_checkout_slug,
                            &convoy_repository.workspace_slug,
                            checkout_slug.as_deref().expect("repository checkout requires a branch"),
                        )
                    }
                }
                PlacementStrategy::DockerFreshCloneInContainer { clone_path, .. } => {
                    if multi_repository {
                        format!("{}/{}", clone_path.trim_end_matches('/'), convoy_repository.workspace_slug)
                    } else {
                        clone_path.clone()
                    }
                }
            };
            match self.checkouts.get(&checkout_name).await {
                Ok(existing) => {
                    if existing.spec.repo_ref() != &repository_key {
                        return Ok(VesselDeps::failed(format!("checkout {checkout_name} belongs to a different repository")));
                    }
                    if adopted_checkout_ref.is_some() && existing.metadata.lifecycle_authority()? != Some(LifecycleAuthority::Adopted) {
                        return Ok(VesselDeps::failed(format!("checkout {checkout_name} is not adopted")));
                    }
                    if existing.status.as_ref().map(|status| status.phase) == Some(CheckoutPhase::Failed) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("checkout {checkout_name} failed"));
                        return Ok(VesselDeps::failed(message));
                    }
                    if existing.status.as_ref().map(|status| status.phase) == Some(CheckoutPhase::Ready) {
                        let Some(path) = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.path.clone())
                            .or_else(|| existing.spec.target_path().map(str::to_string))
                        else {
                            return Ok(VesselDeps::failed(format!("checkout {checkout_name} is ready but has no target path")));
                        };
                        if matches!(&strategy, PlacementStrategy::DockerWorktreeOnHostAndMount { .. }) {
                            contained_worktree_checkouts.push((checkout_name.clone(), match &existing.spec {
                                CheckoutSpec::Worktree(spec) => Some(spec.clone_ref.clone()),
                                CheckoutSpec::FreshClone(_) | CheckoutSpec::Observed(_) => None,
                            }));
                        }
                        checkout_refs.insert(repository_key.clone(), checkout_name);
                        checkout_paths.insert(repository_key, path);
                    } else {
                        waiting_for_checkouts.push(checkout_name);
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if adopted_checkout_ref.is_some() {
                        return Ok(VesselDeps::failed(format!("adopted checkout {checkout_name} not found")));
                    }
                    let spec = match &strategy {
                        PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerWorktreeOnHostAndMount { .. } => {
                            CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                                repo_ref: repository_key.clone(),
                                env_ref: clone_env_ref.clone(),
                                r#ref: git_ref.clone().expect("repository checkout requires a convoy ref"),
                                base_ref: Some(convoy_repository.source_ref.clone()),
                                target_path: checkout_target_path,
                                clone_ref: clone_name.expect("shared-clone strategy requires clone name"),
                            })
                        }
                        PlacementStrategy::DockerFreshCloneInContainer { .. } => CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
                            repo_ref: repository_key.clone(),
                            env_ref: precreated_environment
                                .as_ref()
                                .map(|(environment_ref, _)| environment_ref.clone())
                                .expect("fresh-clone strategy should precreate environment"),
                            r#ref: git_ref.clone().expect("repository checkout requires a convoy ref"),
                            base_ref: Some(convoy_repository.source_ref.clone()),
                            target_path: checkout_target_path,
                            url: convoy_repository.url.clone(),
                        }),
                    };
                    let env_ref = spec.env_ref().expect("managed checkout should have an env_ref").to_string();
                    let mut labels = BTreeMap::from([(ENV_LABEL.to_string(), env_ref), (REPO_KEY_LABEL.to_string(), repo_key)]);
                    if let Some(change_request) =
                        convoy.spec.change_request.as_ref().filter(|change_request| change_request.repository_ref == repository_key)
                    {
                        labels.insert(CHANGE_REQUEST_ID_LABEL.to_string(), change_request.id.clone());
                    }
                    let meta = convoy_owned_child_meta(&checkout_name, &convoy, obj, labels);
                    actuations.push(Actuation::CreateCheckout { meta, spec });
                    waiting_for_checkouts.push(checkout_name);
                }
                Err(err) => return Err(err),
            }
        }
        if !waiting_for_checkouts.is_empty() {
            let checkout_label = if waiting_for_checkouts.len() == 1 { "checkout" } else { "checkouts" };
            return Ok(VesselDeps::provisioning(
                &placement_policy,
                placement_decision.clone(),
                format!("{checkout_label} {} to become ready", waiting_for_checkouts.join(", ")),
                actuations,
            ));
        }
        let has_repositories = !repository_refs.is_empty();
        let workspace_root = if multi_repository {
            workspace_root
        } else if let Some(path) = checkout_paths.values().next() {
            path.clone()
        } else {
            shared_clone_root.clone().unwrap_or_default()
        };
        let (resolved_environment_ref, image) = match &strategy {
            PlacementStrategy::HostDirect { host_ref, .. } => {
                let env_name = host_direct_environment_name(host_ref);
                let environment = match self.environments.get(&env_name).await {
                    Ok(environment) => environment,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(VesselDeps::failed(self.missing_host_local_environment(&env_name, host_ref)))
                    }
                    Err(err) => return Err(err),
                };
                if environment.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Failed) {
                    return Ok(VesselDeps::failed(format!("environment {env_name} failed")));
                }
                if environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                    return Ok(VesselDeps::provisioning(
                        &placement_policy,
                        placement_decision.clone(),
                        format!("environment {env_name} to become ready"),
                        actuations,
                    ));
                }
                (env_name, None)
            }
            PlacementStrategy::DockerWorktreeOnHostAndMount { host_ref, image, pull_policy, env, mount_path, .. } => {
                let env_name = environment_name(&obj.metadata.name);
                let image = match self.environments.get(&env_name).await {
                    Ok(existing) => {
                        if existing.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Failed) {
                            let message = existing
                                .status
                                .as_ref()
                                .and_then(|status| status.message.clone())
                                .unwrap_or_else(|| format!("environment {env_name} failed"));
                            return Ok(VesselDeps::failed(message));
                        }
                        if existing.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready) {
                            let waiting_for = existing
                                .status
                                .as_ref()
                                .and_then(|status| status.message.clone())
                                .unwrap_or_else(|| format!("environment {env_name} to become ready"));
                            return Ok(VesselDeps::provisioning_for_environment(
                                &placement_policy,
                                placement_decision.clone(),
                                &existing,
                                waiting_for,
                                actuations,
                            ));
                        }
                        match image_stamp(&existing) {
                            Ok(image) => image,
                            Err(message) => return Ok(VesselDeps::failed(message)),
                        }
                    }
                    Err(ResourceError::NotFound { .. }) => {
                        let mut mounts = has_repositories
                            .then(|| EnvironmentMount {
                                source_path: workspace_root.clone(),
                                target_path: mount_path.clone(),
                                mode: EnvironmentMountMode::Rw,
                            })
                            .into_iter()
                            .collect::<Vec<_>>();
                        // A linked worktree's `.git` file names its administrative
                        // directory beneath the shared clone with an absolute host
                        // path. Expose the clone metadata at that same path so Git
                        // can follow the pointer from the differently-mounted
                        // workspace. Keeping the shared clone authoritative also
                        // preserves its remotes and host worktree registrations.
                        let mut mounted_common_dirs = BTreeSet::new();
                        for (checkout_name, clone_ref) in &contained_worktree_checkouts {
                            let Some(clone_ref) = clone_ref else {
                                return Ok(VesselDeps::failed(format!(
                                    "contained worktree placement requires checkout {checkout_name} to be a managed worktree"
                                )));
                            };
                            let clone = match self.clones.get(clone_ref).await {
                                Ok(clone) => clone,
                                Err(ResourceError::NotFound { .. }) => {
                                    return Ok(VesselDeps::failed(format!(
                                        "contained worktree checkout {checkout_name} refers to missing clone {clone_ref}"
                                    )))
                                }
                                Err(error) => return Err(error),
                            };
                            mounted_common_dirs.insert(format!("{}/.git", clone.spec.path.trim_end_matches('/')));
                        }
                        mounts.extend(mounted_common_dirs.into_iter().map(|git_common_dir| EnvironmentMount {
                            source_path: git_common_dir.clone(),
                            target_path: git_common_dir,
                            mode: EnvironmentMountMode::Rw,
                        }));
                        actuations.push(Actuation::CreateEnvironment {
                            meta: owned_child_meta(&env_name, obj, BTreeMap::new()),
                            spec: EnvironmentSpec {
                                host_direct: None,
                                docker: Some(DockerEnvironmentSpec {
                                    host_ref: host_ref.clone(),
                                    image: image.clone(),
                                    declared_agent_adapters: declared_agent_adapters.clone(),
                                    required_agent_adapters: required_agent_adapters.clone(),
                                    pull_policy: *pull_policy,
                                    mounts,
                                    env: environment_with_credentials(env.clone(), &requirement.credential_refs),
                                }),
                            },
                        });
                        return Ok(VesselDeps::provisioning(
                            &placement_policy,
                            placement_decision.clone(),
                            format!("environment {env_name} to become ready"),
                            actuations,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                (env_name, Some(image))
            }
            PlacementStrategy::DockerFreshCloneInContainer { .. } => {
                let (environment_ref, image) = precreated_environment.expect("fresh-clone strategy should resolve environment first");
                (environment_ref, Some(image))
            }
        };
        let terminal_cwd = strategy.terminal_cwd(&workspace_root, multi_repository, has_repositories);
        let issue_repository_refs = convoy.spec.issues.iter().map(|issue| issue.repository_ref.as_ref()).collect::<Vec<_>>();
        let copy_to_all_repositories = issue_repository_refs.iter().any(|repository_ref| repository_ref.is_none());
        let brief_copies = checkout_paths
            .iter()
            .filter(|(repo_ref, _)| {
                copy_to_all_repositories
                    || issue_repository_refs.is_empty()
                    || issue_repository_refs.iter().flatten().any(|relevant| *relevant == *repo_ref)
            })
            .map(|(repo_ref, path)| match &strategy {
                PlacementStrategy::DockerWorktreeOnHostAndMount { mount_path, .. } => {
                    if multi_repository {
                        format!("{}/{}", mount_path.trim_end_matches('/'), workspace_slugs[repo_ref])
                    } else {
                        mount_path.clone()
                    }
                }
                PlacementStrategy::HostDirect { .. } | PlacementStrategy::DockerFreshCloneInContainer { .. } => path.clone(),
            })
            .collect::<Vec<_>>();

        let mut terminal_refs = Vec::new();
        for (crew_index, process) in requirement.crew.iter().enumerate() {
            let should_start = requirement.starts_eagerly(crew_index);
            let identity = TerminalSessionIdentity::builder()
                .vessel_ref(obj.metadata.name.clone())
                .convoy(obj.spec.convoy_ref.clone())
                .vessel(obj.spec.vessel_name.clone())
                .role(process.role.clone())
                .vessel_index(vessel_index)
                .crew_index(crew_index)
                .labels(process.labels.clone())
                .build();
            let terminal_name = identity.name();
            match self.terminal_sessions.get(&terminal_name).await {
                Ok(existing) => {
                    terminal_refs.push(terminal_name.clone());
                    let phase = existing.status.as_ref().map(|status| status.phase);
                    if phase == Some(TerminalSessionPhase::Failed) {
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(VesselDeps::failed(message));
                    }
                    if phase == Some(TerminalSessionPhase::Stopped) {
                        if matches!(process.source, CrewSource::Agent { .. }) {
                            let work_phase =
                                convoy.status.as_ref().and_then(|status| status.work.get(&obj.spec.vessel_name)).map(|work| work.phase);
                            let crew_phase = convoy
                                .status
                                .as_ref()
                                .and_then(|status| status.crew_work.get(&obj.spec.vessel_name))
                                .and_then(|crew| crew.get(&process.role))
                                .map(|work| work.phase);
                            let active = matches!(crew_phase, Some(CrewWorkPhase::Working | CrewWorkPhase::Interrupted))
                                || (matches!(crew_phase, None | Some(CrewWorkPhase::Pending))
                                    && should_start
                                    && matches!(work_phase, Some(WorkPhase::Launching | WorkPhase::Running)));
                            if !active {
                                continue;
                            }
                            return Ok(VesselDeps::interrupted(
                                process.role.clone(),
                                &terminal_name,
                                work_phase == Some(WorkPhase::Interrupted),
                                actuations,
                            ));
                        }
                        let message = existing
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("terminal session {terminal_name} stopped"));
                        return Ok(VesselDeps::failed(message));
                    }
                    if phase == Some(TerminalSessionPhase::Starting) || (should_start && phase != Some(TerminalSessionPhase::Running)) {
                        return Ok(VesselDeps::provisioning(
                            &placement_policy,
                            placement_decision.clone(),
                            format!("terminal session {terminal_name} to become running"),
                            actuations,
                        ));
                    }
                }
                Err(ResourceError::NotFound { .. }) => {
                    if !should_start {
                        continue;
                    }
                    let source = match &process.source {
                        CrewSource::Tool { command } => flotilla_resources::TerminalSessionSource::Tool { command: command.clone() },
                        CrewSource::Agent { selector, prompt, brief_template } => {
                            let context = flotilla_resources::TerminalCrewContext {
                                namespace: self.namespace.clone(),
                                convoy: obj.spec.convoy_ref.clone(),
                                vessel_ref: obj.metadata.name.clone(),
                            };
                            let members = requirement
                                .crew
                                .iter()
                                .enumerate()
                                .map(|(index, member)| CrewBriefMember {
                                    role: member.role.clone(),
                                    state: if requirement.starts_eagerly(index) { "active" } else { "latent" }.to_string(),
                                    is_agent: matches!(member.source, CrewSource::Agent { .. }),
                                })
                                .collect::<Vec<_>>();
                            let assignment = match prompt.as_deref() {
                                Some(prompt) => CrewAssignment::Prompt(prompt),
                                None if !convoy.spec.issues.is_empty() => CrewAssignment::CarriedIssue,
                                None if convoy.spec.change_request.is_some() => CrewAssignment::CarriedChangeRequest,
                                None => CrewAssignment::Unassigned,
                            };
                            let render_options = self.brief_templates.render_options_with_fork_stance(
                                brief_template.as_deref(),
                                convoy.spec.project_ref.as_deref(),
                                checkout_paths.values().map(PathBuf::from),
                                fork_stance,
                            );
                            let mut brief = match build_crew_brief_with_options(
                                &context,
                                &obj.spec.vessel_name,
                                &process.role,
                                assignment,
                                &members,
                                &render_options,
                            ) {
                                Ok(brief) => brief,
                                Err(message) => return Ok(VesselDeps::failed(message)),
                            };
                            append_convoy_work_context(&mut brief.content, &convoy, &repository_refs);
                            brief.copies = brief_copies.clone();
                            flotilla_resources::TerminalSessionSource::Agent { selector: selector.clone(), brief, context, message: None }
                        }
                    };
                    let mut terminal_meta = identity.input_meta();
                    terminal_meta.annotations.extend(actuator_annotations(obj));
                    if !requirement.credential_refs.is_empty() {
                        terminal_meta.annotations.insert(
                            CREDENTIAL_REFS_ANNOTATION.to_string(),
                            serde_json::to_string(&requirement.credential_refs).expect("credential names serialize"),
                        );
                    }
                    actuations.push(Actuation::CreateTerminalSession {
                        meta: terminal_meta,
                        spec: TerminalSessionSpec {
                            env_ref: resolved_environment_ref.clone(),
                            role: process.role.clone(),
                            source,
                            cwd: terminal_cwd.clone(),
                            pool: strategy.pool().to_string(),
                        },
                    });
                    return Ok(VesselDeps::provisioning(
                        &placement_policy,
                        placement_decision.clone(),
                        format!("terminal session {terminal_name} to become running"),
                        actuations,
                    ));
                }
                Err(err) => return Err(err),
            }
        }

        Ok(VesselDeps {
            patch: PlannedPatch::Ready {
                placement_decision,
                environment_ref: resolved_environment_ref,
                image,
                checkout_refs,
                terminal_session_refs: terminal_refs,
                requested_stance: requirement.stance,
                effective_stance,
            },
            actuations,
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = match &deps.patch {
            PlannedPatch::None => None,
            PlannedPatch::Provisioning { observed_policy_ref, observed_policy_version, placement_decision, waiting_for, wait_reason } => {
                Some(VesselStatusPatch::MarkProvisioning {
                    observed_policy_ref: observed_policy_ref.clone(),
                    observed_policy_version: observed_policy_version.clone(),
                    placement_decision: placement_decision.clone(),
                    started_at: now,
                    message: provisioning_stuck_message(obj, waiting_for, wait_reason.as_ref(), now),
                })
            }
            PlannedPatch::Ready {
                placement_decision,
                environment_ref,
                image,
                checkout_refs,
                terminal_session_refs,
                requested_stance,
                effective_stance,
            } => Some(VesselStatusPatch::MarkReady {
                placement_decision: placement_decision.clone(),
                environment_ref: Some(environment_ref.clone()),
                image_ref: image.as_ref().map(|image| image.image_ref.clone()),
                image_digest: image.as_ref().map(|image| image.image_digest.clone()),
                checkout_refs: checkout_refs.clone(),
                terminal_session_refs: terminal_session_refs.clone(),
                requested_stance: *requested_stance,
                effective_stance: *effective_stance,
                ready_at: now,
            }),
            PlannedPatch::Interrupted { roles, message }
                if obj.status.as_ref().is_none_or(|status| {
                    status.phase != VesselPhase::Interrupted
                        || status.interrupted_roles != *roles
                        || status.message.as_deref() != Some(message.as_str())
                }) =>
            {
                Some(VesselStatusPatch::MarkInterrupted { roles: roles.clone(), message: message.clone() })
            }
            PlannedPatch::Interrupted { .. } => None,
            PlannedPatch::Failed { message } => Some(VesselStatusPatch::MarkFailed { message: message.clone() }),
        };

        let mut outcome = ReconcileOutcome::with_actuations(patch, deps.actuations.clone());
        if matches!(&deps.patch, PlannedPatch::Provisioning { .. }) {
            outcome.requeue_after = Some(VESSEL_PROVISIONING_REQUEUE_AFTER);
        } else if matches!(&deps.patch, PlannedPatch::Interrupted { .. }) {
            outcome.requeue_after = Some(VESSEL_INTERRUPTED_REQUEUE_AFTER);
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(VESSEL_REF_LABEL.to_string(), obj.metadata.name.clone())]);

        delete_lifecycle_owned_matching(&self.terminal_sessions, &selector).await?;
        // Checkouts are convoy-owned and are removed by the convoy finalizer's
        // ownership cascade, independently of vessel teardown.
        delete_lifecycle_owned_matching(&self.environments, &selector).await?;

        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/vessel-workspace-teardown")
    }
}

fn image_stamp(environment: &ResourceObject<Environment>) -> Result<ImageStamp, String> {
    let status = environment.status.as_ref().expect("ready environment has status");
    let image_ref =
        status.image_ref.clone().ok_or_else(|| format!("environment {} is missing its image ref", environment.metadata.name))?;
    let image_digest =
        status.image_digest.clone().ok_or_else(|| format!("environment {} is missing its image digest", environment.metadata.name))?;
    Ok(ImageStamp { image_ref, image_digest })
}

fn environment_with_credentials(
    mut env: BTreeMap<String, String>,
    credentials: &std::collections::BTreeSet<String>,
) -> BTreeMap<String, String> {
    if !credentials.is_empty() {
        env.insert(CREDENTIAL_REFS_ENV.to_string(), serde_json::to_string(credentials).expect("credential names serialize"));
    }
    env
}

fn placement_strategy(spec: &PlacementPolicySpec) -> Result<PlacementStrategy, String> {
    if let Some(HostDirectPlacementPolicySpec { host_ref, checkout: HostDirectPlacementPolicyCheckout::Worktree }) = &spec.host_direct {
        return Ok(PlacementStrategy::HostDirect { host_ref: host_ref.clone(), pool: spec.pool.clone() });
    }

    if let Some(docker) = &spec.docker_per_vessel {
        return match &docker.checkout {
            DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path } => Ok(PlacementStrategy::DockerWorktreeOnHostAndMount {
                host_ref: docker.host_ref.clone(),
                pool: spec.pool.clone(),
                image: docker.image.clone(),
                pull_policy: docker.pull_policy,
                env: docker.env.clone(),
                mount_path: mount_path.clone(),
                default_cwd: docker.default_cwd.clone(),
            }),
            DockerCheckoutStrategy::FreshCloneInContainer { clone_path } => Ok(PlacementStrategy::DockerFreshCloneInContainer {
                host_ref: docker.host_ref.clone(),
                pool: spec.pool.clone(),
                image: docker.image.clone(),
                pull_policy: docker.pull_policy,
                env: docker.env.clone(),
                clone_path: clone_path.clone(),
                default_cwd: docker.default_cwd.clone(),
            }),
        };
    }

    Err("placement policy must define exactly one supported strategy".to_string())
}

fn provisioning_patch(
    placement_policy: &ResourceObject<PlacementPolicy>,
    placement_decision: Option<PlacementDecision>,
    waiting_for: String,
    wait_reason: Option<EnvironmentWaitReason>,
) -> PlannedPatch {
    PlannedPatch::Provisioning {
        observed_policy_ref: placement_policy.metadata.name.clone(),
        observed_policy_version: placement_policy.metadata.resource_version.clone(),
        placement_decision,
        waiting_for,
        wait_reason,
    }
}

fn provisioning_stuck_message(
    obj: &ResourceObject<Vessel>,
    waiting_for: &str,
    wait_reason: Option<&EnvironmentWaitReason>,
    now: DateTime<Utc>,
) -> Option<String> {
    if matches!(wait_reason, Some(EnvironmentWaitReason::MaterialPoolExhausted { .. })) {
        return Some(waiting_for.to_string());
    }
    let started_at = obj.status.as_ref().filter(|status| status.phase == VesselPhase::Provisioning).and_then(|status| status.started_at)?;
    (now.signed_duration_since(started_at) >= chrono::Duration::seconds(VESSEL_PROVISIONING_STUCK_SECONDS))
        .then(|| format!("provisioning is stalled while waiting for {waiting_for}; reconciliation will continue retrying"))
}

fn host_direct_environment_name(host_ref: &str) -> String {
    format!("host-direct-{host_ref}")
}

fn legible_waiting_for(mut waiting_for: String, placement_decision: Option<&PlacementDecision>) -> String {
    if let Some(decision) = placement_decision {
        let target_environment = host_direct_environment_name(&decision.target_host.reference);
        let display_environment = host_direct_environment_name(&decision.target_host.display_name);
        waiting_for = waiting_for.replace(&target_environment, &display_environment);
    }
    waiting_for
}

#[cfg(test)]
mod material_pool_wait_tests {
    use flotilla_resources::ObjectMeta;

    use super::*;

    #[test]
    fn material_pool_wait_is_visible_immediately_in_vessel_status() {
        let now = Utc::now();
        let vessel = ResourceObject::<Vessel> {
            metadata: ObjectMeta {
                name: "vessel-a".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: now,
                merge: None,
            },
            spec: flotilla_resources::VesselSpec {
                convoy_ref: "convoy-a".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "docker-a".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
            status: None,
        };
        let message = "waiting for agent login material; 2 in pool, all leased";
        let reason = EnvironmentWaitReason::MaterialPoolExhausted { pool_ref: "agent-login".to_string() };

        assert_eq!(provisioning_stuck_message(&vessel, message, Some(&reason), now).as_deref(), Some(message));
    }
}

fn environment_name(vessel_name: &str) -> String {
    format!("env-{vessel_name}")
}

fn checkout_name(vessel_name: &str, workspace_slug: &str, multi_repository: bool) -> String {
    if multi_repository {
        format!("checkout-{vessel_name}-{workspace_slug}")
    } else {
        format!("checkout-{vessel_name}")
    }
}

fn checkout_path_component(branch: &str) -> String {
    let normalized = branch
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') { character } else { '-' })
        .collect::<String>();
    let normalized = normalized.trim_matches(['-', '.']).to_string();
    if normalized.is_empty() {
        "work".to_string()
    } else {
        normalized
    }
}

fn checkout_target_path(repo_default_dir: &str, convoy_slug: &str, repo_slug: &str, branch_slug: &str) -> String {
    format!("{}/{}/{}.{}", repo_default_dir.trim_end_matches('/'), convoy_slug, repo_slug, branch_slug)
}

fn owned_child_meta(name: &str, workspace: &ResourceObject<Vessel>, mut extra_labels: BTreeMap<String, String>) -> InputMeta {
    extra_labels.insert(VESSEL_REF_LABEL.to_string(), workspace.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .annotations(actuator_annotations(workspace))
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
            kind: Vessel::API_PATHS.kind.to_string(),
            name: workspace.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

fn actuator_annotations(workspace: &ResourceObject<Vessel>) -> BTreeMap<String, String> {
    [ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION]
        .into_iter()
        .filter_map(|key| workspace.metadata.annotations.get(key).map(|value| (key.to_string(), value.clone())))
        .collect()
}

fn convoy_owned_child_meta(
    name: &str,
    convoy: &ResourceObject<Convoy>,
    workspace: &ResourceObject<Vessel>,
    mut extra_labels: BTreeMap<String, String>,
) -> InputMeta {
    extra_labels.insert(CONVOY_LABEL.to_string(), convoy.metadata.name.clone());
    InputMeta::builder()
        .name(name.to_string())
        .labels(extra_labels)
        .annotations(actuator_annotations(workspace))
        .owner_references(vec![OwnerReference {
            api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
            kind: Convoy::API_PATHS.kind.to_string(),
            name: convoy.metadata.name.clone(),
            controller: true,
        }])
        .build()
}

impl PlacementStrategy {
    fn effective_stance(&self) -> Stance {
        match self {
            Self::HostDirect { .. } => Stance::Trusted,
            Self::DockerWorktreeOnHostAndMount { .. } | Self::DockerFreshCloneInContainer { .. } => Stance::Contained,
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Self::HostDirect { .. } => "host-direct",
            Self::DockerWorktreeOnHostAndMount { .. } => "docker-worktree",
            Self::DockerFreshCloneInContainer { .. } => "docker-fresh-clone",
        }
    }

    fn host_ref(&self) -> &str {
        match self {
            Self::HostDirect { host_ref, .. }
            | Self::DockerWorktreeOnHostAndMount { host_ref, .. }
            | Self::DockerFreshCloneInContainer { host_ref, .. } => host_ref,
        }
    }

    fn pool(&self) -> &str {
        match self {
            Self::HostDirect { pool, .. }
            | Self::DockerWorktreeOnHostAndMount { pool, .. }
            | Self::DockerFreshCloneInContainer { pool, .. } => pool,
        }
    }

    fn needs_shared_clone(&self) -> bool {
        !matches!(self, Self::DockerFreshCloneInContainer { .. })
    }

    fn terminal_cwd(&self, workspace_root: &str, multi_repository: bool, has_repositories: bool) -> String {
        match self {
            Self::HostDirect { .. } => workspace_root.to_string(),
            Self::DockerWorktreeOnHostAndMount { mount_path, default_cwd, .. } => {
                if !has_repositories {
                    default_cwd.clone().unwrap_or_else(|| "/".to_string())
                } else if multi_repository {
                    mount_path.clone()
                } else {
                    default_cwd.clone().unwrap_or_else(|| mount_path.clone())
                }
            }
            Self::DockerFreshCloneInContainer { clone_path, default_cwd, .. } => {
                if !has_repositories {
                    default_cwd.clone().unwrap_or_else(|| "/".to_string())
                } else if multi_repository {
                    clone_path.clone()
                } else {
                    default_cwd.clone().unwrap_or_else(|| clone_path.clone())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use flotilla_protocol::{PlacementDecision, PlacementTargetHost};
    use flotilla_resources::ResourceBackend;

    use super::{legible_waiting_for, VesselReconciler};

    #[derive(Clone)]
    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log output lock should be healthy").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn waiting_message_replaces_the_structured_host_direct_environment_name() {
        let decision = PlacementDecision {
            policy_name: "host-direct-test".to_string(),
            target_host: PlacementTargetHost { reference: "01HXYZ".to_string(), display_name: "kiwi".to_string() },
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
        };

        assert_eq!(
            legible_waiting_for("environment host-direct-01HXYZ to become ready".to_string(), Some(&decision)),
            "environment host-direct-kiwi to become ready"
        );
        assert_eq!(
            legible_waiting_for("checkout checkout-01HXYZ to become ready".to_string(), Some(&decision)),
            "checkout checkout-01HXYZ to become ready",
            "unrelated names containing the host ref must not be rewritten"
        );
    }

    #[test]
    fn missing_host_local_environment_warns_with_store_and_host_context() {
        let backend = ResourceBackend::InMemory(Default::default());
        let reconciler = VesselReconciler::new(backend.clone(), "flotilla").with_federated_dependencies(&backend, "feta-host");
        let log_output = Arc::new(Mutex::new(Vec::new()));
        let message;
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
            message = reconciler.missing_host_local_environment("host-direct-kiwi", "kiwi-host");
        }

        let logs =
            String::from_utf8(log_output.lock().expect("log output lock should be healthy").clone()).expect("logs should be valid UTF-8");
        assert!(logs.contains(" WARN "), "missing warning level: {logs}");
        assert!(logs.contains("environment_ref=host-direct-kiwi"), "missing Environment context: {logs}");
        assert!(logs.contains("placement_host_ref=kiwi-host"), "missing placement-host context: {logs}");
        assert!(logs.contains("reconciler_host_ref=feta-host"), "missing store-host context: {logs}");
        assert!(message.contains("resource store for reconciler host feta-host"));
    }
}
